# D-011 — Secret-shaped-substring redaction at `Fabric::execute`, `Store::append` and `Store::store_artifact`

**Status:** accepted · **Date:** 2026-09-20 · **Supersedes:** nothing

## Context

`docs/audit-2026-09-18-fable.md`'s M-04 ("Ecosystem table stakes") lists "secret redaction at
`Fabric::execute` boundary with local restore" as a done-looks-like item, citing
`opencode-vibeguard`'s shape. `docs/backlog.md` ("Secret redaction before the model call") asks
for the same thing plus, explicitly, "a redaction test that fails the build if credential
material can reach an event, a context pack, a [log/snapshot]".

This is a different problem from the one `crates/tm-auth/src/credential.rs`'s `Credential`
already solves. `Credential` redacts a *known* secret this codebase itself resolved (an API key
read via `EnvApiKey`, an OAuth token) by never printing or serializing its own value — a
structural guarantee (`Credential` does not derive `Serialize`; `Debug`/`Display` are a fixed
`<redacted>`). Nothing about that type helps with a secret-*shaped* substring that shows up in
text this codebase did not resolve and has no a priori reason to flag: a tool's captured stdout
that happens to echo an `.env` file, a model's own output quoting something back. That is the gap
this track closes, and it follows `Credential`'s own philosophy where it can (never print/derive
`Serialize` for anything holding a mapped secret) while necessarily being a different mechanism
(pattern matching over arbitrary text, not a type wrapper around a value this codebase already
holds).

Two design questions had to be settled before writing any code (both surfaced by this session's
own adversarial review before implementation, not after):

1. **Can one redactor serve both the outbound-request boundary (`Fabric::execute`) and every
   persistence boundary (an event, a context pack, an artifact)?** No. `Fabric::execute`'s local
   restore only makes sense with a keyed, per-secret fingerprint and an in-memory mapping — but a
   persisted value outlives the process, so a mapping tied to it would be a mapping to nowhere.
   Worse, a keyed fingerprint is by construction session-random, and two of the three persistence
   targets have their own determinism requirements this codebase already enforces mechanically:
   `tm_context::pack::compile` is documented byte-identical given identical inputs (snapshot
   tests depend on this), and `xtask hygiene`'s `check_time_and_rand` forbids non-deterministic
   sources workspace-wide. The resolution: two entry points in `crates/tm-auth/src/redact.rs`,
   `redact`/`redact_json` (pure, deterministic, keyless — every match of a category becomes the
   identical bare placeholder, no mapping, no restore) for every persistence boundary, and
   `SessionRedactor` (keyed fingerprint per distinct secret, in-memory `placeholder -> original`
   map, `restore()`) for `Fabric::execute` alone.
2. **Where does the session key for `SessionRedactor`'s fingerprint come from, given `xtask
   hygiene`'s `check_time_and_rand` forbids `rand::random`/`rand::thread_rng`/`SystemTime::now`
   outside `tm-types/src/clock.rs`?** The sanctioned substrate for "give me something unique" is
   `tm_types::IdSource` (already how `Store`/`EventLog` mint ids), but threading an `IdSource`
   into `Fabric::new` would change its signature across the dozens of existing call sites in this
   workspace for a security property that does not actually need cryptographic
   unpredictability — the guarantee this track provides is "the mapping never leaves process
   memory," not "the fingerprint is infeasible to compute." `fresh_session_key` instead derives a
   blake3-keyed-hash key from `std::process::id()` plus a process-local `AtomicU64` counter,
   which is deterministic-by-construction (no forbidden entropy source) and sufficiently distinct
   per `SessionRedactor` instance without rippling `Fabric::new`'s signature.

## Decision

- **`crates/tm-auth/src/redact.rs`** (new module, `pub use`d from `tm-auth`'s root): a fixed
  pattern set — an OpenAI/Anthropic-style `sk-` key (minimum length, not the cited source's exact
  48 chars, since Anthropic's own `sk-ant-api03-...` keys run longer), an AWS access key id
  (`AKIA[0-9A-Z]{16}`, exact), GitHub tokens (`(ghp|gho|ghu|ghs|ghr)_...`, with a 20+ char lower
  bound the cited source's own pattern lacks), a PEM private-key header, and a generic
  label-adjacent high-entropy token (`[A-Za-z0-9_-]{32,}` immediately after a
  `key`/`token`/`secret`/`password`-shaped label and a `:`/`=`) — based on the public shape of
  `opencode-vibeguard`'s own default config (`github.com/inkdust2021/opencode-vibeguard`,
  `vibeguard.config.json.example`, retrieved 2026-09-20; see `redact.rs`'s module docs for the
  exact citations and the two deliberate departures from it). `redact(text) -> String` and
  `redact_json(&Value) -> Value` are the pure/keyless form; `SessionRedactor` wraps the same scan
  logic with a keyed fingerprint and an in-memory mapping, exposing `redact`/`redact_json`/
  `restore`/`mapping_len`, and (following `Credential`'s precedent) a hand-written `Debug` that
  never prints a held secret and no `Serialize` derive.
- **`Fabric::execute` (`crates/tm-provider/src/fabric.rs`)** — the boundary the audit names —
  owns one `SessionRedactor` and redacts the outbound `CompletionRequest`'s system prompt, every
  message's text, every `ToolUse`'s JSON input, and every `ToolResult`'s nested content
  (recursively) immediately before `Provider::complete`. `Fabric::restore_local` exposes the one
  sanctioned way back, documented as local-view-only: never forward the result to a model or a
  persisted store.
- **`Store::store_artifact` (`crates/tm-core/src/store.rs`)** redacts `bytes` (if they decode as
  valid UTF-8 — binary artifacts are left untouched rather than risked) and `meta` before
  `crate::artifact::plan_storage` runs, so the recorded content hash matches what is actually
  written to disk/SQLite.
- **`StoreTx::append` (`crates/tm-core/src/store.rs`)** — the single real choke point every event
  in the system passes through (`Store::append`/`append_all` and every typed convenience method
  funnel through here) — compares `draft.payload.to_json()` against `tm_auth::redact_json`'s
  output and, only when they differ, reconstructs the payload via `Payload::from_json` before the
  hash chain is computed or `materialize::apply` runs; when nothing matched, `draft.payload` is
  returned exactly as given, with no JSON round-trip at all (see "What this costs" for why this
  matters beyond performance). Either way, the persisted hash chain, the returned in-memory
  `Event`, and the materialized `ProjectView` stay consistently redacted rather than three
  different views of the same event disagreeing about what it contains.
- **`tm_context::pack::compile` (`crates/tm-context/src/pack.rs`)** redacts each section's
  rendered body (via the pure `redact`, not a keyed one — see the determinism point above) before
  it is charged against the token budget or admitted, so `rent_report`'s accounting reflects what
  a section actually costs once redacted.
- **Both halves of the backlog's explicit test ask are real regression gates against production
  code paths, not unit tests of the regex in isolation**: `crates/tm-provider/src/fabric.rs`'s
  `execute_redacts_secret_shaped_content_before_it_reaches_the_provider` plants a canary across
  every `ContentBlock` variant and inspects `MockProvider::call_log()` — what actually left the
  process — then proves `restore_local` recovers it; `crates/tm-core/src/store.rs`'s
  `event_payload_redacts_a_secret_shaped_substring_before_it_is_persisted` reads the raw
  `events.payload` SQLite column back, not just the in-memory `Event`;
  `store_artifact_redacts_secret_shaped_bytes_and_meta` reads the materialized `Artifact`'s
  bytes/meta; `crates/tm-context/src/pack.rs`'s
  `compile_redacts_a_secret_shaped_substring_in_the_objective` plants the canary in
  `ticket.objective` (the field a human most plausibly pastes a stray credential into) and
  inspects every admitted section body from a real `compile()` call. Each of the four has a
  false-positive sibling test asserting a real blake3 content hash (and, in `tm-provider`'s case,
  a git-sha-shaped string) survives unredacted, per the task's explicit "don't break ticket ids/
  hashes/session ids" scoping requirement — `crates/tm-auth/src/redact.rs`'s own
  `hashes_and_ids_survive_unredacted` unit test covers the pattern set directly against a blake3
  hash, a `T-`/`ART-`/`S-`-shaped id, and a git commit sha.

## Why

- Splitting pure/persistence redaction from the keyed/session one is not incidental caution — it
  is the only way to keep `pack::compile`'s documented determinism guarantee and `xtask
  hygiene`'s workspace-wide no-nondeterminism rule both true while still giving `Fabric::execute`
  a real local-restore mapping. Building one redactor and bolting a "sometimes deterministic"
  flag onto it would have been both a worse API and a real risk of quietly breaking
  `compile_is_deterministic_given_identical_inputs` (which — spoiler — passed unmodified once the
  pure/keyed split was made; see the test run for this track).
- Redacting at `StoreTx::append` (the shared kernel every typed `Store` method funnels through)
  rather than in `Store::append`'s own wrapper, or scattered across each of the ~40 typed
  convenience methods, means every event in the system gets the guarantee for free, including
  ones written by code this track never touched.
- Citing `opencode-vibeguard`'s actual published config rather than inventing a pattern set from
  scratch grounds the choice in a real, working implementation of the same idea, and the two
  documented departures from it (a minimum- not exact-length `sk-` pattern; a lower bound on the
  GitHub pattern the source lacks) are both concrete, explainable improvements for this
  codebase's actual usage, not arbitrary tightening.

## What this costs, stated plainly

- **The generic label-adjacent pattern requires an explicit `key`/`token`/`secret`/`password`
  label next to the value.** A secret-shaped string with no such label and no recognized provider
  prefix (a bespoke internal token format, a base64 blob with no context) is not caught. This is
  a deliberate precision/recall tradeoff, not an oversight — the task itself asked for "a real,
  not-overly-broad pattern set," and a broader unlabeled-high-entropy rule would false-positive on
  this codebase's own blake3 hashes and hex ids constantly (`check-drift`'s own module docs give
  the identical "a broader pattern match was tried and rejected... noise, not triage" reasoning
  for an unrelated check, which is the same tradeoff shape here).
- **`redact_json`'s object-key label check is one level deep and requires the whole string value
  to look like a bare token.** `{"api_key": "<32+ char token>"}` is caught (the label is the
  sibling JSON key); a secret buried inside a longer prose sentence under an unrelated key is not,
  and neither is a label two levels of nesting away from its value. No typed
  `tm_events::payload` struct in this workspace today has a `key`/`token`/`secret`/`password`-
  named field, so this closes a gap for payload shapes that do not exist yet, not a hole in
  today's actual coverage — but it is a real limit on the mechanism, not just this snapshot of
  the data.
- **A payload whose redaction happens to touch a structured (non-freeform) field badly enough
  that the redacted JSON no longer deserializes into its own typed shape fails the whole
  `StoreTx::append` call with a `TmError`**, rather than silently persisting a corrupted event.
  Given the pattern set's conservatism this is a theoretical risk today (confirmed by a full
  `cargo test --workspace` run — 2934 tests, 0 failures — immediately after wiring this in, with
  no regression), but it means a future payload struct that happens to have a legitimately
  long, label-adjacent string field (an actual API key stored as ordinary data, say, in a
  hypothetical future feature) would need to either exempt that field or accept the append
  failing loudly rather than the secret leaking silently — fail-loud was the deliberate choice
  here.
- **`redact_event_draft`'s `to_json`/`from_json` round-trip is not a guaranteed byte-identity for
  every JSON shape** (map key order, number representation) even when nothing matches, which
  would put every single append — not just ones that actually redact something — at risk of
  hashing a subtly renormalized payload instead of the original. `redact_event_draft` sidesteps
  this for the overwhelming majority of appends by comparing the redacted `Value` against the
  original and skipping the reconstruction entirely when they're equal (`draft.payload` is
  returned unchanged, no round-trip at all); `crates/tm-core/src/store.rs`'s
  `event_payload_value_field_round_trips_exactly_around_redaction` exercises the one payload in
  this workspace with an open-ended `serde_json::Value` field
  (`TicketUpdatedPayload::fields`) both on the skip path and forced through a real round-trip
  (by adding a sibling secret-shaped value), asserting a float, a nested object and an array all
  survive structurally unchanged either way.
- **`SessionRedactor`'s restore mapping is capped at 10,000 distinct secrets per instance**
  (`MAX_MAPPING_ENTRIES`, modeled on `opencode-vibeguard`'s own `max 100,000 mappings` bound, sized
  down since this workspace has one `SessionRedactor` per process-lifetime `Fabric` rather than
  per HTTP request). Past the cap, a new secret still redacts correctly — the placeholder is
  still produced — but is not remembered, so `Fabric::restore_local` simply cannot recover that
  particular value once the cap is hit. This is a deliberate bounded-memory tradeoff (an
  unbounded mapping would itself work against "the mapping never leaves process memory" by making
  that memory footprint unbounded) rather than an oversight; unlike vibeguard, no TTL/eviction on
  top of the count cap was added, since nothing in this workspace's own `Fabric` lifecycle needs
  session expiry semantics today.
- **`Fabric::execute`'s `SessionRedactor` key is not cryptographically unpredictable.** It is
  derived from `std::process::id()` and a process-local counter, not real OS randomness — see
  the Context section above for why (`xtask hygiene` forbids the alternative outside
  `tm-types/src/clock.rs`, and threading `IdSource` through `Fabric::new` was judged not worth
  rippling across its call sites for a property the design does not actually need). The security
  property this track provides is "the restore mapping never leaves process memory and is never
  sent to a model or persisted," not "the fingerprint is infeasible to reverse without the key" —
  stated here so a future reader does not assume more cryptographic strength than was built.
- **Only `Fabric::execute`'s outbound `CompletionRequest` is redacted, not `tools: Vec<ToolDef>`
  or `stop_sequences`.** Tool definitions are static schema (not user-supplied data) and stop
  sequences are not a plausible secret-carrying field in this codebase's actual call sites, so
  this was scoped out rather than covered speculatively.
- **Nothing in this track touches `tm-agent`'s own logging/tracing calls, ACP wire framing, or
  `tm-server`'s HTTP responses directly** — those are separate surfaces from the three named
  boundaries (`Fabric::execute`, `Store::append`, `Store::store_artifact`) this track wires, and
  a secret that reaches one of those surfaces through a path that never touches any of the three
  (a `tracing::info!` call logging a raw tool argument, say) is not covered. `SPEC.md` §28.2's
  narrower, already-satisfied guarantee ("`Credential` material never enters the event log...")
  is a different, tighter claim about known credentials specifically and remains true
  independently of this track.
