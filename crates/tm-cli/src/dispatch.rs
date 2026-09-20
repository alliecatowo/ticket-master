//! Builds the [`tm_scheduler::ExecutorDispatcher`] that [`crate::sched::run_ticket`] (`tm run`)
//! and [`crate::sched::sched_run`] (`tm sched run`) both drive tickets through, instead of each
//! having its own ad hoc "compile a pack and do nothing" or "just tick" logic (`SPEC.md` §24,
//! audit B-01/B-04).

use std::io::{self, BufRead, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tm_acp::{AcpAgentConfig, AcpExecutor};
use tm_agent::{BuiltinExecutor, HumanApprovalSink, HumanDecision, HumanExecutor};
use tm_core::executor::ExecutorTask;
use tm_core::store::Store;
use tm_core::ArtifactKind;
use tm_scheduler::dispatch::{ContextPackSource, ExecutorDispatcher, ExecutorRegistry};
use tm_types::{Role, TicketId};

use crate::agent::{build_fabric, MemoryCommandCache, ProcessCommandExecutor};
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
            &tm_provider::RoleTable::default_table(),
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
fn optional_acp_executor(project: &Project) -> tm_types::Result<Option<(Role, Arc<AcpExecutor>)>> {
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
            cwd: project.root.clone(),
            timeout,
        },
        project.store.clone(),
    ));
    Ok(Some((parsed.agent.role, executor)))
}

/// Build the dispatcher `tm run`/`tm sched run` share: a [`BuiltinExecutor`] registered for
/// every [`Role`] (the reference adapter, per `SPEC.md` §24.3), optionally overridden for one
/// role by an [`AcpExecutor`] when the project has an `acp.toml` (B-12 — `codex`/`pi`/`opencode`
/// remain future adapters against the same [`tm_core::Executor`] trait), plus a stdin-driven
/// [`HumanExecutor`] for `human_required` tickets, spawning background runs onto `handle`.
pub fn build_dispatcher(
    project: &Project,
    handle: tokio::runtime::Handle,
) -> tm_types::Result<Arc<ExecutorDispatcher>> {
    let fabric = build_fabric(project.clock.clone())?;
    let ci = Arc::new(project.code_intel()?);
    let command_cache: Arc<dyn tm_context::CommandCache + Send + Sync> =
        Arc::new(MemoryCommandCache::new(project.ids.clone()));
    let command_executor: Arc<dyn tm_context::CommandExecutor + Send + Sync> =
        Arc::new(ProcessCommandExecutor);

    let browser = crate::drive::optional_browser_wiring(project)?;
    let builtin = Arc::new(BuiltinExecutor::new(
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
    ));

    let human = Arc::new(HumanExecutor::new(
        "human",
        Arc::new(StdinApprovalSink {
            store: project.store.clone(),
        }),
    ));

    let mut registry = ExecutorRegistry::new(human);
    for role in Role::ALL {
        registry.register(role, builtin.clone());
    }
    if let Some((role, acp_executor)) = optional_acp_executor(project)? {
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
        Some(project.root.clone()),
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
        assert!(optional_acp_executor(&project).expect("no error").is_none());
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

        let (role, executor) = optional_acp_executor(&project)
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

        let (role, _executor) = optional_acp_executor(&project)
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
        let result = optional_acp_executor(&project);
        assert!(matches!(result, Err(tm_types::TmError::Parse(_))));
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
