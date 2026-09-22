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

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tm_codeintel::CodeIntel;
use tm_context::command::{CommandCache, CommandExecutor};
use tm_core::executor::{
    CostClass, ExecutionHandle, Executor, ExecutorCapabilities, ExecutorFailure, ExecutorOutcome,
    ExecutorTask,
};
use tm_core::{ArtifactKind, FailureClass, Store};
use tm_provider::fabric::Fabric;
use tm_types::{CapabilityProvider, Clock, IdKind, IdSource, Oversight, ParticipantId, SessionId};

use crate::agent_loop::AgentLoop;
use crate::outcome::{AgentOutcome, AgentTask};
use crate::tools::ToolRegistry;

/// Oversized `tm-browser` tool outputs (screenshots, network bodies, PDFs) at or above this size
/// are spilled to a stored artifact instead of inlined — the same threshold
/// [`crate::tools::MAX_INLINE_RESULT_BYTES`] uses for the builtin capability's own results, kept
/// as a separate constant because `tm_browser::session::BrowserSessionConfig` takes it as a raw
/// `usize` rather than importing this crate's constant.
const BROWSER_ARTIFACT_THRESHOLD_BYTES: usize = crate::tools::MAX_INLINE_RESULT_BYTES;

/// A [`tm_browser::session::ArtifactSink`] backed by `tm-core`'s `Store`, for wiring
/// [`tm_browser::BrowserCapability`] to durable storage the same way
/// [`crate::tools::bound_result`] does for the builtin capability's own oversized results.
///
/// Every artifact is stored under [`ParticipantId::system`] rather than the specific ticket/actor
/// a browser tool call happened on behalf of: `ArtifactSink::store` carries no ticket/actor
/// context (a `BrowserSession`'s sink is bound once at session-launch time, not threaded through
/// every call), and `Store::store_artifact` requires one. This is a real simplification, not an
/// oversight — an artifact stored this way is attributable to "the browser capability" but not to
/// the exact ticket that triggered it. Fixing that would need `ArtifactSink::store` itself to
/// grow ticket/actor parameters, a `tm-browser` API change out of this task's scope.
pub struct StoreArtifactSink {
    store: Arc<Store>,
}

impl StoreArtifactSink {
    /// Build a sink writing into `store`.
    pub fn new(store: Arc<Store>) -> Self {
        StoreArtifactSink { store }
    }
}

impl tm_browser::session::ArtifactSink for StoreArtifactSink {
    fn store(&self, bytes: &[u8], content_type: &str) -> tm_types::Result<tm_types::ArtifactId> {
        let events = self.store.store_artifact(
            ArtifactKind::Report,
            content_type.to_string(),
            bytes.to_vec(),
            serde_json::json!({"source": "browser"}),
            None,
            ParticipantId::system(),
        )?;
        events
            .iter()
            .find_map(|e| e.payload.as_artifact_created().map(|p| p.artifact.clone()))
            .ok_or_else(|| {
                tm_types::TmError::invariant("store_artifact did not emit artifact.created")
            })
    }
}

/// What [`BuiltinExecutor`] needs to register a [`tm_browser::BrowserCapability`] per dispatched
/// task: an already-constructed provider registry (from `browser.toml`) and an artifact sink.
/// `BuiltinExecutor::browser` being `None` means the project has no `browser.toml` configured (or
/// the binary assembling this executor chose not to load one) — browser tools are then simply
/// never registered for any task this executor runs, rather than every dispatch failing.
pub struct BrowserWiring {
    /// The provider fallback order `browser.toml` selected (`SPEC.md` §19.1a).
    pub providers: Arc<tm_browser::ProviderRegistry>,
    /// Where oversized browser tool outputs are stored; [`StoreArtifactSink`] is the ready-made
    /// choice when the caller already has this executor's own `Store`.
    pub sink: Arc<dyn tm_browser::session::ArtifactSink>,
}

/// What [`BuiltinExecutor`] needs to register a [`tm_computer::ComputerCapability`] per
/// dispatched task. Unlike [`BrowserWiring`], this is never optional: unlike a browser session, a
/// computer session needs no project-level config file to be worth registering — an unsupported
/// or permission-less backend fails clearly on first tool call
/// (`tm_computer::ComputerError::BackendUnavailable`/`PermissionMissing`) rather than at
/// construction, so there is no "silently no computer tools at all" state to choose here the way
/// there is for a missing `browser.toml`.
#[derive(Clone)]
pub struct ComputerWiring {
    /// Backend selection signals (`WAYLAND_DISPLAY`/`DISPLAY`/`TM_COMPUTER_BACKEND`), mirroring
    /// `tm computer`'s own `SelectionEnv::from_process`.
    pub env: tm_computer::SelectionEnv,
    /// Whether to request a headless (`Xvfb`) session — Linux only, per `SPEC.md` §20.3.
    pub headless: bool,
    /// The panic-stop policy (`SPEC.md` §20.3) attended sessions are configured with. Note this
    /// module's doc comment on why the panic stop is not actually polled on this path yet.
    pub panic_stop: tm_computer::session::PanicStopConfig,
}

impl Default for ComputerWiring {
    /// The same defaults `tm computer`'s CLI dispatcher uses (`crates/tm-cli/src/drive.rs`):
    /// backend selection from the real process environment, attended (not headless), a 5px
    /// panic-stop threshold and no configured abort chord.
    fn default() -> Self {
        ComputerWiring {
            env: tm_computer::SelectionEnv::from_process(),
            headless: false,
            panic_stop: tm_computer::session::PanicStopConfig {
                abort_chord: None,
                mouse_move_threshold_px: 5.0,
            },
        }
    }
}

/// Concrete handles to one call's browser/computer session registries, kept alongside their
/// registration as `Arc<dyn CapabilityProvider>` in the [`ToolRegistry`] so
/// [`Executor::execute`] can tear every session down once the dispatched task ends without
/// downcasting a trait object. See `tm_browser::capability`'s module doc comment for the honest
/// scope of what "torn down" means here: this is task-dispatch-scoped teardown, not true
/// lease-expiry-triggered teardown (`SPEC.md` §19.1b) — nothing in this workspace yet fires a
/// callback when a lease expires mid-task.
struct SessionHandles {
    browser: Option<Arc<tm_browser::BrowserCapability>>,
    computer: Arc<tm_computer::ComputerCapability>,
}

impl SessionHandles {
    async fn close_all(&self) {
        if let Some(browser) = &self.browser {
            if let Err(e) = browser.close_all().await {
                tracing::warn!(
                    error = %e,
                    "failed to close one or more browser sessions after task dispatch"
                );
            }
        }
        if let Err(e) = self.computer.close_all().await {
            tracing::warn!(
                error = %e,
                "failed to close one or more computer sessions after task dispatch"
            );
        }
    }
}

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
    /// `docs/audit-2026-09-18-fable.md` B-02: `None` when no `browser.toml` is configured.
    browser: Option<BrowserWiring>,
    /// B-02: always present — see [`ComputerWiring`]'s doc comment for why this has no `None`
    /// state the way [`BuiltinExecutor::browser`] does.
    computer: ComputerWiring,
    /// The human-approval policy threaded into every [`AgentLoop`] this executor builds
    /// (`docs/audit-2026-09-18-fable.md` M-16). Always present, like [`BuiltinExecutor::computer`]
    /// — a project with no `oversight.toml` still has a concrete policy,
    /// [`Oversight::default`] (asks nothing), rather than a `None` state to branch on.
    oversight: Oversight,
    /// Threaded into every [`AgentLoop`] this executor builds via [`AgentLoop::with_root`], when
    /// set — see [`BuiltinExecutor::with_root`]. `None` (the default, via [`BuiltinExecutor::new`])
    /// preserves this executor's behavior before `--worktree` existed exactly: every tool call
    /// resolves against the process's own current directory, the same as before.
    root_override: Option<PathBuf>,
}

impl BuiltinExecutor {
    /// Build a `BuiltinExecutor` identified as `id` (used in the dispatcher's
    /// `agent:<id>/<ticket>` lease holder), sharing the given infrastructure across every
    /// [`Executor::execute`] call. A fresh [`ToolRegistry`], [`AgentLoop`] and pair of
    /// browser/computer session registries are built per call (`ToolRegistry` is not `Clone`, a
    /// fresh loop per run keeps concurrent executions from sharing mutable prompt-cache state,
    /// and a fresh `tm_browser`/`tm_computer` `SessionRegistry` per run is what lets
    /// [`Executor::execute`] tear every session it opened down before returning — see
    /// [`SessionHandles`]).
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
        browser: Option<BrowserWiring>,
        computer: ComputerWiring,
        oversight: Oversight,
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
            browser,
            computer,
            oversight,
            root_override: None,
        }
    }

    /// Point every run this executor drives at `root` instead of the process's own current
    /// directory — the seam `tm-cli`'s `build_dispatcher` uses for `tm run <ticket> --worktree`
    /// (`docs/decisions/D-012-run-worktree-isolation.md`) to run a ticket's `fs.*`/`edit.*`/
    /// `git.*`/`shell.*` tool calls against an isolated `git worktree` checkout rather than the
    /// main working tree. A builder rather than a `new()` parameter so every existing call site
    /// (real and test) is unaffected — the default is `None`, byte-identical to this executor's
    /// behavior before `--worktree` existed.
    pub fn with_root(mut self, root: PathBuf) -> Self {
        self.root_override = Some(root);
        self
    }

    /// Build the [`AgentLoop`], [`AgentTask`] and [`SessionHandles`] `execute` would drive for
    /// `task`, without running anything — the seam a test uses to observe what ceiling the loop
    /// was actually constructed with (see
    /// `crate::executor::tests::builtin_executor_passes_ticket_authority`).
    fn build(&self, task: &ExecutorTask) -> (AgentLoop, AgentTask, SessionHandles) {
        let session = task
            .session
            .clone()
            .unwrap_or_else(|| session_id_from(self.ids.next(IdKind::Session).as_str()));

        let mut extra: Vec<Arc<dyn CapabilityProvider>> = Vec::new();
        let browser_handle = self.browser.as_ref().map(|wiring| {
            let registry = tm_browser::SessionRegistry::new(
                wiring.providers.clone(),
                wiring.sink.clone(),
                self.clock.clone(),
                BROWSER_ARTIFACT_THRESHOLD_BYTES,
            );
            let capability = Arc::new(tm_browser::BrowserCapability::new(registry));
            extra.push(capability.clone() as Arc<dyn CapabilityProvider>);
            capability
        });

        let computer_registry = tm_computer::ComputerSessionRegistry::new(
            self.computer.env.clone(),
            self.computer.headless,
            self.computer.panic_stop.clone(),
        );
        let computer_handle = Arc::new(tm_computer::ComputerCapability::new(computer_registry));
        extra.push(computer_handle.clone() as Arc<dyn CapabilityProvider>);

        // `skill.load` (`docs/audit-2026-09-18-fable.md` M-04): registered here too, not just
        // `tm-cli`'s interactive path, so a scheduler-dispatched worker (`tm run`/`tm sched run`)
        // sees it as well — see `crate::skill_capability`'s module doc comment.
        extra.push(Arc::new(crate::skill_capability::SkillCapability::new())
            as Arc<dyn CapabilityProvider>);

        let tools = ToolRegistry::with_capabilities(
            self.ci.clone(),
            self.store.clone(),
            self.command_cache.clone(),
            self.command_executor.clone(),
            extra,
        );
        // `hooks.toml`, loaded from the project root `self.ci` was opened against (see
        // `tm_codeintel::CodeIntel::project_root`). This method's signature (this trait's
        // `Executor::execute` contract, transitively) is infallible — unlike `tm-cli`'s
        // interactive path (`AgentSession::run_turn_streaming`), which surfaces a malformed
        // `hooks.toml` as a turn-ending `Err` a human sees immediately, there is no `Result`
        // here to propagate one through, so a parse failure logs a warning and falls back to no
        // hooks configured rather than being silently swallowed or requiring a broader signature
        // change to this trait to fix properly.
        let hooks = match crate::hooks::load_hooks_toml(self.ci.project_root()) {
            Ok(hooks) => hooks,
            Err(e) => {
                tracing::warn!(error = %e, "failed to load hooks.toml; proceeding with no hooks configured");
                crate::hooks::HookConfig::default()
            }
        };
        let tools = tools.with_hooks(hooks);
        let mut agent_loop = AgentLoop::new(
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
            self.store.clone(),
        )
        .with_oversight(self.oversight.clone());
        if let Some(root) = self.root_override.clone() {
            agent_loop = agent_loop.with_root(root);
        }

        let agent_task = AgentTask {
            ticket: task.ticket.clone(),
            context_pack: wrap_context_pack(&task.context_pack),
            authority: task.authority.clone(),
            budget: task.budget,
            harness_epoch: task.harness_epoch,
            session,
            conversation: None,
        };

        let handles = SessionHandles {
            browser: browser_handle,
            computer: computer_handle,
        };

        (agent_loop, agent_task, handles)
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
        let (mut agent_loop, agent_task, handles) = self.build(&task);

        // Tear every browser/computer session this run opened down before returning, on every
        // path — success, failure, or a mid-run `?` — not just the happy path. This is the
        // task-dispatch-scoped teardown `SessionHandles`'s doc comment describes.
        let outcome = agent_loop.run(agent_task).await;
        handles.close_all().await;
        let outcome = outcome?;

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
            AgentOutcome::Replied { steps, .. } => {
                // Unreachable in practice: this executor never sets `AgentTask::conversation`, the
                // only way the loop produces `Replied`. Mapped to the same failure a ticketed run
                // that stops talking without submitting has always produced.
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
                        class: FailureClass::Other,
                        detail: "model ended turn without submitting".to_string(),
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
            None,
            ComputerWiring::default(),
            Oversight::default(),
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

        let (agent_loop, agent_task, _handles) = executor.build(&task);

        assert_eq!(agent_loop.authority(), &restricted);
        assert_ne!(agent_loop.authority(), &Authority::root());
        assert_eq!(agent_loop.budget(), &scoped_budget);
        assert_ne!(agent_loop.budget(), &Budget::unlimited());
        assert_eq!(agent_task.authority, restricted);
        assert_eq!(agent_task.budget, scoped_budget);
    }

    #[test]
    fn with_root_overrides_the_built_loops_root_instead_of_the_process_cwd() {
        // `docs/decisions/D-012-run-worktree-isolation.md`: `tm run <ticket> --worktree` needs
        // `BuiltinExecutor::with_root` to actually reach the `AgentLoop` it constructs, not just
        // be stored and ignored — this is the seam `AgentLoop::root()`'s own doc comment says it
        // exists for.
        let dir = tempfile::tempdir().expect("tempdir");
        let worktree_dir = tempfile::tempdir().expect("worktree tempdir");
        let executor = test_executor(dir.path()).with_root(worktree_dir.path().to_path_buf());
        let task = ExecutorTask {
            ticket: TicketId::new("T-1").expect("ticket id"),
            role: Role::CoderFast,
            objective: "narrow work".to_string(),
            context_pack: "pack text".to_string(),
            authority: Authority::root(),
            budget: Budget::unlimited(),
            harness_epoch: 0,
            actor: ParticipantId::new("agent:test-builtin/T-1").expect("participant"),
            session: None,
        };

        let (agent_loop, _agent_task, _handles) = executor.build(&task);

        assert_eq!(agent_loop.root(), worktree_dir.path());
        assert_ne!(
            agent_loop.root(),
            std::env::current_dir().expect("cwd"),
            "the override must actually take effect, not silently fall back to the process cwd"
        );
    }

    #[test]
    fn without_with_root_the_built_loop_falls_back_to_the_process_cwd_unchanged() {
        // The zero-`--worktree` default path must stay byte-identical to this executor's
        // behavior before `with_root` existed.
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = test_executor(dir.path());
        let task = ExecutorTask {
            ticket: TicketId::new("T-1").expect("ticket id"),
            role: Role::CoderFast,
            objective: "narrow work".to_string(),
            context_pack: "pack text".to_string(),
            authority: Authority::root(),
            budget: Budget::unlimited(),
            harness_epoch: 0,
            actor: ParticipantId::new("agent:test-builtin/T-1").expect("participant"),
            session: None,
        };

        let (agent_loop, _agent_task, _handles) = executor.build(&task);

        assert_eq!(
            agent_loop.root(),
            std::env::current_dir().expect("cwd"),
            "with no override, the loop's root must be exactly the process cwd, unchanged"
        );
    }

    #[test]
    fn build_registers_the_computer_capability_and_no_browser_capability_by_default() {
        // `test_executor` passes `browser: None` and the default `ComputerWiring`: this asserts
        // the resulting tool surface reflects exactly that — computer.* tools present,
        // browser.* tools absent — rather than merely that `build` doesn't panic.
        let dir = tempfile::tempdir().expect("tempdir");
        let executor = test_executor(dir.path());
        let task = ExecutorTask {
            ticket: TicketId::new("T-1").expect("ticket id"),
            role: Role::CoderFast,
            objective: "narrow work".to_string(),
            context_pack: "pack text".to_string(),
            authority: Authority::root(),
            budget: Budget::unlimited(),
            harness_epoch: 0,
            actor: ParticipantId::new("agent:test-builtin/T-1").expect("participant"),
            session: None,
        };

        let (agent_loop, _agent_task, handles) = executor.build(&task);
        assert!(handles.browser.is_none());
        let defs = agent_loop.tools().tool_defs();
        assert!(defs.iter().any(|d| d.name == "computer.snapshot"));
        assert!(!defs.iter().any(|d| d.name.starts_with("browser.")));
        // `skill.load` (`docs/audit-2026-09-18-fable.md` M-04): registered on this path too,
        // not just `tm-cli`'s interactive one — see `crate::skill_capability`'s doc comment.
        assert!(defs.iter().any(|d| d.name == "skill.load"));
    }
}
