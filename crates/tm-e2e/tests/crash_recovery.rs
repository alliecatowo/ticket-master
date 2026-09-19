//! `SPEC.md` §16.9 / §17 invariant 5: a dead worker cannot block the project. A lease that
//! expires without a heartbeat must return its ticket to `Ready` with authority reverted and
//! exactly one attempt consumed, and the ticket must be leasable again afterward.
//!
//! `SPEC.md` §21.5 / audit B-11: crash recovery restores execution, but must not double-apply an
//! effect that already happened externally, and must not get permanently stuck on one that never
//! did. `effect_ran_but_process_died_before_the_receipt_was_recorded` below is the item 5 case
//! `SPEC.md` §16 promised: an effect journaled but never completed, simulating the crash window
//! between an external write landing and `Store::complete_effect`/`EffectGuard::complete` being
//! called.

mod common;

use tempfile::TempDir;
use tm_core::TicketState;
use tm_mirror::tracker::{ExternalChange, ExternalRef, Tracker, TrackerCapabilities};
use tm_mirror::Projection;
use tm_types::{Authority, TicketId, Timestamp, TmError};

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

/// A test double standing in for `tm_mirror::GitHubTracker`/`LinearTracker`: a `Tracker` whose
/// `confirm` reports a marker it was pre-seeded with, the way a real adapter's `tm-id:<ticket>`
/// label search would after the external write actually landed. `push`/`pull` are never called
/// by this test (the whole point is exercising the *recovery* path, not a fresh push), so they
/// panic if reached.
struct AlreadyPushedTracker {
    external_id: String,
}

impl AlreadyPushedTracker {
    fn new(external_id: &str) -> Self {
        AlreadyPushedTracker {
            external_id: external_id.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Tracker for AlreadyPushedTracker {
    fn name(&self) -> &str {
        "already-pushed"
    }

    fn capabilities(&self) -> TrackerCapabilities {
        TrackerCapabilities {
            parent_child: false,
            arbitrary_states: false,
            milestones: false,
            labels: true,
            comments: false,
            max_body_bytes: 65536,
        }
    }

    async fn push(&self, _projection: &Projection) -> tm_types::Result<ExternalRef> {
        panic!("this test exercises confirm(), not a fresh push")
    }

    async fn pull(&self, _since: Timestamp) -> tm_types::Result<Vec<ExternalChange>> {
        panic!("this test exercises confirm(), not pull")
    }

    async fn confirm(&self, _ticket: &TicketId) -> tm_types::Result<Option<ExternalRef>> {
        Ok(Some(ExternalRef {
            adapter: self.name().to_string(),
            external_id: self.external_id.clone(),
            url: None,
        }))
    }
}

/// `SPEC.md` §21.5 / audit B-11: an effect journaled but never completed is the crash window
/// between an external write landing and the receipt being recorded. `Store::begin_effect`
/// reports this case via `EffectGuard::resumed()`, and each effect kind decides what "correct
/// resume behavior" means for it:
///
/// * a kind with a reliable `confirm()` probe (a `tm_mirror::Tracker` searching by its own
///   durable marker, e.g. GitHub's `tm-id:<ticket>` label) recovers the pre-crash receipt and
///   completes the guard without repeating the external write;
/// * a kind with no such probe (the "generic command" case `SPEC.md` §21.5 and this crate's own
///   `docs/audit-2026-09-18-fable.md` B-11 both call out as accepted behavior) safely re-runs and
///   records a fresh receipt -- correct precisely because re-running a plain command is not
///   destructive the way re-pushing to an external tracker could be.
#[tokio::test]
async fn effect_ran_but_process_died_before_the_receipt_was_recorded() {
    let dir = TempDir::new().expect("tempdir");
    let (_clock, store) = common::open_store(dir.path());
    let ticket = common::ready_ticket(&store, "push a ticket to github", Authority::root());
    let actor = common::agent("worker-a");

    // ---- Case (a): a confirm()-capable effect recovers the pre-crash receipt ----------------
    let key_a = tm_core::EffectKey::compute(&ticket, 0, "mirror.push:github", "projection-hash-1");

    // The effect "runs": journaled, then the process dies before `complete` is ever called.
    let pre_crash = store
        .begin_effect(
            key_a.clone(),
            ticket.clone(),
            0,
            "mirror.push:github",
            actor.clone(),
        )
        .expect("begin_effect before the simulated crash");
    assert!(!pre_crash.already_completed());
    assert!(
        !pre_crash.resumed(),
        "the very first attempt at this key is not a resume"
    );
    drop(pre_crash); // simulated crash: never completed

    // Resume: the same key's `begin_effect` call must report `resumed()` rather than silently
    // starting a second, independent journal entry.
    let resumed = store
        .begin_effect(
            key_a.clone(),
            ticket.clone(),
            0,
            "mirror.push:github",
            actor.clone(),
        )
        .expect("begin_effect on resume");
    assert!(!resumed.already_completed());
    assert!(
        resumed.resumed(),
        "a journaled-but-uncompleted row must be reported as a resume"
    );

    // The adapter's `confirm()` probe finds the external write already landed before the crash
    // (standing in for a real tracker's `tm-id:<ticket>` label search), so the correct resume
    // behavior is to complete with that receipt -- not to push a second, duplicate issue.
    let tracker = AlreadyPushedTracker::new("owner/repo#7");
    let confirmed = tracker
        .confirm(&ticket)
        .await
        .expect("confirm")
        .expect("confirm finds the pre-crash external state");
    resumed
        .complete(&store, Some(&confirmed.external_id))
        .expect("complete from the confirmed receipt");

    let row_a = store
        .effect_status(&key_a)
        .expect("effect_status")
        .expect("row exists");
    assert_eq!(row_a.status, tm_core::EffectStatus::Completed);
    assert_eq!(row_a.receipt_artifact.as_deref(), Some("owner/repo#7"));

    // A subsequent `begin_effect` for the identical key must now short-circuit entirely: this is
    // the actual idempotency guarantee, exercised end to end through the crash window.
    let post_recovery = store
        .begin_effect(
            key_a,
            ticket.clone(),
            0,
            "mirror.push:github",
            actor.clone(),
        )
        .expect("begin_effect after recovery");
    assert!(post_recovery.already_completed());
    assert_eq!(post_recovery.prior_receipt(), Some("owner/repo#7"));

    // ---- Case (b): no confirm probe -> the documented, accepted behavior is to re-run -------
    let key_b = tm_core::EffectKey::compute(&ticket, 0, "shell.run", "argv-hash-xyz");

    let pre_crash_b = store
        .begin_effect(key_b.clone(), ticket.clone(), 0, "shell.run", actor.clone())
        .expect("begin_effect before the simulated crash");
    drop(pre_crash_b); // simulated crash: never completed

    let resumed_b = store
        .begin_effect(key_b.clone(), ticket.clone(), 0, "shell.run", actor.clone())
        .expect("begin_effect on resume");
    assert!(resumed_b.resumed());
    // No confirm surface exists for an arbitrary shell command (mirrors `Tracker::confirm`'s
    // default `Ok(None)` and `tm-agent`'s plain `command::run` call sites, neither of which has
    // one): the correct, documented resume behavior is to just re-run and record a fresh
    // receipt, not to block waiting for a signal that will never come.
    resumed_b
        .complete(&store, Some("rerun-stdout-hash"))
        .expect("complete after re-running");

    let row_b = store
        .effect_status(&key_b)
        .expect("effect_status")
        .expect("row exists");
    assert_eq!(row_b.status, tm_core::EffectStatus::Completed);
    assert_eq!(row_b.receipt_artifact.as_deref(), Some("rerun-stdout-hash"));
}
