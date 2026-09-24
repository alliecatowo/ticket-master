# D-027 — `tm genesis` stops for work instead of spinning on a failing maturity gate

**Status:** accepted · **Date:** 2026-09-24 · **Supersedes:** nothing

## Context

`tm genesis`'s CLI loop (`crates/tm-cli/src/project.rs`'s `genesis`) drove `GenesisDriver::advance`
in a bare `loop` until `Stage::SteadyState`, with no stop condition in between. Three gaps combine
so that loop never terminates on a real prompt:

- `Stage::GraphCompilation` commits the ticket graph's tickets as `Draft`
  (`crates/tm-genesis/src/compile.rs`'s `commit_graph`), but nothing in `genesis()` activates
  them. Draft tickets never run, so their milestone never closes.
- `Stage::V0` and `Stage::V1` transition unconditionally on their own event
  (`stages.rs::transition`) — they do not check that their milestone is actually closed. So the
  loop sails straight through `V0 -> Evaluation -> V1 -> Stabilization -> MaturityGate` on tickets
  nobody worked.
- Once at `MaturityGate`, a failing verdict (the only possible one, since nothing closed V1)
  transitions back to `Stabilization`, which re-enters `MaturityGate` unconditionally
  (`transition`'s `(Stage::Stabilization, StabilizationEntered) => MaturityGate`). Every pass makes
  a real `judge_maturity` provider call — a real, metered cost repeated forever with no operator in
  the loop to notice or stop it.

Three designs were considered:

1. **A hard cap on advances** (e.g. stop after N loop iterations). Simple, but the cap number is
   arbitrary, and it stops at the same unhelpful place regardless of what's actually going on —
   mid-`MaturityGate` retry vs. stuck at `V0` look identical from the counter's point of view, so
   the printed status can't say anything specific.
2. **Auto-activate `Draft` tickets from within `genesis()`** so the loop's own assumption (V0/V1
   milestones close on their own) becomes true. Rejected for this track: it silently starts running
   an agent loop's real work from inside what a user thinks of as a planning command, and picking
   an activation policy (which tickets, what authority, what budget) is exactly the kind of
   decision `tm sched run`/`tm run` already own — duplicating it here just to keep the loop moving
   is the wrong layer for it.
3. **Exit-and-resume** (chosen): stop the loop at the earliest point further progress requires work
   nobody has done yet, print a concrete status, and exit 0. `GenesisDriver::resume` already exists
   (scans for the latest persisted snapshot) and can rebuild a stopped run's `GenesisState`, but
   `genesis()` does not call it yet — today's `tm genesis` always starts a fresh
   `GenesisState::new`, so re-running after a stop begins a new run rather than resuming the
   stopped one. Wiring `--resume` into `genesis()` itself is the follow-up
   `genesis-cli-resume-flag` task; this decision only makes stopping safe, not resuming automatic.

## Decision

Extract the CLI loop into a testable `run_genesis_stages` (`project.rs`), which advances one stage
at a time exactly as before but applies an explicit termination policy, stopping (not erroring;
`Ok` with `Some(reason)`) at the earliest of:

- **`V0`/`V1` can't advance**: before calling `advance` for `Stage::V0` or `Stage::V1`, check
  whether that stage's milestone (resolved via the new `tm_genesis::stages::milestone_for_stage`, shared
  with `Stage::MaturityGate`'s own "which milestone is V1" approximation so the two can't drift
  apart) is `MilestoneState::Closed`. If not, stop. Since nothing activates the Draft graph today,
  this is hit immediately at `V0` — i.e., in practice this *is* "stop right after `Ignition`
  commits the graph," just expressed as the general, still-correct check rather than a special
  case for "the first time only."
- **The maturity gate just failed**: after an `advance` call that moves `MaturityGate ->
  Stabilization` (the transition table's failure arm), stop rather than looping back into another
  `Stabilization -> MaturityGate` pass. This bounds a single `tm genesis` invocation to at most one
  real `judge_maturity` provider call.

On stopping, the CLI prints a status naming what actually happened — e.g. "3 tickets committed
under milestone M-000000000002. Run `tm sched run` (or `tm run <T>`) to work them, then re-run `tm
genesis` to resume." — and exits 0 (not an error: stopping for work is normal operation, not a
failure). The message says "resume" because that is the eventual, spec-intended behavior once
`--resume` lands (see "What this costs" below for what actually happens today).

## Why

Exit-and-resume matches how every other long-running unit of work in this codebase is already
modeled: a ticket that can't proceed becomes `Blocked`/`Escalated` and waits for a human or worker,
it doesn't spin. `GenesisState` was already snapshotted and resumable (`GenesisDriver::resume`)
before this change — exit-and-resume is the design that was already half-built, just missing the
"stop" half. It also keeps `tm genesis` a planning command: it hands back a concrete, actionable
status and lets `tm sched run`/`tm run`/a human own actually doing the work, rather than genesis
silently deciding to activate tickets on its own behalf.

Checking the same milestone the `V0`/`V1` stages are gated on (rather than a purely mechanical
"stop after N stages" counter) means the printed status is always specific and correct: it names
the real milestone and the real ticket count, not a guess.

## What this costs, stated plainly

- `Stage::V0`/`Stage::V1`'s own `transition`/`advance` logic is *not* changed to require a closed
  milestone — they still transition unconditionally if something else drives them past the CLI's
  stop check (e.g. a future caller that doesn't use `run_genesis_stages`). The correctness burden
  for "don't advance on an open milestone" lives in the CLI loop's policy, not in the stage machine
  itself; a second caller of `GenesisDriver::advance` that skips `run_genesis_stages` would not get
  this protection for free.
- `milestone_for_stage`'s `V1` resolution is still the same approximation `MaturityGate` already
  used (first milestone distinct from V0, or the only one) — `GenesisState` still has no dedicated
  "V1 milestone" field. This change makes that approximation shared and consistent, not more
  precise.
- **`genesis()` does not call `GenesisDriver::resume` yet.** Re-running `tm genesis` after a stop
  today starts a brand-new `GenesisState` from `Stage::Seed` — a second full run (a second Seed,
  Vision, Spec, and a second committed ticket graph under a new milestone), not a continuation of
  the stopped one. The stopped run's snapshot is preserved (nothing is lost), but nothing
  automatically picks it back up; that wiring is `genesis-cli-resume-flag`, a separate task. Until
  it lands, the printed "re-run `tm genesis` to resume" is aspirational for the *state*, accurate
  only for the *intent* (do the outstanding work, then continue genesis).
- Relatedly, the printed next step says `tm sched run` (or `tm run <T>`) works the outstanding
  tickets — true for `tm run <T>` (which activates a Draft ticket itself), but `tm sched run` only
  works *Ready* tickets, and `GraphCompilation` commits them as `Draft`. This wording is carried
  over from this task's own spec; flagging it here rather than silently rewording it, since whether
  Draft tickets should auto-activate, or `tm sched run` should, is a call for whoever owns that
  flow, not this task.
- A run that stops at `V0` and is re-run before its milestone closes will stop at the exact same
  place again (once resume lands, this becomes "resumes to the same stop point"; today it starts
  over and stops at the same *kind* of point, since nothing closed the new run's milestone either) —
  expected, not a bug, but it means a tight `tm genesis` retry loop without doing the intervening
  work makes no progress.
