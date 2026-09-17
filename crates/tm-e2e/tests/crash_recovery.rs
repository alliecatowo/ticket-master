//! `SPEC.md` §16.9 / §17 invariant 5: a dead worker cannot block the project. A lease that
//! expires without a heartbeat must return its ticket to `Ready` with authority reverted and
//! exactly one attempt consumed, and the ticket must be leasable again afterward.

mod common;

use tempfile::TempDir;
use tm_core::TicketState;
use tm_types::{Authority, TmError};

#[test]
fn an_expired_lease_reverts_the_ticket_to_ready_with_one_attempt_consumed() {
    let dir = TempDir::new().expect("tempdir");
    let (clock, store) = common::open_store(dir.path());
    let ticket = common::ready_ticket(&store, "long-running work", Authority::root());
    let worker = common::agent("worker-a");

    let events = store
        .acquire_lease(
            &ticket,
            worker.clone(),
            Authority::none(),
            vec![],
            30,
            worker.clone(),
        )
        .expect("acquire_lease");
    let lease_id = tm_types::LeaseId::new(
        events[0]
            .payload
            .as_ticket_leased()
            .expect("ticket_leased payload")
            .lease
            .as_str(),
    )
    .expect("lease id");

    let leased = store.view().expect("view").tickets[&ticket].clone();
    assert_eq!(leased.state, TicketState::Leased);
    assert_eq!(
        leased.attempts, 1,
        "the attempt is spent the moment the lease is handed out"
    );

    // The worker vanishes: no heartbeat, no submission, no failure report, just silence past the
    // TTL.
    clock.advance_seconds(31);
    let revert_events = store.expire_leases().expect("expire_leases");
    assert!(
        !revert_events.is_empty(),
        "expiring a live-but-stale lease must produce events"
    );

    let view = store.view().expect("view after expiry");
    let reverted = &view.tickets[&ticket];
    assert_eq!(
        reverted.state,
        TicketState::Ready,
        "a dead worker cannot leave the ticket stuck off the ready set"
    );
    assert_eq!(
        reverted.attempts, 1,
        "expiry must not double-count what leasing already charged"
    );
    // Authority reverted: the sweep must have emitted `authority.reverted` naming the holder.
    let reverted_authority = revert_events.iter().any(|e| {
        e.payload
            .as_authority_reverted()
            .is_some_and(|p| p.subject == worker && p.ticket.as_ref() == Some(&ticket))
    });
    assert!(
        reverted_authority,
        "expiry must revert the authority it granted to the dead worker"
    );

    // A heartbeat against the now-expired lease is refused, not silently accepted: expiry
    // removes the lease from the live set entirely (`materialize`'s handling of
    // `ticket.lease_expired`, so a stale worker can never block a future lease via the
    // double-lease invariant), so the lookup itself fails rather than reporting `LeaseExpired`.
    let err = store.heartbeat(&lease_id, worker.clone()).unwrap_err();
    assert!(matches!(err, TmError::NotFound { .. }));

    // The ticket must be leasable again: a fresh worker can pick it up.
    let second_worker = common::agent("worker-b");
    let second_lease_events = store
        .acquire_lease(
            &ticket,
            second_worker.clone(),
            Authority::none(),
            vec![],
            30,
            second_worker.clone(),
        )
        .expect("ticket must be re-leasable after its previous lease expired");
    assert!(!second_lease_events.is_empty());
    let after_release = store.view().expect("view").tickets[&ticket].clone();
    assert_eq!(after_release.state, TicketState::Leased);
    assert_eq!(
        after_release.attempts, 2,
        "the second attempt is charged when the second lease is handed out"
    );
}

/// Lease expiry alone (a worker that never even reports back) has no attempt ceiling wired into
/// it — `Store::expire_leases` always reverts `Leased`/`Running` straight back to `Ready`
/// (`machine::transition`'s `LeaseExpired` arm), never routing through `Recovery`. That is
/// exactly invariant 5's guarantee: a silently dead worker can *never* block the project, no
/// matter how many times it recurs, because there is no path from "the worker vanished" to a
/// state that refuses a new lease.
#[test]
fn a_worker_that_keeps_vanishing_never_blocks_the_ticket() {
    let dir = TempDir::new().expect("tempdir");
    let (clock, store) = common::open_store(dir.path());
    let ticket = common::ready_ticket(&store, "unlucky work", Authority::root());

    for round in 1..=8u32 {
        let worker = common::agent(&format!("worker-{round}"));
        store
            .acquire_lease(
                &ticket,
                worker.clone(),
                Authority::none(),
                vec![],
                10,
                worker,
            )
            .unwrap_or_else(|e| panic!("round {round}: ticket should still be leasable: {e}"));
        clock.advance_seconds(11);
        store.expire_leases().expect("expire_leases");

        let t = store.view().expect("view").tickets[&ticket].clone();
        assert_eq!(
            t.state,
            TicketState::Ready,
            "round {round}: must always come back to Ready"
        );
        assert_eq!(
            t.attempts, round,
            "round {round}: attempts keep counting, but never gate re-leasing"
        );
    }
}

/// The bounded path: a worker that actually *reports* its failures (rather than just vanishing)
/// is subject to `RetryPolicy::max_attempts` via `Store::record_failure`, and does escalate once
/// it is exhausted (`SPEC.md` §4.3 rule 8).
#[test]
fn repeated_reported_failures_escalate_once_attempts_are_exhausted() {
    let dir = TempDir::new().expect("tempdir");
    let (_clock, store) = common::open_store(dir.path());
    let ticket = common::ready_ticket(&store, "flaky work", Authority::root());

    let mut escalated = false;
    for round in 1..=6u32 {
        let worker = common::agent(&format!("worker-{round}"));
        store
            .acquire_lease(
                &ticket,
                worker.clone(),
                Authority::none(),
                vec![],
                60,
                worker.clone(),
            )
            .expect("acquire_lease");
        store
            .record_failure(
                &ticket,
                tm_core::FailureClass::ExecutorCrash,
                "worker reported a crash".to_string(),
                worker,
            )
            .expect("record_failure");

        // `record_failure` decides `Recovery -> Ready` or `Recovery -> Escalated` in the same
        // call that enters `Recovery` (see `tm-core::store::Store::record_failure`), so the
        // ticket never rests in `Recovery` between calls: it lands on exactly one of these two.
        let state = store.view().expect("view").tickets[&ticket].state;
        match state {
            TicketState::Escalated => {
                escalated = true;
                break;
            }
            TicketState::Ready => continue,
            other => panic!("unexpected state after reported failure #{round}: {other:?}"),
        }
    }
    assert!(
        escalated,
        "a ticket whose failures keep being reported must eventually escalate, not retry forever"
    );

    // Escalated does not accept a new lease; it takes `EscalationResolved` first.
    let err = store
        .acquire_lease(
            &ticket,
            common::agent("late"),
            Authority::none(),
            vec![],
            10,
            common::agent("late"),
        )
        .unwrap_err();
    assert!(matches!(err, TmError::Conflict(_)));
}
