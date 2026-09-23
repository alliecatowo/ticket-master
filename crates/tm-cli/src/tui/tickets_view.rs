//! Builds the tickets screen's plain data ([`tm_tui::screens::tickets::TicketsData`]) from the
//! project's real state, via the same [`crate::tickets::overview`] `tm tickets --json` prints, so
//! the screen and the JSON cannot disagree about a ticket.

use tm_tui::screens::tickets::{
    compact_age, Group, PeekItem, TicketRow, TicketsData, Tint, Worker,
};
use tm_types::Timestamp;

use crate::tickets::overview::{self, ActivityIndex, SummaryTone, TicketGroup, TicketOverview};

/// Everything the tickets screen shows. `local_worker` says the in-process scheduler is running;
/// `notice` is shown in the header (e.g. why it could not start).
pub(super) fn build(
    view: &tm_core::ProjectView,
    index: &ActivityIndex,
    now: Timestamp,
    local_worker: bool,
    header: (String, String),
    notice: Option<String>,
) -> TicketsData {
    let (model, place) = header;
    TicketsData {
        version: env!("CARGO_PKG_VERSION").to_string(),
        model,
        place,
        notice,
        rows: overview::overviews(view, index, now, local_worker, true)
            .iter()
            .map(|o| row(o, now))
            .collect(),
    }
}

fn group(g: TicketGroup) -> Group {
    match g {
        TicketGroup::NeedsInput => Group::NeedsInput,
        TicketGroup::Working => Group::Working,
        TicketGroup::Review => Group::Review,
        TicketGroup::Queued => Group::Queued,
        TicketGroup::Completed => Group::Completed,
    }
}

fn tint(t: SummaryTone) -> Tint {
    match t {
        SummaryTone::Normal => Tint::Normal,
        SummaryTone::Failure => Tint::Failure,
        SummaryTone::Stopped => Tint::Stopped,
        SummaryTone::Success => Tint::Success,
    }
}

/// One overview as a screen row, with its peek panel.
fn row(o: &TicketOverview, now: Timestamp) -> TicketRow {
    let worker = match (&o.worker, o.working) {
        (Some(_), true) => Worker::Working,
        (Some(_), false) => Worker::Attached,
        (None, _) => Worker::None,
    };
    // Newest first in the live groups; most recently finished first in Completed.
    let order = match o.group {
        TicketGroup::Completed => (o.updated.unix_nanos() / 1_000_000) as i64,
        _ => o.id.number().unwrap_or(0) as i64,
    };
    TicketRow {
        id: o.id.to_string(),
        title: o.title.clone(),
        group: group(o.group),
        worker,
        summary: o.summary.clone(),
        tint: tint(o.tone),
        age: compact_age(o.age_millis),
        order,
        peek: peek(o, now),
    }
}

/// The peek panel: the row's summary first (the sentence the row may truncate), then the
/// objective, state and attempts, how long it has waited, the latest activity, failures, the
/// submission, and its evidence.
fn peek(o: &TicketOverview, now: Timestamp) -> Vec<PeekItem> {
    let mut items = Vec::new();
    let now_label = match o.group {
        TicketGroup::NeedsInput => "needs",
        TicketGroup::Working => "status",
        TicketGroup::Review => "result",
        TicketGroup::Queued => "status",
        TicketGroup::Completed => "result",
    };
    items.push(PeekItem::new(now_label, o.summary.clone()).tinted(tint(o.tone)));
    items.push(PeekItem::new("objective", overview::one_line(&o.objective)));
    let mut state = format!(
        "{} · attempt {} of {}",
        o.state,
        o.attempts,
        o.max_attempts.max(o.attempts)
    );
    if let Some(worker) = &o.worker {
        state.push_str(&format!(" · worker {worker}"));
    }
    items.push(PeekItem::new("state", state));
    if let Some(since) = o.waiting_since {
        let waited = compact_age(now.millis_since(since));
        items.push(PeekItem::new(
            "waiting",
            match o.waiting_for.as_deref() {
                Some("approval") => format!("{waited} for your approval"),
                _ => format!("{waited} since it escalated"),
            },
        ));
    }
    if let Some(latest) = &o.latest_activity {
        items.push(PeekItem::new("latest", latest.clone()));
    }
    for failure in o.failures.iter().rev().take(5) {
        items.push(
            PeekItem::new(
                format!("attempt {}", failure.attempt),
                format!("{}: {}", failure.class, failure.detail),
            )
            .tinted(Tint::Failure),
        );
    }
    if let Some(submission) = &o.submission {
        items.push(PeekItem::new("submitted", submission.clone()));
    }
    for evidence in &o.evidence {
        items.push(PeekItem::new("evidence", evidence.clone()));
    }
    items
}

/// The one-line recap posted in the chat when attaching from the tickets screen ("what happened
/// while you were away").
pub(super) fn recap(o: &TicketOverview) -> String {
    let mut parts = vec![format!(
        "{} · attempt {} of {}",
        o.state,
        o.attempts,
        o.max_attempts.max(o.attempts)
    )];
    parts.push(o.summary.clone());
    if let Some(latest) = o.latest_activity.as_ref().filter(|l| **l != o.summary) {
        parts.push(format!("latest {latest}"));
    }
    if let Some(failure) = o.failures.last() {
        if !o.summary.contains(&failure.detail) {
            parts.push(format!("last failure: {}", failure.detail));
        }
    }
    format!("Recap of {}: {}", o.id, parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::TicketId;

    fn overview() -> TicketOverview {
        TicketOverview {
            id: TicketId::new("T-3").unwrap(),
            title: "Fix the flaky test".into(),
            objective: "Fix the flaky test, it times out".into(),
            state: "ready".into(),
            group: TicketGroup::Queued,
            summary: "attempt 1 failed: timeout · retrying".into(),
            tone: SummaryTone::Failure,
            waiting_for: None,
            waiting_since: None,
            worker: None,
            working: false,
            attempts: 1,
            max_attempts: 3,
            latest_activity: Some("$ cargo test".into()),
            failures: vec![overview::FailureView {
                attempt: 1,
                class: "Other".into(),
                detail: "timeout".into(),
                at: Timestamp::from_unix_seconds(5),
            }],
            submission: None,
            evidence: vec![],
            created: Timestamp::from_unix_seconds(0),
            updated: Timestamp::from_unix_seconds(5),
            age_millis: 120_000,
        }
    }

    #[test]
    fn a_row_carries_group_glyph_age_and_a_peek_with_the_failure_history() {
        let r = row(&overview(), Timestamp::from_unix_seconds(10));
        assert_eq!(r.group, Group::Queued);
        assert_eq!(r.worker, Worker::None);
        assert_eq!(r.tint, Tint::Failure);
        assert_eq!(r.age, "2m");
        assert!(r
            .peek
            .iter()
            .any(|p| p.label == "attempt 1" && p.text == "Other: timeout"));
        assert!(r
            .peek
            .iter()
            .any(|p| p.label == "state" && p.text == "ready · attempt 1 of 3"));
    }

    #[test]
    fn the_recap_is_one_line_with_state_summary_and_activity() {
        let text = recap(&overview());
        assert_eq!(
            text,
            "Recap of T-3: ready · attempt 1 of 3 · attempt 1 failed: timeout · retrying · \
             latest $ cargo test"
        );
        assert!(!text.contains('\n'));
    }
}
