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
