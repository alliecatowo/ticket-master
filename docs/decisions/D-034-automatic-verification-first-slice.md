# D-034 — Automatic verification, first slice: run `CommandSucceeds`, stay in `Submitted`

**Status:** accepted · **Date:** 2026-09-25 · **Supersedes:** nothing

## Context

`SPEC.md:721-724` ("Verification separation") and `SPEC.md:308` (`Submitted -> Verifying`
automatically) describe a ticket whose submission is checked before a human ever looks at it.
Nothing implemented that: `Store::verify` existed but was reachable only from the HTTP route, and
nothing created a `Verification` ticket or ran a check on `system`'s own initiative. Every
"Ready for review" ticket depended entirely on the worker's own claim that it ran the tests
(`u1-automatic-verification-step`).

A full implementation needs a real `Verification`-ticket path (`Store::verify` takes a verifier
`TicketId` and is subject to `AuditorMustDiffer`) — that is `u1-verification-state-and-review`,
deliberately out of scope here. This is the first slice: run what a ticket already knows how to
check, without touching the state machine's `Verifying`/`Auditing` states at all.

## Decision

In `tm_scheduler::dispatch::report_outcome`, once a ticket has reached `Submitted` (either because
this report just called `Store::submit`, or because the executor already submitted mid-run via its
own `ticket.submit` tool — the common case for `tm-agent`'s `BuiltinExecutor`), collect every
`Predicate::CommandSucceeds { command }` leaf named anywhere in the ticket's own `t.success`
(`Predicate::walk` finds them under `AllOf`/`AnyOf`/`Not` too) and run each `argv` directly — never
through a shell — in the dispatcher's `repo_root`, as `ParticipantId::system()`.

- A pass records the transcript as an `ArtifactKind::CommandOutput` artifact and attaches it as
  `EvidenceKind::CommandOutput` evidence, and leaves the ticket in `Submitted` — `Store::accept`/
  `Store::reject` (which both begin with `Trigger::VerificationStarted` from `Submitted`) keep
  working exactly as before.
- A failure calls a new `Store::fail_automatic_verification(ticket, reason)`, which reuses
  `Store::reject`'s own `Submitted -> Verifying -> Recovery` draft sequence (so the ticket rejoins
  the normal retry/escalate path a human's rejection uses) but is *not* `Store::reject` itself and
  has no `actor` parameter: it always records `ParticipantId::system()` internally and is not
  reachable from any surface (CLI, HTTP, MCP) a human or an agent could call with a spoofed actor —
  `ParticipantId`'s own parser accepts the literal string `"system"`, so a guard that merely
  checked `actor == ParticipantId::system()` on a caller-supplied actor would not be safe.
- A ticket naming no `CommandSucceeds` predicate, or dispatched with no `repo_root` configured
  (nothing checked out to run a command in, e.g. a test harness with no real git working tree),
  is left untouched — same as before this change.

Left out of this slice, on purpose: `Predicate::TestsPass`/`FileExists`/`FileMatches`/`Judgment`
and a project-wide `harness.toml` `verify_command` (that field does not exist yet, and
`tm-scheduler` deliberately does not depend on `tm-harness`, the same reasoning
`ContextPackSource`'s own doc comment gives for not depending on `tm-context`).

## Why

`report_outcome` already runs on the scheduler's own background task after every dispatched run,
already has `repo_root`, and already draws the line between "the executor's own claim" (evidence,
a patch) and "what `Store` durably records" — the natural seam for a check that must run whether or
not the ticket happened to submit mid-run via a tool call. Reusing `Store::reject`'s exact draft
sequence (rather than inventing a new transition) means the retry/escalation policy an operator
already understands (`RetryPolicy`, `docs/decisions`'s existing failure-handling docs) applies
identically to an automatic failure and a human one.

## What this costs, stated plainly

- No `Verification`/`Audit` ticket is created, and `Submitted -> Verifying -> Auditing` is not
  driven for a pass — a passing ticket still needs a human `tm ticket accept` to close, same as
  today. `u1-verification-state-and-review` is the follow-up that drives that transition for real
  and teaches `tm ticket accept`/`reject` to also work from `Verifying`/`Auditing`.
- `Predicate::TestsPass` (the common case a worker would actually name) is not yet runnable — only
  literal `CommandSucceeds` argv. A ticket that wants automatic verification today must spell out
  the command itself (e.g. `cargo test -p tm-foo`), not just say "run my tests".
- A command's stdout/stderr is stored and attached in full via `Store::store_artifact` (subject to
  that store's own inline/on-disk threshold), but the `reason` on a rejection is trimmed to its
  last ~800 characters — long enough to be useful, not a full log dump in the ticket's failure
  history.
