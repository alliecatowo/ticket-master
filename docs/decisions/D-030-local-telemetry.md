# D-030 — Local telemetry: real dollars and provider/model attribution, process-local

**Status:** accepted · **Date:** 2026-09-25 · **Supersedes:** nothing

## Context

`usage.recorded` is the one event kind every executor path (`tm run`, `tm sched run`, `tm serve`,
the TUI's chat) emits after a provider call, and it is the only source `tm-harness`'s bench cost
column and `tm stats` can fold real spend from without inventing new state. (The debit itself
happens earlier and separately, synchronously inside `Store::record_usage` against
`BudgetLedger` — this event is a durable record of a call that already happened, not something
budget enforcement reads back.) Two gaps kept it from being trustworthy telemetry rather than a
records-that-a-call-happened stub:

- **Cost.** `tel-completion-cost-field` (landed `c0980b7`) fixed `dollars_micros` from a hardcoded
  `0` to `Fabric::execute_priced`'s real per-call cost, when the served candidate has a `Price`
  configured in `providers.toml`. Before that, every dollar figure downstream — `tm stats`, the
  bench cost column, budget tier-down — was silently zero regardless of what was actually spent.
- **Attribution.** `tel-usage-payload-model-field` (this task) adds `provider`/`model` to
  `UsageRecordedPayload`, so a `usage.recorded` event names *which* backend served the call it
  accompanies. Before this, the payload carried tokens and dollars with no way to attribute either
  to a specific `(provider, model)` pair — `provider.selected` names a pair but carries no spend,
  and nothing joined the two without inventing state `tm_events` didn't carry (see
  `crates/tm-core/src/materialize.rs`'s `ProviderSelected`/`UsageRecorded` arms).

Both gaps share one shape: the fix is a payload field plus the plumbing to fill it in at the one
call site that has the real value (`AgentLoop::drive`'s provider call), not a new event kind and
not a new durable table.

## Decision

**`usage.recorded` carries the real cost and, optionally, who served it — nothing else changes.**

- `dollars_micros: u64` (`tel-completion-cost-field`) is `Fabric::execute_priced`'s actual
  per-call cost in micro-dollars, `0` when the served candidate has no `Price` configured (most
  local/dev candidates today) rather than an error — an unpriced candidate is a normal
  configuration, not a fault.
- `provider: Option<String>` / `model: Option<String>` (`tel-usage-payload-model-field`) name the
  `(provider, model)` pair `AgentLoop` actually called, sourced from `Completion::model`
  (`ModelId`'s two fields), not from the requested role. Both are `Option`, for two independent
  reasons: `payload_kinds!` (`crates/tm-events/src/payload.rs`) cannot carry per-field serde
  attributes, so there is nowhere to hang `#[serde(default)]` on just these two fields even if the
  macro's *other* fields didn't need it; and the event log is immutable and hash-chained, so an
  already-recorded, pre-attribution `usage.recorded` event has no `provider`/`model` keys at all —
  serde's derive treats a missing `Option<T>` field as `None` without needing `#[serde(default)]`,
  so that old-shape JSON keeps decoding unchanged.
- `Store::record_usage_attributed(ticket, session, amount, actor, served_by: Option<(String,
  String)>)` is the one place that constructs the event; `Store::record_usage` (every existing
  caller, across `tm-core`'s own budget tests) delegates to it with `served_by: None`, so none of
  them had to change to stay green. `AgentLoop::record_usage` is the only caller that passes
  `Some(...)`, built from the same `Completion::model` `StepRecord.served_by` already stringifies.

**`tm stats` (`tel-stats-cli-command`) is the one command that reads this telemetry back.** It
folds the whole event log (`crates/tm-cli/src/stats.rs`'s `read_all_events`, mirroring
`project.rs`'s own private helper of the same name/shape rather than sharing it) and rolls it up
`--by ticket` (default; `tm_harness::metrics::ticket_metrics_from_events` per distinct ticket, or
just `--ticket T` when given), `--by day` (calendar-date buckets from each event's own
timestamp), `--by model` (keyed `"<provider>/<model>"`, or `"unattributed"` for a `usage.recorded`
event with no `served_by` — the pre-attribution shape this same decision's cost/attribution split
above describes), and `--by tool` (count/failures/mean `duration_ms` per `tool_name`, `failures`
counting any `outcome` other than `"completed"`). Every aggregation is a pure function of `&[Event]`
(`stats_by_ticket`/`stats_by_day`/`stats_by_model`/`stats_by_tool`), unit-tested directly; only
`dispatch_stats` itself touches the log or a renderer.

**`tool_call.completed` (`tel-tool-call-event-kind`) is the new event kind `usage.recorded`'s
"Why" section below anticipated.** `EventKind::ToolCallCompleted` gets its own `EventCategory::
ToolCall`, following the `command.*`/`EventCategory::Command` precedent rather than folding into
an existing category. Its payload (`ToolCallCompletedPayload`: `ticket`, `session`, `tool_name`,
`duration_ms`, `outcome`) is deliberately narrower than `crate::outcome::ToolCallResolution`
itself: `outcome` is a plain `String` of `"completed"`/`"denied"`/`"error"` (mirroring that enum's
three arms), not the resolution's own richer per-arm payload (a `Completed`'s result value, a
`Denied`'s reason, an `Errored`'s detail) — those already live on the `ToolCallRecord` a step's
own transcript carries; this event exists for cross-ticket/cross-day rollup (`tel-ticket-metrics-
fold`, `tel-stats-cli-command`), which needs "what happened", not "what it said". Both
`AgentLoop::drive`'s per-turn tool-call loop and `AgentLoop::resume_with`'s single
resume-after-approval dispatch measure `duration_ms` themselves around each
`ToolRegistry::dispatch` call via the injected `Clock` (`Timestamp::millis_since`, clamped to `0`
— `0` for `resume_with`'s declined-without-dispatch path too, since nothing ran) rather than
reading it back from `ToolCallRecord`, which carries no timing field. `AgentLoop::drive` batches a
step's several calls into one `Store::append`, mirroring `AgentLoop::record_provider_events`'s
existing batching of a step's
`provider.*` events — one event per dispatched call, not per step.

## Why

- **A payload field, not a new event kind or table** for cost/attribution specifically.
  `tool_call.completed` (batch B11 in `docs/tasks/TASKS.md`) is a new event kind because a tool
  call has no existing home in the log at all. `usage.recorded` already exists and already names
  the ticket/session/tokens a call spent; cost and attribution are two more facts about the same
  call, not a new kind of fact.
- **`Option`, not a schema migration.** The event log has no migration mechanism — an old event's
  JSON is exactly what was written the day it was appended, immutable and hash-chained. Any field
  added to an existing payload must default cleanly when absent, or every already-recorded event
  of that kind becomes undecodable. Both fields added here follow that same convention the
  payload's own pre-existing `ticket`/`session` fields already used.
- **`record_usage_attributed` as a superset, not a breaking signature change.** Changing
  `record_usage`'s own signature to take `served_by` would have touched every one of `tm-core`'s
  own call sites for a capability only `tm-agent`'s `AgentLoop` actually has (a `Completion` to
  read `.model` off of) — `tm-core`'s budget tests construct `Spend` directly with no `Completion`
  in scope at all.

## What this costs, stated plainly

- **`provider_usage` (the `(provider, model)`-keyed freshness table in `tm-core`'s schema) does
  not roll this new attribution into a running total.** `usage.recorded`'s materialize.rs arm
  stays a deliberate no-op against that table even now that the payload carries enough to key a
  row on — see that arm's own comment. Doing so correctly (concurrent writers, an accumulating
  rather than replacing update) is a separate task; `provider_usage` remains "which
  providers/models have been selected, and how recently", not a precise spend ledger. Real spend
  enforcement still happens through `budgets`/`BudgetLedger`, unaffected by this.
- **`dollars_micros` is `0` for any unpriced candidate**, which today is effectively all of them —
  `providers.toml` has no non-test `RoleCandidate`s with `price: Some` configured
  (`critic-real-prices-and-price-unit`, a separate open task, fills these in and fixes a
  sub-$1/M-token truncation bug in the price unit itself). A `0` in `tm stats`/the bench cost
  column for those candidates means "not priced", not "free".
- **This stays process-local, like the rest of the provider fabric's live state.** Neither
  `dollars_micros` nor `provider`/`model` attribution feeds any cross-process aggregation:
  `FabricState`/`LedgerEntry` (`crates/tm-provider/src/state.rs`) already track in-memory routing
  state per process, not shared across a multi-worker deployment — see
  `crates/tm-cli/src/ops.rs`'s `PROVIDER_LIVE_STATE_NOTE`. `usage.recorded` events themselves are
  durable and shared (the event log, not the fabric's live state), but nothing here changes that
  boundary; it only makes each individual event more informative.

## Not yet done

`clients/ts/src/domain.ts`'s hand-mirrored `"usage.recorded"` payload type (and
`clients/web/src/views/timelineModel.ts`'s consumer of it) do not yet carry `provider`/`model` —
out of scope for this batch's owned files; a follow-up should add them as `string | null` to match
the Rust `Option<String>` shape.

Same gap for `tool_call.completed`: `clients/ts/src/domain.ts` has no `"tool_call.completed"` arm
yet, so a TS-side consumer parsing the event log's raw JSON doesn't see it typed — again out of
scope for this batch's owned files. `SPEC.md` §3.2's event catalogue table likewise has no
`tool_call.completed` row yet — `SPEC.md` is owned by another track in this batch, so this is
flagged rather than edited directly.
