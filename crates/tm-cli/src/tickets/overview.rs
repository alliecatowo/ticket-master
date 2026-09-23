//! The ticket overview behind `tm tickets` and the TUI's tickets screen (D-019 §2): every ticket
//! reduced to what the `claude agents`-style view shows — which group it sits in, a one-line
//! summary of what it is doing or needs, who (if anyone) is working it, and the detail the peek
//! panel expands.
//!
//! The ticket projection alone cannot say what a worker last *did*, what a submission said, or
//! why a ticket escalated: those live only in the event log. [`ActivityIndex`] folds the handful
//! of event kinds that carry them into per-ticket facts, incrementally (it remembers the last
//! `seq` it read, so a refresh every two seconds reads only what is new).
//!
//! Everything here is plain data in, plain data out; [`overviews`] takes the clock reading as an
//! argument, and nothing reads the wall clock or the network.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;
use tm_core::ticket::{Ticket, TicketState};
use tm_types::{TicketId, Timestamp};

/// Which group of the tickets view a ticket sits in, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketGroup {
    /// Waiting on a human: escalated, or a live worker is waiting for an approval.
    NeedsInput,
    /// A worker holds it: leased, running, verifying, auditing.
    Working,
    /// Submitted work waiting for a human to accept or reject.
    Review,
    /// Waiting to be worked: ready, blocked, draft, rework, replan, recovery.
    Queued,
    /// Closed or cancelled.
    Completed,
}

/// How a row's summary reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryTone {
    /// Ordinary text.
    Normal,
    /// A failure: the last attempt failed, or it escalated.
    Failure,
    /// Stopped (cancelled).
    Stopped,
    /// Finished well.
    Success,
}

/// One recorded failure, for the peek panel and `--json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailureView {
    /// Which attempt failed.
    pub attempt: u32,
    /// The failure class (`Other`, `VerificationFailed`, ...).
    pub class: String,
    /// What went wrong (sanitized, one line).
    pub detail: String,
    /// When it was recorded.
    pub at: Timestamp,
}

/// One ticket as the tickets view shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TicketOverview {
    /// The ticket id.
    pub id: TicketId,
    /// A short title: the first clause of the objective.
    pub title: String,
    /// The full objective (sanitized).
    pub objective: String,
    /// The raw state, lowercase (`running`).
    pub state: String,
    /// Which group it is listed under.
    pub group: TicketGroup,
    /// The one-line summary.
    pub summary: String,
    /// How the summary reads.
    pub tone: SummaryTone,
    /// What a Needs-input ticket is waiting for (`escalation`, `approval`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    /// Since when it has been waiting on a human.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_since: Option<Timestamp>,
    /// The live lease holder, if a worker holds it right now.
    pub worker: Option<String>,
    /// Whether that worker is actively working it (a working state, not just holding it).
    pub working: bool,
    /// Lease attempts so far.
    pub attempts: u32,
    /// The retry policy's attempt limit.
    pub max_attempts: u32,
    /// The latest thing a worker did (`$ cargo test`), if the log records one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_activity: Option<String>,
    /// Failure history, oldest first.
    pub failures: Vec<FailureView>,
    /// The latest submission's summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submission: Option<String>,
    /// Evidence attached to the ticket (`A-3 review: submission evidence`).
    pub evidence: Vec<String>,
    /// When the ticket was created.
    pub created: Timestamp,
    /// When it last changed.
    pub updated: Timestamp,
    /// The row's age: since creation, frozen at the run's length once completed.
    pub age_millis: i64,
}

/// What the event log says about one ticket, beyond its projection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TicketActivity {
    /// The last command a worker ran for it, and when.
    pub last_command: Option<(String, Timestamp)>,
    /// The latest goal step a worker added.
    pub last_step: Option<(String, Timestamp)>,
    /// The latest submission summary.
    pub submission: Option<String>,
    /// The latest escalation reason, and when.
    pub escalation: Option<(String, Timestamp)>,
    /// Why it was cancelled, if a reason was given.
    pub cancel_reason: Option<String>,
    /// An approval request no decision has answered yet, and when it was asked.
    pub pending_approval: Option<(String, Timestamp)>,
    /// When the latest scheduled retry may start.
    pub retry_at: Option<Timestamp>,
}

/// The event kinds [`ActivityIndex`] reads; everything else is irrelevant to the view.
const ACTIVITY_KINDS: &[&str] = &[
    "command.started",
    "goal.step_added",
    "ticket.submitted",
    "ticket.escalated",
    "ticket.cancelled",
    "ticket.retry_scheduled",
    "ticket.leased",
    "approval.requested",
    "approval.decided",
];

/// Per-ticket [`TicketActivity`], folded incrementally from the event log.
#[derive(Debug, Default, Clone)]
pub struct ActivityIndex {
    last_seq: i64,
    tickets: HashMap<String, TicketActivity>,
}

impl ActivityIndex {
    /// An empty index that has read nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The facts recorded for `ticket` (empty if none).
    pub fn get(&self, ticket: &TicketId) -> TicketActivity {
        self.tickets
            .get(ticket.as_str())
            .cloned()
            .unwrap_or_default()
    }

    /// Read every relevant event appended since the last refresh from `state_dir`'s log. A read
    /// failure leaves the index as it was (the view shows slightly stale activity rather than
    /// none) and is reported to the caller.
    pub fn refresh(&mut self, state_dir: &Path) -> tm_types::Result<()> {
        let path = state_dir.join("project.db");
        if !path.exists() {
            return Ok(());
        }
        let conn = tm_events::schema::open_read_connection(&path)?;
        // Read the head first and only up to it: an event appended while this runs (this
        // process's own scheduler writes concurrently) is picked up next time, never skipped.
        let head: i64 = conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))
            .map_err(|e| tm_types::TmError::storage(e.to_string()))?;
        if head <= self.last_seq {
            return Ok(());
        }
        let placeholders = vec!["?"; ACTIVITY_KINDS.len()].join(", ");
        let sql = format!(
            "SELECT seq, kind, subject, ts, payload FROM events \
             WHERE seq > ? AND seq <= ? AND kind IN ({placeholders}) ORDER BY seq"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| tm_types::TmError::storage(e.to_string()))?;
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&self.last_seq, &head];
        for kind in ACTIVITY_KINDS {
            params.push(kind);
        }
        let rows = stmt
            .query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(|e| tm_types::TmError::storage(e.to_string()))?;
        for row in rows {
            let (_seq, kind, subject, ts, payload) =
                row.map_err(|e| tm_types::TmError::storage(e.to_string()))?;
            let ts = Timestamp::parse_rfc3339(&ts).unwrap_or(Timestamp::from_unix_seconds(0));
            let payload: serde_json::Value =
                serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
            self.apply(&kind, &subject, ts, &payload);
        }
        self.last_seq = head;
        Ok(())
    }

    /// Fold one event in. `subject` is the event's subject; a payload's own `ticket` field wins
    /// when present (it is the same id for every kind read here, but the payload is explicit).
    pub fn apply(&mut self, kind: &str, subject: &str, ts: Timestamp, payload: &serde_json::Value) {
        let text = |field: &str| {
            payload
                .get(field)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let ticket = text("ticket").unwrap_or_else(|| subject.to_string());
        if TicketId::new(&ticket).is_err() {
            return;
        }
        let entry = self.tickets.entry(ticket).or_default();
        match kind {
            "command.started" => {
                if let Some(command) = text("command") {
                    entry.last_command = Some((command, ts));
                }
            }
            "goal.step_added" => {
                if let Some(step) = text("text") {
                    entry.last_step = Some((step, ts));
                }
            }
            "ticket.submitted" => entry.submission = text("summary"),
            "ticket.escalated" => {
                entry.escalation = Some((text("reason").unwrap_or_default(), ts));
            }
            "ticket.cancelled" => entry.cancel_reason = text("reason"),
            "ticket.retry_scheduled" => {
                entry.retry_at = text("not_before").and_then(|t| Timestamp::parse_rfc3339(&t).ok());
            }
            "approval.requested" => {
                entry.pending_approval = Some((text("note").unwrap_or_default(), ts));
            }
            "approval.decided" => entry.pending_approval = None,
            // A new lease is a new attempt: an approval the previous one left unanswered (its
            // worker died waiting) is not what this one is waiting on.
            "ticket.leased" => entry.pending_approval = None,
            _ => {}
        }
    }
}

/// Make event-derived text safe for one row: control/escape sequences stripped, every run of
/// whitespace (newlines included) collapsed to one space.
pub fn one_line(text: &str) -> String {
    tm_tui::chat::sanitize::sanitize(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The group a ticket in `state` belongs to. `awaiting_approval` is only honoured while a worker
/// is actually mid-run (leased or running): an approval request whose worker died must not pin a
/// ticket under Needs input forever.
pub fn group_for(state: TicketState, awaiting_approval: bool) -> TicketGroup {
    use TicketState as S;
    match state {
        S::Escalated => TicketGroup::NeedsInput,
        S::Leased | S::Running if awaiting_approval => TicketGroup::NeedsInput,
        S::Leased | S::Running | S::Verifying | S::Auditing => TicketGroup::Working,
        S::Submitted => TicketGroup::Review,
        S::Ready | S::Blocked | S::Draft | S::Rework | S::Replan | S::Recovery => {
            TicketGroup::Queued
        }
        S::Closed | S::Cancelled => TicketGroup::Completed,
    }
}

/// The inputs to a row's summary beyond the ticket itself.
#[derive(Debug, Clone, Copy)]
pub struct SummaryContext<'a> {
    /// What the log says about the ticket.
    pub activity: &'a TicketActivity,
    /// The live lease holder, if any.
    pub worker: Option<&'a str>,
    /// Whether anything will pick queued work up: a worker is running in this process, or some
    /// live lease shows one is running elsewhere.
    pub workers_available: bool,
    /// Dependencies that are not closed yet.
    pub open_dependencies: &'a [TicketId],
    /// The clock reading the summary is for (retry countdowns).
    pub now: Timestamp,
}

/// The one-line summary for `ticket` and how it reads: latest activity for working tickets, the
/// submission for review, the failure for failed or retried ones, the reason for needs-input.
pub fn summary_for(ticket: &Ticket, ctx: SummaryContext<'_>) -> (String, SummaryTone) {
    use TicketState as S;
    let act = ctx.activity;
    let last_failure = ticket.failures.last().map(|f| one_line(&f.detail));
    let latest = latest_activity(act);
    let normal = |s: String| (s, SummaryTone::Normal);
    match ticket.state {
        S::Escalated => {
            let reason = act
                .escalation
                .as_ref()
                .map(|(r, _)| one_line(r))
                .filter(|r| !r.is_empty())
                .or_else(|| last_failure.clone());
            let text = match reason {
                // Escalated because its retries ran out: say so, since the reason alone reads
                // like one more failure rather than a question for the human.
                Some(r) if last_failure.as_deref() == Some(r.as_str()) => format!(
                    "gave up after {} attempt{}: {r}",
                    ticket.attempts,
                    if ticket.attempts == 1 { "" } else { "s" }
                ),
                Some(r) => r,
                None => "escalated: needs a decision from you".to_string(),
            };
            (text, SummaryTone::Normal)
        }
        S::Leased | S::Running if act.pending_approval.is_some() => {
            let note = act
                .pending_approval
                .as_ref()
                .map(|(n, _)| one_line(n))
                .unwrap_or_default();
            (
                if note.is_empty() {
                    "approval needed".to_string()
                } else {
                    format!("approval needed: {note}")
                },
                SummaryTone::Normal,
            )
        }
        S::Leased | S::Running if ctx.worker.is_none() => normal(
            "No worker attached — its lease lapsed. Run `tm sched run` to pick it back up."
                .to_string(),
        ),
        S::Leased => normal(format!("starting attempt {}", ticket.attempts.max(1))),
        S::Running => normal(
            latest.unwrap_or_else(|| format!("working on attempt {}", ticket.attempts.max(1))),
        ),
        S::Verifying => normal("verifying the submission".to_string()),
        S::Auditing => normal("auditing the verification".to_string()),
        S::Submitted => normal(
            act.submission
                .as_deref()
                .map(one_line)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "submitted for review".to_string()),
        ),
        S::Rework | S::Replan | S::Recovery => (
            match &last_failure {
                Some(detail) => format!("attempt {} failed: {detail}", ticket.attempts),
                None => state_word(ticket.state),
            },
            SummaryTone::Failure,
        ),
        S::Ready if !ticket.failures.is_empty() => {
            let wait = act
                .retry_at
                .map(|at| at.millis_since(ctx.now))
                .filter(|ms| *ms > 0);
            let retry = match wait {
                Some(ms) => format!("retrying in {}s", (ms + 999) / 1000),
                None => "retrying".to_string(),
            };
            (
                format!(
                    "attempt {} failed: {} · {retry}",
                    ticket.attempts,
                    last_failure.unwrap_or_default()
                ),
                SummaryTone::Failure,
            )
        }
        S::Ready if ctx.workers_available => normal("queued · waiting for a worker".to_string()),
        S::Ready => {
            normal("Queued, but no worker is running. Run `tm sched run` to start one.".to_string())
        }
        S::Blocked => normal(if ctx.open_dependencies.is_empty() {
            "blocked".to_string()
        } else {
            let deps: Vec<String> = ctx
                .open_dependencies
                .iter()
                .map(|d| d.to_string())
                .collect();
            format!("blocked on {}", deps.join(", "))
        }),
        S::Draft => normal("draft, not queued yet".to_string()),
        S::Closed => (
            act.submission
                .as_deref()
                .map(one_line)
                .filter(|s| !s.is_empty())
                .map(|s| format!("result: {s}"))
                .unwrap_or_else(|| "closed".to_string()),
            SummaryTone::Success,
        ),
        S::Cancelled => (
            act.cancel_reason
                .as_deref()
                .map(one_line)
                .filter(|s| !s.is_empty())
                .map(|r| format!("stopped: {r}"))
                .unwrap_or_else(|| "stopped".to_string()),
            SummaryTone::Stopped,
        ),
    }
}

/// The latest thing a worker did, newest of its last command and its last goal step.
fn latest_activity(act: &TicketActivity) -> Option<String> {
    let command = act
        .last_command
        .as_ref()
        .map(|(c, at)| (format!("$ {}", one_line(c)), *at));
    let step = act.last_step.as_ref().map(|(s, at)| (one_line(s), *at));
    match (command, step) {
        (Some(c), Some(s)) => Some(if s.1 > c.1 { s.0 } else { c.0 }),
        (Some(c), None) => Some(c.0),
        (None, Some(s)) => Some(s.0),
        (None, None) => None,
    }
}

/// `running`, `awaiting approval` — the state as a lowercase word.
pub fn state_word(state: TicketState) -> String {
    format!("{state:?}").to_ascii_lowercase()
}

/// Every ticket in `view` as the tickets view shows it, grouped order not applied (the screen
/// sorts). `include_completed` keeps closed and cancelled tickets. `local_worker` says a
/// scheduler is running in this very process (so queued work will be picked up even with no
/// lease held yet).
pub fn overviews(
    view: &tm_core::ProjectView,
    index: &ActivityIndex,
    now: Timestamp,
    local_worker: bool,
    include_completed: bool,
) -> Vec<TicketOverview> {
    let live_holder = |ticket: &TicketId| {
        view.leases
            .values()
            .find(|l| &l.ticket == ticket && !l.is_expired(now))
            .map(|l| l.holder.to_string())
    };
    let workers_available = local_worker || view.leases.values().any(|l| !l.is_expired(now));
    view.tickets
        .values()
        .filter(|t| {
            include_completed || !matches!(t.state, TicketState::Closed | TicketState::Cancelled)
        })
        .map(|t| {
            let activity = index.get(&t.id);
            let worker = live_holder(&t.id);
            let open_dependencies: Vec<TicketId> = t
                .dependencies
                .iter()
                .filter(|d| {
                    view.tickets
                        .get(d)
                        .is_some_and(|dep| dep.state != TicketState::Closed)
                })
                .cloned()
                .collect();
            let awaiting_approval = activity.pending_approval.is_some()
                && matches!(t.state, TicketState::Leased | TicketState::Running);
            let group = group_for(t.state, awaiting_approval);
            let (summary, tone) = summary_for(
                t,
                SummaryContext {
                    activity: &activity,
                    worker: worker.as_deref(),
                    workers_available,
                    open_dependencies: &open_dependencies,
                    now,
                },
            );
            let (waiting_for, waiting_since) = match group {
                TicketGroup::NeedsInput if awaiting_approval => (
                    Some("approval".to_string()),
                    activity.pending_approval.as_ref().map(|(_, at)| *at),
                ),
                TicketGroup::NeedsInput => (
                    Some("escalation".to_string()),
                    activity
                        .escalation
                        .as_ref()
                        .map(|(_, at)| *at)
                        .or(Some(t.updated)),
                ),
                _ => (None, None),
            };
            let working = worker.is_some()
                && matches!(
                    t.state,
                    TicketState::Leased
                        | TicketState::Running
                        | TicketState::Verifying
                        | TicketState::Auditing
                )
                && !awaiting_approval;
            let objective = tm_tui::chat::sanitize::sanitize(&t.objective)
                .trim()
                .to_string();
            let age_millis = if group == TicketGroup::Completed {
                t.updated.millis_since(t.created)
            } else {
                now.millis_since(t.created)
            }
            .max(0);
            TicketOverview {
                id: t.id.clone(),
                title: tm_tui::screens::tickets::short_title(&one_line(&t.objective)),
                objective,
                state: state_word(t.state),
                group,
                summary,
                tone,
                waiting_for,
                waiting_since,
                worker,
                working,
                attempts: t.attempts,
                max_attempts: t.retry.max_attempts,
                latest_activity: latest_activity(&activity),
                failures: t
                    .failures
                    .iter()
                    .map(|f| FailureView {
                        attempt: f.attempt,
                        class: format!("{:?}", f.class),
                        detail: one_line(&f.detail),
                        at: f.at,
                    })
                    .collect(),
                submission: activity.submission.as_deref().map(one_line),
                evidence: view
                    .evidence
                    .iter()
                    .filter(|e| e.ticket == t.id)
                    .map(|e| {
                        format!(
                            "{} {}: {}",
                            e.artifact,
                            format!("{:?}", e.kind).to_ascii_lowercase(),
                            one_line(&e.summary)
                        )
                    })
                    .collect(),
                created: t.created,
                updated: t.updated,
                age_millis,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::ticket::{FailureClass, FailureRecord};

    fn ticket(state: TicketState) -> Ticket {
        let at = Timestamp::from_unix_seconds(1_000);
        Ticket {
            id: TicketId::new("T-4").unwrap(),
            objective: "Fix the flaky checkout test, it times out on CI".to_string(),
            kind: tm_core::ticket::TicketKind::Work,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            state,
            priority: 0,
            authority: tm_types::Authority::default(),
            resources: vec![],
            executor: crate::tickets::default_executor_requirements(),
            context_refs: vec![],
            success: vec![],
            verification: tm_core::ticket::VerificationPolicy::Single,
            budget: tm_types::Budget::unlimited(),
            retry: crate::tickets::default_retry_policy(),
            attempts: 0,
            failures: vec![],
            created: at,
            updated: at,
            cycle: None,
        }
    }

    fn summary(t: &Ticket, act: &TicketActivity, worker: Option<&str>, avail: bool) -> String {
        summary_for(
            t,
            SummaryContext {
                activity: act,
                worker,
                workers_available: avail,
                open_dependencies: &[],
                now: Timestamp::from_unix_seconds(0),
            },
        )
        .0
    }

    #[test]
    fn every_state_has_a_group_in_the_documented_order() {
        use TicketState as S;
        assert_eq!(group_for(S::Escalated, false), TicketGroup::NeedsInput);
        assert_eq!(group_for(S::Running, true), TicketGroup::NeedsInput);
        assert_eq!(group_for(S::Leased, true), TicketGroup::NeedsInput);
        for s in [S::Leased, S::Running, S::Verifying, S::Auditing] {
            assert_eq!(group_for(s, false), TicketGroup::Working, "{s:?}");
        }
        // A stale approval on a ticket no worker is running does not pin it under Needs input.
        assert_eq!(group_for(S::Ready, true), TicketGroup::Queued);
        assert_eq!(group_for(S::Submitted, false), TicketGroup::Review);
        for s in [
            S::Ready,
            S::Blocked,
            S::Draft,
            S::Rework,
            S::Replan,
            S::Recovery,
        ] {
            assert_eq!(group_for(s, false), TicketGroup::Queued, "{s:?}");
        }
        assert_eq!(group_for(S::Closed, false), TicketGroup::Completed);
        assert_eq!(group_for(S::Cancelled, false), TicketGroup::Completed);
        for s in TicketState::ALL {
            let _ = group_for(*s, false);
        }
        assert!(TicketGroup::NeedsInput < TicketGroup::Working);
        assert!(TicketGroup::Working < TicketGroup::Review);
        assert!(TicketGroup::Review < TicketGroup::Queued);
        assert!(TicketGroup::Queued < TicketGroup::Completed);
    }

    #[test]
    fn working_rows_show_the_latest_command_or_step() {
        let t = Ticket {
            attempts: 1,
            ..ticket(TicketState::Running)
        };
        let mut act = TicketActivity::default();
        assert_eq!(
            summary(&t, &act, Some("agent:w"), true),
            "working on attempt 1"
        );
        act.last_command = Some((
            "cargo test\n-p x".to_string(),
            Timestamp::from_unix_seconds(5),
        ));
        assert_eq!(
            summary(&t, &act, Some("agent:w"), true),
            "$ cargo test -p x"
        );
        act.last_step = Some((
            "Fix the timeout".to_string(),
            Timestamp::from_unix_seconds(9),
        ));
        assert_eq!(summary(&t, &act, Some("agent:w"), true), "Fix the timeout");
        // Without a live lease, say so rather than pretending it is being worked.
        assert!(summary(&t, &act, None, true).starts_with("No worker attached"));
    }

    #[test]
    fn queued_rows_are_honest_about_whether_a_worker_exists() {
        let t = ticket(TicketState::Ready);
        let act = TicketActivity::default();
        assert_eq!(
            summary(&t, &act, None, false),
            "Queued, but no worker is running. Run `tm sched run` to start one."
        );
        assert_eq!(
            summary(&t, &act, None, true),
            "queued · waiting for a worker"
        );
    }

    #[test]
    fn failures_escalations_submissions_and_approvals_are_summarized() {
        let failure = FailureRecord {
            class: FailureClass::Other,
            detail: "model ended turn\x1b[31m without submitting".to_string(),
            at: Timestamp::from_unix_seconds(2_000),
            attempt: 1,
        };
        let retried = Ticket {
            attempts: 1,
            failures: vec![failure.clone()],
            ..ticket(TicketState::Ready)
        };
        let act = TicketActivity::default();
        let (text, tone) = summary_for(
            &retried,
            SummaryContext {
                activity: &act,
                worker: None,
                workers_available: false,
                open_dependencies: &[],
                now: Timestamp::from_unix_seconds(0),
            },
        );
        assert_eq!(
            text,
            "attempt 1 failed: model ended turn without submitting · retrying"
        );
        assert_eq!(tone, SummaryTone::Failure);

        let mut act = TicketActivity {
            escalation: Some((
                "needs a decision:\nretry or give up?".to_string(),
                Timestamp::from_unix_seconds(3),
            )),
            ..TicketActivity::default()
        };
        assert_eq!(
            summary(&ticket(TicketState::Escalated), &act, None, false),
            "needs a decision: retry or give up?"
        );
        let gave_up = Ticket {
            attempts: 3,
            failures: vec![failure.clone()],
            ..ticket(TicketState::Escalated)
        };
        let same = TicketActivity {
            escalation: Some((failure.detail.clone(), Timestamp::from_unix_seconds(3))),
            ..TicketActivity::default()
        };
        assert_eq!(
            summary(&gave_up, &same, None, false),
            "gave up after 3 attempts: model ended turn without submitting"
        );

        act.submission = Some("All tests pass".to_string());
        assert_eq!(
            summary(&ticket(TicketState::Submitted), &act, None, false),
            "All tests pass"
        );
        assert_eq!(
            summary(&ticket(TicketState::Closed), &act, None, false),
            "result: All tests pass"
        );

        act.pending_approval = Some((
            "shell.run rm -rf build".to_string(),
            Timestamp::from_unix_seconds(4),
        ));
        assert_eq!(
            summary(&ticket(TicketState::Running), &act, Some("agent:w"), true),
            "approval needed: shell.run rm -rf build"
        );
        act.cancel_reason = Some("not needed".to_string());
        assert_eq!(
            summary(&ticket(TicketState::Cancelled), &act, None, false),
            "stopped: not needed"
        );
    }

    #[test]
    fn the_index_folds_only_the_kinds_it_needs_and_clears_answered_approvals() {
        let mut index = ActivityIndex::new();
        let ts = Timestamp::from_unix_seconds(10);
        index.apply(
            "command.started",
            "T-2",
            ts,
            &serde_json::json!({"command": "cargo test", "ticket": "T-2", "session": null}),
        );
        index.apply(
            "approval.requested",
            "T-2",
            ts,
            &serde_json::json!({"ticket": "T-2", "requested_of": "agent:w", "note": "rm"}),
        );
        let t2 = TicketId::new("T-2").unwrap();
        assert_eq!(
            index.get(&t2).last_command,
            Some(("cargo test".to_string(), ts))
        );
        assert!(index.get(&t2).pending_approval.is_some());
        index.apply(
            "approval.decided",
            "T-2",
            ts,
            &serde_json::json!({"ticket": "T-2", "decided_by": "human:x", "approved": true, "note": null}),
        );
        assert!(index.get(&t2).pending_approval.is_none());
        index.apply(
            "approval.requested",
            "T-2",
            ts,
            &serde_json::json!({"ticket": "T-2", "requested_of": "agent:w", "note": "rm"}),
        );
        index.apply(
            "ticket.leased",
            "T-2",
            ts,
            &serde_json::json!({"ticket": "T-2", "lease": "L-1", "holder": "agent:w"}),
        );
        assert!(
            index.get(&t2).pending_approval.is_none(),
            "a fresh lease clears a dead attempt's unanswered approval"
        );
        // A command outside any ticket is not attributed to one.
        index.apply(
            "command.started",
            "",
            ts,
            &serde_json::json!({"command": "ls", "ticket": null}),
        );
        assert_eq!(index.tickets.len(), 1);
    }

    #[test]
    fn overviews_hide_completed_unless_asked_and_freeze_completed_ages() {
        let mut view = tm_core::ProjectView::empty();
        let open = Ticket {
            id: TicketId::new("T-1").unwrap(),
            ..ticket(TicketState::Ready)
        };
        let done = Ticket {
            id: TicketId::new("T-2").unwrap(),
            updated: Timestamp::from_unix_seconds(1_060),
            ..ticket(TicketState::Closed)
        };
        view.tickets.insert(open.id.clone(), open);
        view.tickets.insert(done.id.clone(), done);
        let now = Timestamp::from_unix_seconds(5_000);
        let index = ActivityIndex::new();
        let some = overviews(&view, &index, now, false, false);
        assert_eq!(some.len(), 1);
        assert_eq!(some[0].title, "Fix the flaky checkout test");
        let all = overviews(&view, &index, now, false, true);
        assert_eq!(all.len(), 2);
        let closed = all.iter().find(|o| o.id.as_str() == "T-2").unwrap();
        assert_eq!(closed.age_millis, 60_000);
        assert_eq!(closed.group, TicketGroup::Completed);
    }
}
