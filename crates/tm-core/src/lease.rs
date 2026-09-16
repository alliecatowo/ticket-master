//! Leases: acquisition, heartbeat, release, and the expiry sweep.
//!
//! Pure logic over an in-memory view of live leases and resource claims (injected by the
//! caller, ultimately `Store`, from [`crate::view::ProjectView`]); no SQLite here. `Store`
//! layers persistence and event emission on top of these functions inside one `Tx`.

use tm_types::{Authority, ParticipantId, TicketId, Timestamp};

use crate::ticket::{FailureClass, ResourceClaim, TicketState};

/// A live lease, per `SPEC.md` §4.5.
#[derive(Debug, Clone, PartialEq)]
pub struct Lease {
    /// Identity.
    pub id: tm_types::LeaseId,
    /// The ticket this lease holds.
    pub ticket: TicketId,
    /// Who holds the lease.
    pub holder: ParticipantId,
    /// The authority delegated to the holder for the lease's duration.
    pub authority: Authority,
    /// Resource claims held for the lease's duration.
    pub resources: Vec<ResourceClaim>,
    /// When the lease was acquired.
    pub acquired: Timestamp,
    /// The last heartbeat timestamp.
    pub heartbeat: Timestamp,
    /// Time-to-live in seconds since the last heartbeat.
    pub ttl_seconds: u32,
    /// Monotonic epoch, incremented each time this ticket is re-leased; distinguishes a stale
    /// worker's late heartbeat/submission from the current lease.
    pub epoch: u64,
}

impl Lease {
    /// True when `now` is past `heartbeat + ttl_seconds`.
    pub fn is_expired(&self, now: Timestamp) -> bool {
        now.seconds_since(self.heartbeat) >= i64::from(self.ttl_seconds)
    }
}

/// Why [`acquire`] refused to grant a lease.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AcquireError {
    /// The ticket was not in [`TicketState::Ready`].
    #[error("ticket {0} is not Ready")]
    NotReady(TicketId),
    /// The requested authority was not contained by the ticket's authority.
    #[error("requested authority exceeds ticket authority for {0}")]
    AuthorityNotContained(TicketId),
    /// A live lease holds a conflicting exclusive resource claim.
    #[error("resource conflict acquiring lease for {0}")]
    ResourceConflict(TicketId),
}

/// Why [`LeaseStore::heartbeat`] refused to extend a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HeartbeatError {
    /// The lease had already expired before this heartbeat arrived.
    #[error("lease already expired")]
    Expired,
}

/// One reversion action produced by [`expire_due`]: everything the caller must apply (as events
/// plus materialized-state writes, inside one `Tx`) to make an expired lease's effects undone.
#[derive(Debug, Clone, PartialEq)]
pub struct ReversionAction {
    /// The ticket whose lease expired.
    pub ticket: TicketId,
    /// The expired lease's id.
    pub lease: tm_types::LeaseId,
    /// The holder whose authority is being reverted.
    pub holder: ParticipantId,
    /// The state the ticket reverts to: always [`TicketState::Ready`] per `SPEC.md` §4.5.
    pub reverts_to: TicketState,
    /// The failure record to append to the ticket's history.
    pub failure_class: FailureClass,
}

/// A minimal read-only view of currently-live leases, sufficient for [`acquire`]'s conflict
/// check. `Store` implements this over `ProjectView`; kept as a trait so lease logic stays
/// testable against hand-built fixtures.
pub trait LeaseView {
    /// Every currently-live lease (not expired, not released).
    fn live_leases(&self) -> Vec<&Lease>;
}

/// Facade the rest of the crate calls through; `Store` owns the actual persistence and wraps
/// these pure functions with event emission.
pub struct LeaseStore;

impl LeaseStore {
    /// Acquire a lease on `ticket` for `holder`, requesting `authority` and `resources`, valid
    /// for `ttl_seconds` from `now`.
    ///
    /// # Errors
    /// - [`AcquireError::NotReady`] unless `ticket_state == TicketState::Ready`.
    /// - [`AcquireError::AuthorityNotContained`] unless `ticket_authority.contains(&authority)`.
    /// - [`AcquireError::ResourceConflict`] if any live lease in `view` holds a resource claim
    ///   that conflicts with `resources` per [`claims_conflict`].
    #[allow(clippy::too_many_arguments)]
    pub fn acquire(
        ticket_state: TicketState,
        ticket_authority: &Authority,
        view: &dyn LeaseView,
        id: tm_types::LeaseId,
        ticket: TicketId,
        holder: ParticipantId,
        authority: Authority,
        resources: Vec<ResourceClaim>,
        now: Timestamp,
        ttl_seconds: u32,
        epoch: u64,
    ) -> Result<Lease, AcquireError> {
        if ticket_state != TicketState::Ready {
            return Err(AcquireError::NotReady(ticket));
        }
        if !ticket_authority.contains(&authority) {
            return Err(AcquireError::AuthorityNotContained(ticket));
        }
        for live in view.live_leases() {
            for existing in &live.resources {
                for requested in &resources {
                    if claims_conflict(existing, requested) {
                        return Err(AcquireError::ResourceConflict(ticket));
                    }
                }
            }
        }
        Ok(Lease {
            id,
            ticket,
            holder,
            authority,
            resources,
            acquired: now,
            heartbeat: now,
            ttl_seconds,
            epoch,
        })
    }

    /// Refresh `lease`'s heartbeat to `now`.
    ///
    /// # Errors
    /// [`HeartbeatError::Expired`] (the caller maps this to `TmError::LeaseExpired`) when
    /// `lease.is_expired(now)` was already true *before* this heartbeat — a worker whose lease
    /// has expired must stop, not silently extend it.
    pub fn heartbeat(lease: &mut Lease, now: Timestamp) -> Result<(), HeartbeatError> {
        if lease.is_expired(now) {
            return Err(HeartbeatError::Expired);
        }
        lease.heartbeat = now;
        Ok(())
    }

    /// Sweep every live lease in `view` for expiry as of `now`, returning the reversion actions
    /// needed for each. Pure: does not mutate `view` or emit events itself.
    pub fn expire_due(view: &dyn LeaseView, now: Timestamp) -> Vec<ReversionAction> {
        let mut actions: Vec<ReversionAction> = view
            .live_leases()
            .into_iter()
            .filter(|lease| lease.is_expired(now))
            .map(|lease| ReversionAction {
                ticket: lease.ticket.clone(),
                lease: lease.id.clone(),
                holder: lease.holder.clone(),
                reverts_to: TicketState::Ready,
                failure_class: FailureClass::ExecutorCrash,
            })
            .collect();
        actions.sort_by(|a, b| a.ticket.as_str().cmp(b.ticket.as_str()));
        actions
    }
}

/// True when `a` and `b` conflict: their path pattern sets can overlap
/// ([`tm_types::PatternSet::overlaps`]) and at least one is
/// [`crate::ticket::ResourceMode::Exclusive`]. Conservative in the safe direction per
/// `SPEC.md` §4.5: may report a conflict that could not occur, must never miss a real one.
pub fn claims_conflict(a: &ResourceClaim, b: &ResourceClaim) -> bool {
    use crate::ticket::ResourceMode::Shared;
    match (a.mode, b.mode) {
        (Shared, Shared) => false,
        _ => a.paths.overlaps(&b.paths),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{Clock, FixedClock, PatternSet};

    fn ticket_id() -> TicketId {
        TicketId::new("T-1").unwrap()
    }

    fn lease_id(n: &str) -> tm_types::LeaseId {
        tm_types::LeaseId::new(format!("L-{n}")).unwrap()
    }

    fn holder() -> ParticipantId {
        ParticipantId::system()
    }

    fn claim(paths: &[&str], mode: crate::ticket::ResourceMode) -> ResourceClaim {
        ResourceClaim {
            paths: PatternSet::parse(paths.iter().copied()).unwrap(),
            mode,
        }
    }

    struct FixedView(Vec<Lease>);

    impl LeaseView for FixedView {
        fn live_leases(&self) -> Vec<&Lease> {
            self.0.iter().collect()
        }
    }

    fn base_lease(now: Timestamp) -> Lease {
        Lease {
            id: lease_id("aaaaaaaaaaaa"),
            ticket: ticket_id(),
            holder: holder(),
            authority: Authority::none(),
            resources: vec![claim(&["src/**"], crate::ticket::ResourceMode::Exclusive)],
            acquired: now,
            heartbeat: now,
            ttl_seconds: 30,
            epoch: 0,
        }
    }

    #[test]
    fn acquire_succeeds_when_ready_and_authority_contained_and_no_conflict() {
        let clock = FixedClock::epoch();
        let now = clock.now();
        let view = FixedView(vec![]);
        let result = LeaseStore::acquire(
            TicketState::Ready,
            &Authority::root(),
            &view,
            lease_id("bbbbbbbbbbbb"),
            ticket_id(),
            holder(),
            Authority::none(),
            vec![claim(&["src/x.rs"], crate::ticket::ResourceMode::Exclusive)],
            now,
            60,
            0,
        );
        let lease = result.expect("acquire should succeed");
        assert_eq!(lease.acquired, now);
        assert_eq!(lease.heartbeat, now);
    }

    #[test]
    fn acquire_fails_when_ticket_not_ready() {
        let now = FixedClock::epoch().now();
        let view = FixedView(vec![]);
        let err = LeaseStore::acquire(
            TicketState::Blocked,
            &Authority::root(),
            &view,
            lease_id("bbbbbbbbbbbb"),
            ticket_id(),
            holder(),
            Authority::none(),
            vec![],
            now,
            60,
            0,
        )
        .unwrap_err();
        assert_eq!(err, AcquireError::NotReady(ticket_id()));
    }

    #[test]
    fn acquire_fails_when_requested_authority_exceeds_ticket_authority() {
        let now = FixedClock::epoch().now();
        let view = FixedView(vec![]);
        let err = LeaseStore::acquire(
            TicketState::Ready,
            &Authority::none(),
            &view,
            lease_id("bbbbbbbbbbbb"),
            ticket_id(),
            holder(),
            Authority::root(),
            vec![],
            now,
            60,
            0,
        )
        .unwrap_err();
        assert_eq!(err, AcquireError::AuthorityNotContained(ticket_id()));
    }

    #[test]
    fn acquire_fails_on_overlapping_exclusive_resource_claim() {
        let now = FixedClock::epoch().now();
        let existing = base_lease(now);
        let view = FixedView(vec![existing]);
        let err = LeaseStore::acquire(
            TicketState::Ready,
            &Authority::root(),
            &view,
            lease_id("cccccccccccc"),
            ticket_id(),
            holder(),
            Authority::none(),
            vec![claim(&["src/x.rs"], crate::ticket::ResourceMode::Exclusive)],
            now,
            60,
            0,
        )
        .unwrap_err();
        assert_eq!(err, AcquireError::ResourceConflict(ticket_id()));
    }

    #[test]
    fn acquire_succeeds_when_both_claims_are_shared() {
        let now = FixedClock::epoch().now();
        let mut existing = base_lease(now);
        existing.resources = vec![claim(&["src/**"], crate::ticket::ResourceMode::Shared)];
        let view = FixedView(vec![existing]);
        let result = LeaseStore::acquire(
            TicketState::Ready,
            &Authority::root(),
            &view,
            lease_id("cccccccccccc"),
            ticket_id(),
            holder(),
            Authority::none(),
            vec![claim(&["src/x.rs"], crate::ticket::ResourceMode::Shared)],
            now,
            60,
            0,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn heartbeat_bumps_timestamp_when_not_expired() {
        let clock = FixedClock::epoch();
        let mut lease = base_lease(clock.now());
        let later = clock.now().plus_seconds(10);
        LeaseStore::heartbeat(&mut lease, later).expect("should succeed");
        assert_eq!(lease.heartbeat, later);
    }

    #[test]
    fn heartbeat_fails_once_already_expired() {
        let clock = FixedClock::epoch();
        let mut lease = base_lease(clock.now());
        let later = clock.now().plus_seconds(lease.ttl_seconds as i64 + 1);
        let err = LeaseStore::heartbeat(&mut lease, later);
        assert_eq!(err, Err(HeartbeatError::Expired));
    }

    #[test]
    fn expire_due_returns_reversion_action_for_expired_lease() {
        let clock = FixedClock::epoch();
        let lease = base_lease(clock.now());
        let later = clock.now().plus_seconds(lease.ttl_seconds as i64 + 1);
        let view = FixedView(vec![lease.clone()]);
        let actions = LeaseStore::expire_due(&view, later);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].ticket, lease.ticket);
        assert_eq!(actions[0].lease, lease.id);
        assert_eq!(actions[0].reverts_to, TicketState::Ready);
        assert_eq!(actions[0].failure_class, FailureClass::ExecutorCrash);
    }

    #[test]
    fn expire_due_skips_leases_still_within_ttl() {
        let clock = FixedClock::epoch();
        let lease = base_lease(clock.now());
        let view = FixedView(vec![lease]);
        let actions = LeaseStore::expire_due(&view, clock.now().plus_seconds(1));
        assert!(actions.is_empty());
    }

    #[test]
    fn claims_conflict_true_for_overlapping_exclusive_paths() {
        let a = claim(&["src/**"], crate::ticket::ResourceMode::Exclusive);
        let b = claim(&["src/x.rs"], crate::ticket::ResourceMode::Exclusive);
        assert!(claims_conflict(&a, &b));
    }

    #[test]
    fn claims_conflict_false_for_disjoint_paths() {
        let a = claim(&["src/**"], crate::ticket::ResourceMode::Exclusive);
        let b = claim(&["docs/**"], crate::ticket::ResourceMode::Exclusive);
        assert!(!claims_conflict(&a, &b));
    }

    #[test]
    fn claims_conflict_false_for_two_shared_overlapping_claims() {
        let a = claim(&["src/**"], crate::ticket::ResourceMode::Shared);
        let b = claim(&["src/x.rs"], crate::ticket::ResourceMode::Shared);
        assert!(!claims_conflict(&a, &b));
    }

    #[test]
    fn claims_conflict_true_when_one_side_exclusive_overlapping() {
        let a = claim(&["src/**"], crate::ticket::ResourceMode::Exclusive);
        let b = claim(&["src/x.rs"], crate::ticket::ResourceMode::Shared);
        assert!(claims_conflict(&a, &b));
    }
}
