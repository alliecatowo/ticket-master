# D-021 — Waiting for provider capacity is not a failed attempt

**Status:** accepted · **Date:** 2026-09-23 · **Supersedes:** nothing

## Context

We drove the web client against `tm serve` and hit this. Two tickets were dispatched in the same
scheduler tick by the in-process runner (`sched::spawn_background_runner`). The provider fabric
served one request at a time (`max_concurrency = 1`, as the mock fabric has). The second request
found the slot taken, and `Fabric::route` answered `RouteDecision::Wait(until)`. `Fabric::execute`
turns that answer into an `Err`. The agent loop classed it as `FailureClass::ProviderUnavailable`,
and `Store::record_failure` counted it as a failed attempt. The attempt had already been spent
when the lease was taken, so each collision cost an attempt. One ticket escalated after a single
real attempt: attempts 1 and 2 were both "no candidate available for role coder.fast until …".

A `Wait` is not a failure. The router says so itself: "no candidate is admissible right now, but
at least one will free up by `until`; the caller should retry then".

## Decision

**The agent loop waits for capacity, bounded, instead of failing** (`AgentLoop::
execute_with_capacity_wait` in `crates/tm-agent/src/agent_loop.rs`).

- Before each provider call, the loop asks `Fabric::route` whether a candidate is free. If the
  answer is `Wait(until)`, it sleeps until `until`, re-checking at least once a second.
- It retries a call only when the fabric refused to route it: the fabric's `Wait` error, which
  means no provider was called. It retries only if a fresh `route` still says `Wait` (or already
  says a slot is free). A provider that was called and failed is never retried here. That stays
  a real failure, as before.
- The check after a refusal is what matters. Two loops dispatched in the same tick both see a
  free slot, and one of them loses it. The check before the call only saves a wasted refusal.
- The wait is bounded by `DEFAULT_CAPACITY_WAIT` (5 minutes; `AgentLoop::with_capacity_wait`
  overrides it, and `Duration::ZERO` restores fail-fast). The bound is counted in time actually
  slept, because an injected clock need not advance. A `Wait` whose `until` lies beyond what is
  left of the allowance fails at once, rather than sleeping out the allowance for a daily or
  monthly cap.
- Nothing is appended to the event log while waiting. The loop records `provider.*` events once,
  for the final result.

The fabric's own code is unchanged. It reports the `Wait` as a plain `TmError::Provider`, so the
loop recognizes it by its text (`CAPACITY_REFUSAL_PREFIX`).
`capacity_refusal_text_matches_the_fabric` pins that text against the real fabric, so a wording
change there fails a test instead of quietly bringing the bug back.

## Why

- **Not scheduler admission control.** The scheduler can't see provider capacity:
  `SchedulerView` carries no fabric state, and the fabric lives inside each executor. Admission
  also couldn't cover the other `Wait` causes (per-minute quota, an open breaker). Throttling
  tickets to the provider's concurrency would also serialize tool execution, which uses no
  provider slot.
- **Not "record the failure without spending an attempt".** Recording is too late: by then the
  attempt has ended. The worker gave up on a turn it could have finished a second later, and the
  ticket returned to `Ready` behind a retry delay. The log would also fill with "failures" that
  weren't.
- **Waiting in the loop fixes the problem where it happens.** The turn continues, the ticket keeps
  its lease (the dispatcher's heartbeat is still running), and the one path every executor,
  `tm run`, `tm sched run`, `tm serve` and the TUI's chat share gets the fix.

## What this costs, stated plainly

- **An attempt can still be spent once the allowance runs out.** A provider saturated for more
  than 5 minutes still produces a `ProviderUnavailable` failure that counts. That is deliberate:
  by then the provider really is unavailable to this ticket.
- **There is no fairness.** At concurrency 1, a waiter polls about once a second, while the
  ticket holding the slot re-requests as soon as its tools finish. A busy ticket can starve a
  waiting one until the allowance runs out. A real queue in the fabric would fix this. That code
  belongs to the provider track.
- **Matching the error by text is fragile.** The test above is the guard. A typed `Wait` error
  from `Fabric::execute` would remove the need for it, and is the natural follow-up.
- **A waiting chat turn looks idle.** It says nothing for up to the allowance. Esc still
  interrupts it, because the turn is dropped from a `select!`. Only a `tracing::debug!` line
  records the wait. The chat builds its own fabric (`AgentSession`), separate from the
  dispatcher's, so a chat turn and a ticket never wait on each other. The flip side: they don't
  share one concurrency limit either.
- **Many leased tickets can sit `Running` without doing anything.** The scheduler still leases up
  to its in-flight limit, and the extra tickets wait inside their turns. Each wait holds a lease,
  kept alive by the dispatcher's heartbeat.
