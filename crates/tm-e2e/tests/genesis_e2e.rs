//! `SPEC.md` §16.10: a seed prompt driven through Genesis's full stage machine — vision, spec,
//! graph compilation, ignition, (this suite's own) scheduling/execution/verification/audit to a
//! closed milestone, then the maturity gate and authority reconvergence to steady state — all
//! offline and deterministic under a [`tm_types::FixedClock`].
//!
//! `GenesisDriver` itself only ever calls a provider for `Seed`/`Vision`/`Spec`/
//! `GraphCompilation`/`MaturityGate`; every other stage is mechanical. This test scripts each
//! provider-backed stage by *probing* it first: calling `advance` once with nothing scripted
//! (which fails with `TmError::Provider` and logs the exact request the stage sent), then
//! scripting a schema-correct response for that logged request and calling `advance` again. That
//! sidesteps needing to reimplement each stage's private prompt-building function to predict its
//! request hash, and stays correct even if a stage's prompt text changes.
//!
//! `Stage::V0`/`Evaluation`/`V1`/`Stabilization` are themselves mechanical stubs in
//! `tm-genesis` today (they do not themselves drive any ticket to completion) — the actual
//! scheduling/execution/verification/audit this test's docstring promises is driven directly
//! against the same `tm_core::Store` the driver uses, between the `GraphCompilation` and
//! `MaturityGate` stages, so that by the time the maturity gate evaluates real project state it
//! reflects genuinely closed work rather than an empty project.
//!
//! **CLI-level provider resolution path**: These tests drive `GenesisDriver` in-process without
//! going through the CLI's provider resolution (`project::resolve_genesis_provider`). The
//! CLI-level path is regression-tested separately in `crates/tm-cli/tests/genesis_offline.rs`
//! (`tm_genesis_runs_offline_end_to_end_through_the_real_binary`), which proves the real binary
//! exercises the mock-provider check that was added in `genesis-cli-wire-mock-provider`.

mod common;

use std::sync::Arc;

use tm_core::{MilestoneState, TicketKind};
use tm_genesis::{GenesisDriver, GenesisState, Stage};
use tm_provider::{Candidate, Completion, ContentBlock, MockProvider, ModelId, StopReason, Usage};
use tm_types::{
    Authority, Budget, Clock, CounterIds, FixedClock, ParticipantId, Result as TmResult, TmError,
};

fn completion_with(model: &ModelId, now: tm_types::Timestamp, text: &str) -> Completion {
    Completion {
        model: model.clone(),
        candidates: vec![Candidate {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: StopReason::EndTurn,
        }],
        usage: Usage::default(),
        latency: std::time::Duration::ZERO,
        received_at: now,
    }
}

/// Advance `state` by one stage, scripting whatever provider call that stage makes (if any) with
/// `response`. Stages that don't call a provider at all (`Ignition`, `V0`, ...) succeed on the
/// first try and `response` is simply unused.
async fn advance_stage(
    driver: &GenesisDriver<'_>,
    state: &GenesisState,
    actor: &ParticipantId,
    provider: &MockProvider,
    model: &ModelId,
    clock: &FixedClock,
    response: &str,
) -> GenesisState {
    // `GenesisDriver::resume` picks the snapshot with the strictly greatest `updated` timestamp
    // (ties keep whichever it saw first, in `ArtifactId` order, not creation order) — advancing
    // between stages keeps every snapshot's timestamp distinct and `resume` deterministic, the
    // way a real `SystemClock` naturally would.
    clock.advance_seconds(1);
    match driver.advance(state, actor.clone()).await {
        Ok(next) => next,
        Err(TmError::Provider(_)) => {
            let req = provider
                .call_log()
                .last()
                .cloned()
                .expect("a Provider error implies the request was logged");
            provider.script_response(&req, completion_with(model, clock.now(), response));
            driver
                .advance(state, actor.clone())
                .await
                .expect("stage should succeed once its provider call is scripted")
        }
        Err(other) => panic!("unexpected error advancing {:?}: {other}", state.stage),
    }
}

fn vision_response() -> &'static str {
    r#"{
        "product_thesis": "A tiny offline ticket tracker.",
        "user_experience": "Predictable and boring.",
        "taste": "No surprises.",
        "governing_constraints": ["stays offline"],
        "identity": "A minimal ticket tracker for a single project.",
        "non_goals": ["multi-tenant SaaS"],
        "architectural_character": "boring and auditable",
        "spiritually_wrong": ["a hidden queue nobody can inspect"]
    }"#
}

fn spec_response() -> &'static str {
    r#"{
        "requirements": [
            {"id": "R1", "text": "tickets persist across restarts", "priority": 0}
        ],
        "architecture": "single embedded store",
        "interfaces": [{"name": "CLI", "description": "create and close tickets"}],
        "data_model": "a ticket has an id, objective and state",
        "technology_choices": [
            {"area": "storage", "choice": "sqlite", "rationale": "simple and embedded"}
        ],
        "quality_bar": "green CI",
        "security_model": "single local user, no auth",
        "testing_strategy": "unit tests per module",
        "milestones": [
            {"title": "v1", "objective": "ship the core loop", "scope_hint": "everything"}
        ],
        "v0": {
            "objective": "a ticket can be created and closed",
            "exit_criteria": [{"tests_pass": {"suite": null}}]
        },
        "v1": {
            "objective": "the core loop is solid",
            "exit_criteria": [{"tests_pass": {"suite": null}}]
        }
    }"#
}

/// Two independent `Work` tickets under one milestone titled `"v1"`. One milestone doing double
/// duty as both V0 and V1 is itself a documented approximation in `tm-genesis::stages` (used
/// whenever a project has only one milestone), not a shortcut invented by this test.
fn graph_compilation_response() -> String {
    use tm_core::ticket::{ExecutorRequirements, RetryPolicy, VerificationPolicy};

    #[derive(serde::Serialize)]
    struct Ticket {
        ticket_ref: &'static str,
        kind: TicketKind,
        objective: &'static str,
        parent_ref: Option<&'static str>,
        milestone_ref: Option<&'static str>,
        authority: Authority,
        resources: Vec<()>,
        executor: ExecutorRequirements,
        context_refs: Vec<()>,
        success: Vec<()>,
        verification: VerificationPolicy,
        budget: Budget,
        retry: RetryPolicy,
        priority: i32,
    }
    #[derive(serde::Serialize)]
    struct Milestone {
        milestone_ref: &'static str,
        title: &'static str,
        ticket_refs: Vec<&'static str>,
    }

    fn ticket(ticket_ref: &'static str, objective: &'static str) -> Ticket {
        Ticket {
            ticket_ref,
            kind: TicketKind::Work,
            objective,
            parent_ref: None,
            milestone_ref: Some("v1"),
            authority: Authority::root(),
            resources: vec![],
            executor: common::executor_reqs(),
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: common::retry_policy(),
            priority: 0,
        }
    }

    let tickets = vec![
        ticket("core", "implement the core ticket loop"),
        ticket("polish", "polish the CLI output"),
    ];
    let milestones = vec![Milestone {
        milestone_ref: "v1",
        title: "v1",
        ticket_refs: vec![],
    }];

    serde_json::to_string(&serde_json::json!({
        "tickets": tickets,
        "dependencies": Vec::<()>::new(),
        "milestones": milestones,
        "authority_domains": Vec::<()>::new(),
    }))
    .expect("graph compilation fixture serializes")
}

fn maturity_response() -> &'static str {
    r#"{"mature": true, "rationale": "V1 is closed and the sole change made it end to end."}"#
}

#[tokio::test]
async fn seed_to_steady_state_offline_and_deterministic() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let (clock, store): (Arc<FixedClock>, tm_core::Store) = common::open_store(dir.path());
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let model = ModelId::new("mock", "genesis-1");
    let provider = MockProvider::new("mock", model.clone(), clock_dyn.clone());
    let driver_ids = CounterIds::new();
    let actor = ParticipantId::system();

    let driver = GenesisDriver::new(&store, &provider, clock.as_ref(), &driver_ids);
    let mut state = GenesisState::new("demo-project".to_string(), clock.as_ref());
    assert_eq!(state.stage, Stage::Seed);

    // Seed: any well-formed (possibly empty) JSON object satisfies `analyze_prompt`'s schema.
    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        "{}",
    )
    .await;
    assert_eq!(state.stage, Stage::Vision);

    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        vision_response(),
    )
    .await;
    assert_eq!(state.stage, Stage::Spec);

    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        spec_response(),
    )
    .await;
    assert_eq!(state.stage, Stage::GraphCompilation);

    let graph_json = graph_compilation_response();
    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        &graph_json,
    )
    .await;
    assert_eq!(state.stage, Stage::Ignition);

    // Drive the committed graph through real scheduling/execution/verification/audit, entirely
    // via `tm_core::Store` (the same store the driver committed the graph to), before the
    // maturity gate looks at project state.
    run_committed_work_to_a_closed_milestone(&store, &actor);

    // Ignition/V0/Evaluation/V1/Stabilization call no provider at all.
    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        "",
    )
    .await;
    assert_eq!(state.stage, Stage::V0);
    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        "",
    )
    .await;
    assert_eq!(state.stage, Stage::Evaluation);
    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        "",
    )
    .await;
    assert_eq!(state.stage, Stage::V1);
    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        "",
    )
    .await;
    assert_eq!(state.stage, Stage::Stabilization);
    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        "",
    )
    .await;
    assert_eq!(state.stage, Stage::MaturityGate);

    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        maturity_response(),
    )
    .await;
    assert_eq!(
        state.stage,
        Stage::AuthorityReconvergence,
        "the maturity gate must pass given a closed v1 milestone, an end-to-end patch and no open audits"
    );

    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        "",
    )
    .await;
    assert_eq!(state.stage, Stage::SteadyState);

    // The whole run is offline: not one real network-shaped call happened, only scripted ones.
    assert!(!provider.call_log().is_empty());

    let view = store.view().expect("final view");
    assert!(
        view.milestones
            .values()
            .any(|m| m.state == MilestoneState::Closed),
        "the v1 milestone must have actually closed"
    );
    assert!(tm_core::check_invariants(&view).is_empty());

    // Resuming from durable state alone (SPEC.md §17 invariant 12) must reproduce the same
    // terminal stage without redoing any stage.
    let resumed = GenesisDriver::resume(&store).expect("resume");
    assert_eq!(resumed.stage, Stage::SteadyState);
}

fn run_committed_work_to_a_closed_milestone(store: &tm_core::Store, actor: &ParticipantId) {
    let view = store.view().expect("view after graph compilation");
    let milestone = view
        .milestones
        .values()
        .find(|m| m.title == "v1")
        .expect("the graph compilation stage should have committed a \"v1\" milestone")
        .clone();
    assert_eq!(
        milestone.tickets.len(),
        2,
        "both committed tickets should belong to the milestone"
    );

    for (i, ticket) in milestone.tickets.iter().enumerate() {
        store.activate(ticket, actor.clone()).expect("activate");
        assert_eq!(
            store.view().unwrap().tickets[ticket].state,
            tm_core::TicketState::Ready,
            "a dependency-free committed ticket must activate straight to Ready"
        );
        let holder = common::agent(&format!("genesis-worker-{i}"));
        let auditor = common::agent(&format!("genesis-auditor-{i}"));
        common::close_ticket(store, ticket, holder, auditor);
    }

    store
        .close_milestone(&milestone.id, actor.clone())
        .expect("every member ticket is Closed, so the milestone must be closeable");
}

/// Documents that `resume` from a stage before the terminal one also reproduces the in-flight
/// stage, not just the final one — a fresh `GenesisDriver` (standing in for a fresh worker after
/// a crash) can always continue from durable state alone.
#[tokio::test]
async fn resume_reproduces_an_in_flight_stage() -> TmResult<()> {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let (clock, store): (Arc<FixedClock>, tm_core::Store) = common::open_store(dir.path());
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let model = ModelId::new("mock", "genesis-1");
    let provider = MockProvider::new("mock", model.clone(), clock_dyn);
    let driver_ids = CounterIds::new();
    let actor = ParticipantId::system();
    let driver = GenesisDriver::new(&store, &provider, clock.as_ref(), &driver_ids);

    let mut state = GenesisState::new("resumable-project".to_string(), clock.as_ref());
    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        "{}",
    )
    .await;
    state = advance_stage(
        &driver,
        &state,
        &actor,
        &provider,
        &model,
        clock.as_ref(),
        vision_response(),
    )
    .await;
    assert_eq!(state.stage, Stage::Spec);

    // A fresh driver over the same store, standing in for a new process after a crash.
    let resumed = GenesisDriver::resume(&store)?;
    assert_eq!(resumed.stage, Stage::Spec);
    assert_eq!(resumed.vision, state.vision);
    Ok(())
}
