//! [`BuiltinExecutor`] and [`HumanExecutor`]: `tm_core::Executor` implementations a
//! `tm_scheduler::ExecutorDispatcher` can drive (`SPEC.md` §24.3's `builtin` and `human`
//! adapters).
//!
//! `BuiltinExecutor` is the important one: it wraps [`crate::agent_loop::AgentLoop`] and, on
//! every [`Executor::execute`] call, constructs a *fresh* loop whose own authority/budget
//! ceiling is the task's own already-attenuated [`tm_types::Authority`]/[`tm_types::Budget`] —
//! never `Authority::root()`/`Budget::unlimited()`. That is the fix `SPEC.md` audit B-01/B-04
//! asks for: the one path that previously ran an agent (the interactive readline in
//! `tm-cli`) constructed the loop with an unlimited ceiling and relied on
//! `AgentTask::authority`/`budget` alone to narrow it back down on every call; this constructs
//! the ceiling *and* the per-call scope from the same ticket authority, so a future call site
//! that forgets to pass a per-task authority still fails safe instead of defaulting open.

use std::sync::Arc;

use async_trait::async_trait;
use tm_codeintel::CodeIntel;
use tm_context::command::{CommandCache, CommandExecutor};
use tm_core::executor::{
    CostClass, ExecutionHandle, Executor, ExecutorCapabilities, ExecutorFailure, ExecutorOutcome,
    ExecutorTask,
};
use tm_core::{FailureClass, Store};
use tm_provider::fabric::Fabric;
use tm_types::{Clock, IdKind, IdSource, SessionId};

use crate::agent_loop::AgentLoop;
use crate::outcome::{AgentOutcome, AgentTask};
use crate::tools::ToolRegistry;

/// Wrap an [`ExecutorTask::context_pack`]'s rendered text into a single-section
/// `tm_context::ContextPack`, since `tm-core` (where `ExecutorTask` lives) cannot depend on
/// `tm-context`. All of `ContextPack`'s and `Section`'s fields are public, so this is a
/// mechanical wrap, not a re-derivation.
fn wrap_context_pack(rendered: &str) -> tm_context::ContextPack {
    let tokens = tm_context::estimate_tokens_prose(rendered);
    let bytes = rendered.len();
    let section = tm_context::Section {
        kind: tm_context::SectionKind::Objective,
        title: "Executor task".to_string(),
        body: rendered.to_string(),
        tokens,
        bytes,
        provenance: Vec::new(),
    };
    tm_context::ContextPack {
        sections: vec![section],
        tokens,
        bytes,
        provenance: Vec::new(),
        dropped: Vec::new(),
    }
}

/// `tm-agent`'s own loop, exposed as a `tm_core::Executor` a dispatcher can drive generically
/// (`SPEC.md` §24.3's `builtin` adapter — "the reference executor; cheapest, most parallel,
/// fully instrumented").
pub struct BuiltinExecutor {
    id: String,
    fabric: Arc<Fabric>,
    ci: Arc<CodeIntel>,
    store: Arc<Store>,
    command_cache: Arc<dyn CommandCache + Send + Sync>,
    command_executor: Arc<dyn CommandExecutor + Send + Sync>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
}

impl BuiltinExecutor {
    /// Build a `BuiltinExecutor` identified as `id` (used in the dispatcher's
    /// `agent:<id>/<ticket>` lease holder), sharing the given infrastructure across every
    /// [`Executor::execute`] call. A fresh [`ToolRegistry::standard`] and [`AgentLoop`] are
    /// built per call (`ToolRegistry` is not `Clone`, and a fresh loop per run keeps concurrent
    /// executions from sharing mutable prompt-cache state).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        fabric: Arc<Fabric>,
        ci: Arc<CodeIntel>,
        store: Arc<Store>,
        command_cache: Arc<dyn CommandCache + Send + Sync>,
        command_executor: Arc<dyn CommandExecutor + Send + Sync>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
    ) -> Self {
        BuiltinExecutor {
            id: id.into(),
            fabric,
            ci,
            store,
            command_cache,
            command_executor,
            clock,
            ids,
        }
    }

    /// Build the [`AgentLoop`] and [`AgentTask`] `execute` would drive for `task`, without
    /// running anything — the seam a test uses to observe what ceiling the loop was actually
    /// constructed with (see `crate::executor::tests::builtin_executor_passes_ticket_authority`).
    fn build(&self, task: &ExecutorTask) -> (AgentLoop, AgentTask) {
        let session = task
            .session
            .clone()
            .unwrap_or_else(|| session_id_from(self.ids.next(IdKind::Session).as_str()));

        let tools = ToolRegistry::standard(
            self.ci.clone(),
            self.store.clone(),
            self.command_cache.clone(),
            self.command_executor.clone(),
        );
        let agent_loop = AgentLoop::new(
            self.fabric.clone(),
            tools,
            // The loop's own ceiling is the ticket's own authority/budget, not root()/
            // unlimited() — see this module's doc comment.
            task.authority.clone(),
            task.budget,
            self.clock.clone(),
            self.ids.clone(),
            task.role,
            task.actor.clone(),
        );

        let agent_task = AgentTask {
            ticket: task.ticket.clone(),
            context_pack: wrap_context_pack(&task.context_pack),
            authority: task.authority.clone(),
            budget: task.budget,
            harness_epoch: task.harness_epoch,
            session,
        };

        (agent_loop, agent_task)
    }
}

fn session_id_from(rendered: &str) -> SessionId {
    SessionId::new(rendered).unwrap_or_else(|_| {
        SessionId::new("S-0").expect(
            "the literal S-0 matches SessionId's fixed S-<n> numeric format and can never fail",
        )
    })
}

#[async_trait]
impl Executor for BuiltinExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities {
            streaming: false,
            tool_use: true,
            patch_output: false,
            interactive: false,
            accepts_context_pack: true,
            sandboxed: false,
            max_context_tokens: None,
            cost_class: CostClass::Standard,
        }
    }

    async fn execute(&self, task: ExecutorTask) -> tm_types::Result<ExecutorOutcome> {
        let ticket = task.ticket.clone();
        let (mut agent_loop, agent_task) = self.build(&task);

        let outcome = agent_loop.run(agent_task).await?;

        Ok(match outcome {
            AgentOutcome::Submitted { evidence, steps } => {
                let usage = steps
                    .iter()
                    .fold(tm_types::Spend::default(), |acc, step| acc.plus(step.spend));
                ExecutorOutcome {
                    ticket,
                    summary: evidence.summary,
                    evidence: evidence.artifacts,
                    patch: None,
                    usage,
                    decisions: evidence.decisions,
                    failure: None,
                }
            }
            AgentOutcome::BudgetExhausted { steps, exhausted } => {
                let usage = steps
                    .iter()
                    .fold(tm_types::Spend::default(), |acc, step| acc.plus(step.spend));
                ExecutorOutcome {
                    ticket,
                    summary: String::new(),
                    evidence: Vec::new(),
                    patch: None,
                    usage,
                    decisions: Vec::new(),
                    failure: Some(ExecutorFailure {
                        class: FailureClass::BudgetExhausted,
                        detail: format!("budget exhausted: {exhausted:?}"),
                    }),
                }
            }
            AgentOutcome::AwaitingApproval {
                steps,
                pending_call,
            } => {
                // `BuiltinExecutor::execute` runs to completion or failure, never suspends —
                // there is no synchronous human on the far side of a dispatcher-driven run to
                // resume it. `FailureClass::AuthorityDenied` is the closed-vocabulary class
                // closest to what actually happened (the model's action needed an approval
                // this unattended run could not grant); it is retryable
                // (`FailureClass::is_retryable`), so the scheduler's ordinary retry/escalation
                // path gets a chance to route the ticket to a `human_required` recovery ticket
                // rather than the run silently vanishing. A full mid-run approval hand-off to
                // `HumanExecutor` is out of this scope (`SPEC.md` B-07 owns durable
                // `approval.*` events).
                let usage = steps
                    .iter()
                    .fold(tm_types::Spend::default(), |acc, step| acc.plus(step.spend));
                ExecutorOutcome {
                    ticket,
                    summary: String::new(),
                    evidence: Vec::new(),
                    patch: None,
                    usage,
                    decisions: Vec::new(),
                    failure: Some(ExecutorFailure {
                        class: FailureClass::AuthorityDenied,
                        detail: format!(
                            "run suspended awaiting approval for tool {} ({}); no unattended \
                             resolution available",
                            pending_call.tool_name, pending_call.reason
                        ),
                    }),
                }
            }
            AgentOutcome::Failed {
                steps,
                class,
                detail,
            } => {
                let usage = steps
                    .iter()
                    .fold(tm_types::Spend::default(), |acc, step| acc.plus(step.spend));
                ExecutorOutcome {
                    ticket,
                    summary: String::new(),
                    evidence: Vec::new(),
                    patch: None,
                    usage,
                    decisions: Vec::new(),
                    failure: Some(ExecutorFailure { class, detail }),
                }
            }
        })
    }

    async fn cancel(&self, _handle: &ExecutionHandle) -> tm_types::Result<()> {
        // `AgentLoop` has no in-flight cancellation hook today (a run either completes or is
        // dropped with its future); best-effort no-op until one exists.
        Ok(())
    }
}

/// What a human decided when [`HumanApprovalSink::escalate`] returns `Some`: a submission
/// summary plus whatever evidence the sink already stored on the human's behalf (e.g. via
/// `Store::store_artifact`) — a human executor never submits without evidence any more than a
/// model-backed one does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanDecision {
    /// One-line submission summary, as the human phrased it.
    pub summary: String,
    /// Evidence artifacts already stored for this decision.
    pub evidence: Vec<tm_types::ArtifactId>,
}

/// Asks a human to resolve a ticket and waits for the answer, matching [`Executor`] rather than
/// being special-cased by the dispatcher (`SPEC.md` §24.3's `human` adapter).
#[async_trait]
pub trait HumanApprovalSink: Send + Sync {
    /// Present `task` to a human and wait for their decision. `Ok(Some(decision))` means the
    /// human did the work; `Ok(None)` means the human declined; `Err` is an infrastructure
    /// failure (e.g. no terminal attached).
    async fn escalate(&self, task: &ExecutorTask) -> tm_types::Result<Option<HumanDecision>>;
}

/// The dedicated human executor: `execute` opens an escalation via a [`HumanApprovalSink`] and
/// waits on it, rather than the scheduler special-casing `human_required` tickets.
pub struct HumanExecutor {
    id: String,
    sink: Arc<dyn HumanApprovalSink>,
}

impl HumanExecutor {
    /// Build a human executor that escalates through `sink`.
    pub fn new(id: impl Into<String>, sink: Arc<dyn HumanApprovalSink>) -> Self {
        HumanExecutor {
            id: id.into(),
            sink,
        }
    }
}

#[async_trait]
impl Executor for HumanExecutor {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities {
            streaming: false,
            tool_use: false,
            patch_output: false,
            interactive: true,
            accepts_context_pack: true,
            sandboxed: false,
            max_context_tokens: None,
            cost_class: CostClass::Free,
        }
    }

    async fn execute(&self, task: ExecutorTask) -> tm_types::Result<ExecutorOutcome> {
        let ticket = task.ticket.clone();
        match self.sink.escalate(&task).await? {
            Some(decision) => Ok(ExecutorOutcome {
                ticket,
                summary: decision.summary,
                evidence: decision.evidence,
                patch: None,
                usage: tm_types::Spend::default(),
                decisions: Vec::new(),
                failure: None,
            }),
            None => Ok(ExecutorOutcome {
                ticket,
                summary: String::new(),
                evidence: Vec::new(),
                patch: None,
                usage: tm_types::Spend::default(),
                decisions: Vec::new(),
                failure: Some(ExecutorFailure {
                    class: FailureClass::Other,
                    detail: "human declined the escalation".to_string(),
                }),
            }),
        }
    }

    async fn cancel(&self, _handle: &ExecutionHandle) -> tm_types::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_provider::RoleTable;
    use tm_types::{Authority, Budget, CounterIds, FixedClock, ParticipantId, Role, TicketId};

    struct NoopCommandCache;
    impl CommandCache for NoopCommandCache {
        fn get(&self, _key: &str) -> tm_types::Result<Option<tm_context::CommandResult>> {
            Ok(None)
        }
        fn put(
            &self,
            _key: &str,
            _argv: &[String],
            _exit_code: i32,
            _started: tm_types::Timestamp,
            _completed: tm_types::Timestamp,
            _stdout: &[u8],
            _stderr: &[u8],
        ) -> tm_types::Result<tm_context::CommandResult> {
            Err(tm_types::TmError::Provider(
                "not used in this test".to_string(),
            ))
        }
        fn read_artifact(&self, _id: &tm_types::ArtifactId) -> tm_types::Result<Vec<u8>> {
            Err(tm_types::TmError::Provider(
                "not used in this test".to_string(),
            ))
        }
    }

    struct NoopCommandExecutor;
    impl CommandExecutor for NoopCommandExecutor {
        fn execute(
            &self,
            _spec: &tm_context::CommandSpec,
        ) -> tm_types::Result<tm_context::ExecutionOutcome> {
            Err(tm_types::TmError::Provider(
                "not used in this test".to_string(),
            ))
        }
    }

    fn restricted_authority() -> Authority {
        Authority {
            repository: tm_types::RepoAuthority {
                read: tm_types::PatternSet::parse(["src/**"]).unwrap(),
                write: tm_types::PatternSet::parse(["src/only/**"]).unwrap(),
            },
            ..Authority::none()
        }
    }

    fn test_executor(dir: &std::path::Path) -> BuiltinExecutor {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let fabric = Arc::new(Fabric::new(RoleTable::default_table(), clock.clone()));
        let ci = Arc::new(CodeIntel::open(dir).expect("open codeintel"));
        let store = Arc::new(
            tm_core::Store::open_with(dir, clock.clone(), ids.clone()).expect("open store"),
        );
        BuiltinExecutor::new(
            "test-builtin",
            fabric,
            ci,
            store,
            Arc::new(NoopCommandCache),
            Arc::new(NoopCommandExecutor),
            clock,
            ids,
        )
    }

    #[test]
    fn builtin_executor_constructs_the_loop_with_the_tasks_authority_and_budget_not_root_or_unlimited(
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = test_executor(dir.path());

        let restricted = restricted_authority();
        let scoped_budget = Budget::new(1_000, 0, 60);
        let task = ExecutorTask {
            ticket: TicketId::new("T-1").expect("ticket id"),
            role: Role::CoderFast,
            objective: "narrow work".to_string(),
            context_pack: "pack text".to_string(),
            authority: restricted.clone(),
            budget: scoped_budget,
            harness_epoch: 0,
            actor: ParticipantId::new("agent:test-builtin/T-1").expect("participant"),
            session: None,
        };

        let (agent_loop, agent_task) = executor.build(&task);

        assert_eq!(agent_loop.authority(), &restricted);
        assert_ne!(agent_loop.authority(), &Authority::root());
        assert_eq!(agent_loop.budget(), &scoped_budget);
        assert_ne!(agent_loop.budget(), &Budget::unlimited());
        assert_eq!(agent_task.authority, restricted);
        assert_eq!(agent_task.budget, scoped_budget);
    }
}
