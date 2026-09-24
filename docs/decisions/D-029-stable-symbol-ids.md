# D-029 — Content-derived, stable symbol ids

**Status:** accepted · **Date:** 2026-09-24 · **Supersedes:** nothing

## Context

`tm_codeintel::symbols::SymbolIndex` assigned each `Symbol::id` from a per-parse positional
counter (`self.next_id`, incremented once per extracted symbol, in the order `parse_file`'s
tree-sitter query happened to report matches). `CodeIntel::symbol_index()` re-parses every
currently-indexed file into a brand-new `SymbolIndex` on every call rather than caching one
(deliberately — see that method's own doc comment on staleness after `update_incremental`), and
`Store::list_files()` feeds it files in path-sorted order.

That combination means an id was never actually stable across two calls in any but the most
trivial case: adding, removing or renaming any file anywhere in the workspace shifts which
position every symbol in every *other* file lands at during the next `symbol_index()` call, even
though nothing about that other file changed. Concretely, `a.rs` sorts before `z.rs`; parsing
`a.rs` for the first time (a new file, or the workspace's first `update_incremental`) shifts every
symbol id in `z.rs` on the very next `symbol_index()` call, since the counter that produced
`z.rs`'s ids now starts from a different value.

This is more than a cosmetic churn problem. `tm-mcp`'s `symbol_def` hands an agent an `id` that is
documented (`crates/tm-mcp/src/server.rs`'s `symbol_def` doc comment) to round-trip into a later
`symbol_references`/`symbol_callers`/`symbol_callees` call *within the same server session* — and
those tool calls now refresh the index after writes (nav-agent-tool-index-refresh), so an agent
editing any file in the workspace between `symbol_def` and, say, `symbol_callers` could silently
get a `symbol_id` that now names a *different* symbol, or none at all, with no error to signal it.

## Decision

`Symbol::id` is now derived from content, not parse position: `stable_symbol_id(path,
container_chain, kind, name, ordinal)` (`crates/tm-codeintel/src/symbols.rs`), where:

- `path` is the symbol's file path, `container_chain` is the sequence of *names* (not ids — see
  below) of its enclosing symbols, root-first (e.g. `["Foo"]` for a method inside `impl Foo`),
  `kind` is a fixed per-variant string tag (not `{:?}`, so the id can't move if `SymbolKind`'s
  `Debug` output or discriminant order ever changes for unrelated reasons), and `name` is the
  symbol's own name as written.
- `ordinal` is this symbol's position, in ascending source order (byte range), among every other
  symbol in the same file sharing the same `(container_chain, kind, name)` tuple — needed because
  none of the other fields alone disambiguate e.g. two `impl Foo` blocks that each define a method
  named `new`.
- The five fields are fed into a `blake3::Hasher` each length-prefixed (`hash_field`), so
  concatenation can't collide two different field splits into the same hash input (`("ab", "c")`
  vs. `("a", "bc")`).
- The resulting 32-byte hash is truncated to its low 8 bytes (little-endian `u64`) and then masked
  to 53 bits.

Deliberately excluded from the hash: the symbol's byte range. Including it would defeat the whole
point — editing any line above a symbol moves its range on every parse, which is exactly the
churn this task removes.

Deliberately using **names**, not ids, for `container_chain`: an id-based chain would make a
symbol's id depend on its container's id, which itself depends on the *container's* container's
id, transitively pulling in exactly the positional instability this task removes (worse, it also
becomes a definitional cycle to break out of at parse time, since the container's own id would
need to already be settled). Names avoid both problems, at the cost of not distinguishing two
files that legitimately declare identically-named, identically-nested symbols only by look — which
is already exactly what `path` in the hash's first field is for.

**Not implemented: caching the parsed `SymbolIndex` across calls, keyed by `IndexDelta`.** The
task description calls this optional, and per-call full re-parsing was already the accepted cost
of `symbol_index()`'s no-staleness guarantee before this change (see that method's doc comment).
Stable ids remove the *reason* a caller would have needed that cache for id continuity — two
independently-parsed `SymbolIndex`es of the same files now agree on every symbol's id without
being the same index instance, which was the actual problem motivating this task
(`nav-agent-tool-index-refresh`'s ids drifting within one agent turn). Incrementally rebuilding
only `IndexDelta`'s touched files, instead of the current full re-parse, remains a real, separate
performance follow-up this decision leaves open; nothing in this change blocks doing it later.

## Why 53-bit masking

`crates/tm-mcp/src/server.rs`'s `symbol_def` returns `"id": sym.id` as a JSON `serde_json::Value`
number, and the MCP host client is JavaScript, whose `JSON.parse` represents every JSON number as
an IEEE-754 `f64` — exactly representable integers only up to 2^53. An unmasked 64-bit hash output
would silently lose precision above that on the client, breaking the very round-trip
(`symbol_def` → `symbol_references`/`symbol_callers`/`symbol_callees`) this task exists to make
reliable. Masking to the low 53 bits at generation time keeps every id exactly representable on
both sides of that boundary; the Rust side already treats `Symbol::id` as an opaque `u64` and never
relied on the high bits.

## What this costs, stated plainly

- **A rename or a move changes the id, on purpose.** Renaming a symbol, or moving it to a
  different file or a different enclosing container, changes one of the five hashed fields, so
  its id is now unrecoverably different from before the edit — there is no notion of "the same
  symbol, renamed" surviving across the hash. This is the direct, accepted tradeoff against the
  old counter, which (accidentally, not by design) happened to keep an id stable across an
  in-place edit that didn't touch symbol count or order in that same file, at the cost of never
  being stable across edits to *any other* file. The new scheme inverts which edits are stable:
  edits elsewhere in the workspace no longer perturb an unrelated symbol's id; a rename or move of
  the symbol itself now does, every time, deterministically.
- **53-bit masking narrows the collision space.** Two distinct `(path, container_chain, kind,
  name, ordinal)` tuples now collide if their full blake3 hash agrees on the low 53 bits, roughly
  1-in-2^53 per pair. At 100,000 symbols in a workspace (`~100k choose 2 ≈ 5×10^9` pairs by the
  birthday bound), that's on the order of a one-in-a-million chance of any collision across the
  whole workspace — acceptable for a tool-result identifier an agent treats as opaque and
  re-derives fresh on the next `symbol_index()` call, not for anything requiring cryptographic
  uniqueness.
- **Two identically-named, identically-shaped symbols in the same file still need `ordinal` to
  disambiguate, and `ordinal` is itself parse-order-derived** (ascending byte range within the
  file) — the one place this scheme still depends on position rather than pure content. This is
  bounded, though: it depends only on *that one file's own* internal ordering of same-named
  siblings, never on any other file's parse order or on unrelated symbols in the same file, so it
  can't reproduce the original bug (ids across unrelated files/symbols shifting from an unrelated
  edit).
- **No incremental cache.** `symbol_index()` still re-parses every currently-indexed file on every
  call; this task does not add the `IndexDelta`-scoped incremental cache described as optional
  above. On a large workspace this remains the dominant cost of any symbol/reference/outline
  query, unchanged from before this task.
