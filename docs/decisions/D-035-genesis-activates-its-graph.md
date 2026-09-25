# D-035 — Genesis activates its committed ticket graph

**Status:** accepted · **Date:** 2026-09-25 · **Supersedes:** D-027 in part

## Context

Genesis compiled and committed its graph as Draft tickets. Its next-step advice named `tm sched
run`, but the scheduler ignores drafts, making that advice ineffective without manual activation.
The earlier choice to keep genesis purely a planning command also prevented its optional
`--run` mode from carrying a project through its first milestone.

## Decision

After graph compilation, `tm genesis` activates the newly committed Draft tickets through
`Store::activate`, making eligible tickets Ready and dependency-blocked tickets Blocked. The normal
command remains bounded: it reports the next step and exits. `tm genesis --run` additionally starts
the in-process scheduler, waits for the V0 milestone to close or a member ticket to escalate, then
resumes the persisted Genesis stages.

## Why

Activation is a lifecycle transition, not execution. It makes the scheduler command Genesis names
usable while preserving an explicit opt-in for starting worker execution. The optional run mode
reuses the scheduler and stops at observable project-state boundaries rather than hiding a second
ticket executor inside Genesis.

## What this costs, stated plainly

- Genesis now changes committed tickets from Draft, so callers that expected to review an entirely
  draft graph must account for Ready and dependency-blocked tickets.
- `--run` can wait indefinitely for work to finish; escalation ends the wait and Genesis resumes
  from its persisted stage.
- The existing V0/V1 milestone checks and maturity-gate stop policy remain in place.
