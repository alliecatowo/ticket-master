//! Shared fixtures for the `tm-e2e` integration suite.
//!
//! Every test in this crate is offline and deterministic: a [`tm_types::FixedClock`] stands in
//! for wall time, [`tm_types::CounterIds`] for randomness, and [`tm_provider::MockProvider`] for
//! any model call. Nothing here spawns a thread implicitly or touches the network.
#![allow(dead_code)]

use std::sync::Arc;

use tm_core::{
    ArtifactKind, EvidenceKind, ExecutorRequirements, ResourceClaim, RetryPolicy, Store,
    TicketKind, VerificationPolicy,
};
use tm_types::{
    Authority, Budget, Clock, CounterIds, FixedClock, IdSource, ParticipantId, PatternSet, Role,
    TicketId, Tolerance,
};

/// Open a fresh project under `dir`, with a [`FixedClock`] pinned at the Unix epoch and a
/// freshly seeded [`CounterIds`].
pub fn open_store(dir: &std::path::Path) -> (Arc<FixedClock>, Store) {
    let clock = Arc::new(FixedClock::epoch());
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
    let store = Store::open_with(dir, clock_dyn, ids).expect("open store");
    (clock, store)
}

/// The system actor, used wherever a test doesn't care who acted.
pub fn system() -> ParticipantId {
    ParticipantId::system()
}

/// An agent participant distinguished by `name`, for tests that need two distinct actors (e.g.
/// an executor and a different auditor).
pub fn agent(name: &str) -> ParticipantId {
    ParticipantId::new(format!("agent:mock/{name}")).expect("well-formed agent id")
}

/// Minimal executor requirements: a fast coder, no human required, any capability tolerated.
pub fn executor_reqs() -> ExecutorRequirements {
    ExecutorRequirements {
        role: Role::CoderFast,
        human_required: false,
        min_capability: Tolerance::Any,
    }
}

/// A short, bounded retry policy, so tests that exhaust retries don't need many iterations.
pub fn retry_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay_seconds: 1,
        backoff_multiplier: 2.0,
        max_delay_seconds: 60,
    }
}

/// An exclusive resource claim over `paths`.
pub fn exclusive_claim(paths: &[&str]) -> ResourceClaim {
    ResourceClaim {
        paths: PatternSet::parse(paths.iter().copied()).expect("valid glob patterns"),
        mode: tm_core::ResourceMode::Exclusive,
    }
}

/// Create a `Work` ticket with `authority` and no dependencies, then activate it straight
/// through to `Ready` (there is nothing to block on). Returns the new ticket's id.
pub fn ready_ticket(store: &Store, objective: &str, authority: Authority) -> TicketId {
    ready_ticket_with_resources(store, objective, authority, vec![])
}

/// As [`ready_ticket`], but also declaring `resources` the ticket's lease will claim.
pub fn ready_ticket_with_resources(
    store: &Store,
    objective: &str,
    authority: Authority,
    resources: Vec<ResourceClaim>,
) -> TicketId {
    let events = store
        .create_ticket(
            TicketKind::Work,
            objective.to_string(),
            None,
            None,
            authority,
            resources,
            executor_reqs(),
            vec![],
            vec![],
            VerificationPolicy::Single,
            Budget::unlimited(),
            retry_policy(),
            0,
            system(),
        )
        .expect("create_ticket");
    let ticket = TicketId::new(events[0].subject.as_str()).expect("ticket id");
    store.activate(&ticket, system()).expect("activate");
    ticket
}

/// Store a small inline artifact and return its id, for use as submission evidence.
pub fn evidence_artifact(store: &Store) -> tm_types::ArtifactId {
    let events = store
        .store_artifact(
            ArtifactKind::Patch,
            "text/plain".to_string(),
            b"diff --git a/x b/x\n".to_vec(),
            serde_json::json!({}),
            None,
            system(),
        )
        .expect("store_artifact");
    tm_types::ArtifactId::new(events[0].subject.as_str()).expect("artifact id")
}

/// Drive a `Ready` ticket all the way to `Closed`: lease it for `holder`, start work, submit
/// evidence, verify (passing), and audit (passing, by `auditor`, who must differ from `holder`
/// per `SPEC.md` §4.3 rule 6). Returns the lease id that was used.
pub fn close_ticket(
    store: &Store,
    ticket: &TicketId,
    holder: ParticipantId,
    auditor: ParticipantId,
) -> tm_types::LeaseId {
    let events = store
        .acquire_lease(
            ticket,
            holder.clone(),
            Authority::none(),
            vec![],
            60,
            holder.clone(),
        )
        .expect("acquire_lease");
    let lease = tm_types::LeaseId::new(
        events[0]
            .payload
            .as_ticket_leased()
            .expect("ticket_leased payload")
            .lease
            .as_str(),
    )
    .expect("lease id");
    store
        .transition(ticket, tm_core::Trigger::WorkStarted, holder.clone())
        .expect("work started");
    let evidence = evidence_artifact(store);
    store
        .submit(ticket, "done".to_string(), vec![evidence], holder.clone())
        .expect("submit");
    store
        .transition(ticket, tm_core::Trigger::VerificationStarted, system())
        .expect("verification started");
    store
        .verify(ticket, ticket, true, None, system())
        .expect("verify");
    // Record the same `EvidenceKind::CommandOutput`-shaped verification evidence
    // `tm-scheduler`'s automatic verification would attach, so callers reading a `Work`
    // ticket's verification pass rate (`tm_genesis::maturity::evaluate_predicate`) see this
    // ticket's `store.verify(true)` above as a real, countable pass rather than an empty window.
    let verification_artifact_events = store
        .store_artifact(
            ArtifactKind::CommandOutput,
            "text/plain".to_string(),
            b"$ verify\nexit code: 0\n".to_vec(),
            serde_json::json!({ "passed": true }),
            Some(ticket.clone()),
            system(),
        )
        .expect("store_artifact for verification evidence");
    let verification_artifact_id =
        tm_types::ArtifactId::new(verification_artifact_events[0].subject.as_str())
            .expect("artifact id");
    store
        .attach_evidence(
            ticket,
            EvidenceKind::CommandOutput,
            &verification_artifact_id,
            "verification passed".to_string(),
            system(),
        )
        .expect("attach_evidence for verification");
    store
        .audit(
            ticket,
            ticket,
            tm_core::store::AuditOutcome::Passed,
            None,
            auditor,
        )
        .expect("audit");
    lease
}
