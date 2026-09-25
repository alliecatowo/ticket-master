# D-028 — Record/replay cassettes for provider calls

**Status:** accepted · **Date:** 2026-09-24 · **Supersedes:** nothing

## Context

Every test in the workspace already runs against `tm-provider`'s `MockProvider` instead of a
real network call (`SPEC.md` §0). That's fine for unit tests that script an exact response, but
it leaves a gap: there was no way to capture what a *real* provider actually said for a real
scenario and then replay that recording deterministically, for scenarios (a genesis run, an
agent-loop integration test) too large to hand-write a script for. `crates/tm-provider/src/mock.rs`
already had `MockProvider::script_sequence`/`sequence_remaining` (an ordered FIFO of completions,
served regardless of request content) as groundwork for this; this decision adds the recording
half and the cassette file format both sides share.

## Decision

**`crates/tm-provider/src/cassette.rs`**, a new module:

- **Format:** JSONL. The first line is a `CassetteHeader { format_version, harness_epoch:
  Option<u64>, recorded_at }`. Every following line is one `CassetteEntry { seq, role,
  provider_id, request_hash, request, completion }`. `Cassette::read_jsonl`/`write_jsonl` round-trip
  the whole file.
- **`RecordingProvider<P: Provider>`** wraps a real provider and appends one `CassetteEntry` on
  every *successful* `complete()`; a failed call isn't recorded, since a cassette only ever needs
  to reproduce completions. `embed()` delegates straight through and is never recorded.
- **`request_hash` is computed over a normalized request**, via `normalize_request`: every
  occurrence of a given `root` path's string form, anywhere in the request's text content (system
  prompt, message text, and recursively through `ToolUse`'s JSON input and `ToolResult`'s nested
  content), is replaced with a fixed placeholder before hashing. Two requests built from identical
  starting state but under a different tempdir root hash equal.
- **`MockProvider::script_from_cassette(cassette, root)`** loads a cassette's entries into the
  same ordered-sequence mechanism `script_sequence` already used, so replay serves completions
  strictly in recorded order — not by matching each request's exact hash against
  `MockProvider`'s normal hash-keyed script table. Each served request's normalized hash is still
  compared against what was recorded at that position; a mismatch is recorded as a `Divergence`
  (`{seq, expected_hash, actual_hash}`, readable via `MockProvider::divergences()`) instead of
  failing the call. Once the cassette is exhausted, replay falls through to whatever the mock's
  normal hash-keyed scripts or default response would do, same as `script_sequence` always has.

## Why

**Replay is ordered, not exact-hash, on purpose.** A recording was made once, against one
concrete tempdir/project state. Replaying it into a *different* run — even one intended to be
"the same scenario" — means paths, timestamps, ids and any other incidental detail a prompt
happens to embed will differ from what was recorded, so exact-hash lookup diverges on the very
first call. Ordered replay sidesteps that: the scenario's call sequence is what's actually being
tested, not byte-for-byte prompt equality. Divergence reporting keeps that honest — a real drift
in call content still surfaces, as data to inspect, rather than being silently swallowed or
turned into a hard failure that blocks a replay a caller may still want to see through to the end.

**Path normalization only, not full-request normalization.** A real prompt can carry a timestamp
or an id too, and those are *not* normalized here — a general normalizer for "anything that could
differ" risks papering over a genuine divergence, and there's no single placeholder that's
obviously safe for every such value the way there is for a root path. A tempdir/project-root path
is normalized because it provably differs on every single run yet carries no scenario-relevant
information of its own; a timestamp or id embedded in a prompt still surfaces as a real
`Divergence` on replay, and that's accepted as the cost of ordered (not exact-hash) replay rather
than treated as a bug — see the ordered-replay rationale above.

**`embed()` is never recorded.** Nothing in this crate's replay story needs a recorded embedding
call: `MockProvider::embed` is already fully deterministic and network-free on its own (hash of
the input text, no scripting required), so there's no reproducibility gap for `RecordingProvider`
to close there. Recording it anyway would only grow every cassette file for no replay benefit.

## What this costs, stated plainly

- A cassette is not a contract: nothing here validates that a `CassetteEntry`'s `completion` is
  still a plausible response for its `request` after a prompt-shape change elsewhere in the
  workspace. Divergence reporting catches a request-hash mismatch, not a stale-but-hash-matching
  recording.
- `normalize_request` only scrubs the configured `root`'s own string form. A prompt embedding a
  *different* absolute path (e.g. a symlink target outside `root`, or `$TMPDIR` itself rather
  than the project root under it) still diverges; callers that need more than one path
  normalized must pick the `root` that actually covers their prompts.

## Implemented: `tm run <ticket> --record <path>`

`replay-cli-record-flag` wires the format above into the CLI (`crates/tm-cli/src/args.rs`'s
`RunArgs`, `crates/tm-cli/src/dispatch.rs`, `crates/tm-cli/src/agent.rs`):

- `tm run <ticket> --record <path>` runs the ticket exactly as `tm run <ticket>` would, plus:
  every provider `crate::agent::build_fabric_for_project_recording` registers for the run —
  DevPass, Anthropic, every autodetected fallback backend, and the `TM_TEST_MOCK_PROVIDER=1`
  mock alike — is wrapped so a successful `complete()` also appends a `CassetteEntry` to one
  `CassetteSink` (`Arc<Mutex<Vec<CassetteEntry>>>`) shared across every wrapped provider, not one
  private cassette per provider the way bare `tm_provider::RecordingProvider` would if used
  directly. `role` is the ticket's own `ExecutorRequirements::role` — a single `tm run`
  dispatches exactly one ticket, so one role covers the whole recording.
- On dispatch this way, the request-hash-normalizing `root` is the same execution root the run's
  own tool calls resolve against (`--worktree`'s isolated checkout when passed, the project root
  otherwise).
- Once the ticket leaves `Leased`/`Running`, `run_ticket` writes the accumulated entries plus a
  `CassetteHeader` (`harness_epoch` read the same way `tm-scheduler`'s own dispatch does — the
  last promoted epoch, or the genesis epoch `0` if none has promoted yet) to `<path>` via
  `Cassette::write_jsonl`, then stores those same bytes as the ticket's `ArtifactKind::Transcript`
  artifact (`Store::store_artifact`) — the first real use of that previously-declared-but-unused
  kind.
- Without `--record`, behavior is byte-for-byte unchanged: `build_dispatcher` (no recording) is
  still what every other caller uses, and `register_recordable`'s `None` branch is a bare
  `fabric.register_provider(provider)`.
- `tm run --replay <path>` (offline ordered replay against a recorded cassette) is a separate,
  later task (`replay-cli-replay-flag`) — this only covers recording.
