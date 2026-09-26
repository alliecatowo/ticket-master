# D-025 — A real semantic embedder: Potion static embeddings over `model2vec-rs`

**Status:** accepted · **Date:** 2026-09-23 · **Supersedes:** nothing

## Context

`tm-codeintel`'s hybrid search fuses several signals (`hybrid.rs` already does real weighted
reciprocal-rank fusion, matching zvec-grep's own approach — that half of this task's original ask
was already true before this change). But the "semantic" half of that fusion ran on
[`embed::LocalHashEmbedder`](../../crates/tm-codeintel/src/embed.rs): hashed character n-grams and
token TF-IDF, hashed into fixed buckets. That is a real, deterministic, no-network embedding, but
it is lexical in disguise — two chunks that share no tokens or character n-grams cannot be found
similar no matter how semantically close they are, which is exactly the "where are tickets moved
between states" / "how is the provider chosen for a request" kind of query this task's acceptance
check names. The owner's own framing: "use potion like zvec grep for semantic search, basically
zvec grep inspired" — this repo already depends on zvec-grep as a tool, and zvec-grep's own
semantic search runs on MinishLab's Potion static-embedding models.

Separately, the lexical half of hybrid fusion was also weaker than it looked:
`CodeIntel::search_hybrid` fed `Signal::Lexical` from `search_exact(&query.text)`, a literal
substring match on the *entire* query string. A multi-word natural-language query like "where are
tickets moved between states" matches no file as a literal substring, so that signal contributed
nothing to exactly the queries this task's acceptance check runs.

## Decision

Add [`potion::PotionEmbedder`](../../crates/tm-codeintel/src/potion.rs), a thin wrapper around
`model2vec-rs` (MinishLab's official pure-Rust, CPU-only, ONNX-free port) loading
`minishlab/potion-code-16M-v2` — the same model zvec-grep uses — from the local HuggingFace cache
(`$HF_HOME/hub` or `~/.cache/huggingface/hub`). [`embed::build_default_embedder`] picks between
Potion and the hash embedder: `TM_EMBEDDER=hash` forces the old behavior, `TM_EMBEDDER=potion` (or
a future `index.embedder` project-config key, passed as `config_override`) requests Potion, and
unset tries Potion first. Both `try_load`/`try_load_offline` never return an `Err`: any failure
(no cache, corrupt snapshot, disabled download) is logged once via `tracing` and reported as
`None`, so a caller always has `LocalHashEmbedder` as a working fallback — indexing must never
break because a model failed to load.

The vectors table was already keyed by `embedder.identifier()`
([`store.rs`](../../crates/tm-codeintel/src/store.rs)'s schema, `Embedder::identifier()`'s own doc
comment). `update_incremental` did not previously *act* on that key: a blake3-unchanged file was
"touched nowhere" regardless of which embedder produced its stored vector, so switching embedders
would have silently mixed vector spaces in one corpus. `update_incremental` now stores the last
embedder's identifier in `doc_meta` and, when it no longer matches the currently configured
embedder, promotes every currently-walked file (not just blake3-changed ones) into `modified` for
that one run — a real re-embed, not a full reindex from scratch (file/symbol/history state that
doesn't depend on the embedder is untouched).

`CodeIntel::search_hybrid`'s `Signal::Lexical` input is now `CodeIntel::search_lexical_bm25`:
Okapi BM25 (`k1 = 1.2`, `b = 0.75`) over the existing `tokens` inverted index (already populated
per chunk by `write_file_content`, previously only read by `SemanticSearch`'s token prefilter),
tokenizing the query the same way chunk text is tokenized at index time. This is what makes
`search_exact`-style dead lexical matching stop silently zeroing out that signal for
natural-language queries, without adding a new table or changing what gets indexed.

Chunking was already symbol/line-window-sized, not whole-file
([`chunk.rs`](../../crates/tm-codeintel/src/chunk.rs)'s `Chunker`, already wired into
`write_file_content`) — nothing needed to change there either.

**`CodeIntel::open`/`open_at` keep defaulting to `LocalHashEmbedder`, unconditionally.** A new
`CodeIntel::open_auto`/`open_at_auto` pair uses `build_default_embedder` (Potion-if-cached, never
downloads even when explicitly requested via `open_auto`'s `config_override`) instead. This is a
deliberate scope boundary, not an oversight — see "What this costs" below.

## Why

- `model2vec-rs` is pure Rust with no ONNX runtime dependency, matching this crate's existing
  `#![forbid(unsafe_code)]` and no-network-in-tests posture: model loading itself can touch the
  network (downloading a missing model), but `encode`/`embed` never do. Its default Cargo feature
  set pulls in `onig` (a C regex engine via `tokenizers/onig`), which would contradict that "pure
  Rust" story, so this crate depends on it with `default-features = false` plus `hf-hub` and
  `fancy-regex` (model2vec-rs's own pure-Rust alternative to `onig`) instead.
- Reusing the exact model zvec-grep already runs (`potion-code-16M-v2`, a code-tuned static
  embedding) means this repo's own indexing gets directly comparable retrieval quality to the
  tool the team already trusts for the same job, instead of inventing a second, unproven semantic
  model choice.
- A stable `identifier()` string per embedder was already the intended mechanism for detecting a
  vector-space mismatch (the doc comment on `Embedder::identifier` says so); `update_incremental`
  just needed to actually read it back and act on a mismatch, which is a small, targeted change
  scoped to `api.rs`.
- BM25 is the standard, well-understood lexical-ranking formula (the same family zvec-grep's own
  FTS route and most hybrid-search systems use) and needed no new storage: `term_frequency` per
  `(token, chunk_id)` was already indexed, just never read back for ranking, only for
  `SemanticSearch`'s prefilter.

## What this costs, stated plainly

- **`CodeIntel::open`/`open_at` were deliberately *not* switched to Potion-by-default**, even
  though the task's framing asked for that. Dozens of `#[test]`s across `tm-agent`, `tm-context`,
  `tm-wiki` and this crate itself call `CodeIntel::open`/`open_at` directly, with no test-context
  signal this crate can see (`cfg!(test)` inside `tm-codeintel` is false when it's compiled as a
  dependency for another crate's tests) and no way for this change, scoped to
  `crates/tm-codeintel/{embed,potion,api,hybrid}.rs`, to audit or update every one of those call
  sites in the same pass. Silently making `open`/`open_at` try to load a real embedding model
  would have changed retrieval-dependent test behavior across the whole workspace on any machine
  where the model happens to be cached (this dev machine has it cached already), which is exactly
  the kind of untested, wide-blast-radius change this task's instructions said not to risk.
  `open_auto`/`open_at_auto` are used by the CLI project/indexing and dispatch paths, MCP, and
  Genesis indexing. Unit tests pass an explicit `hash` override so their retrieval behavior
  remains stable regardless of whether Potion is cached on the machine running them. Direct
  `open`/`open_at` calls continue to use the hash embedder. Incremental indexing persists the
  selected embedder identifier and re-embeds walked files when it changes, avoiding vectors from
  different embedding spaces being mixed in one index; indexes without a stored identifier are
  re-embedded once on their first update.
- **No download ever happens through `open_auto`/`open_at_auto`, even on explicit request.**
  `build_default_embedder` calls `PotionEmbedder::try_load_offline`, not `try_load`, specifically
  to keep every existing and new call site of it network-free by construction. A real "index with
  Potion, downloading it if needed" command path needs to call
  `potion::PotionEmbedder::try_load()` directly and pass the result to
  `CodeIntel::open_with_embedder` — that's a `tm-cli`/`tm doctor` change outside this task's file
  ownership in this batch.
- **`index.embedder` is not yet a real, persisted project-config key.** `build_default_embedder`
  and `open_auto`/`open_at_auto` accept a `config_override: Option<&str>` for exactly this, but
  reading an actual `index.embedder` value out of a project's config file is `tm-core`/`tm-cli`
  work (`project.rs`, `tm-harness`'s `HarnessConfig`) outside this task's owned files in this
  batch — see the summary handed to the integrator for the exact wiring shape.
- **`model2vec-rs`'s `hf-hub` feature brings in `hf-hub` + `ureq`** (a real HTTP client) — a
  second, smaller dependency tree than D-010's `reqwest` stack, but a real one; unlike D-010's
  `otel` feature this is not gated behind an opt-in Cargo feature, since the whole point is that
  Potion is the attempted default. `mise run test:crate -- tm-codeintel` is the crate-scoped way
  to check the build cost stays reasonable on this machine's `-j 2`/disk constraints.
- **BM25's per-query-token `SELECT COUNT(DISTINCT chunk_id) ... / SELECT chunk_id, term_frequency
  ...` pair is two extra queries per distinct query token**, plus one `GROUP BY chunk_id` pass
  over the whole `tokens` table for the corpus's average chunk length. Fine at this repo's scale
  (thousands of chunks); a much larger corpus would want a cached/precomputed doc-length column
  instead of recomputing it per `search_hybrid` call — not done here, since `store.rs`'s schema
  is outside this task's owned files.
- **A project with no `doc_meta.embedder_identifier` row yet (every existing index built before
  this change) re-embeds its entire corpus once**, the first time `update_incremental` runs after
  upgrading, even if the embedder itself didn't actually change — `stored_embedder_id` reads back
  `None`, which compares unequal to `Some(current_identifier)` the same way a real switch would.
  This is a one-time, self-correcting cost (the next run is a true no-op again) rather than a bug,
  but it means the first post-upgrade `update_incremental` on a large repo takes noticeably
  longer than usual.
- **`model2vec-rs::StaticModel::from_pretrained` decides local-vs-hub purely by
  `Path::exists()`**: a local snapshot path that has been deleted or was never created is treated
  as a HuggingFace repo id and, with `hf-hub` enabled, falls through to a real network download —
  confirmed by reading `from_pretrained`'s source, not just its documented signature.
  `PotionEmbedder::try_load_with` therefore checks `dir.is_dir()` itself before ever calling
  `from_pretrained` on a candidate local path, so a missing/corrupt cache entry can only reach
  the explicit, `allow_download`-gated branch, never that hub fallback — this is what keeps
  `try_load_offline`/the `potion.rs` unit tests genuinely network-free rather than accidentally
  relying on `allow_download = false` alone.
- **This change was authored without running `cargo build`/`cargo test`** (this task's own
  instructions forbid it in this batch — see the shared integration worktree's build queue).
  `model2vec-rs`'s exact public API was confirmed against its published `docs.rs`/GitHub
  documentation (`StaticModel::from_pretrained(repo_or_path, token, normalize, subfolder) ->
  Result<Self>`, `encode(&self, &[String]) -> Vec<Vec<f32>>`, feature names from its own
  `Cargo.toml`) rather than by compiling against it. The integrator's `cargo build` is the first
  real compile of this dependency; if `model2vec-rs`'s actual signatures differ from what's
  documented, that build will fail loudly rather than silently, and `mise run test:crate --
  tm-codeintel` is the fast way to find out. `Cargo.lock` was deliberately left untouched rather
  than hand-edited — the integrator's first build regenerates it with `model2vec-rs` and its
  transitive dependencies pinned for real.

## Consequences

- `crates/tm-codeintel/Cargo.toml` gains `model2vec-rs = { version = "0.3", default-features =
  false, features = ["hf-hub", "fancy-regex"] }` and `dirs = { workspace = true }` (already a
  workspace dependency elsewhere, reused here for HF cache resolution). No root `Cargo.toml`
  change was needed: `dirs` was already a workspace dependency, and `model2vec-rs` is pinned
  crate-locally, matching this crate's existing convention for its `tree-sitter-*` dependencies.
- `TM_EMBEDDER_DOWNLOAD` (`0`/`false`/`off`/`no` to disable, matching `TM_NOTIFY`'s spellings) lets
  an operator opt out of ever downloading Potion, independent of which embedder is selected.
- A follow-up should: wire `tm-cli`'s real (non-test) `CodeIntel::open`/`open_at` call sites over
  to `open_auto`/`open_at_auto`; add the `index.embedder` project-config key in `tm-core`/
  `tm-harness`; add a `tm doctor`/explicit index command that calls `PotionEmbedder::try_load()`
  (with download) once, rather than relying on every `update_incremental` call to attempt it; and
  run the nav-quality eval task (`nav-eval-harness`/similar, if scheduled) to get the before/after
  hit-rate@3 numbers this decision doc's own acceptance check asks for, once a real build can
  execute `tm search hybrid`.
