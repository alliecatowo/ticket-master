//! Watches a project's event log for newly appended events and fires a [`Notifier`] for every
//! one [`should_notify`] selects.
//!
//! # Why a second, polled [`EventLog`] instead of hooking `tm_core::Store` directly
//! This mirrors `tm-server`'s own `AppState::spawn_broadcast_poller`
//! (`crates/tm-server/src/state.rs`) exactly, for the same reason stated there: `tm_core::Store`
//! does not expose its internal `EventLog` or a subscribe hook, and [`tm_events::stream::EventHub`]
//! only fans events out to subscribers of the *same* `EventLog` instance that appended them — a
//! second, freshly opened `EventLog` on the same `project.db` never sees those publishes. Two
//! `EventLog` handles on the same WAL-mode SQLite file do see each other's committed writes fine
//! for reads (`read_from`/`head`), though, so polling is the pattern the rest of this codebase
//! already reaches for here rather than adding a new observation mechanism to `tm-core`. This is
//! also, independently, the architecturally right seam for notifications specifically: both
//! event kinds this crate cares about can be appended from a background tokio task spawned deep
//! inside `tm-scheduler`'s dispatcher (`crates/tm-scheduler/src/dispatch.rs`'s `run_and_report`,
//! for `approval.requested` via `tm-agent`'s loop) rather than synchronously from whatever call
//! site started a scheduler tick, so there is no single return value anywhere in-process that is
//! guaranteed to carry every notification-worthy event — polling the log itself is the one place
//! that sees all of them, in the same process a human is watching, without requiring a second
//! `tm` invocation to observe a different one's writes.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tm_events::EventLog;

use crate::decision::{format_notification, should_notify};
use crate::notifier::Notifier;

/// How often [`spawn_notification_watcher`] polls the log for newly appended events. Matches
/// `tm-server`'s `ServerConfig::broadcast_poll_interval` default order of magnitude: frequent
/// enough that a human sees an approval/escalation notification promptly, infrequent enough not
/// to matter for a SQLite read connection opened on each tick.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How many events [`spawn_notification_watcher`] reads per poll. A `tm sched run` tick applies
/// at most a handful of actions; this just needs to be comfortably larger than that so one poll
/// never leaves a backlog for the next.
const POLL_PAGE_SIZE: usize = 256;

/// Spawn a background task that polls `state_dir`'s `project.db` for events appended after the
/// log's current head, and calls `notifier.notify` for every one [`should_notify`] selects.
/// Starts from the log's head *at call time* — never replays history from before the watcher
/// existed, matching a human's actual expectation ("tell me about what happens next", not
/// "replay everything that already happened").
///
/// Returns the [`tokio::task::JoinHandle`]; as with `tm-server`'s identical poller, dropping the
/// handle does not stop the task — callers that need to stop it should abort the handle
/// explicitly. Runs until the process exits or the handle is aborted; a transient read failure is
/// logged and retried on the next tick rather than ending the task, since the process this runs
/// inside (`tm sched run`) is meant to keep going through exactly that kind of hiccup.
///
/// # Errors
/// Only for failure to open `state_dir/project.db` at all, or to read its current head — both
/// checked once, up front, so a caller (e.g. `tm sched run`) can decide whether that is fatal to
/// starting at all, rather than discovering it silently several polls in.
pub fn spawn_notification_watcher(
    state_dir: &Path,
    notifier: Arc<dyn Notifier>,
    poll_interval: Duration,
) -> tm_types::Result<tokio::task::JoinHandle<()>> {
    let db_path = state_dir.join("project.db");
    let log = EventLog::open(&db_path)?;
    let mut last_seq = log.head()?;

    Ok(tokio::spawn(async move {
        loop {
            // Read first, then sleep — matching `tm-server`'s `AppState::spawn_broadcast_poller`
            // exactly (`crates/tm-server/src/state.rs`), and for the same reason: it reads
            // anything already appended before the watcher's first sleep rather than waiting out
            // a full `poll_interval` for no reason, which matters most for a short-lived process
            // like `tm run` racing its own exit against this same interval (see this module's
            // docs and `docs/decisions/D-007-desktop-notifications.md`'s "what this costs" for
            // the residual race this narrows but does not eliminate).
            match log.read_from(last_seq + 1, POLL_PAGE_SIZE) {
                Ok(events) => {
                    for event in events {
                        last_seq = event.seq;
                        if !should_notify(event.kind) {
                            continue;
                        }
                        let Some(notification) = format_notification(&event) else {
                            continue;
                        };
                        if let Err(e) = notifier.notify(&notification) {
                            tracing::warn!(
                                seq = event.seq,
                                kind = ?event.kind,
                                error = %e,
                                "desktop notification failed"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "notification watcher failed to read the event log");
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use tempfile::TempDir;
    use tm_events::payload::{ApprovalRequestedPayload, Payload, TicketEscalatedPayload};
    use tm_events::EventDraft;
    use tm_types::{Id, ParticipantId, TicketId};

    use crate::notifier::Notification;

    /// Records every [`Notification`] it receives instead of ever touching a real OS API — the
    /// fake every test in this crate dispatches through, per the crate's own testing rule.
    #[derive(Default)]
    struct RecordingNotifier {
        received: Mutex<Vec<Notification>>,
    }

    impl RecordingNotifier {
        fn snapshot(&self) -> Vec<Notification> {
            self.received.lock().unwrap().clone()
        }
    }

    impl Notifier for RecordingNotifier {
        fn notify(&self, notification: &Notification) -> Result<(), crate::notifier::NotifyError> {
            self.received.lock().unwrap().push(notification.clone());
            Ok(())
        }
    }

    async fn wait_for<F: Fn() -> bool>(condition: F) {
        for _ in 0..100 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not met within the test's wait budget");
    }

    #[tokio::test]
    async fn watcher_notifies_for_approval_requested_appended_after_it_started() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("project.db");
        // Open (and drop) once first so the watcher's own `EventLog::open` finds a real,
        // already-migrated schema rather than racing its own first-open migration.
        drop(EventLog::open(&db_path).expect("create log"));

        let notifier = Arc::new(RecordingNotifier::default());
        let handle =
            spawn_notification_watcher(dir.path(), notifier.clone(), Duration::from_millis(10))
                .expect("spawn watcher");

        let ticket = TicketId::new("T-1").expect("valid ticket id");
        let writer = EventLog::open(&db_path).expect("open log for writing");
        writer
            .append_all(vec![EventDraft::new(
                ParticipantId::system(),
                Id::from(ticket.clone()),
                Payload::from(ApprovalRequestedPayload {
                    ticket: Some(ticket),
                    requested_of: ParticipantId::system(),
                    note: "needs a human decision".to_string(),
                }),
            )])
            .expect("append approval.requested");

        wait_for(|| !notifier.snapshot().is_empty()).await;

        let received = notifier.snapshot();
        assert_eq!(received.len(), 1);
        assert!(received[0].title.contains("T-1"));
        assert_eq!(received[0].body, "needs a human decision");

        handle.abort();
    }

    #[tokio::test]
    async fn watcher_ignores_uninteresting_kinds_and_notifies_for_ticket_escalated() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("project.db");
        drop(EventLog::open(&db_path).expect("create log"));

        let notifier = Arc::new(RecordingNotifier::default());
        let handle =
            spawn_notification_watcher(dir.path(), notifier.clone(), Duration::from_millis(10))
                .expect("spawn watcher");

        let ticket = TicketId::new("T-2").expect("valid ticket id");
        let writer = EventLog::open(&db_path).expect("open log for writing");
        writer
            .append_all(vec![
                EventDraft::new(
                    ParticipantId::system(),
                    Id::from(ticket.clone()),
                    Payload::from(tm_events::payload::TicketCreatedPayload {
                        ticket: ticket.clone(),
                        title: "do the thing".to_string(),
                        parent: None,
                    }),
                ),
                EventDraft::new(
                    ParticipantId::system(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketEscalatedPayload {
                        ticket: ticket.clone(),
                        reason: "retries exhausted".to_string(),
                    }),
                ),
            ])
            .expect("append events");

        wait_for(|| !notifier.snapshot().is_empty()).await;

        let received = notifier.snapshot();
        assert_eq!(
            received.len(),
            1,
            "ticket.created must not notify: {received:?}"
        );
        assert!(received[0].title.contains("T-2"));
        assert_eq!(received[0].body, "retries exhausted");

        handle.abort();
    }

    #[tokio::test]
    async fn watcher_never_replays_history_from_before_it_started() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("project.db");
        let ticket = TicketId::new("T-3").expect("valid ticket id");

        // Append an approval.requested *before* the watcher ever starts.
        let writer = EventLog::open(&db_path).expect("create log");
        writer
            .append_all(vec![EventDraft::new(
                ParticipantId::system(),
                Id::from(ticket.clone()),
                Payload::from(ApprovalRequestedPayload {
                    ticket: Some(ticket),
                    requested_of: ParticipantId::system(),
                    note: "pre-existing, should not be replayed".to_string(),
                }),
            )])
            .expect("append approval.requested");

        let notifier = Arc::new(RecordingNotifier::default());
        let handle =
            spawn_notification_watcher(dir.path(), notifier.clone(), Duration::from_millis(10))
                .expect("spawn watcher");

        // Give the watcher several poll cycles to (wrongly) pick up the pre-existing event if it
        // were going to.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            notifier.snapshot().is_empty(),
            "watcher replayed history from before it started: {:?}",
            notifier.snapshot()
        );

        handle.abort();
    }
}
