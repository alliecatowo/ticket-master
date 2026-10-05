+++
title = "Tickets and workers"
weight = 2
description = "How a sentence becomes a ticket, how a background worker takes it from queued to submitted, and what only you can do next."
+++

A **ticket** is a unit of work with an objective, a state, an authority and a budget. A **worker** is
a background agent that leases a ticket, does the work, and submits it with evidence. You stay in the
chat; tickets are how you hand work off and take it back.

## The journey of a ticket

```
draft -> ready -> leased -> running -> submitted -> closed
                                 \-> escalated      (needs you)
```

The full state machine in `tm` has fourteen states: `draft`, `blocked`, `ready`, `leased`, `running`,
`submitted`, `verifying`, `auditing`, `rework`, `replan`, `recovery`, `escalated`, `closed` and
`cancelled`. The tickets screen folds them into the groups you act on: **Needs input**, **Working**,
**Ready for review**, **Queued** and **Completed**.

| You see | State | Who moves it next |
| --- | --- | --- |
| Queued | `ready` | a worker, when the scheduler leases it |
| Working | `leased`, `running` | the worker |
| Ready for review | `submitted` | **you**: accept, reject or retry |
| Needs input | `escalated` | **you**: `tm ticket retry`, optionally with guidance |
| Completed | `closed` | nobody; `tm ticket reopen` undoes it |

## Creating tickets

```sh
tm ticket dispatch "Add per-key rate limiting to POST /v1/export"
```

```
Dispatched ticket T-1: ready for a worker (tm run T-1)
```

`dispatch` creates the ticket and queues it in one step. In the chat, `/bg <task>` does the same, and
the tickets screen has a dispatch input at the bottom. If you want to edit first, `tm ticket new` makes a
`draft`, and `tm ticket activate` queues it.

```sh
tm ticket list
```

```
ID   Kind  State  Priority  Due  Objective
T-1  work  ready  0         -    Add per-key rate limiting to POST /v1/export
```

`tm ticket show T-1` prints everything about it, including its authority and token and wall-time totals.

## How workers run

While the TUI or `tm serve` is open, a scheduler runs in the same process and works ready tickets. You
can also drive it yourself:

```sh
tm run T-1          # work one ticket in the foreground, printing each step
tm sched run        # run the scheduler loop until nothing is left
```

A worker takes a **lease** on the ticket, so two workers never edit the same one. If a worker dies, its
lease lapses and the scheduler picks the ticket back up (the tickets screen shows "no worker attached; its
lease lapsed"). Each attempt runs under the ticket's budget. A failed attempt is retried within a bounded
retry policy, and a ticket that runs out of attempts becomes `escalated` and waits for you.

For isolation, `tm run T-1 --worktree` runs the ticket in its own git worktree on a fresh branch, so the
worker never touches your checkout. The worktree is removed once the run reaches a forward state.

### What a worker is allowed to do

New tickets get the **worker** authority: read and write the repository, run commands, `git commit` and
`git branch`. A worker cannot merge, push or force-push, cannot close, cancel or reopen tickets (it never
verifies its own work), cannot edit project-level settings, and has no arbitrary network access. See
[Oversight and approvals](@/reference/oversight.md) for gating more.

## Submitting and deciding

A worker ends by **submitting**: a summary and evidence, such as a test report or a diff. The ticket
moves to `submitted` and waits for you.

```sh
tm ticket show T-1                      # read the summary and evidence
tm ticket accept T-1                    # done; the ticket closes
tm ticket reject T-1 --reason "..."     # back to work; the next attempt sees your reason
tm ticket retry T-1 --guidance "..."    # for an escalated ticket: a fresh round of attempts
```

Accept, reject and retry are **human-only**. The HTTP server answers `403` to an agent that tries, and
the MCP server does not expose them. In the TUI, open a submission with `Space` and press `1` to accept or
`2` to reject.

Trying to skip a step fails with a clear message instead of doing something surprising:

```
error: can't do that from this state: T-1 is ready, so it can't be accepted. Run it: tm run T-1
```

## Everything is an event

Each transition, lease, tool call and decision is appended to a hash-chained event log in the project's
state directory. `tm doctor` verifies the chain, `tm events tail --follow` streams it, and
`tm run <ticket> --record cassette.json` captures a run so `--replay` can rerun it offline. `tm ticket fork
<ticket> --at <seq>` starts a new ticket from an earlier point in another one's history.
