//! The [`Tracker`] trait every external adapter implements, the capability declaration that lets
//! [`crate::projection::ProjectionPolicy`] degrade deliberately instead of guessing, the shapes
//! an adapter hands back across the push/pull boundary, and two test doubles ([`NullTracker`],
//! [`RecordingTracker`]) that let the rest of this crate be tested with no network.
//!
//! This module owns the *contract* between the sync engine and an adapter. It owns none of the
//! HTTP/GraphQL shaping — that lives per-adapter in `github.rs`/`linear.rs`/`jira.rs`/`gitlab.rs`.

use std::sync::Mutex;

use async_trait::async_trait;
use tm_types::{Result, Timestamp};

use crate::projection::Projection;

/// What an external system can represent, declared per adapter rather than assumed, so
/// [`crate::projection::ProjectionPolicy`] can degrade deliberately and record the degradation
/// instead of silently dropping graph shape it can't express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackerCapabilities {
    /// True if the external system has native parent/child issues (e.g. Linear sub-issues).
    /// False means descendants must roll up into a checklist in the parent issue's body.
    pub parent_child: bool,
    /// True if the external system accepts any workflow state name. False means internal states
    /// must be mapped onto a small fixed set (e.g. GitHub's `open`/`closed`).
    pub arbitrary_states: bool,
    /// True if the external system has a milestone/iteration concept this adapter maps onto.
    pub milestones: bool,
    /// True if the external system has labels/tags this adapter can set.
    pub labels: bool,
    /// True if the external system has threaded comments this adapter can read and write.
    pub comments: bool,
    /// Maximum body size, in bytes, the external system accepts for a single issue description.
    pub max_body_bytes: usize,
}

/// A pointer to the external object a Ticketmaster ticket was pushed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalRef {
    /// The adapter instance name this reference belongs to (matches the `mirror.toml` table).
    pub adapter: String,
    /// The external system's stable identifier for the object (issue number, GraphQL node id,
    /// Jira key, ...).
    pub external_id: String,
    /// A human-followable URL to the object, when the external system exposes one.
    pub url: Option<String>,
}

/// The semantically meaningful allowlist of inbound change shapes `sync` will translate into
/// events (`SPEC.md` §13). Anything not representable here is dropped at the adapter boundary,
/// never forwarded as a raw external payload.
#[derive(Debug, Clone, PartialEq)]
pub enum ExternalChangeKind {
    /// The external issue's state changed; carries the external system's own state name,
    /// unmapped — turning it into an actual transition is `sync`'s job, since only it has
    /// enough context to know whether Ticketmaster can honor it.
    StatusHint {
        /// The external system's state name (e.g. GitHub `"closed"`, Jira `"In Review"`).
        state: String,
    },
    /// The external issue's assignee changed.
    Assigned {
        /// Display name/handle of the new assignee, or `None` if unassigned.
        assignee: Option<String>,
    },
    /// A comment was added on the external issue.
    CommentAdded {
        /// Author display name/handle on the external system.
        author: String,
        /// Comment body, as authored externally.
        body: String,
    },
    /// The external issue's priority field changed.
    PriorityChanged {
        /// The external system's priority label.
        priority: String,
    },
    /// A human created a new issue on the external system with no corresponding ticket.
    IssueCreated {
        /// Issue title.
        title: String,
        /// Issue body, as authored externally.
        body: String,
        /// Author display name/handle on the external system.
        author: String,
    },
}

/// One inbound change observed on an external tracker, prior to allowlist translation.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalChange {
    /// Which external object changed.
    pub external: ExternalRef,
    /// What changed.
    pub kind: ExternalChangeKind,
    /// When the external system recorded the change (its clock, not ours).
    pub observed_at: Timestamp,
}

/// Implemented by every external adapter (`github`, `linear`, `jira`, `gitlab`) plus the two
/// test doubles in this module. `push`/`pull` are the only I/O surface in this crate; everything
/// else — projection, config, conflict resolution — is pure and adapter-agnostic.
#[async_trait]
pub trait Tracker: Send + Sync {
    /// Adapter instance name, matching the `mirror.toml` table this tracker was built from.
    fn name(&self) -> &str;

    /// What this adapter can represent; consulted by `ProjectionPolicy` before `push`.
    fn capabilities(&self) -> TrackerCapabilities;

    /// Push a projection to the external system, creating or updating the mirrored issue.
    /// Returns the reference the caller records on the ticket's mirror link.
    async fn push(&self, projection: &Projection) -> Result<ExternalRef>;

    /// Pull every change observed on the external system since `since` (the adapter's own
    /// notion of "since", e.g. an updated-after query parameter or GraphQL filter).
    async fn pull(&self, since: Timestamp) -> Result<Vec<ExternalChange>>;

    /// Best-effort recovery probe for `SPEC.md` §21.5 (audit B-11): search the external system
    /// for evidence that `ticket` was already pushed, for the case where a prior `push` may have
    /// completed externally but the local idempotency receipt was lost (a crash between the
    /// external write succeeding and [`tm_core::EffectGuard::complete`] being called — see
    /// `crate::sync`'s callers, which call this only when
    /// [`tm_core::EffectGuard::resumed`] is true, not on every push).
    ///
    /// `Ok(None)` means "no marker found" — a fresh `push` should proceed. This is a false
    /// negative in the rare case where the external write is not yet visible to a search (e.g.
    /// read-after-write lag); the cost of a false negative here is a possible duplicate push in
    /// an already-rare crash window, not silent data loss, so adapters are free to return
    /// `Ok(None)` whenever they have no reliable search surface for their own marker. The default
    /// implementation does exactly that: an adapter with no durable "did I already push this"
    /// signal (or one, like `tm-mirror`'s Jira/GitLab adapters today, whose `push` is not yet a
    /// live external effect at all) has nothing to confirm against, and the documented, accepted
    /// behavior is to re-run rather than guess.
    async fn confirm(&self, _ticket: &tm_types::TicketId) -> Result<Option<ExternalRef>> {
        Ok(None)
    }
}

/// Accepts and discards every push, returns no changes on pull. The default tracker when
/// nothing is configured in `mirror.toml` (`SPEC.md` §13.1).
#[derive(Debug, Clone)]
pub struct NullTracker {
    name: String,
}

impl NullTracker {
    /// Build a null tracker with the given adapter instance name.
    pub fn new(name: impl Into<String>) -> Self {
        NullTracker { name: name.into() }
    }
}

#[async_trait]
impl Tracker for NullTracker {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> TrackerCapabilities {
        TrackerCapabilities {
            parent_child: true,
            arbitrary_states: true,
            milestones: true,
            labels: true,
            comments: true,
            max_body_bytes: usize::MAX,
        }
    }

    async fn push(&self, projection: &Projection) -> Result<ExternalRef> {
        Ok(ExternalRef {
            adapter: self.name.clone(),
            external_id: projection.ticket.to_string(),
            url: None,
        })
    }

    async fn pull(&self, _since: Timestamp) -> Result<Vec<ExternalChange>> {
        Ok(Vec::new())
    }
}

/// Records every `push`/`pull` call for test assertions, and lets tests script the changes the
/// next `pull` returns. Interior mutability so it can be shared behind `&dyn Tracker`. This is
/// the double every adapter's round-trip idempotence test is checked against, so it (and
/// [`NullTracker`]) are the only network-free way to exercise [`crate::sync::SyncEngine`].
pub struct RecordingTracker {
    name: String,
    capabilities: TrackerCapabilities,
    pushed: Mutex<Vec<Projection>>,
    scripted_changes: Mutex<Vec<ExternalChange>>,
    pull_calls: Mutex<Vec<Timestamp>>,
}

impl RecordingTracker {
    /// Build a recording tracker with the given name and declared capabilities.
    pub fn new(name: impl Into<String>, capabilities: TrackerCapabilities) -> Self {
        RecordingTracker {
            name: name.into(),
            capabilities,
            pushed: Mutex::new(Vec::new()),
            scripted_changes: Mutex::new(Vec::new()),
            pull_calls: Mutex::new(Vec::new()),
        }
    }

    /// Queue the changes the next (and every subsequent, until called again) `pull` returns.
    pub fn script_pull(&self, changes: Vec<ExternalChange>) {
        *self
            .scripted_changes
            .lock()
            .expect("mutex is only ever held for the duration of a single call, never poisoned") =
            changes;
    }

    /// Every projection passed to `push`, in call order.
    pub fn pushed(&self) -> Vec<Projection> {
        self.pushed
            .lock()
            .expect("mutex is only ever held for the duration of a single call, never poisoned")
            .clone()
    }

    /// Every `since` timestamp `pull` was called with, in call order.
    pub fn pull_calls(&self) -> Vec<Timestamp> {
        self.pull_calls
            .lock()
            .expect("mutex is only ever held for the duration of a single call, never poisoned")
            .clone()
    }
}

#[async_trait]
impl Tracker for RecordingTracker {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> TrackerCapabilities {
        self.capabilities
    }

    async fn push(&self, projection: &Projection) -> Result<ExternalRef> {
        self.pushed
            .lock()
            .expect("mutex is only ever held for the duration of a single call, never poisoned")
            .push(projection.clone());
        Ok(ExternalRef {
            adapter: self.name.clone(),
            external_id: projection.ticket.to_string(),
            url: None,
        })
    }

    async fn pull(&self, since: Timestamp) -> Result<Vec<ExternalChange>> {
        self.pull_calls
            .lock()
            .expect("mutex is only ever held for the duration of a single call, never poisoned")
            .push(since);
        Ok(self
            .scripted_changes
            .lock()
            .expect("mutex is only ever held for the duration of a single call, never poisoned")
            .clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{TicketId, Timestamp};

    // Helper to create a minimal projection for testing.
    fn test_projection(ticket_id: &str, title: &str) -> Projection {
        Projection {
            ticket: ticket_id.parse().expect("valid ticket id"),
            title: title.to_string(),
            body: "test body".to_string(),
            state_hint: "open".to_string(),
            labels: vec![],
            milestone: None,
            checklist: vec![],
            degradations: vec![],
        }
    }

    #[tokio::test]
    async fn recording_tracker_push_records_projection() {
        let tracker = RecordingTracker::new(
            "test-tracker",
            TrackerCapabilities {
                parent_child: true,
                arbitrary_states: true,
                milestones: true,
                labels: true,
                comments: true,
                max_body_bytes: 1024,
            },
        );
        let proj = test_projection("T-1", "Test Issue");

        let result = tracker.push(&proj).await;
        assert!(result.is_ok());

        let pushed = tracker.pushed();
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].ticket, proj.ticket);
        assert_eq!(pushed[0].title, proj.title);
    }

    #[tokio::test]
    async fn recording_tracker_push_returns_correct_external_ref() {
        let tracker = RecordingTracker::new(
            "github",
            TrackerCapabilities {
                parent_child: true,
                arbitrary_states: true,
                milestones: true,
                labels: true,
                comments: true,
                max_body_bytes: 2048,
            },
        );
        let proj = test_projection("T-42", "Issue Title");

        let result = tracker.push(&proj).await.expect("push should succeed");
        assert_eq!(result.adapter, "github");
        assert_eq!(result.external_id, "T-42");
        assert_eq!(result.url, None);
    }

    #[tokio::test]
    async fn recording_tracker_push_multiple_calls_records_all() {
        let tracker = RecordingTracker::new(
            "test",
            TrackerCapabilities {
                parent_child: false,
                arbitrary_states: false,
                milestones: false,
                labels: false,
                comments: false,
                max_body_bytes: 512,
            },
        );

        let proj1 = test_projection("T-1", "First");
        let proj2 = test_projection("T-2", "Second");
        let proj3 = test_projection("T-3", "Third");

        tracker.push(&proj1).await.expect("push 1");
        tracker.push(&proj2).await.expect("push 2");
        tracker.push(&proj3).await.expect("push 3");

        let pushed = tracker.pushed();
        assert_eq!(pushed.len(), 3);
        assert_eq!(pushed[0].ticket.to_string(), "T-1");
        assert_eq!(pushed[1].ticket.to_string(), "T-2");
        assert_eq!(pushed[2].ticket.to_string(), "T-3");
    }

    #[tokio::test]
    async fn recording_tracker_pull_records_since_timestamp() {
        let tracker = RecordingTracker::new(
            "test",
            TrackerCapabilities {
                parent_child: true,
                arbitrary_states: true,
                milestones: true,
                labels: true,
                comments: true,
                max_body_bytes: 1024,
            },
        );

        let ts1 = Timestamp::EPOCH.plus_millis(1000);
        let ts2 = Timestamp::EPOCH.plus_millis(2000);

        tracker.pull(ts1).await.expect("pull 1");
        tracker.pull(ts2).await.expect("pull 2");

        let calls = tracker.pull_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], ts1);
        assert_eq!(calls[1], ts2);
    }

    #[tokio::test]
    async fn recording_tracker_pull_returns_scripted_changes() {
        let tracker = RecordingTracker::new(
            "test",
            TrackerCapabilities {
                parent_child: true,
                arbitrary_states: true,
                milestones: true,
                labels: true,
                comments: true,
                max_body_bytes: 1024,
            },
        );

        let change1 = ExternalChange {
            external: ExternalRef {
                adapter: "test".to_string(),
                external_id: "issue-1".to_string(),
                url: None,
            },
            kind: ExternalChangeKind::StatusHint {
                state: "closed".to_string(),
            },
            observed_at: Timestamp::EPOCH.plus_millis(100),
        };

        let change2 = ExternalChange {
            external: ExternalRef {
                adapter: "test".to_string(),
                external_id: "issue-2".to_string(),
                url: Some("https://example.com/2".to_string()),
            },
            kind: ExternalChangeKind::Assigned {
                assignee: Some("alice".to_string()),
            },
            observed_at: Timestamp::EPOCH.plus_millis(200),
        };

        tracker.script_pull(vec![change1.clone(), change2.clone()]);

        let result = tracker
            .pull(Timestamp::EPOCH.plus_millis(0))
            .await
            .expect("pull should succeed");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], change1);
        assert_eq!(result[1], change2);
    }

    #[tokio::test]
    async fn recording_tracker_pull_persists_scripted_changes_across_calls() {
        let tracker = RecordingTracker::new(
            "test",
            TrackerCapabilities {
                parent_child: true,
                arbitrary_states: true,
                milestones: true,
                labels: true,
                comments: true,
                max_body_bytes: 1024,
            },
        );

        let change = ExternalChange {
            external: ExternalRef {
                adapter: "test".to_string(),
                external_id: "issue-1".to_string(),
                url: None,
            },
            kind: ExternalChangeKind::StatusHint {
                state: "open".to_string(),
            },
            observed_at: Timestamp::EPOCH.plus_millis(100),
        };

        tracker.script_pull(vec![change.clone()]);

        let first = tracker
            .pull(Timestamp::EPOCH.plus_millis(0))
            .await
            .expect("first pull");
        let second = tracker
            .pull(Timestamp::EPOCH.plus_millis(0))
            .await
            .expect("second pull");

        assert_eq!(first, vec![change.clone()]);
        assert_eq!(second, vec![change]);
    }

    #[tokio::test]
    async fn recording_tracker_pull_replaces_scripted_changes_on_rescripting() {
        let tracker = RecordingTracker::new(
            "test",
            TrackerCapabilities {
                parent_child: true,
                arbitrary_states: true,
                milestones: true,
                labels: true,
                comments: true,
                max_body_bytes: 1024,
            },
        );

        let change1 = ExternalChange {
            external: ExternalRef {
                adapter: "test".to_string(),
                external_id: "issue-1".to_string(),
                url: None,
            },
            kind: ExternalChangeKind::StatusHint {
                state: "open".to_string(),
            },
            observed_at: Timestamp::EPOCH.plus_millis(100),
        };

        let change2 = ExternalChange {
            external: ExternalRef {
                adapter: "test".to_string(),
                external_id: "issue-2".to_string(),
                url: None,
            },
            kind: ExternalChangeKind::Assigned { assignee: None },
            observed_at: Timestamp::EPOCH.plus_millis(200),
        };

        tracker.script_pull(vec![change1]);
        let first = tracker
            .pull(Timestamp::EPOCH.plus_millis(0))
            .await
            .expect("first pull");
        assert_eq!(first.len(), 1);

        tracker.script_pull(vec![change2.clone()]);
        let second = tracker
            .pull(Timestamp::EPOCH.plus_millis(0))
            .await
            .expect("second pull");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0], change2);
    }

    #[tokio::test]
    async fn null_tracker_name_matches_input() {
        let tracker = NullTracker::new("my-tracker");
        assert_eq!(tracker.name(), "my-tracker");
    }

    #[tokio::test]
    async fn null_tracker_capabilities_all_true() {
        let tracker = NullTracker::new("test");
        let caps = tracker.capabilities();
        assert!(caps.parent_child);
        assert!(caps.arbitrary_states);
        assert!(caps.milestones);
        assert!(caps.labels);
        assert!(caps.comments);
        assert_eq!(caps.max_body_bytes, usize::MAX);
    }

    #[tokio::test]
    async fn null_tracker_push_returns_synthetic_ref() {
        let tracker = NullTracker::new("null");
        let proj = test_projection("T-99", "Test");

        let result = tracker.push(&proj).await.expect("push should succeed");
        assert_eq!(result.adapter, "null");
        assert_eq!(result.external_id, "T-99");
        assert_eq!(result.url, None);
    }

    #[tokio::test]
    async fn null_tracker_pull_returns_empty_vec() {
        let tracker = NullTracker::new("null");

        let result = tracker
            .pull(Timestamp::EPOCH.plus_millis(0))
            .await
            .expect("pull should succeed");
        assert_eq!(result, vec![]);
    }

    #[tokio::test]
    async fn default_confirm_returns_none_for_adapters_that_do_not_override_it() {
        // `SPEC.md` §21.5 (audit B-11): an adapter with no reliable "did this already happen"
        // search surface inherits `Tracker::confirm`'s default rather than guessing, and the
        // documented, accepted behavior for such a kind is to re-run. `NullTracker`/
        // `RecordingTracker` are exactly that (no external system behind them at all).
        let ticket: TicketId = "T-1".parse().expect("valid ticket id");
        let null_tracker = NullTracker::new("null");
        assert_eq!(null_tracker.confirm(&ticket).await.expect("confirm"), None);

        let recording_tracker = RecordingTracker::new(
            "test",
            TrackerCapabilities {
                parent_child: true,
                arbitrary_states: true,
                milestones: true,
                labels: true,
                comments: true,
                max_body_bytes: 1024,
            },
        );
        assert_eq!(
            recording_tracker.confirm(&ticket).await.expect("confirm"),
            None
        );
    }

    #[tokio::test]
    async fn recording_tracker_capabilities_stored() {
        let caps = TrackerCapabilities {
            parent_child: false,
            arbitrary_states: false,
            milestones: true,
            labels: true,
            comments: false,
            max_body_bytes: 256,
        };
        let tracker = RecordingTracker::new("test", caps);

        assert_eq!(tracker.capabilities(), caps);
    }

    #[tokio::test]
    async fn recording_tracker_name_matches_input() {
        let tracker = RecordingTracker::new(
            "custom-adapter",
            TrackerCapabilities {
                parent_child: true,
                arbitrary_states: true,
                milestones: true,
                labels: true,
                comments: true,
                max_body_bytes: 1024,
            },
        );
        assert_eq!(tracker.name(), "custom-adapter");
    }

    #[tokio::test]
    async fn recording_tracker_pull_returns_empty_before_scripting() {
        let tracker = RecordingTracker::new(
            "test",
            TrackerCapabilities {
                parent_child: true,
                arbitrary_states: true,
                milestones: true,
                labels: true,
                comments: true,
                max_body_bytes: 1024,
            },
        );

        let result = tracker
            .pull(Timestamp::EPOCH.plus_millis(0))
            .await
            .expect("pull should succeed");
        assert_eq!(result, vec![]);
    }

    #[tokio::test]
    async fn external_change_comment_added_variant() {
        let change = ExternalChange {
            external: ExternalRef {
                adapter: "github".to_string(),
                external_id: "#123".to_string(),
                url: Some("https://github.com/org/repo/issues/123".to_string()),
            },
            kind: ExternalChangeKind::CommentAdded {
                author: "bob".to_string(),
                body: "This is a comment".to_string(),
            },
            observed_at: Timestamp::EPOCH.plus_millis(500),
        };

        assert_eq!(change.external.adapter, "github");
        assert_eq!(change.external.external_id, "#123");
        match change.kind {
            ExternalChangeKind::CommentAdded { author, body } => {
                assert_eq!(author, "bob");
                assert_eq!(body, "This is a comment");
            }
            _ => panic!("expected CommentAdded"),
        }
    }

    #[tokio::test]
    async fn external_change_priority_changed_variant() {
        let change = ExternalChange {
            external: ExternalRef {
                adapter: "jira".to_string(),
                external_id: "PROJ-456".to_string(),
                url: None,
            },
            kind: ExternalChangeKind::PriorityChanged {
                priority: "High".to_string(),
            },
            observed_at: Timestamp::EPOCH.plus_millis(600),
        };

        match change.kind {
            ExternalChangeKind::PriorityChanged { priority } => {
                assert_eq!(priority, "High");
            }
            _ => panic!("expected PriorityChanged"),
        }
    }

    #[tokio::test]
    async fn external_change_issue_created_variant() {
        let change = ExternalChange {
            external: ExternalRef {
                adapter: "linear".to_string(),
                external_id: "NEW-001".to_string(),
                url: Some("https://linear.app/team/issue/NEW-001".to_string()),
            },
            kind: ExternalChangeKind::IssueCreated {
                title: "New Issue".to_string(),
                body: "Issue body".to_string(),
                author: "alice".to_string(),
            },
            observed_at: Timestamp::EPOCH.plus_millis(700),
        };

        match change.kind {
            ExternalChangeKind::IssueCreated {
                title,
                body,
                author,
            } => {
                assert_eq!(title, "New Issue");
                assert_eq!(body, "Issue body");
                assert_eq!(author, "alice");
            }
            _ => panic!("expected IssueCreated"),
        }
    }

    #[tokio::test]
    async fn external_change_assigned_unassigned() {
        let change = ExternalChange {
            external: ExternalRef {
                adapter: "test".to_string(),
                external_id: "issue-1".to_string(),
                url: None,
            },
            kind: ExternalChangeKind::Assigned { assignee: None },
            observed_at: Timestamp::EPOCH.plus_millis(300),
        };

        match change.kind {
            ExternalChangeKind::Assigned { assignee } => {
                assert_eq!(assignee, None);
            }
            _ => panic!("expected Assigned"),
        }
    }
}
