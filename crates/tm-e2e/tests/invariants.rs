//! One test per invariant in `SPEC.md` §17, named `invariant_<n>_<snake_case>`.
//!
//! Where an invariant is a pure product promise this suite can exercise directly (state
//! persistence, replay, authority, leases, evidence chains), the test drives real crate APIs
//! end to end. Where an invariant names an architectural property no single runtime assertion
//! can fully capture (invariant 3 in particular), the test asserts the strongest thing that
//! *is* checkable and says so in its own doc comment, rather than pretending otherwise.

mod common;

use std::sync::Arc;

use tempfile::TempDir;
use tm_types::{Authority, Clock, CounterIds, FixedClock, IdSource, ParticipantId, TmError};

/// The project — not any agent or session — is the persistent entity: work done by one
/// "session" (one actor identity, one clock/id source) is fully visible to, and mutable by, a
/// completely different one, because both only ever read/write the same durable log.
#[test]
fn invariant_1_the_project_is_the_persistent_entity() {
    let dir = TempDir::new().expect("tempdir");

    // "Session A": one clock/id source, one actor.
    let ticket = {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = tm_core::Store::open_with(dir.path(), clock, ids).expect("open store");
        common::ready_ticket(&store, "started in session A", Authority::root())
    };
    // Session A's `Store` is now dropped entirely: no session object, no in-memory state,
    // survives it.

    // "Session B": a different clock, a different id source, a different actor, later.
    let clock_b: Arc<dyn Clock> =
        Arc::new(FixedClock::new(tm_types::Timestamp::from_unix_seconds(999)));
    let ids_b: Arc<dyn IdSource> = Arc::new(CounterIds::seeded(7));
    let store_b = tm_core::Store::open_with(dir.path(), clock_b, ids_b).expect("reopen store");
    let view = store_b.view().expect("view");
    assert!(
        view.tickets.contains_key(&ticket),
        "the ticket session A created must be visible to session B, which never ran alongside it"
    );

    // Session B can act on it — the project does not care which session originated it.
    // `common::ready_ticket` already drove the ticket to `Ready` inside session A, so session
    // B's action here is leasing it, not re-activating an already-active ticket.
    let holder = ParticipantId::new("agent:mock/session-b").unwrap();
    store_b
        .acquire_lease(
            &ticket,
            holder.clone(),
            Authority::none(),
            vec![],
            60,
            holder,
        )
        .expect("a later, unrelated session can still act on earlier work");
    assert_eq!(
        store_b.view().unwrap().tickets[&ticket].state,
        tm_core::TicketState::Leased
    );
}

/// All durable truth is derivable from the event log by replay: `Store::rebuild` drops every
/// materialized table and reconstructs the same ticket/lease/milestone state purely from
/// replaying events.
#[test]
fn invariant_2_all_durable_truth_is_derivable_by_replay() {
    let dir = TempDir::new().expect("tempdir");
    let (_clock, store) = common::open_store(dir.path());

    let root = common::ready_ticket(&store, "root", Authority::root());
    common::close_ticket(&store, &root, common::agent("exec"), common::agent("audit"));
    let milestone = store
        .create_milestone(
            "m1".to_string(),
            vec![root.clone()],
            vec![],
            common::system(),
        )
        .unwrap();
    let milestone_id = tm_types::MilestoneId::new(milestone[0].subject.as_str()).unwrap();
    store
        .close_milestone(&milestone_id, common::system())
        .unwrap();

    let before = store.view().expect("view before rebuild");
    store.rebuild().expect("rebuild");
    let after = store.view().expect("view after rebuild");

    assert_eq!(before.tickets, after.tickets);
    assert_eq!(before.milestones, after.milestones);
    assert_eq!(before.graph, after.graph);
}

/// No model decides a transition that software can decide: [`tm_core::machine::transition`] is
/// a plain, synchronous, total function of `(TicketState, Trigger)` — its type alone proves it
/// takes no provider/model input and cannot await one, and it is deterministic under repetition.
/// This is the strongest runtime-checkable proxy for an architectural promise about *which
/// component* decides something; the deeper claim (no code path anywhere ever routes a
/// transition decision through a model) is enforced by code review and the crate boundary
/// (`tm-core` does not depend on `tm-provider` at all — see its `Cargo.toml`), not by a single
/// assertion this suite can make.
#[test]
fn invariant_3_no_model_decides_a_transition_software_can_decide() {
    // The type itself: synchronous, no provider/context parameter, no `Result` wrapping I/O.
    let _proof: fn(
        tm_core::TicketState,
        tm_core::Trigger,
    ) -> Result<tm_core::TicketState, tm_core::InvalidTransition> = tm_core::machine::transition;

    for _ in 0..1000 {
        assert_eq!(
            tm_core::machine::transition(
                tm_core::TicketState::Ready,
                tm_core::Trigger::LeaseAcquired
            ),
            Ok(tm_core::TicketState::Leased),
            "the same (state, trigger) pair must always decide the same way"
        );
    }
}

/// Authority is explicit, scoped, leased, attenuating, revocable and auditable; a child can
/// never exceed its parent (see `authority_e2e.rs` for the full delegation-chain treatment).
#[test]
fn invariant_4_authority_is_attenuating_and_a_child_never_exceeds_its_parent() {
    let root = Authority::root();
    let requested = Authority::root(); // a child asking for literally everything its parent has
    let granted = root
        .attenuate(&requested)
        .expect("asking for exactly the parent's authority is fine");
    assert!(root.contains(&granted));

    // Asking for one bit more than the grantor holds is refused, not silently widened.
    let mut narrow = Authority::none();
    narrow.git.commit = true;
    let mut overreach = Authority::none();
    overreach.git.push = true;
    assert!(narrow.attenuate(&overreach).is_err());
}

/// A dead worker cannot block the project: an expired lease reverts its ticket to `Ready` with
/// authority reverted and exactly one attempt consumed (see `crash_recovery.rs` for the fuller
/// treatment, including repeated-crash and repeated-reported-failure variants).
#[test]
fn invariant_5_a_dead_worker_cannot_block_the_project() {
    let dir = TempDir::new().expect("tempdir");
    let (clock, store) = common::open_store(dir.path());
    let ticket = common::ready_ticket(&store, "work", Authority::root());

    store
        .acquire_lease(
            &ticket,
            common::agent("worker"),
            Authority::none(),
            vec![],
            10,
            common::agent("worker"),
        )
        .unwrap();
    clock.advance_seconds(11);
    let events = store.expire_leases().unwrap();
    assert!(events
        .iter()
        .any(|e| e.kind == tm_events::EventKind::TicketLeaseExpired));
    assert!(events
        .iter()
        .any(|e| e.kind == tm_events::EventKind::AuthorityReverted));

    let t = store.view().unwrap().tickets[&ticket].clone();
    assert_eq!(t.state, tm_core::TicketState::Ready);
    assert_eq!(t.attempts, 1);
}

/// Workers submit evidence; a different executor verifies; a different one audits: `submit`
/// refuses an empty evidence list, and `audit` refuses an auditor who is also the ticket's
/// executor.
#[test]
fn invariant_6_workers_submit_evidence_and_a_different_executor_verifies_and_audits() {
    let dir = TempDir::new().expect("tempdir");
    let (_clock, store) = common::open_store(dir.path());
    let ticket = common::ready_ticket(&store, "work", Authority::root());

    let err = store
        .submit(&ticket, "done".to_string(), vec![], common::system())
        .unwrap_err();
    assert!(
        matches!(err, TmError::Invariant(_)),
        "a submission must carry evidence"
    );

    let worker = common::agent("worker");
    store
        .acquire_lease(
            &ticket,
            worker.clone(),
            Authority::none(),
            vec![],
            60,
            worker.clone(),
        )
        .unwrap();
    store
        .transition(&ticket, tm_core::Trigger::WorkStarted, worker.clone())
        .unwrap();
    let evidence = common::evidence_artifact(&store);
    store
        .submit(&ticket, "done".to_string(), vec![evidence], worker.clone())
        .unwrap();
    store
        .transition(
            &ticket,
            tm_core::Trigger::VerificationStarted,
            common::system(),
        )
        .unwrap();
    store
        .verify(&ticket, &ticket, true, None, common::system())
        .unwrap();

    // The worker who executed the ticket cannot also audit it.
    let self_audit = store
        .audit(
            &ticket,
            &ticket,
            tm_core::store::AuditOutcome::Passed,
            None,
            worker.clone(),
        )
        .unwrap_err();
    assert!(
        matches!(self_audit, TmError::Invariant(_)),
        "a worker must never certify itself"
    );

    // A different participant can.
    store
        .audit(
            &ticket,
            &ticket,
            tm_core::store::AuditOutcome::Passed,
            None,
            common::agent("someone-else"),
        )
        .expect("a distinct auditor may pass the audit");
    assert_eq!(
        store.view().unwrap().tickets[&ticket].state,
        tm_core::TicketState::Closed
    );
}

/// Expensive commands run once; their full output is durable and queryable:
/// [`tm_context::command::run`] serves a second call for the same cache key from the cache
/// without invoking the executor again, and [`tm_core::artifact`]-backed output stays readable
/// via [`tm_context::command::CommandResult::query`] without re-running anything.
#[test]
fn invariant_7_expensive_commands_run_once_and_their_output_is_queryable() {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tm_context::command::{
        ArtifactStream, CommandCache, CommandExecutor, CommandResult, CommandSpec,
        ExecutionOutcome, Query, QueryAnswer,
    };
    use tm_types::ArtifactId;

    struct FakeCache {
        ids: CounterIds,
        results: Mutex<HashMap<String, CommandResult>>,
        bytes: Mutex<HashMap<ArtifactId, Vec<u8>>>,
    }
    impl CommandCache for FakeCache {
        fn get(&self, key: &str) -> tm_types::Result<Option<CommandResult>> {
            Ok(self.results.lock().unwrap().get(key).cloned())
        }
        fn put(
            &self,
            key: &str,
            argv: &[String],
            exit_code: i32,
            started: tm_types::Timestamp,
            completed: tm_types::Timestamp,
            stdout: &[u8],
            stderr: &[u8],
        ) -> tm_types::Result<CommandResult> {
            let stdout_artifact =
                ArtifactId::new(self.ids.next(tm_types::IdKind::Artifact).as_str())?;
            let stderr_artifact =
                ArtifactId::new(self.ids.next(tm_types::IdKind::Artifact).as_str())?;
            self.bytes
                .lock()
                .unwrap()
                .insert(stdout_artifact.clone(), stdout.to_vec());
            self.bytes
                .lock()
                .unwrap()
                .insert(stderr_artifact.clone(), stderr.to_vec());
            let result = CommandResult {
                key: key.to_string(),
                argv: argv.to_vec(),
                exit_code,
                duration_ms: completed.millis_since(started).max(0) as u64,
                stdout_artifact,
                stderr_artifact,
                started,
                completed,
                from_cache: false,
            };
            self.results
                .lock()
                .unwrap()
                .insert(key.to_string(), result.clone());
            Ok(result)
        }
        fn read_artifact(&self, id: &ArtifactId) -> tm_types::Result<Vec<u8>> {
            self.bytes
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| TmError::not_found("artifact", id.as_str()))
        }
    }

    struct CountingExecutor(AtomicUsize);
    impl CommandExecutor for CountingExecutor {
        fn execute(&self, _spec: &CommandSpec) -> tm_types::Result<ExecutionOutcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ExecutionOutcome {
                exit_code: 0,
                stdout: b"line one\nline two\n".to_vec(),
                stderr: vec![],
            })
        }
    }

    let cache = FakeCache {
        ids: CounterIds::new(),
        results: Mutex::new(HashMap::new()),
        bytes: Mutex::new(HashMap::new()),
    };
    let executor = CountingExecutor(AtomicUsize::new(0));
    let clock = FixedClock::epoch();
    let auth = Authority::root();
    let spec = CommandSpec {
        argv: vec!["echo".to_string(), "hi".to_string()],
        cwd: ".".to_string(),
        env_allowlist: vec![],
        declared_inputs: vec![],
        cacheable: true,
        ticket: None,
        session: None,
    };

    let (first, _) = tm_context::command::run(
        &spec,
        "key-1",
        &cache,
        &auth,
        &executor,
        &clock,
        &common::system(),
    )
    .expect("first run executes");
    assert!(!first.from_cache);
    assert_eq!(executor.0.load(Ordering::SeqCst), 1);

    let (second, _) = tm_context::command::run(
        &spec,
        "key-1",
        &cache,
        &auth,
        &executor,
        &clock,
        &common::system(),
    )
    .expect("second run hits the cache");
    assert!(second.from_cache);
    assert_eq!(
        executor.0.load(Ordering::SeqCst),
        1,
        "the expensive command must run at most once per key"
    );
    assert_eq!(first.stdout_artifact, second.stdout_artifact);

    let answer = second
        .query(ArtifactStream::Stdout, &cache, Query::Head(1))
        .expect("stored output must be queryable without re-running the command");
    assert_eq!(answer, QueryAnswer::Lines(vec!["line one".to_string()]));
    assert_eq!(
        executor.0.load(Ordering::SeqCst),
        1,
        "querying stored output must not re-run anything"
    );
}

/// Documentation knows when the facts beneath it changed; human prose is never overwritten:
/// [`tm_docs::reconcile::apply_regeneration`] is the only path back to `Fresh` without a human
/// attestation, and it hard-refuses for anything that isn't `DocMode::Generated`.
#[test]
fn invariant_8_human_prose_is_never_overwritten() {
    use tm_docs::reconcile::apply_regeneration;
    use tm_docs::registry::{DocMode, DocRecord, DocState};

    let mut human_doc = DocRecord::new(
        "readme".to_string(),
        "README.md".to_string(),
        DocMode::Human,
        vec!["README.md".to_string()],
    );
    human_doc.state = DocState::Stale;
    let err =
        apply_regeneration(&mut human_doc, "deadbeef", tm_types::Timestamp::EPOCH).unwrap_err();
    assert!(matches!(err, TmError::AuthorityDenied(_)));
    assert_eq!(
        human_doc.state,
        DocState::Stale,
        "a refused regeneration must not mutate the doc at all"
    );

    let mut generated_doc = DocRecord::new(
        "api-ref".to_string(),
        "docs/api.md".to_string(),
        DocMode::Generated,
        vec!["src/**/*.rs".to_string()],
    );
    generated_doc.state = DocState::Stale;
    apply_regeneration(&mut generated_doc, "deadbeef", tm_types::Timestamp::EPOCH)
        .expect("the system may regenerate a Generated doc's content");
    assert_eq!(generated_doc.state, DocState::Fresh);
}

/// External trackers are mirrors, never the orchestration database: pushing the same projection
/// twice is idempotent (the second push is a no-op that never even calls the adapter), because
/// the mirror's job is to reflect internal truth outward, not to accumulate its own state.
#[tokio::test]
async fn invariant_9_external_trackers_are_mirrors_not_the_database() {
    use tm_mirror::projection::{Projection, ProjectionPolicy};
    use tm_mirror::sync::SyncEngine;
    use tm_mirror::tracker::{RecordingTracker, TrackerCapabilities};

    let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
    let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
    let engine = SyncEngine::new(clock, ids, common::system());
    let tracker = RecordingTracker::new(
        "github",
        TrackerCapabilities {
            parent_child: false,
            arbitrary_states: false,
            milestones: true,
            labels: true,
            comments: true,
            max_body_bytes: 10_000,
        },
    );

    let projection = Projection {
        ticket: tm_types::TicketId::new("T-1").unwrap(),
        title: "do the thing".to_string(),
        body: "objective prose".to_string(),
        state_hint: "open".to_string(),
        labels: vec![],
        milestone: None,
        checklist: vec![],
        degradations: vec![],
    };

    let (link, first_draft) = engine
        .push(&tracker, &projection, None)
        .await
        .expect("first push");
    assert!(
        first_draft.is_some(),
        "a genuinely new projection must push and record an event"
    );
    assert_eq!(tracker.pushed().len(), 1);

    let (_link_again, second_draft) = engine
        .push(&tracker, &projection, Some(&link))
        .await
        .expect("second push, unchanged");
    assert!(
        second_draft.is_none(),
        "pushing an unchanged projection twice must be a no-op"
    );
    assert_eq!(
        tracker.pushed().len(),
        1,
        "the adapter must not be called again for a no-op push"
    );

    let _ = ProjectionPolicy::default_policy();
}

/// Provider exhaustion is routable state, not an exception: [`tm_provider::route::route`]
/// returns an ordinary [`tm_provider::RouteDecision::Wait`] once a candidate's per-minute quota
/// is spent, rather than an `Err`/panic.
#[test]
fn invariant_10_provider_exhaustion_is_routable_state_not_an_exception() {
    use tm_provider::{route, FabricEvent, FabricState, ModelId, Need, RoleTable, RouteDecision};
    use tm_types::{Role, Tolerance};

    let table = RoleTable::parse(
        r#"
        [coder_fast]
        candidates = [
          { provider = "a", model = "only", max_concurrency = 5, limits = { requests_per_minute = 1 } },
        ]
        "#,
    )
    .expect("valid providers.toml fixture");

    let now = tm_types::Timestamp::from_unix_seconds(1_000);
    let mut state = FabricState::new();
    let candidate = ModelId::new("a", "only");
    state.register(
        candidate.clone(),
        now,
        3,
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(30),
        0.3,
    );

    let need = Need {
        tolerance: Tolerance::Any,
        estimated_tokens: 10,
        max_cost_micros: None,
    };
    assert_eq!(
        route(&table, &state, Role::CoderFast, &need, now),
        RouteDecision::Use(candidate.clone())
    );

    state.apply(
        FabricEvent::RequestStarted {
            candidate: candidate.clone(),
        },
        now,
    );
    state.apply(
        FabricEvent::RequestSucceeded {
            candidate: candidate.clone(),
            latency: std::time::Duration::from_millis(10),
            usage: tm_provider::Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            cost_micros: None,
        },
        now,
    );

    // The single candidate's 1-request-per-minute quota is now spent: routing again must
    // produce ordinary data describing when to retry, not an error and not a panic.
    let decision = route(&table, &state, Role::CoderFast, &need, now);
    assert!(
        matches!(decision, RouteDecision::Wait(_)),
        "an exhausted sole candidate must route to Wait, not fail: {decision:?}"
    );
}

/// Harness changes are benchmarked, promoted as epochs, and never mutate a live session: a
/// [`tm_harness::SessionPin`] taken before a promotion still resolves to the pre-promotion
/// epoch afterward, because promotion only ever appends a new epoch — it never mutates one a
/// live session already pinned.
#[test]
fn invariant_11_harness_changes_are_promoted_as_epochs_never_mutating_a_live_session() {
    use tm_harness::{EpochRegistry, HarnessConfig, HarnessEpoch, PromotionGate, PromotionOutcome};

    let genesis_config = HarnessConfig::default();
    let genesis = HarnessEpoch {
        number: 0,
        config_hash: genesis_config.config_hash(),
        config: genesis_config,
        promoted_at: tm_types::Timestamp::EPOCH,
        promoted_by: common::system(),
        benchmark: None,
    };
    let mut registry = EpochRegistry::new(genesis);

    let session = tm_types::SessionId::new("S-1").unwrap();
    let clock = FixedClock::epoch();
    let pin = registry.pin_current(session, &clock);
    assert_eq!(pin.epoch_number, 0);

    // `EpochRegistry::promote` unconditionally unwraps `benchmark` before consulting the gate
    // (see its source), even when `require_benchmark_gain` is false, so a `None` benchmark
    // panics there regardless of the gate — a bug in `tm-harness` (a crate this suite does not
    // own) worth routing around rather than tripping over: a `Default` report satisfies it
    // without asserting anything about gain-gating, which this test isn't about.
    let gate = PromotionGate {
        require_benchmark_gain: false,
        min_gain: 0.0,
    };
    let outcome = registry
        .promote(
            HarnessConfig::default(),
            tm_types::Timestamp::from_unix_seconds(1),
            common::system(),
            Some(tm_harness::BenchmarkReport::default()),
            &gate,
        )
        .expect("promotion should succeed");
    assert!(matches!(outcome, PromotionOutcome::Promoted(_)));
    assert_eq!(
        registry.current().number,
        1,
        "promotion appends; it does not replace epoch 0"
    );

    // The session pinned before the promotion still resolves to epoch 0, unaffected.
    let resolved = registry
        .resolve(&pin)
        .expect("pinned epoch must still resolve");
    assert_eq!(
        resolved.number, 0,
        "a live session must never observe a later epoch mid-session"
    );
}

/// Sessions are views; a fresh worker can always continue from durable state alone: a brand-new
/// `Store` (standing in for a fresh worker process, with its own clock and id source, no
/// in-memory hand-off from whoever did the earlier work) can immediately act on a ticket a
/// completely different session created.
#[test]
fn invariant_12_sessions_are_views_a_fresh_worker_can_always_continue() {
    let dir = TempDir::new().expect("tempdir");
    let ticket = {
        let (_clock, store) = common::open_store(dir.path());
        common::ready_ticket(&store, "handed off work", Authority::root())
    };

    // A fresh worker: new clock, new id source, no memory of the session that created the
    // ticket, driven purely from what `Store::open_with` reads back off disk.
    let fresh_clock: Arc<dyn Clock> =
        Arc::new(FixedClock::new(tm_types::Timestamp::from_unix_seconds(500)));
    let fresh_ids: Arc<dyn IdSource> = Arc::new(CounterIds::seeded(42));
    let fresh_worker_store = tm_core::Store::open_with(dir.path(), fresh_clock, fresh_ids)
        .expect("fresh worker opens the same project");

    let holder = ParticipantId::new("agent:mock/fresh-worker").unwrap();
    fresh_worker_store
        .acquire_lease(
            &ticket,
            holder.clone(),
            Authority::none(),
            vec![],
            60,
            holder,
        )
        .expect("a fresh worker can act on durable state alone, with no session hand-off");
    assert_eq!(
        fresh_worker_store.view().unwrap().tickets[&ticket].state,
        tm_core::TicketState::Leased
    );
}
