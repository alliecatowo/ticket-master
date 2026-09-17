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
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = request.id.clone();
        let entry = Entry {
            request: request.clone(),
            waiter: Some(tx),
            result: None,
        };
        self.entries.lock().unwrap().insert(id.clone(), entry);
        ApprovalWaiter { id, receiver: rx }
    }

    /// Every currently-pending request, for `GET /approvals`.
    pub fn pending(&self) -> Vec<PendingApproval> {
        let entries = self.entries.lock().unwrap();
        entries
            .values()
            .filter(|entry| entry.result.is_none())
            .map(|entry| PendingApproval {
                request: entry.request.clone(),
            })
            .collect()
    }

    /// Look up one request's current status.
    pub fn get(&self, id: &ApprovalId) -> Option<ApprovalStatus> {
        let entries = self.entries.lock().unwrap();
        entries.get(id).map(|entry| {
            if let Some((decision, decided_by, decided_at)) = &entry.result {
                ApprovalStatus::Decided {
                    decision: decision.clone(),
                    decided_by: decided_by.clone(),
                    decided_at: *decided_at,
                }
            } else {
                ApprovalStatus::Pending
            }
        })
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
        let mut entries = self.entries.lock().unwrap();
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| ApprovalError::NotFound(id.clone()))?;

        if entry.result.is_some() {
            return Err(ApprovalError::AlreadyDecided(id.clone()));
        }

        entry.result = Some((decision.clone(), decided_by, decided_at));

        if let Some(tx) = entry.waiter.take() {
            tx.send(decision)
                .map_err(|_| ApprovalError::WaiterGone(id.clone()))
        } else {
            Err(ApprovalError::WaiterGone(id.clone()))
        }
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
        self.receiver
            .await
            .map_err(|_| ApprovalError::WaiterGone(self.id))
    }
}

/// Public alias for the registry backing approvals.
pub type ApprovalStore = ApprovalRegistry;

/// Public alias for an approval request.
pub type Approval = ApprovalRequest;

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{Clock, FixedClock, IdKind, IdSource, ParticipantId, TestIds};

    fn make_request(id: &str) -> ApprovalRequest {
        let clock = FixedClock::epoch();
        let ids = TestIds::new();
        ApprovalRequest {
            id: id.to_string(),
            ticket: None,
            requested_by: ParticipantId::new(ids.next(IdKind::Participant).as_str()).expect("valid participant id"),
            subject: "test subject".to_string(),
            detail: "test detail".to_string(),
            requested_at: clock.now(),
        }
    }

    #[tokio::test]
    async fn happy_path_approve_and_wait() {
        let registry = ApprovalRegistry::new();
        let req = make_request("req-1");
        let waiter = registry.open(req);

        let ids = TestIds::new();
        let clock = FixedClock::epoch();
        let decision = ApprovalDecision::Approve {
            note: Some("looks good".to_string()),
        };

        registry
            .decide(&"req-1".to_string(), decision.clone(), ParticipantId::new(ids.next(IdKind::Participant).as_str()).expect("valid participant id"), clock.now())
            .unwrap();

        let result = waiter.wait().await.unwrap();
        assert_eq!(result, decision);
    }

    #[tokio::test]
    async fn happy_path_deny_and_wait() {
        let registry = ApprovalRegistry::new();
        let req = make_request("req-2");
        let waiter = registry.open(req);

        let ids = TestIds::new();
        let clock = FixedClock::epoch();
        let decision = ApprovalDecision::Deny {
            reason: "not approved".to_string(),
        };

        registry
            .decide(&"req-2".to_string(), decision.clone(), ParticipantId::new(ids.next(IdKind::Participant).as_str()).expect("valid participant id"), clock.now())
            .unwrap();

        let result = waiter.wait().await.unwrap();
        assert_eq!(result, decision);
    }

    #[test]
    fn get_returns_none_for_unknown() {
        let registry = ApprovalRegistry::new();
        assert_eq!(registry.get(&"unknown".to_string()), None);
    }

    #[test]
    fn get_returns_pending() {
        let registry = ApprovalRegistry::new();
        let req = make_request("req-3");
        let _waiter = registry.open(req);

        assert_eq!(registry.get(&"req-3".to_string()), Some(ApprovalStatus::Pending));
    }

    #[test]
    fn get_returns_decided() {
        let registry = ApprovalRegistry::new();
        let req = make_request("req-4");
        let _waiter = registry.open(req);

        let ids = TestIds::new();
        let clock = FixedClock::epoch();
        let decided_by = ParticipantId::new(ids.next(IdKind::Participant).as_str()).expect("valid participant id");
        let decided_at = clock.now();
        let decision = ApprovalDecision::Approve { note: None };

        registry
            .decide(&"req-4".to_string(), decision.clone(), decided_by.clone(), decided_at)
            .ok();

        if let Some(ApprovalStatus::Decided {
            decision: d,
            decided_by: db,
            decided_at: da,
        }) = registry.get(&"req-4".to_string())
        {
            assert_eq!(d, decision);
            assert_eq!(db, decided_by);
            assert_eq!(da, decided_at);
        } else {
            panic!("expected Decided status");
        }
    }

    #[test]
    fn decide_not_found() {
        let registry = ApprovalRegistry::new();
        let ids = TestIds::new();
        let clock = FixedClock::epoch();

        let err = registry.decide(
            &"unknown".to_string(),
            ApprovalDecision::Approve { note: None },
            ParticipantId::new(ids.next(IdKind::Participant).as_str()).expect("valid participant id"),
            clock.now(),
        );

        assert_eq!(err, Err(ApprovalError::NotFound("unknown".to_string())));
    }

    #[test]
    fn decide_already_decided() {
        let registry = ApprovalRegistry::new();
        let req = make_request("req-5");
        let _waiter = registry.open(req);

        let ids = TestIds::new();
        let clock = FixedClock::epoch();
        let decision = ApprovalDecision::Approve { note: None };

        registry
            .decide(&"req-5".to_string(), decision.clone(), ParticipantId::new(ids.next(IdKind::Participant).as_str()).expect("valid participant id"), clock.now())
            .ok();

        let err = registry.decide(&"req-5".to_string(), decision, ParticipantId::new(ids.next(IdKind::Participant).as_str()).expect("valid participant id"), clock.now());

        assert_eq!(err, Err(ApprovalError::AlreadyDecided("req-5".to_string())));
    }

    #[test]
    fn decide_waiter_gone() {
        let registry = ApprovalRegistry::new();
        let req = make_request("req-6");
        let waiter = registry.open(req);

        drop(waiter);

        let ids = TestIds::new();
        let clock = FixedClock::epoch();
        let decision = ApprovalDecision::Approve { note: None };

        let err = registry.decide(&"req-6".to_string(), decision, ParticipantId::new(ids.next(IdKind::Participant).as_str()).expect("valid participant id"), clock.now());

        assert_eq!(err, Err(ApprovalError::WaiterGone("req-6".to_string())));

        if let Some(ApprovalStatus::Decided { .. }) = registry.get(&"req-6".to_string()) {
        } else {
            panic!("decision should be recorded even if waiter gone");
        }
    }

    #[tokio::test]
    async fn wait_sender_dropped() {
        let registry = ApprovalRegistry::new();
        let req = make_request("req-7");
        let waiter = registry.open(req);

        drop(registry);

        let err = waiter.wait().await;
        assert_eq!(err, Err(ApprovalError::WaiterGone("req-7".to_string())));
    }

    #[test]
    fn pending_empty_initially() {
        let registry = ApprovalRegistry::new();
        assert_eq!(registry.pending().len(), 0);
    }

    #[test]
    fn pending_lists_all_pending() {
        let registry = ApprovalRegistry::new();
        let req1 = make_request("req-8");
        let req2 = make_request("req-9");
        let req3 = make_request("req-10");

        let _w1 = registry.open(req1.clone());
        let _w2 = registry.open(req2.clone());
        let _w3 = registry.open(req3.clone());

        // `pending` makes no ordering promise (the registry keys by id in a `BTreeMap`, so the
        // order is lexicographic, not insertion order); assert membership instead of position.
        let pending = registry.pending();
        let mut ids: Vec<&str> = pending.iter().map(|p| p.request.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["req-10", "req-8", "req-9"]);
    }

    #[test]
    fn pending_excludes_decided() {
        let registry = ApprovalRegistry::new();
        let req1 = make_request("req-11");
        let req2 = make_request("req-12");

        let _w1 = registry.open(req1);
        let _w2 = registry.open(req2);

        let ids = TestIds::new();
        let clock = FixedClock::epoch();

        registry
            .decide(
                &"req-11".to_string(),
                ApprovalDecision::Approve { note: None },
                ParticipantId::new(ids.next(IdKind::Participant).as_str()).expect("valid participant id"),
                clock.now(),
            )
            .ok();

        let pending = registry.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request.id, "req-12");
    }

    #[test]
    fn default_creates_empty_registry() {
        let registry = ApprovalRegistry::default();
        assert_eq!(registry.pending().len(), 0);
    }
}
