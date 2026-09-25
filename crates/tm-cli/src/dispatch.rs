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
    /// The workspace `CodeIntel` indexes and searches — `exec_root` (`tm run --worktree`'s
    /// isolated checkout) when it differs from `project.root`, so retrieval sees the tree the
    /// run is actually executing in rather than the main checkout (`critic-worktree-exec-root-
    /// indexing`). Equal to `project.root` outside `--worktree`.
    root: PathBuf,
    /// Where this source's own `CodeIntel` reads/writes `index.db` — `root.join(".tm")` when
    /// `root` is a worktree, so a worktree run's incremental reindex never contends with the main
    /// checkout's `index.db` for writes. Equal to `project.state_dir` outside `--worktree`.
    state_dir: PathBuf,
    /// Used to refresh the index in [`ContextPackSource::compile`] before every pack, the same
    /// way [`Project::code_intel`](crate::project::Project::code_intel) refreshes on open — this
    /// source opens its own `CodeIntel` rather than sharing `Project`'s, so it needs its own
    /// clock to do that (see this struct's own doc comment for why it can't just hold `&Project`).
    clock: Arc<dyn tm_types::Clock>,
}

impl ContextPackSource for ProjectContextPackSource {
    fn compile(&self, ticket: &TicketId) -> tm_types::Result<String> {
        let view = self.store.view()?;
        let ticket_state = view
            .tickets
            .get(ticket)
            .ok_or_else(|| tm_types::TmError::not_found("ticket", ticket))?;
        let ci = open_and_refresh_code_intel(&self.state_dir, &self.root, self.clock.as_ref())?;
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
        writeln!(stdout, "\n--- this ticket needs a human ---")?;
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
            "Ticket {} needs a human, but this process has no terminal to ask on. Run `tm run {}` instead.",
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
/// (what `tm sched run` and a normal `tm run` pass) resolves tool calls against `project.root`;
/// `--worktree` passes that isolated root instead. The launcher process's current directory is
/// never an execution-root fallback. Retrieval (`ProjectContextPackSource`'s `CodeIntel`, and the
/// `CodeIntel` wired into the `BuiltinExecutor` below) follows `exec_root` too, with its own
/// per-worktree `index.db` under `exec_root.join(".tm")` — see `index_root_and_state_dir` —
/// rather than sharing the main checkout's index, so `search.*`/`symbol.*` tool calls inside a
/// `--worktree` run see the worktree's own tree instead of stale main-checkout results
/// (`critic-worktree-exec-root-indexing`).
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
    let fabric = build_fabric_for_project(project, project.clock.clone())?;
    build_dispatcher_with_fabric(project, handle, exec_root, steps, fabric, human_sink)
}

/// [`build_dispatcher`], but every provider the fabric registers is wrapped so a completion it
/// serves also appends to `sink` — used only by `tm run <ticket> --record <path>`
/// (`replay-cli-record-flag`, `docs/decisions/D-028-record-replay-harness.md`) to capture a
/// cassette of the run's real provider traffic. `role` is the ticket's own
/// [`tm_core::ticket::ExecutorRequirements::role`] (a single `tm run` dispatches one ticket, so
/// one role for the whole recording); `sink` is shared across every wrapped provider rather than
/// each keeping its own private cassette, so a mid-run fallback across candidates still lands in
/// one ordered recording instead of splitting across several.
pub(crate) fn build_dispatcher_with_recording(
    project: &Project,
    handle: tokio::runtime::Handle,
    exec_root: Option<&Path>,
    steps: Option<tokio::sync::mpsc::UnboundedSender<tm_agent::StepRecord>>,
    role: Role,
    sink: crate::agent::CassetteSink,
) -> tm_types::Result<Arc<ExecutorDispatcher>> {
    let root = execution_root(&project.root, exec_root).to_path_buf();
    let fabric = crate::agent::build_fabric_for_project_recording(
        project,
        project.clock.clone(),
        crate::agent::RecordingSpec { role, root, sink },
    )?;
    let human = Arc::new(StdinApprovalSink {
        store: project.store.clone(),
    });
    build_dispatcher_with_fabric(project, handle, exec_root, steps, fabric, human)
}

/// [`build_dispatcher`], but `role`'s only candidate is a [`tm_provider::MockProvider`] replaying
/// `cassette` in order — used only by `tm run <ticket> --replay <path>` (`replay-cli-replay-flag`,
/// `docs/decisions/D-028-record-replay-harness.md`) for an offline, network-free rerun of a
/// previously recorded ticket. Returns the [`tm_provider::MockProvider`] handle alongside the
/// dispatcher so the caller can read `MockProvider::divergences()` once the run finishes.
pub(crate) fn build_dispatcher_with_replay(
    project: &Project,
    handle: tokio::runtime::Handle,
    exec_root: Option<&Path>,
    steps: Option<tokio::sync::mpsc::UnboundedSender<tm_agent::StepRecord>>,
    role: Role,
    cassette: tm_provider::Cassette,
) -> tm_types::Result<(Arc<ExecutorDispatcher>, Arc<tm_provider::MockProvider>)> {
    let root = execution_root(&project.root, exec_root).to_path_buf();
    let (fabric, mock) = crate::agent::build_fabric_for_project_replay(
        project,
        project.clock.clone(),
        crate::agent::ReplaySpec {
            role,
            root,
            cassette,
        },
    )?;
    let human = Arc::new(StdinApprovalSink {
        store: project.store.clone(),
    });
    let dispatcher =
        build_dispatcher_with_fabric(project, handle, exec_root, steps, fabric, human)?;
    Ok((dispatcher, mock))
}

/// [`build_dispatcher`] over an already-built `fabric` instead of the one
/// [`crate::agent::build_fabric_for_project`] would pick from the project's own configuration —
/// the seam an in-process test uses to run the real dispatcher against a provider it controls.
pub(crate) fn build_dispatcher_with_fabric(
    project: &Project,
    handle: tokio::runtime::Handle,
    exec_root: Option<&Path>,
    steps: Option<tokio::sync::mpsc::UnboundedSender<tm_agent::StepRecord>>,
    fabric: Arc<tm_provider::Fabric>,
    human_sink: Arc<dyn HumanApprovalSink>,
) -> tm_types::Result<Arc<ExecutorDispatcher>> {
    let exec_root = execution_root(&project.root, exec_root);
    let (index_root, index_state_dir) = index_root_and_state_dir(project, exec_root);
    let ci = Arc::new(open_and_refresh_code_intel(
        &index_state_dir,
        &index_root,
        project.clock.as_ref(),
    )?);
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
    // Explicitly root all worker tools. `BuiltinExecutor`'s default is process cwd, which can be
    // unrelated when `--project` targets another directory.
    builtin_executor = builtin_executor.with_root(exec_root.to_path_buf());
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
        root: index_root,
        state_dir: index_state_dir,
        clock: project.clock.clone(),
    });

    Ok(Arc::new(ExecutorDispatcher::new(
        project.store.clone(),
        handle,
        context,
        registry,
        Some(exec_root.to_path_buf()),
    )))
}

fn execution_root<'a>(project_root: &'a Path, override_root: Option<&'a Path>) -> &'a Path {
    override_root.unwrap_or(project_root)
}

/// Where a dispatched run's `CodeIntel` should read/write from, given the tool-call execution
/// root the run resolved (`execution_root`): `(project.root, project.state_dir)` unchanged when
/// `exec_root` is the main checkout, or `(exec_root, exec_root.join(".tm"))` when it's a `tm run
/// --worktree` checkout, so the index tracks the tree the run actually executes in instead of the
/// main checkout's, and a worktree run's incremental reindex writes to its own `index.db` rather
/// than fighting the main checkout for the same one (`critic-worktree-exec-root-indexing`).
/// `.tm` is already `.gitignore`d repo-wide, so this needs no extra ignore entry, and it never
/// collides with a real `tm init`'d project's own `.tm` since a `--worktree` checkout is never
/// itself doctored/initialized as a project.
fn index_root_and_state_dir(project: &Project, exec_root: &Path) -> (PathBuf, PathBuf) {
    if exec_root == project.root.as_path() {
        (project.root.clone(), project.state_dir.clone())
    } else {
        (exec_root.to_path_buf(), exec_root.join(".tm"))
    }
}

/// Opens a `CodeIntel` at `index_dir` over `workspace_root` and best-effort refreshes it via
/// [`tm_codeintel::CodeIntel::update_incremental`], mirroring
/// [`Project::code_intel`](crate::project::Project::code_intel)'s own open-and-refresh behavior
/// (skipped when `workspace_root` isn't inside a git work tree; degrades to a `tracing::warn!`
/// rather than failing when the refresh itself errors) — shared here so both the dispatcher's
/// `BuiltinExecutor` and `ProjectContextPackSource` refresh the *same* index exactly once per
/// dispatch build, whichever root (`project.root` or a `--worktree` checkout) that index is for.
fn open_and_refresh_code_intel(
    index_dir: &Path,
    workspace_root: &Path,
    clock: &dyn tm_types::Clock,
) -> tm_types::Result<tm_codeintel::CodeIntel> {
    let ci = tm_codeintel::CodeIntel::open_at_auto(
        index_dir,
        workspace_root,
        if cfg!(test) { Some("hash") } else { None },
    )?;
    if git2::Repository::open(workspace_root).is_ok() {
        if let Err(e) = ci.update_incremental(clock) {
            tracing::warn!(
                error = %e,
                root = %workspace_root.display(),
                "could not refresh the code index; continuing with what's already indexed"
            );
        }
    }
    Ok(ci)
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
    fn worker_root_defaults_to_external_project_not_launcher_cwd() {
        let launcher_cwd = std::env::current_dir().expect("launcher cwd");
        let external_project = tempfile::tempdir().expect("external project");
        assert_ne!(launcher_cwd, external_project.path());

        assert_eq!(
            execution_root(external_project.path(), None),
            external_project.path(),
            "worker tools must use --project's root even when launched elsewhere"
        );
        let selected_worktree = external_project.path().join("worktree");
        assert_eq!(
            execution_root(external_project.path(), Some(&selected_worktree)),
            selected_worktree
        );
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

    /// `nav-fix-project-codeintel-freshness`, acceptance (b): `ProjectContextPackSource::compile`
    /// over a project that never ran `tm doctor` must still see a committed file's content —
    /// `build_retrieval`'s `ci.search_hybrid` reads from the `chunks`/`tokens` tables, which stay
    /// empty until some `update_incremental` call has run, so this only passes once `compile`
    /// refreshes the index itself before compiling.
    #[test]
    fn compile_over_a_never_doctored_project_includes_a_committed_files_content() {
        use tm_core::{ExecutorRequirements, RetryPolicy, TicketKind, VerificationPolicy};
        use tm_types::{Authority, Budget, ParticipantId, Tolerance};

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .expect("git should run in test environment");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "--quiet", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(
            root.join("widget.rs"),
            "fn compute_widget_total(count: i32) -> i32 { count * 2 }\n",
        )
        .expect("write widget.rs");
        run(&["add", "widget.rs"]);
        run(&["commit", "--quiet", "-m", "add compute_widget_total"]);

        let project = test_project(root);
        let events = project
            .store
            .create_ticket(
                TicketKind::Investigation,
                "find compute_widget_total".to_string(),
                None,
                None,
                Authority::root(),
                Vec::new(),
                ExecutorRequirements {
                    role: Role::CoderFast,
                    human_required: false,
                    min_capability: Tolerance::Preferred,
                },
                Vec::new(),
                Vec::new(),
                VerificationPolicy::None,
                Budget::unlimited(),
                RetryPolicy {
                    max_attempts: 1,
                    base_delay_seconds: 0,
                    backoff_multiplier: 1.0,
                    max_delay_seconds: 0,
                },
                0,
                ParticipantId::new("human:tester").expect("valid participant id"),
            )
            .expect("create ticket");
        let ticket_id = events
            .iter()
            .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.clone()))
            .expect("ticket.created event");

        // Never called `code_intel()`/`tm doctor` on this project before this.
        let source = ProjectContextPackSource {
            store: project.store.clone(),
            root: project.root.clone(),
            state_dir: project.state_dir.clone(),
            clock: project.clock.clone(),
        };
        let rendered = source
            .compile(&ticket_id)
            .expect("compile should refresh and succeed");
        assert!(
            rendered.contains("widget.rs"),
            "expected the never-doctored project's committed widget.rs to show up in the \
             compiled context pack via a freshly refreshed index, got:\n{rendered}"
        );
    }

    /// `critic-worktree-exec-root-indexing`, acceptance: a `tm run --worktree` dispatch's
    /// `CodeIntel` must index the worktree checkout it actually executes in, not the main
    /// checkout, with its own `index.db` so it never fights the main checkout's index for
    /// writes. Exercised at the level this file owns — `index_root_and_state_dir` and
    /// `open_and_refresh_code_intel`, the two helpers `build_dispatcher_with_fabric` and
    /// `ProjectContextPackSource::compile` both now go through — over a real `git worktree`
    /// whose branch has a file the main checkout never gets, the same shape `tm run --worktree`
    /// (D-012) sets up.
    #[test]
    fn worktree_exec_root_indexes_the_worktree_and_finds_its_own_only_file() {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path();

        let run = |dir: &std::path::Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .expect("git should run in test environment");
            assert!(
                status.success(),
                "git {:?} failed in {}",
                args,
                dir.display()
            );
        };
        run(root, &["init", "--quiet", "--initial-branch=main"]);
        run(root, &["config", "user.email", "test@example.com"]);
        run(root, &["config", "user.name", "Test"]);
        std::fs::write(
            root.join("main_only.rs"),
            "fn compute_main_checkout_marker() -> i32 { 1 }\n",
        )
        .expect("write main_only.rs");
        run(root, &["add", "main_only.rs"]);
        run(root, &["commit", "--quiet", "-m", "main checkout file"]);

        // A real `git worktree` branched off `main`, the same shape `tm run --worktree`'s
        // isolation sets up (D-012) — with a file the main checkout never gets. A second, separate
        // tempdir (rather than a path alongside `repo`) so it's cleaned up the same way `repo` is,
        // without a manual `git worktree remove`.
        let worktree_parent = tempfile::tempdir().expect("worktree tempdir");
        let worktree_dir = worktree_parent.path().join("worktree");
        run(
            root,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree_dir.to_str().expect("utf8 path"),
            ],
        );
        std::fs::write(
            worktree_dir.join("worktree_only.rs"),
            "fn find_worktree_only_marker() -> i32 { 2 }\n",
        )
        .expect("write worktree_only.rs");
        run(&worktree_dir, &["add", "worktree_only.rs"]);
        run(
            &worktree_dir,
            &["commit", "--quiet", "-m", "worktree-only file"],
        );

        let project = test_project(root);

        // Outside `--worktree`, `exec_root` is `project.root` and the index stays the main
        // checkout's own.
        let (main_root, main_state_dir) = index_root_and_state_dir(&project, &project.root);
        assert_eq!(main_root, project.root);
        assert_eq!(main_state_dir, project.state_dir);

        // Under `--worktree`, both the workspace `CodeIntel` indexes/searches and where its
        // `index.db` lives follow the worktree instead — and never collide with the main
        // checkout's own index.db.
        let (wt_root, wt_state_dir) = index_root_and_state_dir(&project, &worktree_dir);
        assert_eq!(wt_root, worktree_dir);
        assert_eq!(wt_state_dir, worktree_dir.join(".tm"));
        assert_ne!(
            wt_state_dir, main_state_dir,
            "a worktree run's index must not share the main checkout's index.db"
        );

        let ci = open_and_refresh_code_intel(&wt_state_dir, &wt_root, project.clock.as_ref())
            .expect("open and refresh the worktree's own index");
        let query = tm_codeintel::Query {
            text: "find_worktree_only_marker".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let hits = ci
            .search_hybrid(
                &query,
                &tm_codeintel::RetrievalContext::default(),
                tm_codeintel::SignalWeights::default(),
            )
            .expect("search_hybrid over the worktree's own index");
        assert!(
            hits.iter().any(|h| h.path.contains("worktree_only.rs")),
            "expected a search.*/symbol.* tool call inside a --worktree run to find the \
             worktree-only file via the worktree's own index, got: {hits:?}"
        );
    }
}
