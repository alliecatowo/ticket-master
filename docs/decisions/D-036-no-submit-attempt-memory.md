# D-036 — A no-submit attempt remembers what it tried, scoped to one retry round

**Status:** accepted · **Date:** 2026-09-26 · **Supersedes:** nothing

## Context

Live benchmark trials repeatedly hit the same failure mode (task
`t20260925-1314-BurntSushi-ripgrep-3376-recover-without-repeating-agent-investigation`, also seen in
`pallets-click-3822` and `psf-requests-7432`): a ticketed `AgentLoop::drive` turn ends with the model
producing text but no `ticket.submit` call, `crate::executor::BuiltinExecutor::execute` reports
`FailureClass::Other`/`"model ended turn without submitting"`, and the scheduler schedules a fresh
attempt. That fresh attempt starts from `ExecutorTask::context_pack` alone — a compiled snapshot of
the ticket's current state, with no memory of what the *previous attempt itself* already read,
searched or concluded. On a real repo the model re-reads the same files and re-runs the same
searches, sometimes burning millions of input tokens before ending the same way again.

Neither `tm_agent::outcome::AgentTask` nor `tm_core::executor::ExecutorTask` carries anything from one
dispatch to the next; each `Executor::execute` call is otherwise a clean-slate function of its input.

## Decision

`BuiltinExecutor` persists a concise `InvestigationSummary` (deduplicated tool-call signatures, plus
the model's own last non-empty reply) as a tagged `ArtifactKind::Report` artifact whenever an attempt
ends without submitting, and reads the most recent one back on the *next* dispatch of the same ticket
to fold a steering note onto that attempt's own context. Two consecutive no-submit attempts, where the
*later* one's investigation is mostly a repeat of the earlier one's (more than half of the later
attempt's own tool signatures also appear in the earlier attempt's — directional, not a plain set
overlap; see `InvestigationSummary::mostly_repeats`'s own doc comment for why), flip a sticky
`repeated` flag: every attempt from then on, in the same round, gets a specific,
user-actionable failure detail (naming what was repeated, or the model's own diagnosis/repro-ask) in
place of the generic `"model ended turn without submitting"`, and the *next* attempt's context tells
the model outright to stop investigating and either name a diagnosis or ask for a targeted repro.

"Same round" is scoped by **three** independent signals, all required to match the artifact
`BuiltinExecutor::prior_investigation` reads back, because any one alone misses a real production
path:

- `ExecutorTask::objective`, exact match. `tm ticket retry --guidance "..."` appends the guidance
  onto the objective (`Store::retry`), so a stale investigation from before a human redirected the
  ticket is invisible once the wording changes.
- `Ticket::retry.max_attempts`, exact match. `Store::retry` bumps this (`t.attempts +
  t.retry.max_attempts.max(1)`) on **every** retry, guided or not — so a bare `tm ticket retry T`
  with no `--guidance` at all, which leaves the objective byte-identical, still starts a fresh round
  by this signal alone.
- An explicit `cleared` flag, set by `BuiltinExecutor::mark_investigation_cleared` whenever a
  dispatch of the same round *does* reach `AgentOutcome::Submitted`. Neither of the two signals above
  changes across `Store::reject`'s `Submitted -> Verifying -> Recovery -> Ready` round trip, so
  without this a dispatch that follows a human rejecting a real submission would incorrectly read
  back the earlier no-submit chain's `repeated` flag and tell the model to stop investigating right
  after a human just asked it to try again. `cleared` only ever suppresses *steering* — the raw
  winning record's `attempt_count` (`PriorInvestigation`, before `PriorInvestigation::effective`
  filters it for steering purposes) still numbers the *next* no-submit attempt one past it, so a
  later attempt in the same round can still be caught as a repeat of what happened before the
  clearing, rather than every attempt after a clearing restarting at 1 forever and never
  overlapping anything again.

Without all three, "stop investigating, you already tried this" would be actively wrong advice in at
least one of: a human just re-scoped the work with guidance, a human asked for a plain retry with no
guidance, or a human rejected a real (if wrong) submission and asked for another attempt.

Implementation stays entirely inside `crates/tm-agent/src/agent_loop.rs`
(`InvestigationSummary`/`NO_SUBMIT_DETAIL`) and `crates/tm-agent/src/executor.rs`
(`PriorInvestigation`, `BuiltinExecutor::{prior_investigation, persist_no_submit_investigation,
mark_investigation_cleared, persist_investigation_record,
augmented_context}`) — no `tm-core`/`tm-scheduler` change. `Store::store_artifact`/`Store::view` were
already generic enough to carry this; no new event kind or table was needed.

## Why

- The artifact mechanism already exists and already tolerates a best-effort write/read (see
  `StoreArtifactSink`); reusing it needed no schema change, matching `SPEC.md` §0's preference for
  composing existing deterministic machinery over adding new persisted shape.
- Comparing tool-call *signatures* (tool name plus its most identifying argument — `path`/`query`/
  `pattern`/`command`, or a joined `argv` for `shell.run`/`build.run`/`test.run`, which take that
  instead of `command`) rather than exact byte-identical inputs is deliberate: a "did it touch the
  same ground" check tolerates a slightly different line range or flag on an otherwise-identical
  read, which is the actual repeated-investigation pattern seen in the trial evidence.
- Scoping by `objective`/`max_attempts`/`cleared` rather than a new ticket-side "round" counter avoids
  touching `tm-core`'s `Ticket`/`Store::retry`/`Store::reject` at all: every one of the three signals
  is read from state those already produce (`Ticket::objective`, `Ticket::retry.max_attempts`) or
  from an outcome this crate already observes (`AgentOutcome::Submitted`), rather than inventing a
  new persisted "round id" `tm-core` would need to grow and thread through.

## What this costs, stated plainly

- `BuiltinExecutor::prior_investigation` scans every artifact in `Store::view()` linearly, filtering
  by `meta.kind`/`meta.no_submit_ticket`/`meta.objective`/`meta.max_attempts`. Fine at today's
  per-project artifact counts; a project with a very large artifact history would want an indexed
  lookup instead.
- The three-signal round key is a close approximation of "this round", not a real round identity:
  it is possible (if unusual) to construct a sequence of `tm-core` operations that changes none of
  `objective`/`max_attempts` between two dispatches that a human would consider different rounds, or
  that changes one of them between two dispatches within what a human would call the same round. The
  three signals were chosen because they cover every real path this task found in
  `tm-scheduler`/`tm-core` (an ordinary retry, a guided retry, a bare retry, a reject-and-resubmit),
  not because they are a sound formal definition of "round".
- The "stop scheduling a third identical attempt" half of the acceptance criteria is only partially
  met: `FailureClass` has no variant that means "retryable, but a human should look before the
  scheduler tries again" — every existing class maps either to "keep retrying" or to
  `BudgetExhausted`'s specific meaning, which this is not. This task deliberately does not repurpose
  `BudgetExhausted` or otherwise touch `tm-core::ticket::FailureClass`/`is_retryable`, since that is a
  scheduler-visible behavior change well outside `crates/tm-agent`. Today, a repeated round still gets
  scheduled again by the ordinary retry path — it just carries a specific, actionable detail instead
  of a generic one once it does. Adding a real "needs a human before retrying" failure class (and
  wiring `tm-scheduler`'s escalation policy to it) is a natural follow-up, out of this task's scope.
- `InvestigationSummary::mostly_repeats` is a coarse, directional heuristic (more than half of the
  *later* attempt's own tool signatures also appear in the earlier attempt's), not a semantic
  judgment of whether the model actually made progress; a model that reads the same files but
  reasons its way to real new insight is indistinguishable from one that is genuinely stuck, by
  this check alone.
- This memory is a `BuiltinExecutor`-only side channel, not part of `ExecutorTask::context_pack`
  itself (`SPEC.md` §24.1's "every executor receives the same compiled artifact"). Concretely:
  `tm ticket context <ID>` (which previews `tm_context::pack::compile`'s own output, not anything
  `BuiltinExecutor` augments at dispatch time) never shows the steering note a following attempt
  would actually see, and a different `Executor` impl (an external harness, or `tm_acp::AcpExecutor`
  — already used in production for `codex`/`pi`/`opencode`-backed tickets per B-12, not a future
  possibility) gets none of this memory at all — only a `BuiltinExecutor`-driven run does. Moving
  this into
  `pack::compile` itself (so it is deterministic, executor-agnostic, and previewable) is the more
  architecturally correct home for it; this task keeps it inside `crates/tm-agent` because that is
  its stated scope (`crates/tm-agent/src/executor.rs`, `crates/tm-agent/src/agent_loop.rs`) and
  because `tm-context`/`tm-core` changes are a materially larger, separate piece of work.
- Only the exact `NO_SUBMIT_DETAIL` ending (`AgentLoop::drive`'s own nudge-exhausted failure) is
  recognized and remembered. A different way a ticketed run burns a whole attempt without
  submitting — e.g. `AgentOutcome::Failed`'s `"step limit ({N}) reached without submitting"` — is
  untouched by this task and still starts its next attempt cold. That path is plausibly where some
  of the largest token burns in the trial evidence actually came from (a run that kept calling
  tools productively enough to never hit the nudge path, but never converged on a submission
  either); recognizing it is a natural follow-up, deliberately left out here to keep this change to
  one well-defined failure shape rather than generalizing to "any attempt that didn't submit" in
  one pass.
