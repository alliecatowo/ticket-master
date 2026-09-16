//! Leases: the mechanism by which a `Ready` ticket's authority is handed to an executor.
//!
//! Owns [`Lease`] itself plus the pure decision functions `acquire`/`heartbeat`/`release` operate
//! through, and [`expire_due`], a pure sweep over a snapshot that returns the reversion actions a
//! caller (`store.rs`) must apply — this module never touches SQLite or the clock directly, it is
//! handed `now: Timestamp` and existing state, and answers a pure question.
//!
//! A dead worker cannot block the project: `expire_due` is what makes that true. Resource
//! conflict detection (`conflicts_with`) is conservative in the safe direction per `SPEC.md`
//! §4.5 — it may report a conflict that could not actually occur, but must never miss a real one.

use thiserror::Error;
use tm_types::{Authority, LeaseId, ParticipantId, TicketId, Timestamp};

use crate::ticket::{FailureClass, ResourceClaim, TicketState};

/// A live or expired grant of a ticket's authority to a holder, for the duration of `ttl_seconds`
/// since the last heartbeat. See `SPEC.md` §4.5.
#[derive(Debug, Clone, PartialEq)]
pub struct Lease {
    /// Stable identifier for this lease.
    pub id: LeaseId,
    /// The ticket this lease grants authority over.
    pub ticket: TicketId,
    /// Who holds this lease.
    pub holder: ParticipantId,
    /// The authority attenuated to the holder for this lease; always contained by the ticket's
    /// own authority.
    pub authority: Authority,
    /// Resource claims held for the duration of this lease.
    pub resources: Vec<ResourceClaim>,
    /// When this lease was first acquired.
    pub acquired: Timestamp,
    /// The last heartbeat timestamp; `heartbeat + ttl_seconds` is the expiry instant.
    pub heartbeat: Timestamp,
    /// Seconds of silence tolerated before this lease is considered expired.
    pub ttl_seconds: u32,
    /// Monotonic counter distinguishing repeated leases of the same ticket (a fresh lease after
    /// expiry/release gets the next epoch, so a stale heartbeat from a zombie holder can be
    /// detected and rejected even if it races back in).
    pub epoch: u64,
}

impl Lease {
    /// The instant this lease expires if no further heartbeat arrives.
    pub fn expires_at(&self) -> Timestamp {
        self.heartbeat.plus_seconds(self.ttl_seconds as i64)
    }

    /// True when `now` is at or past [`Lease::expires_at`].
    pub fn is_expired(&self, now: Timestamp) -> bool {
        now >= self.expires_at()
    }
}

/// Why [`acquire`] refused to grant a lease.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AcquireError {
    /// The ticket was not in `TicketState::Ready`.
    #[error("ticket {ticket} is not Ready (state: {state:?})")]
    NotReady {
        /// The ticket that was leased against.
        ticket: TicketId,
        /// Its actual state.
        state: TicketState,
    },
    /// The requested authority was not contained by the ticket's own authority.
    #[error("requested authority for {ticket} exceeds ticket authority")]
    AuthorityExceeded {
        /// The ticket that was leased against.
        ticket: TicketId,
    },
    /// A requested resource claim conflicts with a claim held by another live lease.
    #[error("resource claim for {ticket} conflicts with live lease {conflicting_with}")]
    ResourceConflict {
        /// The ticket that was leased against.
        ticket: TicketId,
        /// The lease whose claim conflicts.
        conflicting_with: LeaseId,
    },
}

/// The lease had already expired; the holder must stop work immediately.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("lease {0} has expired")]
pub struct LeaseExpiredError(pub LeaseId);

/// What `expire_due` says must happen to revert one expired lease's effects.
#[derive(Debug, Clone, PartialEq)]
pub struct ReversionAction {
    /// The lease that expired.
    pub lease: LeaseId,
    /// The ticket whose authority/state must revert.
    pub ticket: TicketId,
    /// The ticket returns to `Ready` (per `SPEC.md` §4.3 rule 4) with this failure recorded.
    pub failure: FailureClass,
}

/// Attempt to grant `ticket` (currently in `ticket_state`) to `holder`, requesting `requested`
/// authority and `resources`, for `ttl_seconds`. Fails per `SPEC.md` §4.5: not `Ready`, requested
/// authority not contained by `ticket_authority`, or a resource conflict with `live_leases`.
pub fn acquire(
    ticket: &TicketId,
    ticket_state: TicketState,
    ticket_authority: &Authority,
    holder: ParticipantId,
    requested: Authority,
    resources: Vec<ResourceClaim>,
    live_leases: &[Lease],
    id: LeaseId,
    next_epoch: u64,
    now: Timestamp,
    ttl_seconds: u32,
) -> Result<Lease, AcquireError> {
    if !crate::machine::can_lease(ticket_state) {
        return Err(AcquireError::NotReady {
            ticket: ticket.clone(),
            state: ticket_state,
        });
    }
    if !ticket_authority.contains(&requested) {
        return Err(AcquireError::AuthorityExceeded {
            ticket: ticket.clone(),
        });
    }
    for live in live_leases {
        for held in &live.resources {
            for requested_claim in &resources {
                if conflicts_with(held, requested_claim) {
                    return Err(AcquireError::ResourceConflict {
                        ticket: ticket.clone(),
                        conflicting_with: live.id.clone(),
                    });
                }
            }
        }
    }
    Ok(Lease {
        id,
        ticket: ticket.clone(),
        holder,
        authority: requested,
        resources,
        acquired: now,
        heartbeat: now,
        ttl_seconds,
        epoch: next_epoch,
    })
}

/// Refresh `lease`'s heartbeat to `now`. Fails once the lease has already expired: the worker
/// must stop, not keep extending a lease that already reverted.
pub fn heartbeat(lease: &Lease, now: Timestamp) -> Result<Lease, LeaseExpiredError> {
    if lease.is_expired(now) {
        return Err(LeaseExpiredError(lease.id.clone()));
    }
    let mut updated = lease.clone();
    updated.heartbeat = now;
    Ok(updated)
}

/// Release `lease` voluntarily (not via expiry). Always succeeds; the caller applies the
/// resulting reversion via `store.rs` (ticket -> Ready, attempt already counted at acquire time).
pub fn release(lease: &Lease) -> ReversionAction {
    ReversionAction {
        lease: lease.id.clone(),
        ticket: lease.ticket.clone(),
        failure: FailureClass::Other,
    }
}

/// Pure sweep: given every live lease and the current instant, return the reversion actions for
/// every lease that has expired. Emits `FailureClass::LeaseTimeout` (distinct from `release`'s
/// voluntary path) so `RetryPolicy` can treat a crash differently from a graceful release.
pub fn expire_due(live_leases: &[Lease], now: Timestamp) -> Vec<ReversionAction> {
    live_leases
        .iter()
        .filter(|l| l.is_expired(now))
        .map(|l| ReversionAction {
            lease: l.id.clone(),
            ticket: l.ticket.clone(),
            failure: FailureClass::LeaseTimeout,
        })
        .collect()
}

/// Conservative overlap check between two resource claims: true when they *might* conflict,
/// i.e. their patterns may overlap (`PathPattern::may_overlap`, itself conservative) and at least
/// one claim is `Exclusive`. Two `Shared` claims never conflict regardless of overlap.
pub fn conflicts_with(a: &ResourceClaim, b: &ResourceClaim) -> bool {
    use crate::ticket::ResourceMode;
    let either_exclusive =
        matches!(a.mode, ResourceMode::Exclusive) || matches!(b.mode, ResourceMode::Exclusive);
    either_exclusive && a.pattern.may_overlap(&b.pattern)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ticket::ResourceMode;
    use tm_types::PathPattern;

    fn ticket_id() -> TicketId {
        TicketId::new("T-1").unwrap()
    }

    fn holder() -> ParticipantId {
        ParticipantId::system()
    }

    fn claim(path: &str, mode: ResourceMode) -> ResourceClaim {
        ResourceClaim {
            pattern: PathPattern::new(path).unwrap(),
            mode,
        }
    }

    fn lease_id(n: &str) -> LeaseId {
        LeaseId::new(format!("L-{n}")).unwrap()
    }

    #[test]
    fn acquire_succeeds_for_ready_ticket_with_contained_authority() {
        let lease = acquire(
            &ticket_id(),
            TicketState::Ready,
            &Authority::root(),
            holder(),
            Authority::none(),
            vec![],
            &[],
            lease_id("000000000001"),
            0,
            Timestamp::EPOCH,
            60,
        )
        .unwrap();
        assert_eq!(lease.epoch, 0);
        assert_eq!(lease.acquired, Timestamp::EPOCH);
        assert_eq!(lease.heartbeat, Timestamp::EPOCH);
    }

    #[test]
    fn acquire_rejects_non_ready_ticket() {
        let err = acquire(
            &ticket_id(),
            TicketState::Blocked,
            &Authority::root(),
            holder(),
            Authority::none(),
            vec![],
            &[],
            lease_id("000000000002"),
            0,
            Timestamp::EPOCH,
            60,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            AcquireError::NotReady {
                state: TicketState::Blocked,
                ..
            }
        ));
    }

    #[test]
    fn acquire_rejects_authority_not_contained_by_ticket() {
        let err = acquire(
            &ticket_id(),
            TicketState::Ready,
            &Authority::none(),
            holder(),
            Authority::root(),
            vec![],
            &[],
            lease_id("000000000003"),
            0,
            Timestamp::EPOCH,
            60,
        )
        .unwrap_err();
        assert!(matches!(err, AcquireError::AuthorityExceeded { .. }));
    }

    #[test]
    fn acquire_rejects_conflicting_exclusive_resource_claim() {
        let holder_lease = Lease {
            id: lease_id("000000000004"),
            ticket: ticket_id(),
            holder: holder(),
            authority: Authority::none(),
            resources: vec![claim("src/lib.rs", ResourceMode::Exclusive)],
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 60,
            epoch: 0,
        };
        let err = acquire(
            &ticket_id(),
            TicketState::Ready,
            &Authority::root(),
            holder(),
            Authority::none(),
            vec![claim("src/lib.rs", ResourceMode::Exclusive)],
            &[holder_lease],
            lease_id("000000000005"),
            1,
            Timestamp::EPOCH,
            60,
        )
        .unwrap_err();
        assert!(matches!(err, AcquireError::ResourceConflict { .. }));
    }

    #[test]
    fn acquire_allows_two_shared_claims_on_same_path() {
        let holder_lease = Lease {
            id: lease_id("000000000006"),
            ticket: ticket_id(),
            holder: holder(),
            authority: Authority::none(),
            resources: vec![claim("src/lib.rs", ResourceMode::Shared)],
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 60,
            epoch: 0,
        };
        let lease = acquire(
            &ticket_id(),
            TicketState::Ready,
            &Authority::root(),
            holder(),
            Authority::none(),
            vec![claim("src/lib.rs", ResourceMode::Shared)],
            &[holder_lease],
            lease_id("000000000007"),
            1,
            Timestamp::EPOCH,
            60,
        )
        .unwrap();
        assert_eq!(lease.epoch, 1);
    }

    #[test]
    fn heartbeat_advances_timestamp_before_expiry() {
        let lease = Lease {
            id: lease_id("000000000008"),
            ticket: ticket_id(),
            holder: holder(),
            authority: Authority::none(),
            resources: vec![],
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 60,
            epoch: 0,
        };
        let refreshed = heartbeat(&lease, Timestamp::EPOCH.plus_seconds(30)).unwrap();
        assert_eq!(refreshed.heartbeat, Timestamp::EPOCH.plus_seconds(30));
    }

    #[test]
    fn heartbeat_fails_once_lease_has_expired() {
        let lease = Lease {
            id: lease_id("000000000009"),
            ticket: ticket_id(),
            holder: holder(),
            authority: Authority::none(),
            resources: vec![],
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 60,
            epoch: 0,
        };
        let err = heartbeat(&lease, Timestamp::EPOCH.plus_seconds(60)).unwrap_err();
        assert_eq!(err, LeaseExpiredError(lease.id.clone()));
    }

    #[test]
    fn release_reverts_to_ready_with_other_failure_class() {
        let lease = Lease {
            id: lease_id("000000000010"),
            ticket: ticket_id(),
            holder: holder(),
            authority: Authority::none(),
            resources: vec![],
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 60,
            epoch: 0,
        };
        let action = release(&lease);
        assert_eq!(action.failure, FailureClass::Other);
        assert_eq!(action.ticket, lease.ticket);
    }

    #[test]
    fn expire_due_reports_only_expired_leases_with_timeout_class() {
        let live = Lease {
            id: lease_id("000000000011"),
            ticket: ticket_id(),
            holder: holder(),
            authority: Authority::none(),
            resources: vec![],
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 60,
            epoch: 0,
        };
        let mut expired = live.clone();
        expired.id = lease_id("000000000012");
        expired.heartbeat = Timestamp::EPOCH;
        expired.ttl_seconds = 0;

        let now = Timestamp::EPOCH.plus_seconds(1);
        let actions = expire_due(&[live, expired.clone()], now);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].lease, expired.id);
        assert_eq!(actions[0].failure, FailureClass::LeaseTimeout);
    }

    #[test]
    fn conflicts_with_ignores_non_overlapping_paths() {
        let a = claim("src/a.rs", ResourceMode::Exclusive);
        let b = claim("src/b.rs", ResourceMode::Exclusive);
        assert!(!conflicts_with(&a, &b));
    }

    #[test]
    fn conflicts_with_ignores_two_shared_claims_even_when_overlapping() {
        let a = claim("src/a.rs", ResourceMode::Shared);
        let b = claim("src/a.rs", ResourceMode::Shared);
        assert!(!conflicts_with(&a, &b));
    }
}
