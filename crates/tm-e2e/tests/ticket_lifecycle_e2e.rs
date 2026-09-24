//! `SPEC.md` §4.3: a plain worker ticket driven end to end offline — `new -> activate -> run ->
//! submitted -> accept -> closed` — against a scripted (mock) provider, in-process.
//!
//! Nothing here spawns the real `tm` binary: `tm-e2e` has no dependency on `tm-cli`, so
//! `CARGO_BIN_EXE_tm` is not available to it on stable Cargo (that env var is only set for a
//! package that owns the binary target, or a package that depends on it — see `crates/tm-cli/
//! tests/*.rs`, which all live in the crate that defines the `tm` binary itself). Instead this
//! drives the same [`tm_agent::AgentLoop`] worker path `tm run`/`tm sched run` use directly:
//! lease the ticket, transition it to `Running` (mirroring what
//! `tm_scheduler::dispatch::ExecutorDispatcher::dispatch` does before handing a task to
//! `tm_agent::executor::BuiltinExecutor`), then run the loop with a small scripted [`Provider`]
//! registered in its [`Fabric`] in place of a real model.
//!
//! [`ScriptedTicketProvider`] mirrors `crates/tm-cli/src/agent.rs`'s private
//! `ScriptedMockProvider` (the fix for `p1-mock-provider-scripted-tool-calls`, landed e939a0a):
//! a real two-step tool-call script — `artifact.store` an evidence note, then `ticket.submit`
//! citing it — so the ticket actually reaches `Submitted`, not just a text-only reply that
//! leaves it `ready` forever. Kept as a local copy rather than an import because the original is
//! `tm-cli`-private and this crate deliberately never depends on `tm-cli`.

mod common;

use std::sync::Arc;

use tm_agent::{AgentLoop, AgentOutcome, AgentTask, ToolRegistry};
use tm_codeintel::CodeIntel;
use tm_context::command::{
    CommandCache, CommandExecutor, CommandResult, CommandSpec, ExecutionOutcome,
};
use tm_context::ContextPack;
use tm_core::{Store, TicketKind, TicketState, Trigger, VerificationPolicy};
use tm_events::{Event, EventKind, EventLog};
use tm_provider::{
    Candidate, Completion, CompletionRequest, ContentBlock, Fabric, ModelId, Provider,
    ProviderError, RoleTable, StopReason, Usage,
};
use tm_types::{
    ArtifactId, Authority, Budget, Clock, CounterIds, FixedClock, Id, IdKind, IdSource,
    ParticipantId, Result as TmResult, SessionId, Timestamp, TmError,
};

/// A [`CommandCache`] that never has anything cached and refuses to store anything — this
/// test's scripted provider never calls a shell tool, so `run`/`command.*` should never be
/// dispatched at all; a `put`/`read_artifact` call reaching this stub is itself a test failure
/// signal, not a thing to silently accept.
struct NullCommandCache;

impl CommandCache for NullCommandCache {
    fn get(&self, _key: &str) -> TmResult<Option<CommandResult>> {
        Ok(None)
    }

    fn put(
        &self,
        _key: &str,
        _argv: &[String],
        _exit_code: i32,
        _started: Timestamp,
        _completed: Timestamp,
        _stdout: &[u8],
        _stderr: &[u8],
    ) -> TmResult<CommandResult> {
        Err(TmError::Invariant(
            "ticket_lifecycle_e2e's scripted script never runs a shell command".to_string(),
        ))
    }

    fn read_artifact(&self, id: &ArtifactId) -> TmResult<Vec<u8>> {
        Err(TmError::not_found("artifact", id.as_str()))
    }
}

/// A [`CommandExecutor`] that always refuses — see [`NullCommandCache`].
struct NullCommandExecutor;

impl CommandExecutor for NullCommandExecutor {
    fn execute(&self, _spec: &CommandSpec) -> TmResult<ExecutionOutcome> {
        Err(TmError::Invariant(
            "ticket_lifecycle_e2e's scripted script never runs a shell command".to_string(),
        ))
    }
}

/// The `mock`/`m1` provider registered into this test's [`Fabric`]: a deterministic, network-free
/// two-step script keyed on whether the request already carries a tool result naming the
/// artifact this provider created in step 1 — see this file's module doc comment.
struct ScriptedTicketProvider {
    model: ModelId,
    clock: Arc<dyn Clock>,
}

impl ScriptedTicketProvider {
    /// The evidence artifact's id from `req`'s most recent `artifact.store` tool result (its
    /// text is the compact JSON `{"artifact":"A-<n>"}` `ToolName::ArtifactStore` returns), or
    /// `None` before that step has run.
    fn stored_artifact_id(req: &CompletionRequest) -> Option<String> {
        req.messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content),
                _ => None,
            })
            .flat_map(|content| content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => serde_json::from_str::<serde_json::Value>(text).ok(),
                _ => None,
            })
            .find_map(|v| v.get("artifact")?.as_str().map(str::to_string))
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedTicketProvider {
    fn id(&self) -> &str {
        "mock"
    }

    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError> {
        let model = req.model_or(&self.model);
        let content = match Self::stored_artifact_id(&req) {
            None => vec![ContentBlock::ToolUse {
                id: "call-0".to_string(),
                name: "artifact.store".to_string(),
                input: serde_json::json!({
                    "kind": "report",
                    "media_type": "text/plain",
                    "content": "scripted evidence for ticket_lifecycle_e2e, not a real model turn.",
                }),
            }],
            Some(artifact) => vec![ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "ticket.submit".to_string(),
                input: serde_json::json!({
                    "summary": "scripted submission for ticket_lifecycle_e2e, not a real model turn.",
                    "evidence": [artifact],
                }),
            }],
        };
        Ok(Completion {
            model,
            candidates: vec![Candidate {
                content,
                stop_reason: StopReason::ToolUse,
            }],
            usage: Usage {
                input_tokens: 10,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: std::time::Duration::from_millis(0),
            received_at: self.clock.now(),
        })
    }

    async fn embed(
        &self,
        _req: tm_provider::EmbedRequest,
    ) -> Result<tm_provider::Embeddings, ProviderError> {
        Err(ProviderError::MalformedResponse(
            "ticket_lifecycle_e2e's scripted provider does not script embeddings".to_string(),
        ))
    }
}

/// A `coder_fast`-only fabric backed by [`ScriptedTicketProvider`], matching
/// `common::executor_reqs()`'s `Role::CoderFast` and mirroring `tm-cli`'s `build_mock_fabric`.
fn build_scripted_fabric(clock: Arc<dyn Clock>) -> Fabric {
    let table = RoleTable::parse(
        "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
    )
    .expect("this test's own static role table always parses");
    let fabric = Fabric::new(table, clock.clone());
    let model = ModelId::new("mock", "m1");
    fabric.register_provider(Arc::new(ScriptedTicketProvider { model, clock }));
    fabric
}

/// Drive `ticket` (already `Running`, leased by `worker`) through one [`AgentLoop::run`] call
/// under the scripted fabric, and return the outcome.
async fn run_worker_turn(
    store: Arc<Store>,
    ci: Arc<CodeIntel>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
    worker: ParticipantId,
    ticket: &tm_types::TicketId,
) -> AgentOutcome {
    let fabric = Arc::new(build_scripted_fabric(clock.clone()));
    let tools = ToolRegistry::standard(
        ci,
        store.clone(),
        Arc::new(NullCommandCache),
        Arc::new(NullCommandExecutor),
    );
    let session = SessionId::new(ids.next(IdKind::Session).as_str())
        .unwrap_or_else(|_| SessionId::new("S-0").expect("S-0 always parses as a SessionId"));
    let mut agent_loop = AgentLoop::new(
        fabric,
        tools,
        Authority::worker(),
        Budget::unlimited(),
        clock,
        ids,
        tm_types::Role::CoderFast,
        worker,
        store,
    )
    .with_prompt_fragments(tm_agent::worker_fragments(&tm_agent::PromptEnvironment {
        root: ".".to_string(),
        platform: std::env::consts::OS.to_string(),
        date: "2024-01-01".to_string(),
        scope: "repo".to_string(),
        attached_ticket: Some(ticket.to_string()),
    }));

    let task = AgentTask {
        ticket: Some(ticket.clone()),
        context_pack: ContextPack {
            sections: Vec::new(),
            tokens: 0,
            bytes: 0,
            provenance: Vec::new(),
            dropped: Vec::new(),
        },
        authority: Authority::worker(),
        budget: Budget::unlimited(),
        harness_epoch: 0,
        session,
        conversation: None,
    };
    agent_loop.run(task).await.expect("agent loop run")
}

/// The happy path this task is about: `new -> activate -> run -> submitted -> accept -> closed`,
/// with `TicketSubmitted` and `TicketClosed` both present in the ticket's event log by the end —
/// failing loudly (via the `Submitted`/`Closed` assertions below, which print the actual state
/// and outcome) if the lifecycle stalls at `ready`/`escalated` the way the `mock-provider-
/// ticket-submit` probe found before `p1-mock-provider-scripted-tool-calls` landed.
#[tokio::test]
async fn ticket_runs_to_closed_under_a_scripted_provider() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Built directly (not via `common::open_store`) so the same `ids` handle can be shared with
    // `AgentLoop` below, exactly as `tm-cli`'s `Project` shares one `ids` source between
    // `Store::open_with` and `AgentLoop::new` — a separate `CounterIds` for each would still be
    // internally consistent (ids are minted per-`IdKind` counter, so it wouldn't collide), but
    // sharing one matches how every real caller wires this up.
    let clock = Arc::new(FixedClock::epoch());
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
    let store = Store::open_with(dir.path(), clock_dyn.clone(), ids.clone()).expect("open store");
    let store = Arc::new(store);
    let ci = Arc::new(CodeIntel::open(dir.path()).expect("open codeintel over the same tempdir"));

    let worker = common::agent("worker");
    let human = ParticipantId::new("human:test").expect("well-formed human actor");

    let events = store
        .create_ticket(
            TicketKind::Work,
            "drive this ticket to closed under a scripted provider".to_string(),
            None,
            None,
            Authority::worker(),
            vec![],
            common::executor_reqs(),
            vec![],
            vec![],
            VerificationPolicy::Single,
            Budget::unlimited(),
            common::retry_policy(),
            0,
            ParticipantId::system(),
        )
        .expect("create_ticket");
    let ticket = tm_types::TicketId::new(events[0].subject.as_str()).expect("ticket id");

    let state = |store: &Store| {
        store
            .view()
            .expect("view")
            .tickets
            .get(&ticket)
            .expect("ticket exists")
            .state
    };
    assert_eq!(
        state(&store),
        TicketState::Draft,
        "a fresh ticket starts in Draft"
    );

    store
        .activate(&ticket, ParticipantId::system())
        .expect("activate");
    assert_eq!(
        state(&store),
        TicketState::Ready,
        "a dependency-free ticket is Ready right after activation"
    );

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
        .transition(&ticket, Trigger::WorkStarted, worker.clone())
        .expect("work started");
    assert_eq!(
        state(&store),
        TicketState::Running,
        "a leased ticket with work started is Running"
    );

    let outcome = run_worker_turn(store.clone(), ci, clock_dyn.clone(), ids, worker, &ticket).await;
    assert!(
        matches!(outcome, AgentOutcome::Submitted { .. }),
        "a ticket-attached turn under the scripted provider must reach Submitted, not stall at \
         ready/escalated; got {outcome:?}"
    );
    assert_eq!(
        state(&store),
        TicketState::Submitted,
        "the ticket itself must have moved Running -> Submitted; outcome was {outcome:?}"
    );

    store
        .accept(&ticket, None, human)
        .expect("accept the submission");
    assert_eq!(
        state(&store),
        TicketState::Closed,
        "accepting a submitted ticket must close it"
    );

    let db_path = dir.path().join(".tm").join("project.db");
    let log = EventLog::open_with_clock(&db_path, clock_dyn).expect("open event log for readback");
    let ticket_events: Vec<Event> = log
        .read_subject(&Id::from(ticket.clone()))
        .expect("read the ticket's own event log");
    let kinds: Vec<EventKind> = ticket_events.iter().map(|e| e.payload.kind()).collect();
    assert!(
        kinds.contains(&EventKind::TicketSubmitted),
        "the ticket's event log must contain ticket.submitted; got {kinds:?}"
    );
    assert!(
        kinds.contains(&EventKind::TicketClosed),
        "the ticket's event log must contain ticket.closed; got {kinds:?}"
    );
    let submitted_idx = kinds
        .iter()
        .position(|k| *k == EventKind::TicketSubmitted)
        .expect("submitted present");
    let closed_idx = kinds
        .iter()
        .position(|k| *k == EventKind::TicketClosed)
        .expect("closed present");
    assert!(
        submitted_idx < closed_idx,
        "ticket.submitted must precede ticket.closed in the log; got {kinds:?}"
    );
}

/// The `lifecycle-reject-retry` probe, automated: a ticket that exhausts every attempt of its
/// `RetryPolicy` (`common::retry_policy()`'s `max_attempts: 3`) via `Store::record_failure` with
/// a *retryable* failure class (`FailureClass::ExecutorCrash` — `BudgetExhausted` is deliberately
/// non-retryable and would escalate on the very first failure, per `FailureClass::is_retryable`)
/// lands in `Escalated` only once every attempt is spent, not on the first failure. A human's
/// `Store::retry` with guidance must then both append that guidance to the ticket's objective
/// and give it a fresh round of attempts (a non-human actor is refused, per `SPEC.md` §4.3) —
/// mirroring `Store::retry`'s own `a_human_retries_an_escalated_ticket_with_guidance_and_fresh_
/// attempts` unit test, but reaching `Escalated` by exhausting the retry budget rather than via a
/// single non-retryable failure, and driven through `tm-e2e`'s offline fixtures rather than
/// `tm-core`'s internal test helpers.
#[tokio::test]
async fn escalated_ticket_retried_with_guidance_gets_fresh_attempts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_clock, store) = common::open_store(dir.path());
    let worker = common::agent("worker");
    let human = ParticipantId::new("human:owner").expect("well-formed human actor");

    let ticket = common::ready_ticket(
        &store,
        "drive this ticket to escalated by exhausting every attempt",
        Authority::worker(),
    );

    let max_attempts = common::retry_policy().max_attempts;
    let state = |store: &Store| {
        store
            .view()
            .expect("view")
            .tickets
            .get(&ticket)
            .expect("ticket exists")
            .state
    };
    for attempt in 1..=max_attempts {
        assert_eq!(
            state(&store),
            TicketState::Ready,
            "attempt {attempt} must start from Ready"
        );
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
            .transition(&ticket, Trigger::WorkStarted, worker.clone())
            .expect("work started");
        store
            .record_failure(
                &ticket,
                tm_core::FailureClass::ExecutorCrash,
                format!("the executor crashed on attempt {attempt}"),
                ParticipantId::system(),
            )
            .expect("record_failure");
    }
    let before = store.view().expect("view").tickets[&ticket].clone();
    assert_eq!(
        before.state,
        TicketState::Escalated,
        "exhausting every attempt of the retry policy must escalate the ticket, not leave it \
         mid-recovery; ticket was {before:?}"
    );
    assert_eq!(
        before.attempts, max_attempts,
        "the ticket must have spent exactly max_attempts attempts before escalating"
    );

    // Only a human may resolve an escalation (SPEC.md §4.3); an agent actor is refused.
    assert!(
        matches!(
            store
                .retry(&ticket, None, worker.clone())
                .expect_err("an agent actor must not be able to retry an escalated ticket"),
            TmError::AuthorityDenied(_)
        ),
        "a non-human actor retrying an escalated ticket must be refused with AuthorityDenied"
    );
    assert_eq!(
        state(&store),
        TicketState::Escalated,
        "a refused retry attempt must not change the ticket's state"
    );

    store
        .retry(
            &ticket,
            Some("focus on the retry path, not the happy path".to_string()),
            human,
        )
        .expect("a human's retry with guidance must succeed");
    let after = store.view().expect("view").tickets[&ticket].clone();
    assert_eq!(
        after.state,
        TicketState::Ready,
        "a dependency-free ticket retried by a human must land back in Ready"
    );
    assert!(
        after.objective.starts_with(&before.objective),
        "retry must append guidance to the existing objective, not replace it; got \
         {}",
        after.objective
    );
    assert!(
        after.objective.ends_with(&format!(
            "From the user, after attempt {}: focus on the retry path, not the happy path",
            before.attempts
        )),
        "the guidance text must be visible in the ticket's objective; got {}",
        after.objective
    );
    assert_eq!(
        after.retry.max_attempts,
        before.attempts + before.retry.max_attempts,
        "a retry must give the ticket a fresh round of attempts on top of what it already spent"
    );
}

/// The `deps-sched` probe, automated: a `Hard` dependency edge keeps the dependent ticket out of
/// `tm_scheduler::plan`'s `Lease` actions while the dependency is open, and `plan` only offers to
/// lease it once the dependency closes — asserted against `tm_scheduler::plan` itself (the same
/// pure function `SchedulerLoop::tick`/`tm sched tick` call), not just the dependent ticket's own
/// `TicketState`.
#[tokio::test]
async fn hard_dependency_blocks_scheduler_lease_until_dependency_closes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (clock, store) = common::open_store(dir.path());

    let dependency = common::ready_ticket(
        &store,
        "the dependency this test's dependent ticket blocks on",
        Authority::worker(),
    );
    let dependent_events = store
        .create_ticket(
            TicketKind::Work,
            "a ticket that must wait for its Hard dependency to close".to_string(),
            None,
            None,
            Authority::worker(),
            vec![],
            common::executor_reqs(),
            vec![],
            vec![],
            VerificationPolicy::Single,
            Budget::unlimited(),
            common::retry_policy(),
            0,
            common::system(),
        )
        .expect("create_ticket");
    let dependent =
        tm_types::TicketId::new(dependent_events[0].subject.as_str()).expect("ticket id");
    store
        .add_dependency(
            &dependent,
            &dependency,
            tm_core::DependencyKind::Hard,
            common::system(),
        )
        .expect("add_dependency");
    store
        .activate(&dependent, common::system())
        .expect("activate never errors on an unsatisfied dependency; it just stops short of Ready");
    assert_eq!(
        store.view().expect("view").tickets[&dependent].state,
        TicketState::Blocked,
        "a ticket activated with an open Hard dependency must land in Blocked, not Ready"
    );

    let mut policy = tm_scheduler::SchedulingPolicy::conservative_default();
    policy.available_roles = [tm_types::Role::CoderFast].into_iter().collect();
    let now = clock.now();

    let view = store.scheduler_view().expect("scheduler_view");
    let actions = tm_scheduler::plan(&view, now, &policy);
    assert!(
        !actions.iter().any(
            |a| matches!(a, tm_scheduler::SchedulerAction::Lease { ticket, .. } if *ticket == dependent)
        ),
        "the scheduler must never plan to lease a ticket whose Hard dependency is still open; \
         got {actions:?}"
    );

    // Close the dependency the ordinary way: lease it, submit evidence, verify and audit it.
    let auditor = common::agent("auditor");
    common::close_ticket(&store, &dependency, common::agent("worker"), auditor);
    assert_eq!(
        store.view().expect("view").tickets[&dependency].state,
        TicketState::Closed,
        "close_ticket must have closed the dependency"
    );

    // One tick to notice the now-satisfied dependency and mark the dependent Ready...
    let view = store.scheduler_view().expect("scheduler_view");
    let actions = tm_scheduler::plan(&view, now, &policy);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, tm_scheduler::SchedulerAction::MarkReady(t) if *t == dependent)),
        "once its Hard dependency closes, the scheduler must plan to mark the dependent ticket \
         Ready; got {actions:?}"
    );
    store
        .transition(&dependent, Trigger::DependenciesSatisfied, common::system())
        .expect("DependenciesSatisfied always succeeds once the graph agrees");
    assert_eq!(
        store.view().expect("view").tickets[&dependent].state,
        TicketState::Ready,
        "the dependent ticket must now be Ready"
    );

    // ...and a second tick, over the now-Ready state, must offer to lease it.
    let view = store.scheduler_view().expect("scheduler_view");
    let actions = tm_scheduler::plan(&view, now, &policy);
    assert!(
        actions.iter().any(
            |a| matches!(a, tm_scheduler::SchedulerAction::Lease { ticket, .. } if *ticket == dependent)
        ),
        "a Ready ticket with its dependency satisfied must now be leasable by the scheduler; \
         got {actions:?}"
    );
}
