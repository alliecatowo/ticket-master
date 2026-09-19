//! The interactive coding client bare `tm` opens, and its scriptable `tm -p <prompt>` form.
//!
//! This is the standalone-quality client `SPEC.md` §11/§15 calls out: a readline loop over
//! [`tm_agent`] that streams tool activity and model output as it happens, lets the human attach
//! work to tickets and decisions inline, and honours approval requests without leaving the
//! terminal. `tm -p <prompt>` is the same loop with the readline front end swapped for "run one
//! prompt to completion and exit", so it is scriptable and testable without a pty.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex};

use tm_agent::outcome::{
    AgentOutcome, AgentTask, BudgetDimension, PendingApproval, StepRecord, ToolCallRecord,
    ToolCallResolution,
};
use tm_agent::{AgentLoop, ToolRegistry};
use tm_codeintel::{CodeIntel, SignalWeights};
use tm_context::{
    compile, CommandCache, CommandExecutor, CommandResult, CommandSpec, ExecutionOutcome,
    TokenBudget,
};
use tm_core::{ExecutorRequirements, RetryPolicy, Ticket, TicketKind, VerificationPolicy};
use tm_provider::{AnthropicProvider, Fabric, ModelId, RoleTable};
use tm_types::{
    ArtifactId, Authority, Budget, Clock, IdKind, IdSource, ParticipantId, Role, SessionId,
    TicketId, Timestamp, TmError, Tolerance,
};

use crate::project::Project;
use crate::render::Renderer;

/// The concrete Anthropic model this session's coder role is bound to. `Fabric`'s provider
/// registry keys by provider slug rather than model, so this is the model every request this
/// session issues is actually served by, regardless of which candidate a role table names.
pub(crate) const AGENT_MODEL: &str = "claude-sonnet-5";

/// The role the interactive/scriptable session executes turns as: well-specified implementation
/// work, the common case for a human driving `tm` directly.
const AGENT_ROLE: Role = Role::CoderFast;

/// Per-turn spend ceiling for a scratch (unattached) ticket's ephemeral ticket record. An
/// attached ticket uses its own recorded budget instead.
fn default_turn_budget() -> Budget {
    Budget::new(200_000, 2_000_000, 600)
}

/// A minimal, effectively-inert retry policy for scratch tickets, which are never leased or
/// retried by the scheduler.
fn default_retry_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        base_delay_seconds: 0,
        backoff_multiplier: 1.0,
        max_delay_seconds: 0,
    }
}

/// The bare-`tm` interactive agent session: one readline loop, one [`tm_agent::agent_loop::AgentLoop`]
/// reused across turns so its prompt cache carries over, and a running notion of which ticket
/// (if any) the conversation is currently attached to.
pub struct AgentSession {
    /// The project the agent is operating in.
    project: Arc<Project>,
    /// The ticket the current conversation is attached to, if any; `None` means the agent is
    /// operating in a scratch/no-ticket mode (browsing, answering questions) until the human or
    /// the model attaches one.
    attached_ticket: Option<tm_types::TicketId>,
    /// How output is rendered.
    renderer: Renderer,
    /// The session identity every turn's tool calls and events are attributed to; stable across
    /// the lifetime of this `AgentSession` so a fresh worker reading the transcript back sees
    /// one coherent session rather than one per turn.
    session: SessionId,
}

impl AgentSession {
    /// Build a session over an opened project.
    pub fn new(project: Arc<Project>, renderer: Renderer) -> Self {
        let session_id = project.ids.next(IdKind::Session);
        // `IdSource::next` always renders a syntactically valid id for the kind requested, so
        // this can only fail if the id source itself is broken; falling back to `S-0` keeps
        // `new` infallible (its public signature returns `Self`, not a `Result`) without ever
        // panicking.
        let session = SessionId::new(session_id.as_str())
            .unwrap_or_else(|_| SessionId::new("S-0").expect("S-0 is a valid SessionId"));
        AgentSession {
            project,
            attached_ticket: None,
            renderer,
            session,
        }
    }

    /// Attach the session's work to `ticket`, so subsequent turns compile context and submit
    /// evidence against it.
    pub fn attach_ticket(&mut self, ticket: tm_types::TicketId) -> tm_types::Result<()> {
        let view = self.project.store.view()?;
        if !view.tickets.contains_key(&ticket) {
            return Err(TmError::not_found("ticket", ticket.as_str()));
        }
        self.attached_ticket = Some(ticket);
        Ok(())
    }

    /// Run the interactive readline loop until the human exits (`/exit`, ctrl-d, ctrl-c is
    /// treated the same as ctrl-d by the terminal delivering an interrupt, which surfaces here
    /// as a read error and also ends the loop).
    pub async fn run_interactive(&mut self) -> tm_types::Result<()> {
        let stdin = io::stdin();
        let mut lines = stdin.lock();
        let mut stdout = io::stdout();

        loop {
            self.renderer.note("");
            write!(stdout, "tm> ").ok();
            stdout.flush().ok();

            let mut input = String::new();
            let bytes_read = read_line(&mut lines, &mut input)?;
            if bytes_read == 0 {
                // EOF (ctrl-d): exit the loop the same as `/exit`.
                break;
            }
            let input = input.trim();
            if input.is_empty() {
                continue;
            }

            match parse_command(input) {
                Command::Exit => break,
                Command::Attach(ticket) => match self.attach_ticket(ticket.clone()) {
                    Ok(()) => self.renderer.note(&format!("attached to {ticket}")),
                    Err(e) => self.renderer.error(&e),
                },
                Command::Decide(text) => match self.record_decision(&text) {
                    Ok(id) => self.renderer.note(&format!("recorded {id}")),
                    Err(e) => self.renderer.error(&e),
                },
                Command::Unknown(cmd) => {
                    self.renderer.note(&format!(
                        "unrecognized command: {cmd} (try /attach, /decide, /exit)"
                    ));
                }
                Command::Turn(prompt) => {
                    if let Err(e) = self.run_turn(&prompt).await {
                        self.renderer.error(&e);
                    }
                }
            }
        }

        Ok(())
    }

    /// Run one prompt to completion non-interactively: the `tm -p <prompt>` form.
    pub async fn run_prompt(&mut self, prompt: &str) -> tm_types::Result<()> {
        self.run_turn(prompt).await
    }

    /// Record a `/decide <text>` slash command as a project decision, scoped to the attached
    /// ticket if one is set.
    fn record_decision(&self, text: &str) -> tm_types::Result<tm_types::DecisionId> {
        let subject = self
            .attached_ticket
            .as_ref()
            .map(|t| t.to_string())
            .unwrap_or_else(|| "session".to_string());
        let affected = self.attached_ticket.clone().into_iter().collect();
        let events = self.project.store.record_decision(
            subject,
            text.to_string(),
            "recorded via the interactive tm session".to_string(),
            Vec::new(),
            affected,
            Vec::new(),
            self.project.actor.clone(),
        )?;
        events
            .iter()
            .find_map(|e| e.payload.as_decision_created().map(|p| p.decision.clone()))
            .ok_or_else(|| {
                TmError::Invariant("record_decision did not emit decision.created".to_string())
            })
    }

    /// Resolve which ticket this turn executes against: the attached ticket if there is one,
    /// else a fresh scratch ticket whose objective is `prompt`.
    fn resolve_ticket(&mut self, prompt: &str) -> tm_types::Result<Ticket> {
        if let Some(ticket_id) = &self.attached_ticket {
            let view = self.project.store.view()?;
            return view
                .tickets
                .get(ticket_id)
                .cloned()
                .ok_or_else(|| TmError::not_found("ticket", ticket_id.as_str()));
        }

        let ticket_id = create_scratch_ticket(&self.project.store, prompt, &self.project.actor)?;
        self.attached_ticket = Some(ticket_id.clone());
        let view = self.project.store.view()?;
        view.tickets
            .get(&ticket_id)
            .cloned()
            .ok_or_else(|| TmError::not_found("ticket", ticket_id.as_str()))
    }

    /// Run one conversational turn: resolve or create the [`tm_agent::outcome::AgentTask`] this
    /// turn executes against, drive [`tm_agent::agent_loop::AgentLoop::run`], and reflect its
    /// [`tm_agent::outcome::AgentOutcome`] back to the human.
    #[allow(dead_code)]
    async fn run_turn(&mut self, prompt: &str) -> tm_types::Result<()> {
        let ticket = self.resolve_ticket(prompt)?;

        let view = self.project.store.view()?;
        let ci = CodeIntel::open(&self.project.root)?;
        let budget = TokenBudget::even(8_000);
        let context_pack = compile(&ticket, &view, &ci, budget, SignalWeights::default(), &[])?;

        let fabric = build_fabric(self.project.clock.clone())?;
        let command_cache: Arc<dyn CommandCache + Send + Sync> =
            Arc::new(MemoryCommandCache::new(self.project.ids.clone()));
        let command_executor: Arc<dyn CommandExecutor + Send + Sync> =
            Arc::new(ProcessCommandExecutor);

        // `docs/audit-2026-09-18-fable.md` B-02: register `tm-browser`/`tm-computer` alongside
        // the builtin tool set, the same `ToolRegistry::with_capabilities` seam
        // `tm_agent::executor::BuiltinExecutor::build` uses. `browser_handle`/`computer_handle`
        // are kept as concrete `Arc`s (not just registered as `dyn CapabilityProvider`) so this
        // turn's sessions can be torn down explicitly once it ends, mirroring
        // `BuiltinExecutor::execute`'s `SessionHandles::close_all`.
        let mut extra: Vec<Arc<dyn tm_types::CapabilityProvider>> = Vec::new();
        let browser_handle = crate::drive::optional_browser_wiring(&self.project)?.map(|wiring| {
            let registry = tm_browser::SessionRegistry::new(
                wiring.providers,
                wiring.sink,
                self.project.clock.clone(),
                tm_agent::tools::MAX_INLINE_RESULT_BYTES,
            );
            let capability = Arc::new(tm_browser::BrowserCapability::new(registry));
            extra.push(capability.clone() as Arc<dyn tm_types::CapabilityProvider>);
            capability
        });
        let computer_registry = tm_computer::ComputerSessionRegistry::new(
            tm_computer::SelectionEnv::from_process(),
            false,
            tm_computer::session::PanicStopConfig {
                abort_chord: None,
                mouse_move_threshold_px: 5.0,
            },
        );
        let computer_handle = Arc::new(tm_computer::ComputerCapability::new(computer_registry));
        extra.push(computer_handle.clone() as Arc<dyn tm_types::CapabilityProvider>);

        let tools = ToolRegistry::with_capabilities(
            Arc::new(ci),
            self.project.store.clone(),
            command_cache,
            command_executor,
            extra,
        );

        let mut agent_loop = AgentLoop::new(
            fabric,
            tools,
            Authority::root(),
            Budget::unlimited(),
            self.project.clock.clone(),
            self.project.ids.clone(),
            AGENT_ROLE,
            self.project.actor.clone(),
            self.project.store.clone(),
        );

        let task = AgentTask {
            ticket: ticket.id.clone(),
            context_pack,
            authority: ticket.authority.clone(),
            budget: ticket.budget,
            harness_epoch: 0,
            session: self.session.clone(),
        };

        let result = self.drive_turn(&mut agent_loop, task).await;

        // Torn down on every path (success or `?` propagation inside `drive_turn`), not just
        // the happy one — see this method's doc comment.
        if let Some(browser) = &browser_handle {
            if let Err(e) = browser.close_all().await {
                tracing::warn!(error = %e, "failed to close one or more browser sessions after this turn");
            }
        }
        if let Err(e) = computer_handle.close_all().await {
            tracing::warn!(error = %e, "failed to close one or more computer sessions after this turn");
        }

        result
    }

    /// The approval-suspend/resume loop `run_turn` drives, split out so `run_turn` can tear down
    /// this turn's browser/computer sessions after this returns regardless of how it returns
    /// (`?` inside this method propagates from *this* method, not from `run_turn`, which is the
    /// point of the split).
    async fn drive_turn(
        &mut self,
        agent_loop: &mut AgentLoop,
        task: AgentTask,
    ) -> tm_types::Result<()> {
        let mut outcome = agent_loop.run(task.clone()).await?;
        loop {
            self.renderer.note(&format_steps(outcome.steps()));
            match outcome {
                AgentOutcome::AwaitingApproval {
                    steps,
                    pending_call,
                } => {
                    self.renderer.note(&format_pending_approval(&pending_call));
                    let approved = prompt_approval_decision()?;
                    outcome = agent_loop
                        .resume(task.clone(), steps, pending_call, approved)
                        .await?;
                    continue;
                }
                other => {
                    self.renderer.note(&format_outcome_summary(&other));
                    return Ok(());
                }
            }
        }
    }
}

/// A locally-parsed line from the interactive readline loop.
#[derive(Debug, Clone, PartialEq)]
enum Command {
    /// `/attach T-42`
    Attach(TicketId),
    /// `/decide <text>`
    Decide(String),
    /// `/exit`
    Exit,
    /// A slash command this loop doesn't recognize.
    Unknown(String),
    /// Ordinary input: a prompt for the model.
    Turn(String),
}

/// Parse one line of interactive input into a [`Command`]. Anything not starting with `/` is a
/// [`Command::Turn`]; unrecognized slash commands become [`Command::Unknown`] rather than being
/// sent to the model, so a human's typo never becomes a confusing turn.
fn parse_command(line: &str) -> Command {
    let Some(rest) = line.strip_prefix('/') else {
        return Command::Turn(line.to_string());
    };
    let (name, arg) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (rest, ""),
    };
    match name {
        "exit" | "quit" => Command::Exit,
        "attach" => match arg.parse::<TicketId>() {
            Ok(id) => Command::Attach(id),
            Err(_) => Command::Unknown(line.to_string()),
        },
        "decide" if !arg.is_empty() => Command::Decide(arg.to_string()),
        _ => Command::Unknown(line.to_string()),
    }
}

/// Prompt the human for an approve/deny decision on stdin/stdout, accepting `y`/`yes`/`/approve`
/// to approve and `n`/`no`/`/deny` (with or without a trailing reason) to deny. Reprompts on
/// anything else. EOF is treated as a denial, since a script piping input that runs out mid-turn
/// should fail closed rather than silently approve.
fn prompt_approval_decision() -> tm_types::Result<bool> {
    let stdin = io::stdin();
    let mut lines = stdin.lock();
    let mut stdout = io::stdout();
    loop {
        write!(stdout, "approve? [y/n]: ").ok();
        stdout.flush().ok();
        let mut input = String::new();
        if read_line(&mut lines, &mut input)? == 0 {
            return Ok(false);
        }
        let trimmed = input.trim();
        if trimmed == "y" || trimmed == "yes" || trimmed == "/approve" {
            return Ok(true);
        }
        if trimmed == "n" || trimmed == "no" || trimmed == "/deny" || trimmed.starts_with("/deny ")
        {
            return Ok(false);
        }
    }
}

/// Read one line from `reader` into `buf`, mapping an I/O failure into `tm_types::Result`.
/// Returns the number of bytes read (`0` at EOF), matching `BufRead::read_line`.
fn read_line<R: BufRead>(reader: &mut R, buf: &mut String) -> tm_types::Result<usize> {
    reader.read_line(buf).map_err(TmError::from)
}

/// Build the fabric this session issues completions through: a single real Anthropic provider
/// bound to [`AGENT_MODEL`], registered against the workspace's default role table.
pub(crate) fn build_fabric(clock: Arc<dyn Clock>) -> tm_types::Result<Arc<Fabric>> {
    let table = RoleTable::default_table();
    let fabric = Fabric::new(table, clock.clone());
    let provider = AnthropicProvider::from_env(ModelId::new("anthropic", AGENT_MODEL), clock)
        .map_err(|e| TmError::Provider(e.to_string()))?;
    fabric.register_provider(Arc::new(provider));
    Ok(Arc::new(fabric))
}

/// Create a fresh scratch ticket for a turn that arrives with no attached ticket: an
/// `Investigation`-kind ticket (the one kind whose `VerificationPolicy::None` is legal to close
/// unverified), so an unattached conversational turn never trips the closed-needs-evidence
/// invariant just by existing.
fn create_scratch_ticket(
    store: &tm_core::Store,
    objective: &str,
    actor: &ParticipantId,
) -> tm_types::Result<TicketId> {
    let events = store.create_ticket(
        TicketKind::Investigation,
        objective.to_string(),
        None,
        None,
        Authority::root(),
        Vec::new(),
        ExecutorRequirements {
            role: AGENT_ROLE,
            human_required: false,
            min_capability: Tolerance::Preferred,
        },
        Vec::new(),
        Vec::new(),
        VerificationPolicy::None,
        default_turn_budget(),
        default_retry_policy(),
        0,
        actor.clone(),
    )?;
    events
        .iter()
        .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.clone()))
        .ok_or_else(|| TmError::Invariant("create_ticket did not emit ticket.created".to_string()))
}

/// Render every step's assistant text and tool calls, in order.
fn format_steps(steps: &[StepRecord]) -> String {
    let mut out = Vec::new();
    for step in steps {
        out.push(format_step(step));
    }
    out.join("\n")
}

/// Render one step: the model's text (if any) plus one line per tool call.
fn format_step(step: &StepRecord) -> String {
    let mut lines = Vec::new();
    if let Some(text) = &step.assistant_text {
        lines.push(text.clone());
    }
    for call in &step.tool_calls {
        lines.push(format_tool_call(call));
    }
    lines.join("\n")
}

/// Render one resolved tool call as a single summary line.
fn format_tool_call(call: &ToolCallRecord) -> String {
    match &call.resolution {
        ToolCallResolution::Completed { .. } => format!("  * {} -> ok", call.tool_name),
        ToolCallResolution::Denied { reason } => {
            format!("  * {} -> denied: {reason}", call.tool_name)
        }
        ToolCallResolution::Errored { detail } => {
            format!("  * {} -> error: {detail}", call.tool_name)
        }
    }
}

/// Render a terminal (non-`AwaitingApproval`) outcome's one-line summary.
fn format_outcome_summary(outcome: &AgentOutcome) -> String {
    match outcome {
        AgentOutcome::Submitted { evidence, .. } => format!("submitted: {}", evidence.summary),
        AgentOutcome::BudgetExhausted { exhausted, .. } => {
            format!("budget exhausted: {}", format_budget_dimension(*exhausted))
        }
        AgentOutcome::Failed { class, detail, .. } => format!("failed ({class:?}): {detail}"),
        AgentOutcome::AwaitingApproval { pending_call, .. } => {
            format_pending_approval(pending_call)
        }
    }
}

/// Render a pending approval request for the human to read before deciding.
fn format_pending_approval(pending: &PendingApproval) -> String {
    format!(
        "approval requested: {} - {}",
        pending.tool_name, pending.reason
    )
}

/// Which budget dimension tripped, as a lowercase word.
fn format_budget_dimension(dim: BudgetDimension) -> &'static str {
    match dim {
        BudgetDimension::Tokens => "tokens",
        BudgetDimension::Dollars => "dollars",
        BudgetDimension::WallSeconds => "wall_seconds",
    }
}

/// An in-process, non-durable [`CommandCache`]: correct within one `tm` invocation (a command
/// run twice in the same session is still deduplicated) but never persisted, unlike the
/// artifact-table-backed cache `SPEC.md` §8.2 describes for the durable multi-worker case.
pub(crate) struct MemoryCommandCache {
    ids: Arc<dyn IdSource>,
    results: Mutex<BTreeMap<String, CommandResult>>,
    artifacts: Mutex<BTreeMap<ArtifactId, Vec<u8>>>,
}

impl MemoryCommandCache {
    pub(crate) fn new(ids: Arc<dyn IdSource>) -> Self {
        MemoryCommandCache {
            ids,
            results: Mutex::new(BTreeMap::new()),
            artifacts: Mutex::new(BTreeMap::new()),
        }
    }

    fn mint_artifact(&self) -> tm_types::Result<ArtifactId> {
        ArtifactId::new(self.ids.next(IdKind::Artifact).as_str())
    }
}

impl CommandCache for MemoryCommandCache {
    fn get(&self, key: &str) -> tm_types::Result<Option<CommandResult>> {
        Ok(self
            .results
            .lock()
            .expect("command cache mutex poisoned")
            .get(key)
            .cloned())
    }

    fn put(
        &self,
        key: &str,
        argv: &[String],
        exit_code: i32,
        started: Timestamp,
        completed: Timestamp,
        stdout: &[u8],
        stderr: &[u8],
    ) -> tm_types::Result<CommandResult> {
        let stdout_artifact = self.mint_artifact()?;
        let stderr_artifact = self.mint_artifact()?;
        {
            let mut artifacts = self.artifacts.lock().expect("command cache mutex poisoned");
            artifacts.insert(stdout_artifact.clone(), stdout.to_vec());
            artifacts.insert(stderr_artifact.clone(), stderr.to_vec());
        }
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
            .expect("command cache mutex poisoned")
            .insert(key.to_string(), result.clone());
        Ok(result)
    }

    fn read_artifact(&self, id: &ArtifactId) -> tm_types::Result<Vec<u8>> {
        self.artifacts
            .lock()
            .expect("command cache mutex poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| TmError::not_found("artifact", id.as_str()))
    }
}

/// Runs a [`CommandSpec`]'s process via `std::process::Command`, honoring its working directory
/// and environment allowlist (no other ambient environment leaks into the child).
pub(crate) struct ProcessCommandExecutor;

impl CommandExecutor for ProcessCommandExecutor {
    fn execute(&self, spec: &CommandSpec) -> tm_types::Result<ExecutionOutcome> {
        let program = spec
            .argv
            .first()
            .ok_or_else(|| TmError::Invariant("command spec has an empty argv".to_string()))?;
        let mut cmd = StdCommand::new(program);
        cmd.args(&spec.argv[1..]);
        cmd.current_dir(&spec.cwd);
        cmd.env_clear();
        for key in &spec.env_allowlist {
            if let Ok(value) = std::env::var(key) {
                cmd.env(key, value);
            }
        }
        let output = cmd.output().map_err(TmError::from)?;
        Ok(ExecutionOutcome {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tm_types::{CounterIds, FixedClock, Timestamp};

    fn open_test_project(dir: &Path) -> Project {
        let store = Arc::new(tm_core::Store::open(dir).expect("open store"));
        Project {
            root: dir.to_path_buf(),
            store,
            clock: Arc::new(FixedClock::epoch()),
            ids: Arc::new(CounterIds::new()),
            actor: ParticipantId::new("human:tester").expect("valid participant id"),
        }
    }

    fn new_session(dir: &Path) -> AgentSession {
        let project = Arc::new(open_test_project(dir));
        AgentSession::new(project, Renderer::from_flags(false, true, true))
    }

    #[test]
    fn parse_command_recognizes_exit() {
        assert_eq!(parse_command("/exit"), Command::Exit);
        assert_eq!(parse_command("/quit"), Command::Exit);
    }

    #[test]
    fn parse_command_recognizes_attach_with_a_valid_ticket_id() {
        assert_eq!(
            parse_command("/attach T-42"),
            Command::Attach(TicketId::new("T-42").unwrap())
        );
    }

    #[test]
    fn parse_command_rejects_attach_with_a_malformed_ticket_id() {
        assert_eq!(
            parse_command("/attach nope"),
            Command::Unknown("/attach nope".to_string())
        );
    }

    #[test]
    fn parse_command_recognizes_decide() {
        assert_eq!(
            parse_command("/decide use sqlite for storage"),
            Command::Decide("use sqlite for storage".to_string())
        );
    }

    #[test]
    fn parse_command_rejects_decide_with_no_text() {
        assert_eq!(
            parse_command("/decide"),
            Command::Unknown("/decide".to_string())
        );
    }

    #[test]
    fn parse_command_treats_plain_text_as_a_turn() {
        assert_eq!(
            parse_command("fix the build"),
            Command::Turn("fix the build".to_string())
        );
    }

    #[test]
    fn parse_command_treats_unknown_slash_commands_as_unknown() {
        assert_eq!(
            parse_command("/nope"),
            Command::Unknown("/nope".to_string())
        );
    }

    #[test]
    fn attach_ticket_succeeds_for_an_existing_ticket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = new_session(dir.path());
        let ticket_id =
            create_scratch_ticket(&session.project.store, "do a thing", &session.project.actor)
                .expect("create ticket");
        session.attach_ticket(ticket_id.clone()).expect("attach");
        assert_eq!(session.attached_ticket, Some(ticket_id));
    }

    #[test]
    fn attach_ticket_fails_for_a_nonexistent_ticket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = new_session(dir.path());
        let missing = TicketId::new("T-999").unwrap();
        let err = session.attach_ticket(missing).unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }

    #[test]
    fn new_session_starts_unattached() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = new_session(dir.path());
        assert_eq!(session.attached_ticket, None);
    }

    #[test]
    fn create_scratch_ticket_returns_an_investigation_kind_ticket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = new_session(dir.path());
        let id = create_scratch_ticket(
            &session.project.store,
            "explore the repo",
            &session.project.actor,
        )
        .expect("create ticket");
        let view = session.project.store.view().expect("view");
        let ticket = view.tickets.get(&id).expect("ticket exists");
        assert_eq!(ticket.kind, TicketKind::Investigation);
        assert_eq!(ticket.objective, "explore the repo");
        assert_eq!(ticket.verification, VerificationPolicy::None);
    }

    #[test]
    fn record_decision_scopes_to_the_attached_ticket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = new_session(dir.path());
        let ticket_id =
            create_scratch_ticket(&session.project.store, "do a thing", &session.project.actor)
                .expect("create ticket");
        session.attach_ticket(ticket_id.clone()).expect("attach");
        let decision_id = session
            .record_decision("use approach A")
            .expect("record decision");
        let view = session.project.store.view().expect("view");
        let decision = view.decisions.get(&decision_id).expect("decision exists");
        assert!(decision.affected_tickets.contains(&ticket_id));
    }

    #[test]
    fn record_decision_without_an_attached_ticket_uses_session_subject() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = new_session(dir.path());
        let decision_id = session
            .record_decision("no ticket yet")
            .expect("record decision");
        let view = session.project.store.view().expect("view");
        let decision = view.decisions.get(&decision_id).expect("decision exists");
        assert!(decision.affected_tickets.is_empty());
    }

    #[test]
    fn resolve_ticket_creates_and_attaches_a_scratch_ticket_when_none_is_attached() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = new_session(dir.path());
        let ticket = session
            .resolve_ticket("investigate the flaky test")
            .expect("resolve");
        assert_eq!(ticket.objective, "investigate the flaky test");
        assert_eq!(session.attached_ticket, Some(ticket.id));
    }

    #[test]
    fn resolve_ticket_reuses_the_attached_ticket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = new_session(dir.path());
        let ticket_id = create_scratch_ticket(
            &session.project.store,
            "original objective",
            &session.project.actor,
        )
        .expect("create ticket");
        session.attach_ticket(ticket_id.clone()).expect("attach");
        let ticket = session
            .resolve_ticket("a different prompt")
            .expect("resolve");
        assert_eq!(ticket.id, ticket_id);
        assert_eq!(ticket.objective, "original objective");
    }

    #[test]
    fn memory_command_cache_round_trips_a_result() {
        let cache = MemoryCommandCache::new(Arc::new(CounterIds::new()));
        assert!(cache.get("k").unwrap().is_none());
        let result = cache
            .put(
                "k",
                &["echo".to_string(), "hi".to_string()],
                0,
                Timestamp::EPOCH,
                Timestamp::EPOCH,
                b"out",
                b"err",
            )
            .expect("put");
        assert!(!result.from_cache);
        let fetched = cache.get("k").unwrap().expect("hit");
        assert_eq!(fetched.exit_code, 0);
        assert_eq!(
            cache.read_artifact(&fetched.stdout_artifact).unwrap(),
            b"out"
        );
        assert_eq!(
            cache.read_artifact(&fetched.stderr_artifact).unwrap(),
            b"err"
        );
    }

    #[test]
    fn memory_command_cache_read_artifact_fails_for_an_unknown_id() {
        let cache = MemoryCommandCache::new(Arc::new(CounterIds::new()));
        let id = ArtifactId::new("ART-000000000000").unwrap();
        assert!(cache.read_artifact(&id).is_err());
    }

    #[test]
    fn process_executor_runs_a_real_command() {
        let executor = ProcessCommandExecutor;
        let spec = CommandSpec {
            argv: vec!["echo".to_string(), "hello".to_string()],
            cwd: ".".to_string(),
            env_allowlist: Vec::new(),
            declared_inputs: Vec::new(),
            cacheable: true,
            ticket: None,
            session: None,
        };
        let outcome = executor.execute(&spec).expect("execute");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "hello");
    }

    #[test]
    fn process_executor_fails_cleanly_for_an_empty_argv() {
        let executor = ProcessCommandExecutor;
        let spec = CommandSpec {
            argv: Vec::new(),
            cwd: ".".to_string(),
            env_allowlist: Vec::new(),
            declared_inputs: Vec::new(),
            cacheable: false,
            ticket: None,
            session: None,
        };
        assert!(executor.execute(&spec).is_err());
    }

    #[test]
    fn format_pending_approval_includes_tool_name_and_reason() {
        let pending = PendingApproval {
            tool_use_id: "call-1".to_string(),
            tool_name: "shell.run".to_string(),
            input: serde_json::json!({}),
            reason: "modifies a protected path".to_string(),
            requested_at: Timestamp::EPOCH,
        };
        let rendered = format_pending_approval(&pending);
        assert!(rendered.contains("shell.run"));
        assert!(rendered.contains("modifies a protected path"));
    }

    #[test]
    fn format_budget_dimension_covers_every_variant() {
        assert_eq!(format_budget_dimension(BudgetDimension::Tokens), "tokens");
        assert_eq!(format_budget_dimension(BudgetDimension::Dollars), "dollars");
        assert_eq!(
            format_budget_dimension(BudgetDimension::WallSeconds),
            "wall_seconds"
        );
    }

    #[test]
    fn read_line_returns_zero_at_eof() {
        let mut input: &[u8] = b"";
        let mut buf = String::new();
        assert_eq!(read_line(&mut input, &mut buf).unwrap(), 0);
    }

    #[test]
    fn read_line_returns_a_full_line() {
        let mut input: &[u8] = b"hello\n";
        let mut buf = String::new();
        let n = read_line(&mut input, &mut buf).unwrap();
        assert_eq!(n, 6);
        assert_eq!(buf, "hello\n");
    }
}
