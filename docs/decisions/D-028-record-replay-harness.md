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

## Implemented: `tm run <ticket> --replay <path>`

`replay-cli-replay-flag` wires offline ordered replay into the CLI (`crates/tm-cli/src/args.rs`'s
`RunArgs`, `crates/tm-cli/src/dispatch.rs`, `crates/tm-cli/src/agent.rs`, `crates/tm-cli/src/sched.rs`):

- `tm run <ticket> --replay <path>` reads `<path>` as a `Cassette` and dispatches through
  `crate::dispatch::build_dispatcher_with_replay` instead of `build_dispatcher`/
  `build_dispatcher_with_recording`: the ticket's own `ExecutorRequirements::role` gets exactly
  one candidate, a fresh `tm_provider::MockProvider` loaded via
  `MockProvider::script_from_cassette(&cassette, &root)` — no other provider is registered with
  the fabric at all, so no network call is reachable regardless of what `providers.toml` still
  names for that role. `root` is the same execution root `--record` normalizes against
  (`--worktree`'s isolated checkout when passed, the project root otherwise), so a cassette
  recorded in one tempdir/project still replays against a different one without diverging on the
  tempdir path alone (`normalize_request`'s whole point — see "Decision" above).
- `--replay` conflicts with `--record` (`args.rs`'s `conflicts_with`) — replaying and recording
  the same run at once makes no sense — and combines with `--worktree` the same way `--record`
  does.
- `--strict-replay` (only meaningful alongside `--replay`): once the run leaves
  `Leased`/`Running`, `sched::report_replay_divergences` reads `MockProvider::divergences()` and
  compares `MockProvider::call_log().len()` against the cassette's own recorded entry count to
  count calls that ran past the cassette's end (exhaustion — each one already fell through to
  `ProviderError::Unscripted` and failed on its own). If `--strict-replay` was passed and either
  count is nonzero, `run_ticket` turns the run's own outcome into a hard error — even when the
  replayed ticket itself otherwise reached a forward-progress state (an exhausted call already
  fails the attempt on its own either way; `--strict-replay` additionally makes a *divergence* —
  which `MockProvider` still serves, not fail — a hard error too). Without `--strict-replay`,
  both are reported but never turned into a hard error by this function itself.
- Once the run finishes, `report_replay_divergences` reports the divergence count, the first
  divergent entry's `seq`, and the unscripted-call count as one `ReplayReport` JSON object under
  `--json` (`Renderer::emit`), or a one-line human summary otherwise.
- Without `--replay`, behavior is unchanged: `run_ticket` only takes this path when `args.replay`
  is `Some`.

## Tool replay

`replay-tool-replay-mode-design` adds a second, independent replay axis: everything above
(`--record`/`--replay`) replays what a *provider* said — the model's own completions — through a
cassette-scripted `MockProvider`. It says nothing about what a *tool* did. A ticket's own file
reads, shell commands, and search/symbol calls still execute for real even under `tm run
<ticket> --replay <path>`, because `MockProvider::script_from_cassette` only ever intercepts
`Fabric::complete`, not `ToolRegistry::dispatch`. This section adds the matching capability at the
tool layer, inside `tm-agent` itself (`crates/tm-agent/src/agent_loop.rs`,
`crates/tm-agent/src/executor.rs`) rather than the CLI: `AgentLoop::with_replay_tool_source`
takes a `ReplayToolSource` and, for the rest of that loop's life, every tool call is resolved from
it instead of a real `ToolRegistry::dispatch` call.

- **Source of truth: a prior run's `StepRecord`s, never `tool_call.completed` telemetry.**
  `ReplayToolSource::from_steps` builds the source by flattening a transcript's `StepRecord::
  tool_calls` in order. The `tool_call.completed` event (`tel-tool-call-event-kind`, D-030's
  "local telemetry") was considered and rejected as the source: its `ToolCallCompletedPayload` is
  deliberately narrow — `ticket`, `session`, `tool_name`, `duration_ms`, and a collapsed `outcome`
  string (`"completed"`/`"denied"`/`"error"`) — carrying neither the call's `tool_use_id`/`input`
  nor the full `ToolCallResolution` (a `Completed` result body, a `Denied` reason, an `Errored`
  detail) a verbatim replay needs to reproduce. A `StepRecord` (kept durably as part of an
  `AgentOutcome`'s transcript, or reconstructible from a saved session/cassette-adjacent artifact)
  is the only place that data survives.
- **Match order: `tool_use_id` first, then `tool_name` plus a root-normalized input hash.**
  `ReplayToolSource::take` first looks for an exact `tool_use_id` match — the common case, since a
  cassette-replayed provider (`--replay`'s own `MockProvider::script_from_cassette`) reproduces
  the recording's own `ContentBlock::ToolUse` ids verbatim, byte-for-byte. Falling back to
  `tool_name` plus a hash of the call's JSON input — normalized the same way
  `tm_provider::cassette::normalize_request` normalizes a provider prompt, replacing every
  occurrence of the execution root's string form with `tm_provider::cassette::PATH_PLACEHOLDER`
  before hashing — covers the case where the provider side isn't itself a byte-identical cassette
  replay (e.g. a fresh, non-replayed model re-run against the same tool sequence, or any other
  source of fresh `tool_use_id`s), so a tool call whose recorded input differs only in which
  tempdir/project root it was made under still matches. Each recorded call is consumed at most
  once (`take` removes the match it returns), so two structurally identical calls in the
  recording still pair with their own distinct recorded resolutions, in order, rather than both
  replaying the first one found.
- **A miss is a hard, distinctly-tagged failure — never a fallback to real execution.** Unlike
  provider-level replay (which serves an ordered cassette and reports a hash mismatch as a
  `Divergence` the caller can inspect after the fact, without failing the call), a tool-replay
  miss stops the run immediately: `AgentLoop::drive`/`resume_with` produce
  `Ok(AgentOutcome::Failed { class: FailureClass::Other, detail, .. })` with `detail` prefixed by
  `agent_loop::REPLAY_DIVERGENCE_PREFIX` (`is_replay_divergence` recognizes it), matching this
  loop's own documented invariant that `AgentLoop::run`/`resume_with` return `Err` only for an
  infrastructure failure, never for an in-band one. The asymmetry with provider replay's softer
  divergence handling is deliberate: a provider divergence still has *some* real, ordered
  completion to serve and continue with (the recording just may not describe the current run
  precisely); a tool call with no recorded resolution at all has nothing to serve — dispatching it
  for real would mean the "replay" quietly stopped being one, silently mixing a recorded run with
  live, non-reproducible tool execution (a shell command, a file write) that the whole point of
  replay is to avoid. Failing loudly and immediately is the only option that stays honest about
  which mode the run is actually in.
- **The authority/oversight gate is not bypassed.** `AgentLoop::drive`'s existing
  `Oversight::review(&action, effective_authority.permits(&action))` check — whether a dispatched
  action is outright denied, needs human approval, or is allowed — runs exactly as it does for a
  real dispatch, *before* a tool call reaches `AgentLoop::replay_resolution`. Only the actual
  `ToolRegistry::dispatch` call (authority re-check, `PreToolUse`/`PostToolUse` hooks, the real
  `CapabilityProvider::invoke`) is replaced by the recorded resolution. This means a replay driven
  under a different (e.g. more restrictive) `Authority`/`Oversight` than the original recording
  still suspends into `AgentOutcome::AwaitingApproval` or denies exactly as a live run under that
  same ceiling would — tool replay reproduces *what a tool call returned*, not *whether the loop
  was allowed to make it*, and a caller narrowing authority for a replay (e.g. auditing a
  recording under a stricter policy) gets a real, current answer to that second question rather
  than the recording's own, possibly-looser one.
- **`ReplayToolSource` is `Clone`, not consumed by construction.** `AgentLoop` owns and mutates its
  own copy (`Option<ReplayToolSource>`, consumed call-by-call via `Vec::remove`); a caller that
  builds a fresh `AgentLoop` per dispatch (`BuiltinExecutor::build`, once per
  `Executor::execute` call) hands each one its own full, unconsumed clone via
  `BuiltinExecutor::with_replay_tool_source`, rather than sharing one source across loops and
  having a second dispatch see it already partly drained by the first.
- No CLI verb wires this up yet (`crates/tm-cli/src/args.rs`/`ops.rs` are out of this task's file
  scope) — `BuiltinExecutor::with_replay_tool_source` is the plumbing a future `tm run <ticket>
  --replay <path>` extension (or a dedicated tool-replay verb) would call, the same way
  `with_root`/`with_step_sender` existed as plumbing before `--worktree`'s own CLI wiring landed.
  Without it, behavior is unchanged: every existing `BuiltinExecutor`/`AgentLoop` caller dispatches
  real tool calls exactly as before.
