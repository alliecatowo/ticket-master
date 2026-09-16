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
        // IMPL: lock `self.pushed`, push `projection.clone()`. Return a deterministic
        // ExternalRef synthesized from `projection.ticket` (e.g. external_id =
        // projection.ticket.to_string(), url = None) — no id generation needed, this is a
        // recorder rather than a real adapter, so it must not depend on a Clock/IdSource.
        todo!("record the pushed projection and return a synthesized ExternalRef")
    }

    async fn pull(&self, since: Timestamp) -> Result<Vec<ExternalChange>> {
        // IMPL: lock `self.pull_calls`, push `since`. Lock `self.scripted_changes`, clone and
        // return its current contents (do not drain: repeated pulls with no new `script_pull`
        // call should keep returning the same script, matching a real tracker that would keep
        // reporting the same state until it actually changes).
        todo!("record the call and return the currently scripted changes")
    }
}
