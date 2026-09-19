//! Builds the [`tm_scheduler::ExecutorDispatcher`] that [`crate::sched::run_ticket`] (`tm run`)
//! and [`crate::sched::sched_run`] (`tm sched run`) both drive tickets through, instead of each
//! having its own ad hoc "compile a pack and do nothing" or "just tick" logic (`SPEC.md` §24,
//! audit B-01/B-04).

use std::io::{self, BufRead, Write as _};
use std::path::PathBuf;
use std::sync::Arc;

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
}

impl ContextPackSource for ProjectContextPackSource {
    fn compile(&self, ticket: &TicketId) -> tm_types::Result<String> {
        let view = self.store.view()?;
        let ticket_state = view
            .tickets
            .get(ticket)
            .ok_or_else(|| tm_types::TmError::not_found("ticket", ticket))?;
        let ci = tm_codeintel::CodeIntel::open(&self.root)?;
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

/// Build the dispatcher `tm run`/`tm sched run` share: a [`BuiltinExecutor`] registered for
/// every [`Role`] (the only shipped adapter today, per `SPEC.md` §24.3 — `claude-code`/`codex`/
/// `pi`/`opencode` are future adapters against the same [`tm_core::Executor`] trait) plus a
/// stdin-driven [`HumanExecutor`] for `human_required` tickets, spawning background runs onto
/// `handle`.
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

    let context: Arc<dyn ContextPackSource> = Arc::new(ProjectContextPackSource {
        store: project.store.clone(),
        root: project.root.clone(),
    });

    Ok(Arc::new(ExecutorDispatcher::new(
        project.store.clone(),
        handle,
        context,
        registry,
    )))
}
