//! `tm stats`: per-ticket/day/model/tool rollups over local telemetry (D-030 local telemetry).
//!
//! Everything here folds the project's own event log rather than reading any separate ledger —
//! `usage.recorded` and `tool_call.completed` are already the durable record of what a call cost
//! and how it went (`docs/decisions/D-030-local-telemetry.md`); this module just aggregates them
//! a few different ways. The aggregation itself ([`stats_by_ticket`], [`stats_by_day`],
//! [`stats_by_model`], [`stats_by_tool`]) is a pure function of `&[Event]`, unit-tested directly
//! against hand-built fixtures; [`dispatch_stats`] is the only IO (reading the log, rendering the
//! result) layered on top.

use std::collections::BTreeMap;

use serde::Serialize;
use tm_events::{Event, EventKind, EventLog};
use tm_harness::metrics::{ticket_metrics_from_events, TicketMetrics};
use tm_types::TicketId;

use crate::args::{StatsArgs, StatsBy};
use crate::project::Project;
use crate::render::{Renderer, Table};

/// Read every event in the project's log, oldest first. Mirrors `project.rs`'s own private
/// `read_all_events`/`open_event_log` helpers (not reusable from here: they are not `pub`, and
/// `tm-cli`'s convention is that each command module reads the log through its own thin,
/// independent handle rather than sharing `project.store`'s internal one -- see
/// `crate::project::open_event_log`'s doc comment).
fn read_all_events(project: &Project) -> tm_types::Result<Vec<Event>> {
    let db_path = project.state_dir.join("project.db");
    let log = EventLog::open_with_clock(&db_path, project.clock.clone())?;
    const BATCH: usize = 1024;
    let mut out = Vec::new();
    let mut seq = 1u64;
    loop {
        let batch = log.read_from(seq, BATCH)?;
        if batch.is_empty() {
            break;
        }
        seq += batch.len() as u64;
        out.extend(batch);
    }
    Ok(out)
}

/// One calendar day's rollup, bucketed by each folded event's UTC date (the first 10 characters
/// of its RFC 3339 timestamp, `YYYY-MM-DD`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DayStats {
    /// The UTC calendar date, `YYYY-MM-DD`.
    pub day: String,
    /// Sum of `usage.recorded`'s `tokens` for this day.
    pub tokens: u64,
    /// Sum of `usage.recorded`'s `dollars_micros` for this day.
    pub dollars_micros: u64,
    /// Sum of `usage.recorded`'s `wall_seconds` for this day.
    pub wall_seconds: u64,
    /// Count of `tool_call.completed` events for this day.
    pub tool_calls: u64,
}

/// One `(provider, model)` pair's rollup, keyed `"<provider>/<model>"`, or `"unattributed"` for
/// `usage.recorded` events with no `provider`/`model` (every event recorded before
/// `tel-usage-payload-model-field` landed, or from a call that genuinely has no served-by pair).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelStats {
    /// `"<provider>/<model>"`, or `"unattributed"`.
    pub model: String,
    /// Sum of `tokens` across matching `usage.recorded` events.
    pub tokens: u64,
    /// Sum of `dollars_micros` across matching `usage.recorded` events.
    pub dollars_micros: u64,
    /// Sum of `wall_seconds` across matching `usage.recorded` events.
    pub wall_seconds: u64,
    /// Count of matching `usage.recorded` events.
    pub calls: u64,
}

/// One tool's rollup over its `tool_call.completed` events.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolStats {
    /// The tool name, e.g. `"bash"`.
    pub tool: String,
    /// Total calls to this tool.
    pub count: u64,
    /// Calls whose `outcome` was not `"completed"` (`"denied"` or `"error"`).
    pub failures: u64,
    /// Mean `duration_ms` across this tool's calls, `0.0` when `count == 0`.
    pub avg_duration_ms: f64,
    /// Sum of recorded result sizes, displayed in kibibytes.
    pub result_bytes: u64,
    /// Largest request recorded in the selected event set.
    pub max_request_tokens: u64,
}

/// The ticket this event's payload names, if any -- only `usage.recorded`, `tool_call.completed`
/// and `command.completed` carry one.
fn event_ticket(event: &Event) -> Option<TicketId> {
    match event.kind {
        EventKind::UsageRecorded => event
            .payload
            .as_usage_recorded()
            .and_then(|p| p.ticket.clone()),
        EventKind::ToolCallCompleted => event
            .payload
            .as_tool_call_completed()
            .and_then(|p| p.ticket.clone()),
        EventKind::CommandCompleted => event
            .payload
            .as_command_completed()
            .and_then(|p| p.ticket.clone()),
        _ => None,
    }
}

/// Every distinct ticket named by a ticket-carrying event in `events`, in first-seen order.
fn distinct_tickets(events: &[Event]) -> Vec<TicketId> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for event in events {
        if let Some(ticket) = event_ticket(event) {
            if seen.insert(ticket.clone()) {
                out.push(ticket);
            }
        }
    }
    out
}

/// Only the events naming `ticket`, in the same relative order, cloned out of `events` -- how
/// `--ticket` restricts `--by day|model|tool` (`--by ticket` restricts differently, via
/// [`stats_by_ticket`]'s own `only` argument, so a filtered-out ticket still gets a zeroed row
/// instead of silently vanishing).
fn events_for_ticket(events: &[Event], ticket: &TicketId) -> Vec<Event> {
    events
        .iter()
        .filter(|event| event_ticket(event).as_ref() == Some(ticket))
        .cloned()
        .collect()
}

/// Per-ticket rollup: [`ticket_metrics_from_events`] for every ticket named in `events`, in
/// first-seen order, or for just `only` when given (folded even if `only` never appears in
/// `events`, in which case every field but `ticket` stays at its zero default -- matching
/// `ticket_metrics_from_events`'s own documented behavior on an empty fold).
pub fn stats_by_ticket(events: &[Event], only: Option<&TicketId>) -> Vec<TicketMetrics> {
    match only {
        Some(ticket) => vec![ticket_metrics_from_events(ticket, events)],
        None => distinct_tickets(events)
            .iter()
            .map(|ticket| ticket_metrics_from_events(ticket, events))
            .collect(),
    }
}

/// Per-day rollup over every `usage.recorded`/`tool_call.completed` event, sorted by day
/// ascending (`BTreeMap` iteration order).
pub fn stats_by_day(events: &[Event]) -> Vec<DayStats> {
    let mut days: BTreeMap<String, DayStats> = BTreeMap::new();
    for event in events {
        match event.kind {
            EventKind::UsageRecorded => {
                if let Some(p) = event.payload.as_usage_recorded() {
                    let day = day_bucket(event);
                    let row = days.entry(day.clone()).or_insert_with(|| DayStats {
                        day,
                        tokens: 0,
                        dollars_micros: 0,
                        wall_seconds: 0,
                        tool_calls: 0,
                    });
                    row.tokens = row.tokens.saturating_add(p.tokens);
                    row.dollars_micros = row.dollars_micros.saturating_add(p.dollars_micros);
                    row.wall_seconds = row.wall_seconds.saturating_add(p.wall_seconds);
                }
            }
            EventKind::ToolCallCompleted if event.payload.as_tool_call_completed().is_some() => {
                let day = day_bucket(event);
                let row = days.entry(day.clone()).or_insert_with(|| DayStats {
                    day,
                    tokens: 0,
                    dollars_micros: 0,
                    wall_seconds: 0,
                    tool_calls: 0,
                });
                row.tool_calls = row.tool_calls.saturating_add(1);
            }
            _ => {}
        }
    }
    days.into_values().collect()
}

/// This event's UTC calendar date, `YYYY-MM-DD` -- the first 10 characters of its RFC 3339
/// timestamp, which is always `YYYY-MM-DDTHH:MM:SS...` by construction.
fn day_bucket(event: &Event) -> String {
    event.ts.to_rfc3339().chars().take(10).collect()
}

/// Per-`(provider, model)` rollup over `usage.recorded` events, sorted by key ascending.
pub fn stats_by_model(events: &[Event]) -> Vec<ModelStats> {
    let mut models: BTreeMap<String, ModelStats> = BTreeMap::new();
    for event in events {
        if event.kind != EventKind::UsageRecorded {
            continue;
        }
        let Some(p) = event.payload.as_usage_recorded() else {
            continue;
        };
        let key = match (&p.provider, &p.model) {
            (Some(provider), Some(model)) => format!("{provider}/{model}"),
            _ => "unattributed".to_string(),
        };
        let row = models.entry(key.clone()).or_insert_with(|| ModelStats {
            model: key,
            tokens: 0,
            dollars_micros: 0,
            wall_seconds: 0,
            calls: 0,
        });
        row.tokens = row.tokens.saturating_add(p.tokens);
        row.dollars_micros = row.dollars_micros.saturating_add(p.dollars_micros);
        row.wall_seconds = row.wall_seconds.saturating_add(p.wall_seconds);
        row.calls = row.calls.saturating_add(1);
    }
    models.into_values().collect()
}

/// Per-tool rollup over `tool_call.completed` events, sorted by tool name ascending.
pub fn stats_by_tool(events: &[Event]) -> Vec<ToolStats> {
    let max_request_tokens = events
        .iter()
        .filter_map(|event| event.payload.as_usage_recorded().map(|p| p.tokens))
        .max()
        .unwrap_or(0);
    struct Acc {
        count: u64,
        failures: u64,
        duration_sum_ms: u64,
        result_bytes: u64,
    }
    let mut tools: BTreeMap<String, Acc> = BTreeMap::new();
    for event in events {
        if event.kind != EventKind::ToolCallCompleted {
            continue;
        }
        let Some(p) = event.payload.as_tool_call_completed() else {
            continue;
        };
        let acc = tools.entry(p.tool_name.clone()).or_insert(Acc {
            count: 0,
            failures: 0,
            duration_sum_ms: 0,
            result_bytes: 0,
        });
        acc.count += 1;
        if p.outcome != "completed" {
            acc.failures += 1;
        }
        acc.duration_sum_ms = acc.duration_sum_ms.saturating_add(p.duration_ms);
        acc.result_bytes = acc.result_bytes.saturating_add(p.result_bytes.unwrap_or(0));
    }
    tools
        .into_iter()
        .map(|(tool, acc)| ToolStats {
            tool,
            count: acc.count,
            failures: acc.failures,
            avg_duration_ms: if acc.count > 0 {
                acc.duration_sum_ms as f64 / acc.count as f64
            } else {
                0.0
            },
            result_bytes: acc.result_bytes,
            max_request_tokens,
        })
        .collect()
}

/// This row's dollar figure, human-readable: `"not priced"` for `0` -- per
/// `docs/decisions/D-030-local-telemetry.md`, `dollars_micros == 0` means the candidate that
/// served this call has no price configured, not that the call was free -- otherwise
/// `$X.XXXXXX` at micro-dollar precision (a ticket/day/model row's real cost is small enough,
/// fractions of a cent, that two-decimal rounding would print `$0.00` for most of them).
fn format_dollars(micros: u64) -> String {
    if micros == 0 {
        "not priced".to_string()
    } else {
        format!("${:.6}", micros as f64 / 1_000_000.0)
    }
}

/// This ticket's session, or an em dash when it's [`tm_harness::metrics`]'s placeholder for "no
/// session observed" (`session.as_str() == "S-0"`, never a real allocated session id) -- showing
/// the placeholder verbatim in a human table would read as a real session that just happens to
/// be numbered zero.
fn session_label(session: &tm_types::SessionId) -> String {
    if session.as_str() == "S-0" {
        "—".to_string()
    } else {
        session.to_string()
    }
}

/// Restrict `events` to `ticket` when given, otherwise leave them as-is. Used by `--by
/// day|model|tool`; `--by ticket` restricts differently (see [`stats_by_ticket`]'s own `only`
/// argument).
fn restrict_to_ticket(events: &[Event], ticket: Option<&TicketId>) -> Vec<Event> {
    match ticket {
        Some(ticket) => events_for_ticket(events, ticket),
        None => events.to_vec(),
    }
}

fn ticket_result_bytes(events: &[Event], ticket: &TicketId) -> u64 {
    events
        .iter()
        .filter_map(|event| event.payload.as_tool_call_completed())
        .filter(|p| p.ticket.as_ref() == Some(ticket))
        .fold(0u64, |sum, p| {
            sum.saturating_add(p.result_bytes.unwrap_or(0))
        })
}

fn ticket_max_request_tokens(events: &[Event], ticket: &TicketId) -> u64 {
    events
        .iter()
        .filter_map(|event| event.payload.as_usage_recorded())
        .filter(|p| p.ticket.as_ref() == Some(ticket))
        .map(|p| p.tokens)
        .max()
        .unwrap_or(0)
}

/// A friendly one-liner for an empty rollup, naming what to do next rather than printing a bare
/// header row.
fn empty_state_message() -> &'static str {
    "No usage recorded yet. Run `tm run <ticket>` to record some."
}

/// `tm stats`: render the aggregation `args.by` selects.
pub fn dispatch_stats(
    args: &StatsArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let events = read_all_events(project)?;
    let ticket_filter = args.ticket.as_deref().map(TicketId::new).transpose()?;
    match args.by {
        StatsBy::Ticket => {
            let rows = stats_by_ticket(&events, ticket_filter.as_ref());
            if rows.is_empty() && !renderer.is_json() {
                renderer.note(empty_state_message());
                return Ok(());
            }
            let table = Table::new(
                vec![
                    "Ticket".to_string(),
                    "Session".to_string(),
                    "Tool calls".to_string(),
                    "Tokens".to_string(),
                    "Cost".to_string(),
                    "Wall time (s)".to_string(),
                    "Result KB".to_string(),
                    "Max request tokens".to_string(),
                ],
                rows.iter()
                    .map(|m| {
                        vec![
                            m.ticket.to_string(),
                            session_label(&m.session),
                            m.tool_calls.to_string(),
                            m.tokens_in.to_string(),
                            format_dollars(m.dollars_micros),
                            m.wall_seconds.to_string(),
                            format!(
                                "{:.1}",
                                ticket_result_bytes(&events, &m.ticket) as f64 / 1024.0
                            ),
                            ticket_max_request_tokens(&events, &m.ticket).to_string(),
                        ]
                    })
                    .collect(),
            );
            renderer.emit(&rows, &table.render_colored(renderer.color_enabled()))
        }
        StatsBy::Day => {
            let events = restrict_to_ticket(&events, ticket_filter.as_ref());
            let rows = stats_by_day(&events);
            if rows.is_empty() && !renderer.is_json() {
                renderer.note(empty_state_message());
                return Ok(());
            }
            let table = Table::new(
                vec![
                    "Day".to_string(),
                    "Tokens".to_string(),
                    "Cost".to_string(),
                    "Wall time (s)".to_string(),
                    "Tool calls".to_string(),
                ],
                rows.iter()
                    .map(|d| {
                        vec![
                            d.day.clone(),
                            d.tokens.to_string(),
                            format_dollars(d.dollars_micros),
                            d.wall_seconds.to_string(),
                            d.tool_calls.to_string(),
                        ]
                    })
                    .collect(),
            );
            renderer.emit(&rows, &table.render_colored(renderer.color_enabled()))
        }
        StatsBy::Model => {
            let events = restrict_to_ticket(&events, ticket_filter.as_ref());
            let rows = stats_by_model(&events);
            if rows.is_empty() && !renderer.is_json() {
                renderer.note(empty_state_message());
                return Ok(());
            }
            let table = Table::new(
                vec![
                    "Model".to_string(),
                    "Calls".to_string(),
                    "Tokens".to_string(),
                    "Cost".to_string(),
                    "Wall time (s)".to_string(),
                ],
                rows.iter()
                    .map(|m| {
                        vec![
                            m.model.clone(),
                            m.calls.to_string(),
                            m.tokens.to_string(),
                            format_dollars(m.dollars_micros),
                            m.wall_seconds.to_string(),
                        ]
                    })
                    .collect(),
            );
            renderer.emit(&rows, &table.render_colored(renderer.color_enabled()))
        }
        StatsBy::Tool => {
            let events = restrict_to_ticket(&events, ticket_filter.as_ref());
            let rows = stats_by_tool(&events);
            if rows.is_empty() && !renderer.is_json() {
                renderer.note(empty_state_message());
                return Ok(());
            }
            let table = Table::new(
                vec![
                    "Tool".to_string(),
                    "Count".to_string(),
                    "Failures".to_string(),
                    "Avg duration (ms)".to_string(),
                    "Result KB".to_string(),
                    "Max request tokens".to_string(),
                ],
                rows.iter()
                    .map(|t| {
                        vec![
                            t.tool.clone(),
                            t.count.to_string(),
                            t.failures.to_string(),
                            format!("{:.1}", t.avg_duration_ms),
                            format!("{:.1}", t.result_bytes as f64 / 1024.0),
                            t.max_request_tokens.to_string(),
                        ]
                    })
                    .collect(),
            );
            renderer.emit(&rows, &table.render_colored(renderer.color_enabled()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_events::EventDraft;
    use tm_types::{Id, ParticipantId, SessionId, Timestamp};

    fn ticket(id: &str) -> TicketId {
        TicketId::new(id).expect("valid ticket id")
    }

    fn session(id: &str) -> SessionId {
        SessionId::new(id).expect("valid session id")
    }

    fn actor() -> ParticipantId {
        ParticipantId::new("human:test").expect("valid participant id")
    }

    /// Builds an [`Event`] the same shape `EventLog::append` would produce, without needing a
    /// real log: `hash`/`seq`/`ts` are never read by this module's aggregation, only `kind`,
    /// `payload`, and (for [`stats_by_day`]) `ts`.
    fn event(seq: u64, ts: Timestamp, payload: tm_events::Payload) -> Event {
        let draft = EventDraft::new(actor(), Id::none(), payload);
        Event {
            seq,
            ts,
            kind: draft.kind(),
            subject: draft.subject,
            actor: draft.actor,
            session: draft.session,
            causation: draft.causation,
            correlation: draft.correlation,
            payload: draft.payload,
            hash: "test".to_string(),
        }
    }

    fn usage_event(
        seq: u64,
        ts: Timestamp,
        ticket_id: &TicketId,
        tokens: u64,
        dollars_micros: u64,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Event {
        event(
            seq,
            ts,
            tm_events::Payload::from(tm_events::payload::UsageRecordedPayload {
                ticket: Some(ticket_id.clone()),
                session: Some(session("S-1")),
                tokens,
                dollars_micros,
                wall_seconds: 1,
                provider: provider.map(str::to_string),
                model: model.map(str::to_string),
            }),
        )
    }

    fn tool_event(
        seq: u64,
        ts: Timestamp,
        ticket_id: &TicketId,
        tool_name: &str,
        duration_ms: u64,
        outcome: &str,
    ) -> Event {
        event(
            seq,
            ts,
            tm_events::Payload::from(tm_events::payload::ToolCallCompletedPayload {
                ticket: Some(ticket_id.clone()),
                session: Some(session("S-1")),
                tool_name: tool_name.to_string(),
                duration_ms,
                outcome: outcome.to_string(),
                result_bytes: None,
                truncated: None,
            }),
        )
    }

    #[test]
    fn stats_by_ticket_folds_every_distinct_ticket_in_first_seen_order() {
        let t1 = ticket("T-1");
        let t2 = ticket("T-2");
        let events = vec![
            usage_event(1, Timestamp::EPOCH, &t2, 10, 1000, None, None),
            usage_event(2, Timestamp::EPOCH, &t1, 20, 2000, None, None),
        ];
        let rows = stats_by_ticket(&events, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ticket, t2);
        assert_eq!(rows[0].tokens_in, 10);
        assert_eq!(rows[1].ticket, t1);
        assert_eq!(rows[1].tokens_in, 20);
    }

    #[test]
    fn stats_by_ticket_with_only_returns_exactly_one_row_even_if_absent() {
        let t1 = ticket("T-1");
        let missing = ticket("T-99");
        let events = vec![usage_event(1, Timestamp::EPOCH, &t1, 10, 1000, None, None)];
        let rows = stats_by_ticket(&events, Some(&missing));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ticket, missing);
        assert_eq!(rows[0].tokens_in, 0);
    }

    #[test]
    fn stats_by_day_sums_tokens_and_counts_tool_calls_per_calendar_day() {
        let t1 = ticket("T-1");
        let day1 = Timestamp::parse_rfc3339("2026-09-24T10:00:00Z").expect("valid ts");
        let day2 = Timestamp::parse_rfc3339("2026-09-25T10:00:00Z").expect("valid ts");
        let events = vec![
            usage_event(1, day1, &t1, 100, 5000, None, None),
            usage_event(2, day1, &t1, 50, 2500, None, None),
            usage_event(3, day2, &t1, 10, 100, None, None),
            tool_event(4, day1, &t1, "bash", 200, "completed"),
        ];
        let rows = stats_by_day(&events);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].day, "2026-09-24");
        assert_eq!(rows[0].tokens, 150);
        assert_eq!(rows[0].dollars_micros, 7500);
        assert_eq!(rows[0].tool_calls, 1);
        assert_eq!(rows[1].day, "2026-09-25");
        assert_eq!(rows[1].tokens, 10);
        assert_eq!(rows[1].tool_calls, 0);
    }

    #[test]
    fn stats_by_model_groups_by_provider_and_model_with_none_as_unattributed() {
        let t1 = ticket("T-1");
        let events = vec![
            usage_event(
                1,
                Timestamp::EPOCH,
                &t1,
                100,
                1000,
                Some("codex"),
                Some("gpt-5"),
            ),
            usage_event(
                2,
                Timestamp::EPOCH,
                &t1,
                50,
                500,
                Some("codex"),
                Some("gpt-5"),
            ),
            usage_event(3, Timestamp::EPOCH, &t1, 10, 100, None, None),
        ];
        let rows = stats_by_model(&events);
        assert_eq!(rows.len(), 2);
        let attributed = rows.iter().find(|r| r.model == "codex/gpt-5").expect("row");
        assert_eq!(attributed.tokens, 150);
        assert_eq!(attributed.calls, 2);
        let unattributed = rows
            .iter()
            .find(|r| r.model == "unattributed")
            .expect("row");
        assert_eq!(unattributed.tokens, 10);
        assert_eq!(unattributed.calls, 1);
    }

    #[test]
    fn stats_by_tool_counts_failures_and_averages_duration() {
        let t1 = ticket("T-1");
        let events = vec![
            tool_event(1, Timestamp::EPOCH, &t1, "bash", 100, "completed"),
            tool_event(2, Timestamp::EPOCH, &t1, "bash", 300, "error"),
            tool_event(3, Timestamp::EPOCH, &t1, "read", 50, "completed"),
        ];
        let rows = stats_by_tool(&events);
        assert_eq!(rows.len(), 2);
        let bash = rows.iter().find(|r| r.tool == "bash").expect("row");
        assert_eq!(bash.count, 2);
        assert_eq!(bash.failures, 1);
        assert_eq!(bash.avg_duration_ms, 200.0);
        let read = rows.iter().find(|r| r.tool == "read").expect("row");
        assert_eq!(read.count, 1);
        assert_eq!(read.failures, 0);
    }

    #[test]
    fn stats_by_tool_and_ticket_include_result_size_and_max_request_tokens() {
        let t1 = ticket("T-1");
        let call = event(
            1,
            Timestamp::EPOCH,
            tm_events::Payload::from(tm_events::payload::ToolCallCompletedPayload {
                ticket: Some(t1.clone()),
                session: Some(session("S-1")),
                tool_name: "read".into(),
                duration_ms: 1,
                outcome: "completed".into(),
                result_bytes: Some(2048),
                truncated: Some(true),
            }),
        );
        let usage = usage_event(2, Timestamp::EPOCH, &t1, 8192, 0, None, None);
        let events = vec![call, usage];
        let tool = &stats_by_tool(&events)[0];
        assert_eq!(tool.result_bytes, 2048);
        assert_eq!(tool.max_request_tokens, 8192);
        assert_eq!(ticket_result_bytes(&events, &t1), 2048);
        assert_eq!(ticket_max_request_tokens(&events, &t1), 8192);
    }

    #[test]
    fn stats_by_tool_ignores_events_for_other_event_kinds() {
        let t1 = ticket("T-1");
        let events = vec![usage_event(1, Timestamp::EPOCH, &t1, 10, 100, None, None)];
        assert!(stats_by_tool(&events).is_empty());
    }

    #[test]
    fn format_dollars_renders_micro_dollar_precision_or_not_priced_for_zero() {
        assert_eq!(format_dollars(1_500_000), "$1.500000");
        assert_eq!(format_dollars(0), "not priced");
    }

    #[test]
    fn session_label_shows_an_em_dash_for_the_no_session_placeholder() {
        assert_eq!(session_label(&session("S-0")), "—");
        assert_eq!(session_label(&session("S-4")), "S-4");
    }

    #[test]
    fn restrict_to_ticket_scopes_by_and_tool_by_to_one_ticket() {
        // A regression test for a real defect: `--ticket` used to only affect `--by ticket`,
        // silently doing nothing for `--by day|model|tool`.
        let t1 = ticket("T-1");
        let t2 = ticket("T-2");
        let events = vec![
            tool_event(1, Timestamp::EPOCH, &t1, "bash", 100, "completed"),
            tool_event(2, Timestamp::EPOCH, &t2, "read", 50, "completed"),
        ];
        let only_t1 = restrict_to_ticket(&events, Some(&t1));
        let rows = stats_by_tool(&only_t1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool, "bash");

        let unfiltered = restrict_to_ticket(&events, None);
        assert_eq!(stats_by_tool(&unfiltered).len(), 2);
    }

    fn test_project(root: &std::path::Path) -> Project {
        use std::sync::Arc;
        use tm_types::{Clock, CounterIds, FixedClock, IdSource};
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Arc::new(
            tm_core::Store::open_with(root, clock.clone(), ids.clone()).expect("open store"),
        );
        Project::for_test(root, store, clock, ids)
    }

    #[test]
    fn dispatch_stats_end_to_end_over_a_real_store_shows_nonzero_tokens_for_the_ticket() {
        use tm_core::ticket::{TicketKind, VerificationPolicy};
        use tm_types::{Authority, Budget};

        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let renderer = Renderer::new(true, true, true, false);

        let events = project
            .store
            .create_ticket(
                TicketKind::Work,
                "do the thing".to_string(),
                None,
                None,
                Authority::worker(),
                vec![],
                crate::tickets::default_executor_requirements(),
                Vec::new(),
                Vec::new(),
                VerificationPolicy::Single,
                Budget::unlimited(),
                crate::tickets::default_retry_policy(),
                0,
                project.actor.clone(),
            )
            .expect("create ticket");
        let ticket_id = crate::tickets::event_ticket_id(&events[0].subject).expect("ticket id");

        project
            .store
            .record_usage_attributed(
                Some(&ticket_id),
                Some(&session("S-1")),
                tm_types::Spend {
                    tokens: 42,
                    dollars_micros: 7,
                    wall_seconds: 3,
                },
                project.actor.clone(),
                Some(("codex".to_string(), "gpt-5".to_string())),
            )
            .expect("record usage");

        let all_events = read_all_events(&project).expect("read events");
        let by_ticket = stats_by_ticket(&all_events, Some(&ticket_id));
        assert_eq!(by_ticket.len(), 1);
        assert_eq!(by_ticket[0].tokens_in, 42);

        let by_model = stats_by_model(&all_events);
        assert_eq!(by_model.len(), 1);
        assert_eq!(by_model[0].model, "codex/gpt-5");
        assert_eq!(by_model[0].tokens, 42);

        // Also exercised through the real dispatch path, matching the acceptance check's
        // `tm stats --json` shape.
        dispatch_stats(
            &StatsArgs {
                by: StatsBy::Ticket,
                ticket: Some(ticket_id.to_string()),
            },
            &project,
            &renderer,
        )
        .expect("dispatch stats");
    }
}
