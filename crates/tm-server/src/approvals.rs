//! Approvals: open a request, list pending ones, decide, and notify the waiting executor.
//!
//! `SPEC.md` §14: "an approval blocks the requesting agent" — the agent that opened the request
//! holds an HTTP connection open (or polls) until a decision lands. The durable half of an
//! approval (that it happened, who decided it, why) is recorded as a `tm-core` decision via
//! [`crate::routes`]; [`ApprovalRegistry`] is purely the in-process rendezvous that makes the
//! blocking behavior possible — a `tokio::sync::oneshot` per open request, so [`decide`]
//! (called from a different HTTP request, possibly a different connection entirely) can wake the
//! task blocked in [`ApprovalWaiter::wait`] without polling.
//!
//! Losing this table on restart is intentional: a request still `Pending` when the server
//! restarts is not durable (its `tm-core` decision was never recorded, since recording happens
//! *after* the decision, not the request), so the waiting agent's connection simply drops and it
//! must re-request.

use std::collections::BTreeMap;
use std::sync::Mutex;

use tm_types::{ParticipantId, TicketId, Timestamp};

/// Identifies one approval request for the lifetime of the server process. Not a durable
/// `tm-types` id kind (approvals themselves aren't a `tm-core` object; the *decision* they
/// produce is) — a locally-minted opaque string, from the injected `IdSource`.
pub type ApprovalId = String;

/// An open approval request.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    /// This request's id.
    pub id: ApprovalId,
    /// The ticket the requesting agent is blocked on, if any.
    pub ticket: Option<TicketId>,
    /// Who opened the request (the blocked agent).
    pub requested_by: ParticipantId,
    /// One-line subject, e.g. `"force-push to main"`.
    pub subject: String,
    /// Longer free-form detail/justification.
    pub detail: String,
    /// When the request was opened.
    pub requested_at: Timestamp,
}

/// A decision on an [`ApprovalRequest`].
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDecision {
    /// The request is granted.
    Approve {
        /// Optional note from the decider.
        note: Option<String>,
    },
    /// The request is refused.
    Deny {
        /// Why, shown to the blocked agent.
        reason: String,
    },
}

/// The state of one request, as reported by `GET /approvals` and `GET /approvals/:id`.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalStatus {
    /// Still waiting on a decision.
    Pending,
    /// A decision has landed.
    Decided {
        /// What was decided.
        decision: ApprovalDecision,
        /// Who decided it.
        decided_by: ParticipantId,
        /// When.
        decided_at: Timestamp,
    },
}

/// One pending request, as listed by [`ApprovalRegistry::pending`].
#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    /// The request itself.
    pub request: ApprovalRequest,
}

/// Why an [`ApprovalRegistry`] operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    /// No request with this id is known.
    #[error("unknown approval request {0}")]
    NotFound(ApprovalId),
    /// [`ApprovalRegistry::decide`] was called twice for the same request.
    #[error("approval request {0} was already decided")]
    AlreadyDecided(ApprovalId),
    /// The requester's [`ApprovalWaiter`] was dropped (e.g. its connection closed) before a
    /// decision arrived; [`ApprovalRegistry::decide`] still records the decision, but nothing
    /// was listening for the wakeup.
    #[error("the requester for approval {0} is no longer waiting")]
    WaiterGone(ApprovalId),
}

struct Entry {
    request: ApprovalRequest,
    waiter: Option<tokio::sync::oneshot::Sender<ApprovalDecision>>,
    result: Option<(ApprovalDecision, ParticipantId, Timestamp)>,
}

/// The registry: one entry per approval request opened since the server started.
pub struct ApprovalRegistry {
    entries: Mutex<BTreeMap<ApprovalId, Entry>>,
}

impl ApprovalRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        ApprovalRegistry {
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Open a new request, returning the [`ApprovalWaiter`] the requesting handler should
    /// `.await` to block until a decision lands.
    pub fn open(&self, request: ApprovalRequest) -> ApprovalWaiter {
        // IMPL: tokio::sync::oneshot::channel(); lock entries, insert Entry { request:
        // request.clone(), waiter: Some(tx), result: None } keyed by request.id.clone(); return
        // ApprovalWaiter { id: request.id, receiver: rx }.
        todo!("register a pending entry with a fresh oneshot channel, return its waiter half")
    }

    /// Every currently-pending request, for `GET /approvals`.
    pub fn pending(&self) -> Vec<PendingApproval> {
        // IMPL: lock, filter entries where result.is_none(), map to PendingApproval { request:
        // entry.request.clone() }, collect in BTreeMap (id) order.
        todo!("list every entry with no decision yet")
    }

    /// Look up one request's current status.
    pub fn get(&self, id: &ApprovalId) -> Option<ApprovalStatus> {
        // IMPL: lock, look up id; None -> None; Some(entry) with result -> Decided { .. }; Some
        // with no result -> Pending.
        todo!("report Pending or Decided for this request id")
    }

    /// Record a decision and wake the waiting requester.
    ///
    /// # Errors
    /// [`ApprovalError::NotFound`] if `id` is unknown. [`ApprovalError::AlreadyDecided`] if this
    /// request already has a result. On success, if the requester's [`ApprovalWaiter`] was
    /// already dropped, returns [`ApprovalError::WaiterGone`] *after* still recording the
    /// decision (the decision is truth regardless of whether anyone was listening).
    pub fn decide(
        &self,
        id: &ApprovalId,
        decision: ApprovalDecision,
        decided_by: ParticipantId,
        decided_at: Timestamp,
    ) -> Result<(), ApprovalError> {
        // IMPL: lock, get_mut(id).ok_or(NotFound); if entry.result.is_some() ->
        // AlreadyDecided; else set entry.result = Some((decision.clone(), decided_by,
        // decided_at)); take entry.waiter and, if Some(tx), tx.send(decision) — a Err from
        // send() means the receiver was dropped, map that (and a None waiter, meaning open()
        // was never called with this exact path) to WaiterGone; otherwise Ok(()).
        todo!("record the decision, wake the waiter if still listening")
    }
}

impl Default for ApprovalRegistry {
    fn default() -> Self {
        ApprovalRegistry::new()
    }
}

/// The requester's half of an open approval: `.await` this to block until
/// [`ApprovalRegistry::decide`] is called for the same id.
pub struct ApprovalWaiter {
    /// The request this waiter is blocked on.
    pub id: ApprovalId,
    receiver: tokio::sync::oneshot::Receiver<ApprovalDecision>,
}

impl ApprovalWaiter {
    /// Block until a decision lands, or the registry (and thus the sender) is dropped.
    ///
    /// # Errors
    /// [`ApprovalError::WaiterGone`] if the sender was dropped without ever deciding (server
    /// shutdown while this request was still pending).
    pub async fn wait(self) -> Result<ApprovalDecision, ApprovalError> {
        // IMPL: self.receiver.await.map_err(|_| ApprovalError::WaiterGone(self.id)).
        todo!("await the oneshot receiver, translating a dropped sender into WaiterGone")
    }
}
