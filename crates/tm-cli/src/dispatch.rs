//! Builds the [`tm_scheduler::ExecutorDispatcher`] that [`crate::sched::run_ticket`] (`tm run`)
//! and [`crate::sched::sched_run`] (`tm sched run`) both drive tickets through, instead of each
//! having its own ad hoc "compile a pack and do nothing" or "just tick" logic (`SPEC.md` §24,
//! audit B-01/B-04).

use std::io::{self, BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tm_acp::{AcpAgentConfig, AcpExecutor};
use tm_agent::{BuiltinExecutor, HumanApprovalSink, HumanDecision, HumanExecutor};
use tm_core::executor::ExecutorTask;
use tm_core::store::Store;
use tm_core::ArtifactKind;
use tm_scheduler::dispatch::{ContextPackSource, ExecutorDispatcher, ExecutorRegistry};
use tm_types::{Oversight, Role, TicketId};

use crate::agent::{build_fabric_for_project, MemoryCommandCache, ProcessCommandExecutor};
use crate::project::Project;

/// Compiles a ticket's context pack against the live project state and renders it to text via
/// [`tm_agent::render_task_prompt`] — the same rendering the interactive session already uses
/// for its own prompt, reused here so `tm run`/`tm sched run` see the same context shape a human
/// driving `tm` directly would. Holds only the cheap, owned/`Arc` pieces it needs (never
/// `&Project` — the dispatcher's `Arc<dyn ContextPackSource>` must outlive any single command
/// invocation's borrow of `Project`).
struct ProjectContextPackSource {
    store: Arc<Store>,
    root: PathBuf,
    state_dir: PathBuf,
}

impl ContextPackSource for ProjectContextPackSource {
    fn compile(&self, ticket: &TicketId) -> tm_types::Result<String> {
        let view = self.store.view()?;
        let ticket_state = view
            .tickets
            .get(ticket)
            .ok_or_else(|| tm_types::TmError::not_found("ticket", ticket))?;
        let ci = tm_codeintel::CodeIntel::open_at(&self.state_dir, &self.root)?;
        let pack = tm_context::pack::compile(
            ticket_state,
            &view,
            &ci,
            tm_context::tokens::TokenBudget::even(10_000),
            tm_codeintel::SignalWeights::default(),
            &[],
            &crate::ops::load_role_table_for_state_dir(&self.state_dir)?,
        )?;
        Ok(tm_agent::render_task_prompt(ticket, &pack))
    }
}

/// Asks the human at the controlling terminal to resolve a `human_required` ticket over stdin —
/// the CLI's [`tm_agent::HumanApprovalSink`], used by [`build_dispatcher`]'s [`HumanExecutor`].
struct StdinApprovalSink {
    store: Arc<Store>,
}

impl StdinApprovalSink {
    /// Blocking prompt/read, run on a `spawn_blocking` thread so it never stalls the tokio
    /// runtime the dispatcher's background tasks share.
    fn prompt_blocking(objective: &str, context_pack: &str) -> io::Result<Option<String>> {
        let mut stdout = io::stdout();
        writeln!(stdout, "\n--- human-required ticket ---")?;
        writeln!(stdout, "{objective}")?;
        writeln!(stdout, "{context_pack}")?;
        write!(
            stdout,
            "Enter a one-line summary of the completed work, or leave blank to decline: "
        )?;
        stdout.flush()?;

        let stdin = io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let line = line.trim().to_string();
        Ok(if line.is_empty() { None } else { Some(line) })
    }
}

#[async_trait::async_trait]
impl HumanApprovalSink for StdinApprovalSink {
    async fn escalate(&self, task: &ExecutorTask) -> tm_types::Result<Option<HumanDecision>> {
        let objective = task.objective.clone();
        let context_pack = task.context_pack.clone();
        let answer =
            tokio::task::spawn_blocking(move || Self::prompt_blocking(&objective, &context_pack))
                .await
                .map_err(|e| tm_types::TmError::Io(e.to_string()))?
                .map_err(|e| tm_types::TmError::Io(e.to_string()))?;

        let Some(summary) = answer else {
            return Ok(None);
        };

        let events = self.store.store_artifact(
            ArtifactKind::Report,
            "text/plain".to_string(),
            summary.clone().into_bytes(),
            serde_json::json!({ "source": "human executor" }),
            Some(task.ticket.clone()),
            task.actor.clone(),
        )?;
        let artifact = events
            .iter()
            .find_map(|e| e.payload.as_artifact_created().map(|p| p.artifact.clone()))
            .ok_or_else(|| {
                tm_types::TmError::Invariant(
                    "store_artifact did not emit artifact.created".to_string(),
                )
            })?;

        Ok(Some(HumanDecision {
            summary,
            evidence: vec![artifact],
        }))
    }
}

/// The [`HumanApprovalSink`] for a process whose stdin and stdout are not a human's terminal:
/// `tm mcp`, where both carry JSON-RPC. [`StdinApprovalSink`] would print its prompt into the
/// protocol stream and then swallow the host's next request as the "answer". This one declines
/// with an error instead (the trait's "no terminal attached" case), leaving the ticket for a
/// human to pick up from `tm run`, `tm sched run` or the TUI.
pub struct HeadlessApprovalSink;

#[async_trait::async_trait]
impl HumanApprovalSink for HeadlessApprovalSink {
    async fn escalate(&self, task: &ExecutorTask) -> tm_types::Result<Option<HumanDecision>> {
        Err(tm_types::TmError::Io(format!(
            "{} needs a human, and this process has no terminal to ask on; run it with `tm run {}`",
            task.ticket, task.ticket
        )))
    }
}

const ACP_TOML_FILENAME: &str = "acp.toml";

/// The `[agent]` table `acp.toml` names: which external ACP-speaking agent to launch and which
/// ticket [`Role`] it should serve. Deliberately minimal compared to `browser.toml`'s
/// provider-fallback-chain shape (`crate::drive::BrowserToml`) — B-12's scope is one external
/// agent registered as one more role-routed executor, not a fallback chain of several.
#[derive(Debug, serde::Deserialize)]
struct AcpToml {
    agent: AcpAgentToml,
}

/// See [`AcpToml`].
#[derive(Debug, serde::Deserialize)]
struct AcpAgentToml {
    /// argv to launch the agent; `command[0]` is the program (e.g. `["claude-code-acp"]`).
    command: Vec<String>,
    /// Which ticket role this agent should be registered for, replacing [`BuiltinExecutor`] for
    /// exactly that role — [`ExecutorRegistry::register`] replaces an earlier registration for
    /// the same role, the same mechanism [`build_dispatcher`] already relies on to register
    /// `builtin` for every role in the first place.
    role: Role,
    /// Per-call timeout in seconds for each of `initialize`/`session/new`/`session/prompt`;
    /// defaults to 120s (`tm_acp`'s own client default) when absent.
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

/// Build the [`AcpExecutor`] `project.root`'s `acp.toml` describes, paired with the [`Role`] it
/// should be registered for, or `None` when the project has no `acp.toml` — mirrors
/// `crate::drive::optional_browser_wiring`'s "absent config file means this optional executor is
/// simply never registered" shape exactly, so a project with no `acp.toml` dispatches every
/// ticket identically to how it did before this crate existed.
///
/// `exec_root` is the external agent's working directory (`AcpAgentConfig::cwd`) — `project.root`
/// for a normal run, or a `tm run <ticket> --worktree` run's isolated checkout
/// (`docs/decisions/D-012-run-worktree-isolation.md`); `acp.toml` itself is still always read
/// from `project.root`, since it is human-authored, version-controlled project configuration,
/// not something a per-run worktree carries its own copy of.
fn optional_acp_executor(
    project: &Project,
    exec_root: &Path,
) -> tm_types::Result<Option<(Role, Arc<AcpExecutor>)>> {
    let path = project.root.join(ACP_TOML_FILENAME);
    if !path.exists() {
        return Ok(None);
    }
    let source = std::fs::read_to_string(&path)
        .map_err(|e| tm_types::TmError::Io(format!("reading {}: {e}", path.display())))?;
    let parsed: AcpToml = toml::from_str(&source)
        .map_err(|e| tm_types::TmError::parse(format!("{}: {e}", path.display())))?;
    let timeout = Duration::from_secs(parsed.agent.timeout_seconds.unwrap_or(120));
    let executor = Arc::new(AcpExecutor::new(
        "acp",
        AcpAgentConfig {
            command: parsed.agent.command,
            cwd: exec_root.to_path_buf(),
            timeout,
        },
        project.store.clone(),
    ));
    Ok(Some((parsed.agent.role, executor)))
}

/// The human-authored approval policy (`SPEC.md` §4.4), per `docs/decisions/D-009-oversight-policy-wiring.md`.
const OVERSIGHT_TOML_FILENAME: &str = "oversight.toml";

/// Load and parse `project.root`'s `oversight.toml`, or [`Oversight::default`] — identical to
/// [`Oversight::autonomous`], asking nothing — when the file is absent, so a project with no
/// `oversight.toml` dispatches every ticket exactly as it did before `Oversight` had a real
/// caller (`docs/audit-2026-09-18-fable.md` M-16). Root, not `state_dir`, per D-003: like
/// `acp.toml`/`browser.toml`, this is a human-authored, version-controlled policy a team reviews
/// together, not derived-and-gitignored project state the way `harness.toml`/`mirror.toml` are.
///
/// `pub(crate)`, not private: `crate::agent`'s interactive `tm`/`tm -p` loop is the *other* real
/// effect boundary (`build_dispatcher`'s `tm run`/`tm sched run` is the first) and loads this the
/// same way, rather than inventing a second loader.
pub(crate) fn load_oversight(project: &Project) -> tm_types::Result<Oversight> {
    let path = project.root.join(OVERSIGHT_TOML_FILENAME);
    if !path.exists() {
        return Ok(Oversight::default());
    }
    let source = std::fs::read_to_string(&path)
        .map_err(|e| tm_types::TmError::Io(format!("reading {}: {e}", path.display())))?;
    toml::from_str(&source)
        .map_err(|e| tm_types::TmError::parse(format!("{}: {e}", path.display())))
}

/// Build the dispatcher `tm run`/`tm sched run` share: a [`BuiltinExecutor`] registered for
/// every [`Role`] (the reference adapter, per `SPEC.md` §24.3), optionally overridden for one
/// role by an [`AcpExecutor`] when the project has an `acp.toml` (B-12 — `codex`/`pi`/`opencode`
/// remain future adapters against the same [`tm_core::Executor`] trait), plus a stdin-driven
/// [`HumanExecutor`] for `human_required` tickets, spawning background runs onto `handle`.
///
/// `exec_root`, when `Some`, overrides where the dispatched run's own file/git tool calls
/// resolve against — `tm run <ticket> --worktree`'s isolated checkout
/// (`docs/decisions/D-012-run-worktree-isolation.md`) instead of the main working tree. `None`
/// (what `sched::sched_run`'s `tm sched run` always passes; `sched::run_ticket`'s `tm run`
/// passes it only without `--worktree`) is byte-identical to this function's behavior before
/// `--worktree` existed: [`BuiltinExecutor`] keeps resolving tool calls against the process's own
/// current directory, [`AcpExecutor`]'s `cwd` and the dispatcher's snapshot `repo_root` both stay
/// `project.root`. Retrieval (`ProjectContextPackSource`'s `CodeIntel`) is deliberately *not*
/// affected either way — see that struct's construction below.
///
/// `steps`, when `Some`, receives every step of every builtin-executed run as it happens (`tm
/// run`'s live progress).
pub fn build_dispatcher(
    project: &Project,
    handle: tokio::runtime::Handle,
    exec_root: Option<&Path>,
    steps: Option<tokio::sync::mpsc::UnboundedSender<tm_agent::StepRecord>>,
) -> tm_types::Result<Arc<ExecutorDispatcher>> {
    let human = Arc::new(StdinApprovalSink {
        store: project.store.clone(),
    });
    build_dispatcher_with_human(project, handle, exec_root, steps, human)
}

/// [`build_dispatcher`] with the [`HumanApprovalSink`] `human_required` tickets escalate through
/// chosen by the caller: [`HeadlessApprovalSink`] where stdin/stdout aren't a human's terminal.
pub fn build_dispatcher_with_human(
    project: &Project,
    handle: tokio::runtime::Handle,
    exec_root: Option<&Path>,
    steps: Option<tokio::sync::mpsc::UnboundedSender<tm_agent::StepRecord>>,
    human_sink: Arc<dyn HumanApprovalSink>,
) -> tm_types::Result<Arc<ExecutorDispatcher>> {
    let exec_root = exec_root.unwrap_or(project.root.as_path());
    let fabric = build_fabric_for_project(project, project.clock.clone())?;
    let ci = Arc::new(project.code_intel()?);
    let command_cache: Arc<dyn tm_context::CommandCache + Send + Sync> =
        Arc::new(MemoryCommandCache::new(project.ids.clone()));
    let command_executor: Arc<dyn tm_context::CommandExecutor + Send + Sync> =
        Arc::new(ProcessCommandExecutor);

    let browser = crate::drive::optional_browser_wiring(project)?;
    let oversight = load_oversight(project)?;
    let mut builtin_executor = BuiltinExecutor::new(
        "builtin",
        fabric,
        ci,
        project.store.clone(),
        command_cache,
        command_executor,
        project.clock.clone(),
        project.ids.clone(),
        browser,
        tm_agent::ComputerWiring::default(),
        oversight,
    );
    // Only override when a caller actually asked for one (`exec_root` argument `Some`), not
    // unconditionally to `project.root` — see this function's own doc comment on why "no
    // override requested" and "override to project.root" must stay distinguishable.
    if exec_root != project.root.as_path() {
        builtin_executor = builtin_executor.with_root(exec_root.to_path_buf());
    }
    if let Some(steps) = steps {
        builtin_executor = builtin_executor.with_step_sender(steps);
    }
    let builtin = Arc::new(builtin_executor);

    let human = Arc::new(HumanExecutor::new("human", human_sink));

    let mut registry = ExecutorRegistry::new(human);
    for role in Role::ALL {
        registry.register(role, builtin.clone());
    }
    if let Some((role, acp_executor)) = optional_acp_executor(project, exec_root)? {
        registry.register(role, acp_executor);
    }

    let context: Arc<dyn ContextPackSource> = Arc::new(ProjectContextPackSource {
        store: project.store.clone(),
        root: project.root.clone(),
        state_dir: project.state_dir.clone(),
    });

    Ok(Arc::new(ExecutorDispatcher::new(
        project.store.clone(),
        handle,
        context,
        registry,
        Some(exec_root.to_path_buf()),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::Executor as _;
    use tm_types::{Clock, CounterIds, FixedClock, IdSource};

    fn test_project(root: &std::path::Path) -> Project {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store =
            Arc::new(Store::open_with(root, clock.clone(), ids.clone()).expect("open store"));
        Project::for_test(root, store, clock, ids)
    }

    #[test]
    fn optional_acp_executor_is_none_without_an_acp_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        assert!(optional_acp_executor(&project, &project.root)
            .expect("no error")
            .is_none());
    }

    #[test]
    fn optional_acp_executor_reads_the_command_and_role_from_a_real_acp_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(ACP_TOML_FILENAME),
            "[agent]\ncommand = [\"claude-code-acp\"]\nrole = \"coder_fast\"\n",
        )
        .expect("write acp.toml");
        let project = test_project(dir.path());

        let (role, executor) = optional_acp_executor(&project, &project.root)
            .expect("no error")
            .expect("acp.toml is present, so this must be Some");
        assert_eq!(role, Role::CoderFast);
        assert_eq!(executor.id(), "acp");
    }

    #[test]
    fn optional_acp_executor_honors_an_explicit_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(ACP_TOML_FILENAME),
            "[agent]\ncommand = [\"claude-code-acp\", \"--stdio\"]\nrole = \"reviewer_semantic\"\ntimeout_seconds = 30\n",
        )
        .expect("write acp.toml");
        let project = test_project(dir.path());

        let (role, _executor) = optional_acp_executor(&project, &project.root)
            .expect("no error")
            .expect("acp.toml is present");
        assert_eq!(role, Role::ReviewerSemantic);
    }

    #[test]
    fn optional_acp_executor_surfaces_a_parse_error_for_malformed_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(ACP_TOML_FILENAME), "not valid toml [[[")
            .expect("write acp.toml");
        let project = test_project(dir.path());

        // `AcpExecutor` (inside the `Ok(Some(..))` this would otherwise be) doesn't implement
        // `Debug`, so this matches the whole `Result` rather than using `expect_err`/`unwrap_err`
        // (both require `T: Debug`).
        let result = optional_acp_executor(&project, &project.root);
        assert!(matches!(result, Err(tm_types::TmError::Parse(_))));
    }

    // -----------------------------------------------------------------------------------------
    // `oversight.toml` (`docs/decisions/D-009-oversight-policy-wiring.md`): the regression-safe
    // default when absent, and real parsing when present — mirroring `optional_acp_executor`'s
    // tests immediately above, since `load_oversight` follows the same loader shape.
    // -----------------------------------------------------------------------------------------

    #[test]
    fn load_oversight_defaults_to_asking_nothing_without_an_oversight_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        assert_eq!(
            load_oversight(&project).expect("no error"),
            Oversight::default(),
            "a project with no oversight.toml must dispatch exactly as it did before Oversight \
             had a real caller"
        );
    }

    #[test]
    fn load_oversight_parses_approval_required_and_spend_limit_from_a_real_oversight_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(OVERSIGHT_TOML_FILENAME),
            "approval_required = [\"git.commit\", \"project\"]\nspend_over_micros = 5000000\n",
        )
        .expect("write oversight.toml");
        let project = test_project(dir.path());

        let oversight = load_oversight(&project).expect("no error");
        assert!(oversight.approval_required.contains("git.commit"));
        assert!(oversight.approval_required.contains("project"));
        assert_eq!(oversight.spend_over_micros, Some(5_000_000));
    }

    #[test]
    fn load_oversight_surfaces_a_parse_error_for_malformed_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(OVERSIGHT_TOML_FILENAME),
            "not valid toml [[[",
        )
        .expect("write oversight.toml");
        let project = test_project(dir.path());

        assert!(matches!(
            load_oversight(&project),
            Err(tm_types::TmError::Parse(_))
        ));
    }

    #[test]
    fn load_oversight_rejects_a_misspelled_key_instead_of_silently_ignoring_it() {
        // `Oversight`'s `deny_unknown_fields`: a typo here (`approval_requird`) must fail loudly,
        // not parse into an empty, all-autonomous policy a human wrongly believes is gating
        // something.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(OVERSIGHT_TOML_FILENAME),
            "approval_requird = [\"git.force_push\"]\n",
        )
        .expect("write oversight.toml");
        let project = test_project(dir.path());

        assert!(matches!(
            load_oversight(&project),
            Err(tm_types::TmError::Parse(_))
        ));
    }

    /// The `builtin`-for-every-role, `acp`-overrides-one-role registration shape this module's
    /// own doc comment on [`build_dispatcher`] describes, exercised directly against
    /// [`ExecutorRegistry`] rather than only inferred from reading the code: registering `acp`
    /// for one role must not disturb any other role's `builtin` registration.
    #[test]
    fn registering_an_acp_executor_for_one_role_leaves_every_other_role_on_builtin() {
        struct StubExecutor(&'static str);
        #[async_trait::async_trait]
        impl tm_core::Executor for StubExecutor {
            fn id(&self) -> &str {
                self.0
            }
            fn capabilities(&self) -> tm_core::executor::ExecutorCapabilities {
                tm_core::executor::ExecutorCapabilities {
                    streaming: false,
                    tool_use: true,
                    patch_output: false,
                    interactive: false,
                    accepts_context_pack: true,
                    sandboxed: false,
                    max_context_tokens: None,
                    cost_class: tm_core::executor::CostClass::Standard,
                }
            }
            async fn execute(
                &self,
                _task: ExecutorTask,
            ) -> tm_types::Result<tm_core::executor::ExecutorOutcome> {
                unimplemented!("not exercised by this test")
            }
            async fn cancel(
                &self,
                _handle: &tm_core::executor::ExecutionHandle,
            ) -> tm_types::Result<()> {
                Ok(())
            }
        }

        let human: Arc<dyn tm_core::Executor> = Arc::new(StubExecutor("human"));
        let builtin: Arc<dyn tm_core::Executor> = Arc::new(StubExecutor("builtin"));
        let acp: Arc<dyn tm_core::Executor> = Arc::new(StubExecutor("acp"));

        let mut registry = ExecutorRegistry::new(human);
        for role in Role::ALL {
            registry.register(role, builtin.clone());
        }
        registry.register(Role::CoderFast, acp.clone());

        assert_eq!(
            registry.for_role(Role::CoderFast).expect("registered").id(),
            "acp"
        );
        for role in Role::ALL {
            if role == Role::CoderFast {
                continue;
            }
            assert_eq!(
                registry.for_role(role).expect("registered").id(),
                "builtin",
                "role {role:?} must be untouched by the acp override"
            );
        }
    }
}
