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
use tm_codeintel::SignalWeights;
use tm_context::{
    compile, compile_session, CommandCache, CommandExecutor, CommandResult, CommandSpec,
    ExecutionOutcome, TokenBudget,
};
use tm_core::Ticket;
use tm_provider::{AnthropicProvider, DevPassProvider, Fabric, ModelId, RoleTable};
use tm_types::{
    ArtifactId, Authority, Budget, Clock, IdKind, IdSource, Role, SessionId, TicketId, Timestamp,
    TmError,
};

use crate::project::Project;
use crate::render::Renderer;

/// The model the Anthropic provider is built for when DevPass is not configured (see
/// [`build_fabric`]). Requests name the model the role table routed to, so this is only the
/// provider's fallback default.
pub(crate) const AGENT_MODEL: &str = "claude-sonnet-5";

/// The role the interactive/scriptable session executes turns as: well-specified implementation
/// work, the common case for a human driving `tm` directly.
const AGENT_ROLE: Role = Role::CoderFast;

/// Per-turn spend ceiling for a chat turn with no attached ticket (an attached ticket uses its own
/// recorded budget). Generous on purpose: sessions default to autonomous, and every step re-sends
/// the whole conversation so far, so token spend grows with session length. It is a runaway guard,
/// not a cost-control policy.
fn session_turn_budget() -> Budget {
    Budget::new(4_000_000, 20_000_000, 3_600)
}

/// Persists a pty session's recordings as project artifacts (`tm_pty` defines its own sink trait,
/// the same shape as `tm_agent::StoreArtifactSink`'s browser one).
struct PtyArtifactSink(Arc<tm_core::Store>);

impl tm_pty::ArtifactSink for PtyArtifactSink {
    fn store(&self, bytes: &[u8], content_type: &str) -> tm_types::Result<ArtifactId> {
        let events = self.0.store_artifact(
            tm_core::ArtifactKind::Report,
            content_type.to_string(),
            bytes.to_vec(),
            serde_json::json!({"source": "pty"}),
            None,
            tm_types::ParticipantId::system(),
        )?;
        events
            .iter()
            .find_map(|e| e.payload.as_artifact_created().map(|p| p.artifact.clone()))
            .ok_or_else(|| TmError::invariant("store_artifact did not emit artifact.created"))
    }
}

/// How far tm may go without asking. Shift+Tab in the TUI cycles these (D-019); `oversight.toml`
/// still applies on top of every mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// The default: tm acts on its own and asks about nothing beyond `oversight.toml`.
    #[default]
    Auto,
    /// Read-only: tm researches and proposes a plan, but can't edit files or run commands.
    Plan,
    /// tm asks before every edit, command, git operation, or keystroke into a pty.
    Ask,
}

impl PermissionMode {
    /// The next mode in the Shift+Tab cycle: auto, plan, ask, then auto again.
    pub fn next(self) -> Self {
        match self {
            PermissionMode::Auto => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::Ask,
            PermissionMode::Ask => PermissionMode::Auto,
        }
    }

    /// Lowercase display name (`"auto"`, `"plan"`, `"ask"`).
    pub fn label(self) -> &'static str {
        match self {
            PermissionMode::Auto => "auto",
            PermissionMode::Plan => "plan",
            PermissionMode::Ask => "ask",
        }
    }
}

/// Action classes [`PermissionMode::Ask`] asks about (prefixes, as in `oversight.toml`).
const ASK_MODE_CLASSES: &[&str] = &["repository.write", "shell", "git", "pty", "computer"];

/// What [`PermissionMode::Plan`] tells the model, appended to its system prompt.
const PLAN_MODE_NOTE: &str = "Plan mode is on. You can read and search, but you cannot edit \
files or run commands, and you should not try. Investigate what the request needs, then present \
a concise, concrete plan (the files and changes, and how you will verify them) and stop. The user \
will switch out of plan mode when they want it carried out.";

/// A human's answer to a permission prompt, in Claude Code's three-choice form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalAnswer {
    /// "Yes".
    Yes,
    /// "Yes, and don't ask again this session" (for this action class).
    YesForSession,
    /// "No, and tell tm what to do differently": `feedback` is what the model is told.
    No {
        /// The human's instruction, if they gave one.
        feedback: Option<String>,
    },
}

/// Answers permission prompts while a turn runs: the plain loop reads stdin, the TUI shows a
/// prompt and waits for the human.
#[async_trait::async_trait]
pub trait Approver: Send {
    /// Decide `pending`. `Err` aborts the turn.
    async fn decide(&mut self, pending: &PendingApproval) -> tm_types::Result<ApprovalAnswer>;
}

/// An [`Approver`] over a plain yes/no function.
pub struct SyncApprover<F>(pub F);

#[async_trait::async_trait]
impl<F> Approver for SyncApprover<F>
where
    F: FnMut(&PendingApproval) -> tm_types::Result<bool> + Send,
{
    async fn decide(&mut self, pending: &PendingApproval) -> tm_types::Result<ApprovalAnswer> {
        Ok(if (self.0)(pending)? {
            ApprovalAnswer::Yes
        } else {
            ApprovalAnswer::No { feedback: None }
        })
    }
}

/// Interrupts whatever turn a session is running (Esc in the TUI). Cloneable and usable without
/// the session itself, which is locked for the whole turn; a no-op when nothing is running.
#[derive(Debug, Clone, Default)]
pub struct TurnInterrupter(Arc<Mutex<Option<Arc<tokio::sync::Notify>>>>);

impl TurnInterrupter {
    /// Stop the running turn after the step in flight: its completed steps are kept and it ends
    /// as `AgentOutcome::Interrupted`.
    pub fn interrupt(&self) {
        if let Ok(slot) = self.0.lock() {
            if let Some(notify) = slot.as_ref() {
                notify.notify_one();
            }
        }
    }

    fn arm(&self) -> Arc<tokio::sync::Notify> {
        let notify = Arc::new(tokio::sync::Notify::new());
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(Arc::clone(&notify));
        }
        notify
    }

    fn disarm(&self) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = None;
        }
    }
}

/// A shell command the human ran directly (`!` in the TUI), and what it printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRun {
    /// The command line, as typed.
    pub command: String,
    /// Exit code (-1 when killed by a signal).
    pub exit_code: i32,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

/// One saved conversation, as `/resume` and `tm --resume` list them.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SessionSummary {
    /// The session id (`S-12`).
    pub id: SessionId,
    /// When it started.
    pub started: Timestamp,
    /// When its last turn finished.
    pub updated: Timestamp,
    /// How many turns it has.
    pub turns: usize,
    /// The first thing the human said.
    pub first_message: String,
    /// The last thing the human said.
    pub last_message: String,
}

/// The on-disk form of a conversation: `<state_dir>/sessions/<id>.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SavedSession {
    schema: u32,
    session: SessionId,
    started: Timestamp,
    updated: Timestamp,
    #[serde(default)]
    attached_ticket: Option<TicketId>,
    #[serde(default)]
    mode: PermissionMode,
    /// The `/model` choice, if the human made one.
    #[serde(default)]
    model: Option<ModelId>,
    turns: Vec<tm_agent::ConversationTurn>,
}

/// Every saved conversation in `project`, most recently active first.
pub fn list_sessions(project: &Project) -> tm_types::Result<Vec<SessionSummary>> {
    let dir = sessions_dir(project);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(saved) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<SavedSession>(&bytes).ok())
        else {
            continue;
        };
        let message = |turn: Option<&tm_agent::ConversationTurn>| {
            turn.map(|t| {
                t.user_message
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .unwrap_or_default()
        };
        out.push(SessionSummary {
            id: saved.session.clone(),
            started: saved.started,
            updated: saved.updated,
            turns: saved.turns.len(),
            first_message: message(saved.turns.first()),
            last_message: message(saved.turns.last()),
        });
    }
    out.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// `tm --resume` with no id: the saved conversations, newest first, one line each (or JSON).
pub fn print_sessions(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let sessions = list_sessions(project)?;
    let now = project.clock.now();
    let human = if sessions.is_empty() {
        "No saved conversations in this project yet.".to_string()
    } else {
        let mut lines: Vec<String> = sessions
            .iter()
            .map(|s| {
                format!(
                    "{:<6} {:>8}  {:>3} turn{}  {}",
                    s.id.as_str(),
                    format_age(now.seconds_since(s.updated)),
                    s.turns,
                    if s.turns == 1 { " " } else { "s" },
                    s.first_message
                )
            })
            .collect();
        lines.push(String::new());
        lines
            .push("Resume one with `tm --resume <id>`, or the latest with `tm --continue`.".into());
        lines.join("\n")
    };
    renderer.emit(&sessions, &human)
}

/// A compact "how long ago" (`12s`, `5m`, `3h`, `2d`).
fn format_age(seconds: i64) -> String {
    let s = seconds.max(0);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3_599 => format!("{}m ago", s / 60),
        3_600..=86_399 => format!("{}h ago", s / 3_600),
        _ => format!("{}d ago", s / 86_400),
    }
}

fn sessions_dir(project: &Project) -> std::path::PathBuf {
    project.state_dir.join("sessions")
}

/// The message a direct shell command becomes in the conversation, so the next turn sees what
/// the human ran and what it printed (Claude Code's `!` bash mode does the same).
fn shell_context_message(run: &ShellRun) -> String {
    let mut out = format!("<bash-input>{}</bash-input>\n", run.command);
    out.push_str(&format!(
        "<bash-exit-code>{}</bash-exit-code>\n",
        run.exit_code
    ));
    if !run.stdout.is_empty() {
        out.push_str(&format!(
            "<bash-stdout>{}</bash-stdout>\n",
            run.stdout.trim_end()
        ));
    }
    if !run.stderr.is_empty() {
        out.push_str(&format!(
            "<bash-stderr>{}</bash-stderr>\n",
            run.stderr.trim_end()
        ));
    }
    out
}

/// What `/compact` asks the model for.
const COMPACT_PROMPT: &str = "Summarize the conversation below so that it can continue from your \
summary alone. Keep everything needed to carry on the work: what the user asked for and why, \
decisions made, files read or changed (with paths), commands run and their results, errors and \
how they were fixed, what is done, what is still pending, and what was happening at the very end. \
Quote exact names, paths and values. Write it as notes, not a story, and don't add anything the \
conversation doesn't say.";

/// Heads the summary that replaces a compacted conversation, so the model knows what it's reading.
const COMPACTED_MARKER: &str =
    "[This conversation was compacted. A summary of everything before this point:]";

/// Once a turn's context reaches this many tokens, the next turn compacts the conversation first.
const AUTO_COMPACT_TOKENS: u64 = 150_000;

/// What [`AgentSession::compact`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    /// How many turns the summary replaced.
    pub turns: usize,
    /// The summary the conversation now starts from.
    pub summary: String,
    /// Tokens the summarizing call used.
    pub tokens: u64,
}

/// `turns` as plain text for summarizing: messages, replies, and each tool call with a bounded
/// view of its result.
fn conversation_transcript(turns: &[tm_agent::ConversationTurn]) -> String {
    const MAX_RESULT_CHARS: usize = 2_000;
    let mut out = String::new();
    for turn in turns {
        out.push_str(&format!("User: {}\n", turn.user_message));
        for step in &turn.steps {
            if let Some(text) = step
                .assistant_text
                .as_deref()
                .filter(|t| !t.trim().is_empty())
            {
                out.push_str(&format!("Assistant: {text}\n"));
            }
            for call in &step.tool_calls {
                let result = match &call.resolution {
                    ToolCallResolution::Completed { result, .. } => result.to_string(),
                    ToolCallResolution::Denied { reason } => format!("denied: {reason}"),
                    ToolCallResolution::Errored { detail } => format!("error: {detail}"),
                };
                let result = match result.char_indices().nth(MAX_RESULT_CHARS) {
                    Some((cut, _)) => format!("{}… [truncated]", &result[..cut]),
                    None => result,
                };
                out.push_str(&format!(
                    "Tool call {}({}) -> {result}\n",
                    call.tool_name, call.input
                ));
            }
        }
    }
    out
}

/// The prompt `/init` sends: write, or improve, the file of instructions tm (and other coding
/// agents) read at the start of every session in this project.
pub fn init_prompt(root: &std::path::Path) -> String {
    let target = ["AGENTS.md", "CLAUDE.md"]
        .into_iter()
        .find(|name| root.join(name).is_file());
    let task = match target {
        Some(name) => format!(
            "Read the existing {name} and improve it: correct anything the code no longer \
             supports and add what's missing."
        ),
        None => "Create an AGENTS.md file at the repository root.".to_string(),
    };
    format!(
        "{task} It's read at the start of every coding session in this repository, so it should \
         hold what a capable engineer new to this codebase needs and can't quickly get from \
         reading one file:\n\
         1. The commands to build, lint, and test, including how to run a single test.\n\
         2. The high-level architecture: how the main parts fit together and where they live.\n\
         3. Conventions and rules the code follows that aren't obvious from any one file.\n\n\
         Look at the build files, README, existing docs, and any tool config (.cursor/rules, \
         .github/copilot-instructions.md, CLAUDE.md, AGENTS.md) first. Keep it short. Don't list \
         every file, repeat generic advice, or invent anything you can't see in the repository."
    )
}

/// What the conversation shows the model after a turn the human interrupted.
const INTERRUPTED_MARKER: &str = "[Request interrupted by user]";

/// One thing that happened while [`AgentSession::run_turn_streaming`] ran a turn, for a caller
/// to reflect without owning [`AgentLoop`] itself. Deliberately small and independent of
/// `tm-tui`'s own `tm_tui::event::AppMessage` (this crate is where `tm_agent`/`tm_core` types
/// are available at all; `tm-tui` stays domain-agnostic per that enum's own module docs) — the
/// TUI's turn driver (`tui.rs`) translates each variant into an `AppMessage` at its own
/// boundary.
pub(crate) enum TurnEvent {
    /// The ticket this turn resolved to (scratch-created or reused), before any step runs — this
    /// is the "internal state" a caller can show a human immediately, before the model has said
    /// anything.
    // Boxed: `Ticket` is a few hundred bytes and this enum's other variants are much smaller
    // (`Steps`/`AwaitingApproval` are heap-indirect already via `Vec`/their own fields), so an
    // unboxed `Ticket` here would size every `TurnEvent` — including the common `Steps` case —
    // to the largest variant's footprint (`clippy::large_enum_variant`).
    TicketResolved(Box<Ticket>),
    /// The conversation had grown large enough that it was compacted before this turn ran.
    Compacted(Compaction),
    /// The cumulative steps of the run so far (assistant text plus tool calls), exactly as
    /// [`AgentOutcome::steps`] reports them at each point this fires: once after the whole turn
    /// completes with no approval needed, or once per suspend/resume round trip when one is.
    /// `AgentLoop::run`/`resume` only ever return once a turn is fully driven to a terminal or
    /// suspended state — there is no lower-latency, per-provider-call hook to stream from
    /// without a deeper change to `AgentLoop` itself (see `tui.rs` for how the TUI renders each
    /// of these steps individually despite that).
    Steps(Vec<StepRecord>),
    /// A tool call is awaiting approval. Fires once per suspension, immediately before the
    /// `approve` callback passed to `run_turn_streaming` is asked to decide it.
    AwaitingApproval(PendingApproval),
}

/// The bare-`tm` interactive agent session: one readline loop, the conversation so far (every
/// earlier turn's message and steps, replayed to the model on each new turn), and a running notion
/// of which ticket (if any) the conversation is currently attached to. A fresh
/// [`tm_agent::agent_loop::AgentLoop`] is built per turn; continuity lives in `conversation`.
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
    /// Every completed turn of this session, oldest first — what makes turn N see turns 1..N-1.
    conversation: Vec<tm_agent::ConversationTurn>,
    /// A pre-built fabric to use instead of [`build_fabric`], for in-process tests that need to
    /// script a provider and inspect exactly what it was sent.
    fabric_override: Option<Arc<Fabric>>,
    /// How far tm may go without asking.
    mode: PermissionMode,
    /// Action classes the human approved for the rest of this session.
    approved_for_session: std::collections::BTreeSet<String>,
    /// Stops the running turn from outside the session.
    interrupter: TurnInterrupter,
    /// When this conversation started (kept across `--resume`).
    started: Timestamp,
    /// The model the human picked with `/model`; `None` follows the role table.
    model: Option<ModelId>,
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
        let started = project.clock.now();
        AgentSession {
            started,
            project,
            attached_ticket: None,
            renderer,
            session,
            conversation: Vec::new(),
            fabric_override: None,
            mode: PermissionMode::default(),
            approved_for_session: std::collections::BTreeSet::new(),
            interrupter: TurnInterrupter::default(),
            model: None,
        }
    }

    /// Reopen a saved conversation (`tm --resume`, `/resume`): same session id, every earlier
    /// turn replayed to the model, the same attached ticket and permission mode.
    pub fn resume(
        project: Arc<Project>,
        renderer: Renderer,
        session: &SessionId,
    ) -> tm_types::Result<Self> {
        let path = sessions_dir(&project).join(format!("{session}.json"));
        let bytes =
            std::fs::read(&path).map_err(|_| TmError::not_found("session", session.as_str()))?;
        let saved: SavedSession = serde_json::from_slice(&bytes)?;
        let mut resumed = AgentSession::new(project, renderer);
        resumed.session = saved.session;
        resumed.started = saved.started;
        resumed.conversation = saved.turns;
        resumed.mode = saved.mode;
        resumed.attached_ticket = saved.attached_ticket;
        resumed.model = saved.model;
        Ok(resumed)
    }

    /// The most recently active saved conversation (`tm --continue`), if there is one.
    pub fn resume_latest(project: Arc<Project>, renderer: Renderer) -> tm_types::Result<Self> {
        let latest = list_sessions(&project)?
            .into_iter()
            .next()
            .ok_or_else(|| TmError::not_found("session", "(no saved conversations yet)"))?;
        AgentSession::resume(project, renderer, &latest.id)
    }

    /// The permission mode turns run under.
    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Change the permission mode for the following turns.
    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    /// The fabric a turn runs on: the scripted one a test installed, or one built from the
    /// environment, with the session's `/model` choice routed to first.
    fn fabric(&self) -> tm_types::Result<Arc<Fabric>> {
        let fabric = match &self.fabric_override {
            Some(fabric) => Arc::clone(fabric),
            None => build_fabric(self.project.clock.clone())?,
        };
        fabric.prefer(AGENT_ROLE, self.model.as_ref());
        Ok(fabric)
    }

    /// The model turns go to first: the `/model` choice, else the role table's primary.
    pub fn model(&self) -> tm_types::Result<Option<ModelId>> {
        Ok(self.fabric()?.candidates(AGENT_ROLE).into_iter().next())
    }

    /// What `/model` offers: this session's candidates whose provider is configured, in routing
    /// order.
    pub fn model_choices(&self) -> tm_types::Result<Vec<ModelId>> {
        let fabric = self.fabric()?;
        let configured = fabric.provider_ids();
        let mut choices: Vec<ModelId> = Vec::new();
        for model in fabric.candidates(AGENT_ROLE) {
            if configured.contains(&model.provider) && !choices.contains(&model) {
                choices.push(model);
            }
        }
        Ok(choices)
    }

    /// Switch models for the following turns (`/model <spec>`). `spec` is `provider/model`, a
    /// bare model name (one of [`AgentSession::model_choices`], else a model of the current
    /// model's provider), or `default` to go back to the role table's choice. Returns the model
    /// now in use.
    ///
    /// # Errors
    /// The named provider isn't configured, so a turn could never reach it.
    pub fn set_model(&mut self, spec: &str) -> tm_types::Result<Option<ModelId>> {
        let spec = spec.trim();
        if spec.is_empty() || spec.eq_ignore_ascii_case("default") {
            self.model = None;
            self.save_best_effort();
            return self.model();
        }
        let fabric = self.fabric()?;
        let configured = fabric.provider_ids();
        let chosen = match spec.split_once('/') {
            Some((provider, model)) if configured.iter().any(|p| p == provider) => {
                ModelId::new(provider, model)
            }
            Some((provider, _)) => {
                return Err(TmError::Provider(format!(
                    "no provider `{provider}` is configured (configured: {})",
                    configured.join(", ")
                )))
            }
            None => {
                let choices = self.model_choices()?;
                match choices.iter().find(|m| m.model == spec) {
                    Some(model) => model.clone(),
                    None => {
                        let current = choices.first().ok_or_else(|| {
                            TmError::Provider("no model provider is configured".into())
                        })?;
                        ModelId::new(current.provider.clone(), spec)
                    }
                }
            }
        };
        self.model = Some(chosen.clone());
        self.save_best_effort();
        Ok(Some(chosen))
    }

    /// Replace the conversation so far with a summary of it (`/compact`), so later turns carry
    /// a few thousand tokens of history instead of all of it. `instructions` says what the
    /// summary should focus on.
    ///
    /// # Errors
    /// There's nothing to compact yet, or the model call failed or came back empty; the
    /// conversation is left as it was.
    pub async fn compact(&mut self, instructions: Option<&str>) -> tm_types::Result<Compaction> {
        if self.conversation.is_empty() {
            return Err(TmError::conflict("nothing to compact yet"));
        }
        let mut prompt = format!(
            "{COMPACT_PROMPT}\n\n<conversation>\n{}</conversation>",
            conversation_transcript(&self.conversation)
        );
        if let Some(instructions) = instructions.filter(|i| !i.trim().is_empty()) {
            prompt.push_str(&format!(
                "\n\nThe user asked the summary to focus on: {}",
                instructions.trim()
            ));
        }
        let request = tm_provider::CompletionRequest {
            system: Some(
                "You summarize coding conversations so they can continue with less context.".into(),
            ),
            messages: vec![tm_provider::Message {
                role: tm_provider::MessageRole::User,
                content: vec![tm_provider::ContentBlock::Text { text: prompt }],
            }],
            tools: Vec::new(),
            max_tokens: 8_192,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: false,
            n: 1,
            model: None,
        };
        let completion = self.fabric()?.execute(AGENT_ROLE, request).await?;
        let summary = completion
            .candidates
            .first()
            .map(|c| {
                c.content
                    .iter()
                    .filter_map(|b| match b {
                        tm_provider::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let summary = summary.trim();
        if summary.is_empty() {
            return Err(TmError::Provider(
                "the model returned an empty summary; the conversation was left as it was".into(),
            ));
        }
        let turns = self.conversation.len();
        self.conversation = vec![tm_agent::ConversationTurn {
            user_message: format!("{COMPACTED_MARKER}\n\n{summary}"),
            steps: Vec::new(),
        }];
        self.save_best_effort();
        Ok(Compaction {
            turns,
            summary: summary.to_string(),
            tokens: u64::from(completion.usage.input_tokens)
                + u64::from(completion.usage.output_tokens),
        })
    }

    /// How many tokens the conversation's context took on its most recent model call: the size
    /// the next turn starts from.
    pub fn context_tokens(&self) -> u64 {
        self.conversation
            .iter()
            .rev()
            .find_map(|turn| turn.steps.last())
            .map_or(0, |step| step.spend.tokens)
    }

    /// A handle that interrupts this session's running turn (see [`TurnInterrupter`]). Take it
    /// before the turn starts; it keeps working while the session is locked.
    pub fn interrupter(&self) -> TurnInterrupter {
        self.interrupter.clone()
    }

    /// The ticket the conversation is attached to, if any.
    pub fn attached_ticket(&self) -> Option<&TicketId> {
        self.attached_ticket.as_ref()
    }

    /// Stop working against the attached ticket; the conversation continues ticketless.
    pub fn detach_ticket(&mut self) {
        self.attached_ticket = None;
        self.save_best_effort();
    }

    /// Run `command` directly in the project root (`!` in the TUI) and put the command and its
    /// output into the conversation, so the next turn can refer to it.
    pub async fn run_shell(&mut self, command: &str) -> tm_types::Result<ShellRun> {
        let spec = CommandSpec {
            argv: vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()],
            cwd: self.project.root.display().to_string(),
            env_allowlist: Vec::new(),
            declared_inputs: Vec::new(),
            cacheable: false,
            ticket: self.attached_ticket.clone(),
            session: Some(self.session.clone()),
        };
        let outcome = tokio::task::spawn_blocking(move || ProcessCommandExecutor.execute(&spec))
            .await
            .map_err(|e| TmError::Io(format!("shell command task failed: {e}")))??;
        let run = ShellRun {
            command: command.to_string(),
            exit_code: outcome.exit_code,
            stdout: String::from_utf8_lossy(&outcome.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&outcome.stderr).into_owned(),
        };
        self.conversation.push(tm_agent::ConversationTurn {
            user_message: shell_context_message(&run),
            steps: Vec::new(),
        });
        self.save_best_effort();
        Ok(run)
    }

    /// Hand the conversation's work to a background worker (`/bg`): a ticket whose objective is
    /// `instruction` (or, without one, the latest request) plus the recent conversation for
    /// context, queued so the scheduler picks it up. The conversation itself carries on.
    pub fn background(&mut self, instruction: Option<&str>) -> tm_types::Result<TicketId> {
        let request = instruction
            .map(str::to_string)
            .or_else(|| {
                self.conversation
                    .iter()
                    .rev()
                    .find(|t| !t.user_message.starts_with('<') && !t.user_message.starts_with('['))
                    .map(|t| t.user_message.clone())
            })
            .ok_or_else(|| {
                TmError::conflict("nothing to hand off yet: say what the background work is")
            })?;
        let objective = background_objective(&request, &self.conversation);
        let ticket = crate::tickets::create_worker_ticket(&self.project, &objective)?;
        self.project
            .store
            .activate(&ticket, self.project.actor.clone())?;
        Ok(ticket)
    }

    /// Save the conversation, logging rather than failing the turn if the disk write fails.
    fn save_best_effort(&self) {
        if let Err(e) = self.save() {
            tracing::warn!(error = %e, session = %self.session, "failed to save the conversation");
        }
    }

    fn save(&self) -> tm_types::Result<()> {
        if self.conversation.is_empty() {
            return Ok(());
        }
        let dir = sessions_dir(&self.project);
        std::fs::create_dir_all(&dir)?;
        let saved = SavedSession {
            schema: 1,
            session: self.session.clone(),
            started: self.started,
            updated: self.project.clock.now(),
            attached_ticket: self.attached_ticket.clone(),
            mode: self.mode,
            model: self.model.clone(),
            turns: self.conversation.clone(),
        };
        let path = dir.join(format!("{}.json", self.session));
        let tmp = dir.join(format!("{}.json.tmp", self.session));
        std::fs::write(&tmp, serde_json::to_vec(&saved)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Use `fabric` for every turn instead of building one from the environment.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with_fabric(mut self, fabric: Arc<Fabric>) -> Self {
        self.fabric_override = Some(fabric);
        self
    }

    /// The completed turns of this session so far, oldest first.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn conversation(&self) -> &[tm_agent::ConversationTurn] {
        &self.conversation
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

    /// The identity every tool call and evidence artifact this session's turns are attributed
    /// to. Exposed (read-only) so a caller driving this session from outside the plain readline
    /// loop — the TUI's turn driver, in `tm-cli`'s `tui.rs` — can match its own streamed output
    /// against the right session, e.g. `tm_tui::event::AppMessage::StreamChunk`'s `session`
    /// field.
    pub fn session_id(&self) -> &SessionId {
        &self.session
    }

    /// Run the interactive readline loop until the human exits (`/exit`, ctrl-d, ctrl-c is
    /// treated the same as ctrl-d by the terminal delivering an interrupt, which surfaces here
    /// as a read error and also ends the loop).
    pub async fn run_interactive(&mut self) -> tm_types::Result<()> {
        let stdin = io::stdin();
        let mut lines = stdin.lock();
        let mut stdout = io::stdout();

        // D-003: one line, once, before the first prompt, so a human sees up front whether this
        // session is writing into the repo or a global scope elsewhere — not something to
        // discover by surprise later.
        self.renderer.note(&self.project.scope_line());

        // `SessionStart` (`docs/audit-2026-09-18-fable.md` M-04): fires once, here, at the top
        // of the interactive loop — the `-p` one-shot form fires it at the top of `run_prompt`
        // instead, since that entry point never reaches this loop at all.
        self.fire_session_start().await;

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
                    if let Err(e) = self.run_turn(&prompt, true).await {
                        self.renderer.error(&e);
                    }
                }
            }
        }

        Ok(())
    }

    /// Run one prompt to completion non-interactively: the `tm -p <prompt>` form.
    ///
    /// Scripting contract: exits 0 when the turn replied or submitted its ticket's work, and
    /// otherwise returns the error the process exits with — [`TmError::TurnFailed`] (exit 2) when
    /// the agent didn't accomplish the task, [`TmError::BudgetExhausted`] (exit 4) when it ran out
    /// of budget. With `--json`, stdout gets one result object ([`turn_result_json`]) either way.
    pub async fn run_prompt(&mut self, prompt: &str) -> tm_types::Result<()> {
        // `SessionStart`: this entry point's session is exactly this one prompt, so it fires
        // here rather than in `run_interactive`'s loop (which this call never reaches).
        self.fire_session_start().await;
        let outcome = if self.renderer.is_json() {
            // No human at the keyboard to approve anything: suspensions are denied.
            let outcome = self
                .run_turn_streaming(prompt, |_| {}, |_| Ok(false))
                .await?;
            self.renderer.emit(
                &turn_result_json(&outcome, &self.session, self.attached_ticket.as_ref()),
                "",
            )?;
            outcome
        } else {
            self.run_turn(prompt, false).await?
        };
        outcome_result(&outcome)
    }

    /// Load `hooks.toml` (if present) and run its `session_start` entries, logging a load
    /// failure rather than failing the whole session over a malformed `hooks.toml` — matching
    /// `PostToolUse`/`Stop`'s own "observational only" contract (see `tm_agent::hooks`'s module
    /// doc comment): a session should still start even if its hooks config is broken.
    async fn fire_session_start(&self) {
        match tm_agent::hooks::load_hooks_toml(&self.project.root) {
            Ok(hooks) => hooks.run_session_start(&self.session).await,
            Err(e) => tracing::warn!(error = %e, "failed to load hooks.toml for SessionStart"),
        }
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

    /// Where this session is running, for the chat system prompt.
    fn chat_environment(&self, ticket: Option<&Ticket>) -> tm_agent::PromptEnvironment {
        let date = self.project.clock.now().to_rfc3339();
        tm_agent::PromptEnvironment {
            root: self.project.root.display().to_string(),
            platform: std::env::consts::OS.to_string(),
            date: date.get(..10).unwrap_or(&date).to_string(),
            scope: match self.project.scope {
                crate::project::Scope::Repo => "repo".to_string(),
                crate::project::Scope::Global => "global".to_string(),
            },
            attached_ticket: ticket.map(|t| t.id.to_string()),
        }
    }

    /// The ticket this turn executes against: the attached one, if any. A session with nothing
    /// attached runs ticketless — no ticket is created just because someone said something
    /// (`docs/decisions/D-017-session-ticket-executor-model.md`); the model creates tickets
    /// itself when the work warrants tracking.
    fn attached_ticket_record(&self) -> tm_types::Result<Option<Ticket>> {
        let Some(ticket_id) = &self.attached_ticket else {
            return Ok(None);
        };
        let view = self.project.store.view()?;
        view.tickets
            .get(ticket_id)
            .cloned()
            .map(Some)
            .ok_or_else(|| TmError::not_found("ticket", ticket_id.as_str()))
    }

    /// Run one conversational turn: resolve or create the [`tm_agent::outcome::AgentTask`] this
    /// turn executes against, drive [`tm_agent::agent_loop::AgentLoop::run`], and print its
    /// progress and [`tm_agent::outcome::AgentOutcome`] to `self.renderer` exactly as this loop
    /// always has.
    ///
    /// This is now a thin translation over [`AgentSession::run_turn_streaming`] — the shared
    /// turn-running logic (ticket resolution, context compilation, fabric/tool-registry/
    /// capability wiring, running the loop, approval suspend/resume) the TUI's turn driver
    /// (`tm-cli`'s `tui.rs`) also calls, so the two front ends can never drift apart on how a
    /// turn is actually executed. Every `self.renderer.note`/`error` call below reproduces this
    /// method's pre-refactor output byte for byte: `--plain`/piped-stdin behavior is unchanged.
    ///
    /// `report_failures` prints a failed or budget-exhausted turn's summary line; the `-p` path
    /// turns those into the process's error (and exit code) instead, so it passes `false`.
    async fn run_turn(
        &mut self,
        prompt: &str,
        report_failures: bool,
    ) -> tm_types::Result<AgentOutcome> {
        let renderer = self.renderer;
        let mut printed = 0usize;
        let outcome = self
            .run_turn_streaming(
                prompt,
                |event| match event {
                    // Ticket resolution prints nothing in the plain loop today; the TUI is the
                    // first caller that needs to react to it (refreshing its ticket pane).
                    TurnEvent::TicketResolved(_) => {}
                    TurnEvent::Compacted(compaction) => renderer.note(&format!(
                        "(compacted {} earlier turns to save context)",
                        compaction.turns
                    )),
                    // Cumulative: print only the steps not shown yet, as they arrive.
                    TurnEvent::Steps(steps) => {
                        for step in steps.iter().skip(printed) {
                            renderer.note(&format_step(step));
                        }
                        printed = steps.len();
                    }
                    TurnEvent::AwaitingApproval(pending) => {
                        renderer.note(&format_pending_approval(&pending))
                    }
                },
                |_pending| prompt_approval_decision(),
            )
            .await?;
        let is_failure = matches!(
            outcome,
            AgentOutcome::Failed { .. } | AgentOutcome::BudgetExhausted { .. }
        );
        let summary = format_outcome_summary(&outcome);
        if !summary.is_empty() && (report_failures || !is_failure) {
            self.renderer.note(&summary);
        }
        Ok(outcome)
    }

    /// The shared turn-running logic [`AgentSession::run_turn`] (the plain/`-p` loop) and the
    /// TUI's turn driver both call: resolve-or-create the scratch ticket, compile a context
    /// pack, build the fabric/tool registry (including the browser/computer capability wiring),
    /// construct and run [`AgentLoop`], and drive its approval suspend/resume loop to a terminal
    /// [`AgentOutcome`]. `on_event` is called synchronously as progress happens; `approve`
    /// decides any `AwaitingApproval` suspension (`Ok(true)` to approve, `Ok(false)` to deny,
    /// `Err` to abort the turn entirely — matching `prompt_approval_decision`'s own contract).
    ///
    /// # Errors
    /// Returns `Err` for an infrastructure failure (ticket resolution, context compilation,
    /// fabric construction, or `approve` itself erroring) — never for an in-band agent failure,
    /// which is represented as `Ok(AgentOutcome::Failed { .. })` (or the matching variant), the
    /// same contract [`AgentLoop::run`] documents.
    pub(crate) async fn run_turn_streaming(
        &mut self,
        prompt: &str,
        on_event: impl FnMut(TurnEvent),
        approve: impl FnMut(&PendingApproval) -> tm_types::Result<bool> + Send,
    ) -> tm_types::Result<AgentOutcome> {
        self.run_turn_with(prompt, on_event, &mut SyncApprover(approve))
            .await
    }

    /// [`AgentSession::run_turn_streaming`] with an async [`Approver`], so a UI can put a
    /// permission prompt on screen and answer it with any of [`ApprovalAnswer`]'s three choices.
    /// The turn honors the session's [`PermissionMode`] and can be stopped with
    /// [`AgentSession::interrupter`]. Every finished turn (interrupted ones too) is appended to
    /// the conversation and saved.
    pub(crate) async fn run_turn_with(
        &mut self,
        prompt: &str,
        mut on_event: impl FnMut(TurnEvent),
        approver: &mut dyn Approver,
    ) -> tm_types::Result<AgentOutcome> {
        // `hooks.toml` is loaded once per turn and threaded through: `UserPromptSubmit` here,
        // `PreToolUse`/`PostToolUse` via `ToolRegistry::with_hooks` below, `Stop` at the end
        // (`docs/audit-2026-09-18-fable.md` M-04). A malformed `hooks.toml` fails the turn the
        // same way a malformed `oversight.toml` already does (`crate::dispatch::load_oversight`)
        // — loud, not silently-ignored, since a hook a human configured for a reason
        // (e.g. a deny-shaped policy gate) silently not applying would be worse than the turn
        // erroring up front.
        let hooks = tm_agent::hooks::load_hooks_toml(&self.project.root)?;

        let prompt_owned = match hooks
            .evaluate_user_prompt_submit(prompt, &self.session)
            .await
        {
            tm_agent::hooks::HookDecision::Allow => prompt.to_string(),
            tm_agent::hooks::HookDecision::Rewrite(value) => value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| prompt.to_string()),
            tm_agent::hooks::HookDecision::Deny(reason) => {
                return Ok(AgentOutcome::Failed {
                    steps: Vec::new(),
                    class: tm_core::FailureClass::Other,
                    detail: format!("denied by UserPromptSubmit hook: {reason}"),
                });
            }
        };
        let prompt = prompt_owned.as_str();

        if self.context_tokens() >= AUTO_COMPACT_TOKENS {
            // Best effort: a failed summary leaves the conversation whole, and the turn still runs.
            match self.compact(None).await {
                Ok(compaction) => on_event(TurnEvent::Compacted(compaction)),
                Err(e) => tracing::warn!(error = %e, "auto-compact failed"),
            }
        }

        let ticket = self.attached_ticket_record()?;
        if let Some(ticket) = &ticket {
            on_event(TurnEvent::TicketResolved(Box::new(ticket.clone())));
        }

        let view = self.project.store.view()?;
        // D-003: goes through `Project::code_intel` (`CodeIntel::open_at(state_dir, root)`)
        // rather than `CodeIntel::open(root)` — the exact fix that stops this hot path (every
        // turn) from writing an index into the workspace when the project is global-scoped.
        let ci = self.project.code_intel()?;
        // A ticket's pack is its whole brief, so it gets room; a chat turn's is a head start the
        // model can extend with its own search tools, and every token of it rides along on every
        // step of the turn.
        let budget = TokenBudget::even(if ticket.is_some() { 8_000 } else { 3_000 });
        let context_pack = match &ticket {
            Some(ticket) => compile(
                ticket,
                &view,
                &ci,
                budget,
                SignalWeights::default(),
                &[],
                &RoleTable::default_table(),
            )?,
            None => compile_session(prompt, &view, &ci, budget, SignalWeights::default(), &[])?,
        };

        let fabric = self.fabric()?;
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

        // `pty.*`: drive interactive programs (REPLs, TUIs, anything that prompts) in a real
        // pseudo-terminal, authority-gated per `SPEC.md` §22.4. Torn down with the turn below.
        let pty_handle = Arc::new(tm_pty::PtyCapability::new(tm_pty::SessionRegistry::new(
            self.project.clock.clone(),
            Arc::new(PtyArtifactSink(self.project.store.clone())),
        )));
        extra.push(pty_handle.clone() as Arc<dyn tm_types::CapabilityProvider>);

        // `skill.load` (`docs/audit-2026-09-18-fable.md` M-04): a standalone `CapabilityProvider`
        // rather than a `tm_agent::tools::ToolName` variant — see `tm_agent::skill_capability`'s
        // module doc comment for why.
        extra.push(
            Arc::new(tm_agent::SkillCapability::new()) as Arc<dyn tm_types::CapabilityProvider>
        );

        let tools = ToolRegistry::with_capabilities(
            Arc::new(ci),
            self.project.store.clone(),
            command_cache,
            command_executor,
            extra,
        )
        .with_hooks(hooks.clone());

        let mut oversight = crate::dispatch::load_oversight(&self.project)?;
        if self.mode == PermissionMode::Ask {
            oversight
                .approval_required
                .extend(ASK_MODE_CLASSES.iter().map(|c| c.to_string()));
        }
        oversight
            .approved_for_session
            .extend(self.approved_for_session.iter().cloned());
        let (step_tx, mut step_rx) = tokio::sync::mpsc::unbounded_channel();
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
        )
        .with_oversight(oversight)
        .with_prompt_fragments({
            let mut fragments = tm_agent::chat_fragments(&self.chat_environment(ticket.as_ref()));
            if self.mode == PermissionMode::Plan {
                fragments.closing_reminder = PLAN_MODE_NOTE.to_string();
            }
            fragments
        })
        .with_step_sender(step_tx);

        let (authority, budget) = match &ticket {
            Some(ticket) => (ticket.authority.clone(), ticket.budget),
            None => (Authority::root(), session_turn_budget()),
        };
        let authority = match self.mode {
            PermissionMode::Plan => authority.intersect(&plan_mode_authority()),
            PermissionMode::Auto | PermissionMode::Ask => authority,
        };
        let task = AgentTask {
            ticket: ticket.as_ref().map(|t| t.id.clone()),
            context_pack,
            authority,
            budget,
            harness_epoch: 0,
            session: self.session.clone(),
            conversation: Some(tm_agent::Conversation {
                user_message: prompt.to_string(),
                prior_turns: self.conversation.clone(),
            }),
        };

        let interrupt = self.interrupter.arm();
        let result = self
            .drive_turn_streaming(
                &mut agent_loop,
                task,
                &mut step_rx,
                &interrupt,
                &mut on_event,
                approver,
            )
            .await;
        self.interrupter.disarm();

        // Torn down on every path (success or `?` propagation inside `drive_turn_streaming`),
        // not just the happy one — see this method's doc comment.
        if let Some(browser) = &browser_handle {
            if let Err(e) = browser.close_all().await {
                tracing::warn!(error = %e, "failed to close one or more browser sessions after this turn");
            }
        }
        if let Err(e) = computer_handle.close_all().await {
            tracing::warn!(error = %e, "failed to close one or more computer sessions after this turn");
        }
        if let Err(e) = pty_handle.close_all().await {
            tracing::warn!(error = %e, "failed to close one or more pty sessions after this turn");
        }

        // `Stop`: this turn is about to hand control back (to the human at the readline prompt,
        // or to the `-p` caller) — the "obvious lifecycle point" for it, and single-sited here
        // so it fires identically for `run_turn`/`run_prompt`/the TUI's turn driver, all of
        // which call this method.
        hooks
            .run_stop(&self.session, ticket.as_ref().map(|t| t.id.as_str()))
            .await;

        if let Ok(outcome) = &result {
            if outcome.is_terminal() {
                self.conversation.push(tm_agent::ConversationTurn {
                    user_message: prompt.to_string(),
                    steps: outcome.steps().to_vec(),
                });
                if matches!(outcome, AgentOutcome::Interrupted { .. }) {
                    self.conversation.push(tm_agent::ConversationTurn {
                        user_message: INTERRUPTED_MARKER.to_string(),
                        steps: Vec::new(),
                    });
                }
                self.save_best_effort();
            }
        }

        result
    }

    /// The approval-suspend/resume loop [`AgentSession::run_turn_streaming`] drives, split out
    /// so its caller can tear down this turn's browser/computer sessions after this returns
    /// regardless of how it returns (`?` inside this method propagates from *this* method, not
    /// from `run_turn_streaming`, which is the point of the split — unchanged from this method's
    /// pre-refactor shape as `drive_turn`, only its rendering replaced with `on_event`/`approve`
    /// callbacks).
    async fn drive_turn_streaming(
        &mut self,
        agent_loop: &mut AgentLoop,
        task: AgentTask,
        step_rx: &mut tokio::sync::mpsc::UnboundedReceiver<StepRecord>,
        interrupt: &tokio::sync::Notify,
        on_event: &mut impl FnMut(TurnEvent),
        approver: &mut dyn Approver,
    ) -> tm_types::Result<AgentOutcome> {
        let mut live = Vec::new();
        let mut outcome = report_steps_live(
            agent_loop.run(task.clone()),
            step_rx,
            interrupt,
            &mut live,
            on_event,
        )
        .await?;
        loop {
            match outcome {
                AgentOutcome::AwaitingApproval {
                    steps,
                    pending_call,
                } => {
                    on_event(TurnEvent::AwaitingApproval(pending_call.clone()));
                    let denial = match approver.decide(&pending_call).await? {
                        ApprovalAnswer::Yes => None,
                        ApprovalAnswer::YesForSession => {
                            // `reason` is the action class `Oversight::review` asked about.
                            self.approved_for_session
                                .insert(pending_call.reason.clone());
                            agent_loop.approve_for_session(pending_call.reason.clone());
                            None
                        }
                        ApprovalAnswer::No { feedback } => Some(match feedback {
                            Some(feedback) if !feedback.trim().is_empty() => format!(
                                "The user declined this action and said: {}",
                                feedback.trim()
                            ),
                            _ => "The user declined this action.".to_string(),
                        }),
                    };
                    outcome = report_steps_live(
                        agent_loop.resume_with(task.clone(), steps, pending_call, denial),
                        step_rx,
                        interrupt,
                        &mut live,
                        on_event,
                    )
                    .await?;
                    continue;
                }
                other => return Ok(other),
            }
        }
    }
}

/// Drive `run` to completion while forwarding each step [`AgentLoop::with_step_sender`] reports
/// as a cumulative [`TurnEvent::Steps`] the moment it arrives, so a front end shows progress
/// while the turn is still going. `live` accumulates across an approval suspension and resume, so
/// the whole turn reads as one growing transcript.
///
/// `interrupt` firing ends the turn as [`AgentOutcome::Interrupted`] with the steps completed so
/// far; the in-flight provider call or tool dispatch is dropped at its next await point.
async fn report_steps_live(
    run: impl std::future::Future<Output = tm_types::Result<AgentOutcome>>,
    step_rx: &mut tokio::sync::mpsc::UnboundedReceiver<StepRecord>,
    interrupt: &tokio::sync::Notify,
    live: &mut Vec<StepRecord>,
    on_event: &mut impl FnMut(TurnEvent),
) -> tm_types::Result<AgentOutcome> {
    tokio::pin!(run);
    loop {
        tokio::select! {
            biased;
            Some(step) = step_rx.recv() => {
                live.push(step);
                on_event(TurnEvent::Steps(live.clone()));
            }
            _ = interrupt.notified() => {
                while let Ok(step) = step_rx.try_recv() {
                    live.push(step);
                }
                return Ok(AgentOutcome::Interrupted { steps: live.clone() });
            }
            outcome = &mut run => {
                while let Ok(step) = step_rx.try_recv() {
                    live.push(step);
                    on_event(TurnEvent::Steps(live.clone()));
                }
                return outcome;
            }
        }
    }
}

/// The objective `/bg` gives the ticket it creates: the request, then enough of the conversation
/// for a worker with no other context to pick it up.
fn background_objective(request: &str, conversation: &[tm_agent::ConversationTurn]) -> String {
    const RECENT_TURNS: usize = 4;
    const EXCERPT_CHARS: usize = 600;
    let excerpt = |text: &str| -> String {
        let text = text.trim();
        if text.chars().count() <= EXCERPT_CHARS {
            text.to_string()
        } else {
            let cut: String = text.chars().take(EXCERPT_CHARS).collect();
            format!("{cut}…")
        }
    };
    let mut out = request.trim().to_string();
    let recent: Vec<&tm_agent::ConversationTurn> = conversation
        .iter()
        .rev()
        .take(RECENT_TURNS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if !recent.is_empty() {
        out.push_str("\n\nHanded off from an interactive conversation. Recent exchange:");
        for turn in recent {
            out.push_str(&format!("\n- user: {}", excerpt(&turn.user_message)));
            if let Some(reply) = turn
                .steps
                .iter()
                .rev()
                .find_map(|s| s.assistant_text.as_deref())
            {
                out.push_str(&format!("\n- tm: {}", excerpt(reply)));
            }
        }
    }
    out
}

/// The authority [`PermissionMode::Plan`] narrows a turn to: read and search the repository,
/// nothing else.
fn plan_mode_authority() -> Authority {
    let mut authority = Authority::none();
    authority.repository.read = tm_types::PatternSet::all();
    authority
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

/// The env var that switches [`build_fabric`] from a real `AnthropicProvider` to a deterministic
/// `MockProvider`, for an integration test that drives the *compiled* `tm` binary end to end
/// (spawned in a pty) without a network call or a real API key — e.g.
/// `crates/tm-cli/tests/tui_turn.rs`, which types a prompt into the TUI's chat input and asserts
/// on the resulting ticket/output. Out-of-process tests have no way to inject a Rust closure or
/// a pre-built `Fabric` into the child, so this is the one runtime hook that lets them exercise
/// the real turn-running path without hitting the network; nothing outside this module reads it,
/// and it has no effect unless a test explicitly sets it.
const TEST_MOCK_PROVIDER_ENV: &str = "TM_TEST_MOCK_PROVIDER";

/// Build the fabric this session issues completions through, registered against the workspace's
/// default role table ([`RoleTable::default_table`]) — or, when [`TEST_MOCK_PROVIDER_ENV`] is
/// set, a deterministic mock (see that constant's docs).
///
/// Provider selection, in order:
/// - `TEST_MOCK_PROVIDER_ENV` set: a scripted [`tm_provider::MockProvider`], see
///   [`build_mock_fabric`].
/// - `DEVPASS_API_KEY`/`DEVPASS_BASE_URL`/`DEVPASS_MODEL` all set (see
///   [`DevPassProvider::preferred_model`]): a [`DevPassProvider`] is registered under the
///   `devpass` slug — matching [`RoleTable::default_table`]'s own DevPass preference for
///   [`AGENT_ROLE`] — specifically *instead of requiring* `ANTHROPIC_API_KEY`, so a real `tm`
///   session can run end to end without touching Anthropic quota. `AnthropicProvider::from_env`
///   is still attempted best-effort and registered if it happens to succeed too (silently
///   ignored if not), so any *other* role a ticket names (`tm run`/`tm sched run` share this same
///   function, see `crates/tm-cli/src/dispatch.rs`) still gets Anthropic service if a key is
///   also present — only [`AGENT_ROLE`]'s own default candidate actually changes.
/// - Otherwise: a single real `AnthropicProvider` bound to [`AGENT_MODEL`], exactly as before
///   DevPass support existed. `AnthropicProvider::from_env`'s error (typically a missing
///   `ANTHROPIC_API_KEY`) is surfaced directly in this branch, unchanged.
pub(crate) fn build_fabric(clock: Arc<dyn Clock>) -> tm_types::Result<Arc<Fabric>> {
    if std::env::var_os(TEST_MOCK_PROVIDER_ENV).is_some() {
        return Ok(Arc::new(build_mock_fabric(clock)));
    }
    let table = RoleTable::default_table();
    let fabric = Fabric::new(table, clock.clone());

    if DevPassProvider::preferred_model().is_some() {
        let devpass = DevPassProvider::from_env(clock.clone())
            .map_err(|e| TmError::Provider(e.to_string()))?;
        fabric.register_provider(Arc::new(devpass));
        // Best-effort: a role other than AGENT_ROLE may still be routed to `anthropic` (that
        // part of the table is untouched by DevPass preference), so register it too if it
        // happens to be available — but never let its absence fail fabric construction, since
        // the whole point of DevPass preference is running without an Anthropic credential.
        if let Ok(anthropic) =
            AnthropicProvider::from_env(ModelId::new("anthropic", AGENT_MODEL), clock)
        {
            fabric.register_provider(Arc::new(anthropic));
        }
    } else {
        let provider = AnthropicProvider::from_env(ModelId::new("anthropic", AGENT_MODEL), clock)
            .map_err(|e| TmError::Provider(e.to_string()))?;
        fabric.register_provider(Arc::new(provider));
    }

    Ok(Arc::new(fabric))
}

/// The [`TEST_MOCK_PROVIDER_ENV`] fabric: a `mock`/`m1` candidate for [`AGENT_ROLE`] backed by
/// [`tm_provider::MockProvider`], scripted with a single default (any-request) text-only reply
/// so a turn always completes deterministically as `AgentOutcome::Failed { detail: "model ended
/// turn without submitting", .. }` after exactly one step — enough for a test to observe a real
/// ticket getting created and real step output reaching the screen, without needing to predict
/// the exact `CompletionRequest` `AgentLoop::drive` builds (which depends on the rendered system
/// prompt/context pack) the way an in-process `MockProvider::script_response` test would.
fn build_mock_fabric(clock: Arc<dyn Clock>) -> Fabric {
    let table = RoleTable::parse(
        "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
    )
    .expect("this crate's own static mock role table always parses");
    let fabric = Fabric::new(table, clock.clone());
    let model = ModelId::new("mock", "m1");
    let received_at = clock.now();
    let provider = tm_provider::MockProvider::new("mock", model.clone(), clock);
    provider.script_default_response(tm_provider::Completion {
        model,
        candidates: vec![tm_provider::Candidate {
            content: vec![tm_provider::ContentBlock::Text {
                text: "mock provider: this is a scripted reply for TM_TEST_MOCK_PROVIDER, not a \
                       real model turn."
                    .to_string(),
            }],
            stop_reason: tm_provider::StopReason::EndTurn,
        }],
        usage: tm_provider::Usage {
            input_tokens: 10,
            output_tokens: 10,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        latency: std::time::Duration::from_millis(0),
        received_at,
    });
    fabric.register_provider(Arc::new(provider));
    fabric
}

/// A finished turn as the process's result: `Ok` when it replied or submitted, otherwise the error
/// `tm -p` exits with (see [`AgentSession::run_prompt`]).
fn outcome_result(outcome: &AgentOutcome) -> tm_types::Result<()> {
    match outcome {
        AgentOutcome::Failed { class, detail, .. } => {
            Err(TmError::TurnFailed(format!("{class:?}: {detail}")))
        }
        AgentOutcome::BudgetExhausted { exhausted, .. } => Err(TmError::BudgetExhausted(
            format_budget_dimension(*exhausted).to_string(),
        )),
        AgentOutcome::Interrupted { .. } => Err(TmError::TurnFailed("interrupted".to_string())),
        AgentOutcome::Replied { .. }
        | AgentOutcome::Submitted { .. }
        | AgentOutcome::AwaitingApproval { .. } => Ok(()),
    }
}

/// The `tm --json -p` result object: how the turn ended, the reply, which model actually served
/// it, token spend, and every step's text and tool calls.
pub(crate) fn turn_result_json(
    outcome: &AgentOutcome,
    session: &SessionId,
    ticket: Option<&TicketId>,
) -> serde_json::Value {
    let steps = outcome.steps();
    let (status, text, detail) = match outcome {
        AgentOutcome::Replied { text, .. } => ("replied", Some(text.clone()), None),
        AgentOutcome::Submitted { evidence, .. } => {
            ("submitted", Some(evidence.summary.clone()), None)
        }
        AgentOutcome::BudgetExhausted { exhausted, .. } => (
            "budget_exhausted",
            None,
            Some(format_budget_dimension(*exhausted).to_string()),
        ),
        AgentOutcome::AwaitingApproval { pending_call, .. } => (
            "awaiting_approval",
            None,
            Some(format_pending_approval(pending_call)),
        ),
        AgentOutcome::Failed { class, detail, .. } => {
            ("failed", None, Some(format!("{class:?}: {detail}")))
        }
        AgentOutcome::Interrupted { .. } => ("interrupted", None, None),
    };
    let tool_status = |r: &ToolCallResolution| match r {
        ToolCallResolution::Completed { .. } => ("ok", None),
        ToolCallResolution::Denied { reason } => ("denied", Some(reason.clone())),
        ToolCallResolution::Errored { detail } => ("error", Some(detail.clone())),
    };
    serde_json::json!({
        "outcome": status,
        "text": text,
        "detail": detail,
        "session": session.as_str(),
        "ticket": ticket.map(TicketId::as_str),
        "model": steps.last().map(|s| s.served_by.as_str()),
        "tokens": steps.iter().map(|s| s.spend.tokens).sum::<u64>(),
        "steps": steps.iter().map(|step| serde_json::json!({
            "text": step.assistant_text,
            "model": step.served_by,
            "tool_calls": step.tool_calls.iter().map(|call| {
                let (status, detail) = tool_status(&call.resolution);
                serde_json::json!({
                    "tool": call.tool_name,
                    "input": call.input,
                    "status": status,
                    "detail": detail,
                })
            }).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// Render one step: the model's text (if any) plus one line per tool call.
pub(crate) fn format_step(step: &StepRecord) -> String {
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
pub(crate) fn format_tool_call(call: &ToolCallRecord) -> String {
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
pub(crate) fn format_outcome_summary(outcome: &AgentOutcome) -> String {
    match outcome {
        AgentOutcome::Submitted { evidence, .. } => format!("submitted: {}", evidence.summary),
        // The reply text itself was already rendered as the turn's final step.
        AgentOutcome::Replied { .. } => String::new(),
        AgentOutcome::BudgetExhausted { exhausted, .. } => {
            format!("budget exhausted: {}", format_budget_dimension(*exhausted))
        }
        AgentOutcome::Failed { class, detail, .. } => format!("failed ({class:?}): {detail}"),
        AgentOutcome::Interrupted { .. } => "interrupted".to_string(),
        AgentOutcome::AwaitingApproval { pending_call, .. } => {
            format_pending_approval(pending_call)
        }
    }
}

/// Render a pending approval request for the human to read before deciding.
pub(crate) fn format_pending_approval(pending: &PendingApproval) -> String {
    format!(
        "approval requested: {} - {}",
        pending.tool_name, pending.reason
    )
}

/// Which budget dimension tripped, as a lowercase word.
pub(crate) fn format_budget_dimension(dim: BudgetDimension) -> &'static str {
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

/// Runs a [`CommandSpec`]'s process via `std::process::Command` in its working directory.
///
/// Environment: a command the agent runs in someone's project needs that project's toolchain,
/// so an ordinary (non-cacheable) command inherits this process's environment (`PATH`, `HOME`,
/// locale, toolchain managers), minus [`tm_types::child_env::CREDENTIAL_ENV_VARS`]. A cacheable
/// command runs
/// hermetically instead: a cleared environment plus only `env_allowlist`, because its cached
/// result is keyed on argv and cwd alone and must not depend on ambient state.
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
        if spec.cacheable {
            cmd.env_clear();
            for key in &spec.env_allowlist {
                if let Ok(value) = std::env::var(key) {
                    cmd.env(key, value);
                }
            }
        } else {
            for key in tm_types::child_env::CREDENTIAL_ENV_VARS {
                cmd.env_remove(key);
            }
        }
        let output = cmd.output().map_err(|e| {
            TmError::Io(format!("failed to start `{program}` in {}: {e}", spec.cwd))
        })?;
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
    use tm_core::{ExecutorRequirements, RetryPolicy, TicketKind, VerificationPolicy};
    use tm_types::{CounterIds, FixedClock, ParticipantId, Timestamp, Tolerance};

    fn open_test_project(dir: &Path) -> Project {
        let store = Arc::new(tm_core::Store::open(dir).expect("open store"));
        Project::for_test(
            dir,
            store,
            Arc::new(FixedClock::epoch()),
            Arc::new(CounterIds::new()),
        )
    }

    /// A real, persisted ticket for tests that need one to attach to: an
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
            Budget::new(200_000, 2_000_000, 600),
            RetryPolicy {
                max_attempts: 1,
                base_delay_seconds: 0,
                backoff_multiplier: 1.0,
                max_delay_seconds: 0,
            },
            0,
            actor.clone(),
        )?;
        events
            .iter()
            .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.clone()))
            .ok_or_else(|| {
                TmError::Invariant("create_ticket did not emit ticket.created".to_string())
            })
    }

    fn new_session(dir: &Path) -> AgentSession {
        let project = Arc::new(open_test_project(dir));
        AgentSession::new(project, Renderer::from_flags(false, true, true))
    }

    /// A fabric whose only candidate is a `MockProvider` answering every request with `reply`,
    /// plus a handle to that provider so a test can read back exactly what it was sent.
    fn scripted_fabric(
        clock: Arc<dyn Clock>,
        reply: &str,
    ) -> (Arc<Fabric>, Arc<tm_provider::MockProvider>) {
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("static role table parses");
        let fabric = Fabric::new(table, clock.clone());
        let model = ModelId::new("mock", "m1");
        let received_at = clock.now();
        let provider = Arc::new(tm_provider::MockProvider::new("mock", model.clone(), clock));
        provider.script_default_response(tm_provider::Completion {
            model,
            candidates: vec![tm_provider::Candidate {
                content: vec![tm_provider::ContentBlock::Text {
                    text: reply.to_string(),
                }],
                stop_reason: tm_provider::StopReason::EndTurn,
            }],
            usage: tm_provider::Usage {
                input_tokens: 10,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: std::time::Duration::from_millis(0),
            received_at,
        });
        fabric.register_provider(provider.clone());
        (Arc::new(fabric), provider)
    }

    /// Every text block of every message in `request`, joined, for substring assertions.
    fn request_text(request: &tm_provider::CompletionRequest) -> String {
        request
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                tm_provider::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn each_turn_sends_its_own_prompt_plus_every_earlier_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = Arc::new(open_test_project(dir.path()));
        let (fabric, provider) = scripted_fabric(project.clock.clone(), "the answer is 42");
        let mut session =
            AgentSession::new(project, Renderer::from_flags(false, true, true)).with_fabric(fabric);

        let first = session
            .run_turn_streaming("first question alpha", |_| {}, |_| Ok(false))
            .await
            .expect("turn 1 runs");
        assert!(
            matches!(&first, AgentOutcome::Replied { text, .. } if text == "the answer is 42"),
            "a plain text reply is a successful chat turn, got {first:?}"
        );
        let second = session
            .run_turn_streaming("second question beta", |_| {}, |_| Ok(false))
            .await
            .expect("turn 2 runs");
        assert!(matches!(second, AgentOutcome::Replied { .. }), "{second:?}");

        let log = provider.call_log();
        assert_eq!(log.len(), 2, "one provider call per turn");
        let first_request = request_text(&log[0]);
        assert!(first_request.contains("first question alpha"));
        assert!(!first_request.contains("second question beta"));

        let second_request = request_text(&log[1]);
        assert!(
            second_request.contains("second question beta"),
            "turn 2's own message must reach the model: {second_request}"
        );
        assert!(
            second_request.contains("first question alpha"),
            "turn 1 is remembered"
        );
        assert!(
            second_request.contains("the answer is 42"),
            "turn 1's reply is remembered"
        );

        let roles: Vec<_> = log[1].messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                tm_provider::MessageRole::User,
                tm_provider::MessageRole::Assistant,
                tm_provider::MessageRole::User,
            ]
        );
        assert_eq!(session.conversation().len(), 2);
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

    #[tokio::test]
    async fn a_trivial_question_in_a_fresh_session_creates_no_ticket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = Arc::new(open_test_project(dir.path()));
        let (fabric, provider) = scripted_fabric(project.clock.clone(), "Paris.");
        let mut session =
            AgentSession::new(project.clone(), Renderer::from_flags(false, true, true))
                .with_fabric(fabric);

        let outcome = session
            .run_turn_streaming("what is the capital of France?", |_| {}, |_| Ok(false))
            .await
            .expect("turn runs");

        assert!(
            matches!(outcome, AgentOutcome::Replied { .. }),
            "{outcome:?}"
        );
        assert_eq!(session.attached_ticket, None, "a session is not a ticket");
        let view = project.store.view().expect("view");
        assert!(
            view.tickets.is_empty(),
            "saying something must not create a ticket, found {:?}",
            view.tickets.keys().collect::<Vec<_>>()
        );
        let first = request_text(&provider.call_log()[0]);
        assert!(first.contains("what is the capital of France?"));
        assert!(
            !first.contains("# Ticket"),
            "no ticket header without a ticket: {first}"
        );
        let system = provider.call_log()[0].system.clone().unwrap_or_default();
        assert!(
            system.contains("You are tm") && system.contains("has no attached ticket"),
            "a chat turn must carry the chat system prompt, got {system:?}"
        );
        let log = provider.call_log();
        let offered: Vec<&str> = log[0].tools.iter().map(|t| t.name.as_str()).collect();
        for tool in [
            "ticket.list",
            "ticket.transition",
            "ticket.create_child",
            "pty.spawn",
        ] {
            assert!(
                offered.contains(&tool),
                "{tool} must be offered, got {offered:?}"
            );
        }
    }

    #[tokio::test]
    async fn steps_are_reported_live_and_add_up_to_the_outcomes_transcript() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = Arc::new(open_test_project(dir.path()));
        let (fabric, _provider) = scripted_fabric(project.clock.clone(), "hello");
        let mut session =
            AgentSession::new(project, Renderer::from_flags(false, true, true)).with_fabric(fabric);

        let mut reported: Vec<Vec<StepRecord>> = Vec::new();
        let outcome = session
            .run_turn_streaming(
                "hi",
                |event| {
                    if let TurnEvent::Steps(steps) = event {
                        reported.push(steps);
                    }
                },
                |_| Ok(false),
            )
            .await
            .expect("turn runs");

        assert_eq!(reported.len(), outcome.steps().len(), "one report per step");
        assert_eq!(reported.last().map(Vec::as_slice), Some(outcome.steps()));
    }

    #[tokio::test]
    async fn the_json_result_reports_the_reply_model_and_tokens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = Arc::new(open_test_project(dir.path()));
        let (fabric, _provider) = scripted_fabric(project.clock.clone(), "done: 4");
        let mut session =
            AgentSession::new(project, Renderer::from_flags(true, true, true)).with_fabric(fabric);
        let outcome = session
            .run_turn_streaming("2+2?", |_| {}, |_| Ok(false))
            .await
            .expect("turn runs");

        let json = turn_result_json(&outcome, session.session_id(), None);
        assert_eq!(json["outcome"], "replied");
        assert_eq!(json["text"], "done: 4");
        assert_eq!(json["model"], "mock/m1");
        assert_eq!(json["tokens"], 20);
        assert_eq!(json["ticket"], serde_json::Value::Null);
        assert_eq!(json["steps"].as_array().map(Vec::len), Some(1));
        assert!(outcome_result(&outcome).is_ok());
    }

    #[test]
    fn a_failed_turn_is_an_exit_code_2_error_for_scripts() {
        let failed = AgentOutcome::Failed {
            steps: Vec::new(),
            class: tm_core::FailureClass::Other,
            detail: "step limit (64) reached".to_string(),
        };
        let err = outcome_result(&failed).expect_err("a failed turn is an error");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("step limit"), "{err}");

        let exhausted = AgentOutcome::BudgetExhausted {
            steps: Vec::new(),
            exhausted: BudgetDimension::Tokens,
        };
        assert_eq!(
            outcome_result(&exhausted).expect_err("budget").exit_code(),
            4
        );
    }

    /// One scripted provider reply: text, or a single tool call. `Hang` never answers, so a test
    /// can interrupt a turn while a provider call is in flight.
    enum Scripted {
        Text(&'static str),
        Tool(&'static str, serde_json::Value),
        /// A tool call whose input is built from the tool results the model has seen so far (every
        /// tool-result text in the request, joined), the way a model copies a value it was handed.
        ToolFrom(&'static str, fn(&str) -> serde_json::Value),
        Hang,
    }

    /// A provider that plays `script` in order (then answers "done" forever) and records every
    /// request it was sent.
    struct SequenceProvider {
        script: std::sync::Mutex<std::collections::VecDeque<Scripted>>,
        log: std::sync::Mutex<Vec<tm_provider::CompletionRequest>>,
        clock: Arc<dyn Clock>,
    }

    #[async_trait::async_trait]
    impl tm_provider::fabric::Provider for SequenceProvider {
        fn id(&self) -> &str {
            "mock"
        }

        async fn complete(
            &self,
            req: tm_provider::CompletionRequest,
        ) -> Result<tm_provider::Completion, tm_provider::ProviderError> {
            let served_by = req.model_or(&ModelId::new("mock", "m1"));
            let seen = tool_results(&req);
            self.log.lock().unwrap().push(req);
            let next = match self.script.lock().unwrap().pop_front() {
                Some(Scripted::ToolFrom(name, build)) => Some(Scripted::Tool(name, build(&seen))),
                other => other,
            };
            let (content, stop_reason) = match next {
                Some(Scripted::Hang) => {
                    tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                    unreachable!("a hanging call is always interrupted first")
                }
                Some(Scripted::ToolFrom(..)) => unreachable!("resolved to a Tool above"),
                Some(Scripted::Tool(name, input)) => {
                    let n = self.log.lock().unwrap().len();
                    (
                        vec![tm_provider::ContentBlock::ToolUse {
                            id: format!("call-{n}"),
                            name: name.to_string(),
                            input,
                        }],
                        tm_provider::StopReason::ToolUse,
                    )
                }
                Some(Scripted::Text(text)) => (
                    vec![tm_provider::ContentBlock::Text {
                        text: text.to_string(),
                    }],
                    tm_provider::StopReason::EndTurn,
                ),
                None => (
                    vec![tm_provider::ContentBlock::Text {
                        text: "done".to_string(),
                    }],
                    tm_provider::StopReason::EndTurn,
                ),
            };
            Ok(tm_provider::Completion {
                model: served_by,
                candidates: vec![tm_provider::Candidate {
                    content,
                    stop_reason,
                }],
                usage: tm_provider::Usage {
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
        ) -> Result<tm_provider::Embeddings, tm_provider::ProviderError> {
            Err(tm_provider::ProviderError::MalformedResponse(
                "embeddings are not scripted".into(),
            ))
        }
    }

    fn sequence_fabric(
        clock: Arc<dyn Clock>,
        script: Vec<Scripted>,
    ) -> (Arc<Fabric>, Arc<SequenceProvider>) {
        let table = RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"mock\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("static role table parses");
        let fabric = Fabric::new(table, clock.clone());
        let provider = Arc::new(SequenceProvider {
            script: std::sync::Mutex::new(script.into_iter().collect()),
            log: std::sync::Mutex::new(Vec::new()),
            clock,
        });
        fabric.register_provider(provider.clone());
        (Arc::new(fabric), provider)
    }

    /// Every tool-result text block in `request`, joined.
    fn tool_results(request: &tm_provider::CompletionRequest) -> String {
        request
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                tm_provider::ContentBlock::ToolResult { content, .. } => Some(content),
                _ => None,
            })
            .flat_map(|content| content.iter())
            .filter_map(|b| match b {
                tm_provider::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// An [`Approver`] that answers every prompt with `answer` and counts how often it was asked.
    struct CountingApprover {
        answer: ApprovalAnswer,
        asked: Vec<PendingApproval>,
    }

    #[async_trait::async_trait]
    impl Approver for CountingApprover {
        async fn decide(&mut self, pending: &PendingApproval) -> tm_types::Result<ApprovalAnswer> {
            self.asked.push(pending.clone());
            Ok(self.answer.clone())
        }
    }

    fn session_with(dir: &Path, script: Vec<Scripted>) -> (AgentSession, Arc<SequenceProvider>) {
        let project = Arc::new(open_test_project(dir));
        let (fabric, provider) = sequence_fabric(project.clock.clone(), script);
        let session =
            AgentSession::new(project, Renderer::from_flags(false, true, true)).with_fabric(fabric);
        (session, provider)
    }

    /// The last `"hash"` value in `seen` (tool-result text), as a model would copy it.
    fn last_hash(seen: &str) -> String {
        let at = seen
            .rfind("\"hash\"")
            .expect("a tool result carried a hash");
        seen[at + "\"hash\"".len()..]
            .chars()
            .skip_while(|c| !c.is_ascii_hexdigit())
            .take_while(char::is_ascii_hexdigit)
            .collect()
    }

    /// The live bug: fixing `return a - b`, a real model had no hash to pass `edit.apply_patch`,
    /// computed a sha256 itself, and burned ~175k tokens on conflicts. Now fs.read hands over the
    /// exact value, and a model that copies it gets its edit applied on the first try.
    #[tokio::test]
    async fn a_model_that_copies_fs_reads_hash_into_apply_patch_edits_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        let original = "def sub(a, b):\n    return a + b\n";
        std::fs::write(dir.path().join("src/calc.py"), original).expect("write");
        let (mut session, provider) = session_with(
            dir.path(),
            vec![
                Scripted::Tool("fs.read", serde_json::json!({"path": "src/calc.py"})),
                Scripted::ToolFrom("edit.apply_patch", |seen| {
                    let start = "def sub(a, b):\n    return a ".len();
                    serde_json::json!({
                        "path": "src/calc.py",
                        "edits": [{"byte_start": start, "byte_end": start + 1, "replacement": "-"}],
                        "expected_hash": last_hash(seen),
                    })
                }),
                Scripted::Text("fixed"),
            ],
        );
        let outcome = session
            .run_turn_streaming("fix sub", |_| {}, |_| Ok(false))
            .await
            .expect("turn runs");

        assert!(
            matches!(outcome, AgentOutcome::Replied { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/calc.py")).expect("read"),
            "def sub(a, b):\n    return a - b\n"
        );
        let log = provider.log.lock().unwrap();
        let patch_result = tool_results(&log[2]);
        assert!(
            patch_result.contains("\"applied\":true") && !patch_result.contains("conflict"),
            "{patch_result}"
        );
    }

    #[tokio::test]
    async fn ask_mode_asks_before_a_command_and_a_no_with_feedback_reaches_the_model() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut session, provider) = session_with(
            dir.path(),
            vec![
                Scripted::Tool("shell.run", serde_json::json!({"command": "echo hi"})),
                Scripted::Text("understood"),
            ],
        );
        session.set_mode(PermissionMode::Ask);
        let mut approver = CountingApprover {
            answer: ApprovalAnswer::No {
                feedback: Some("use printf instead".into()),
            },
            asked: Vec::new(),
        };
        let outcome = session
            .run_turn_with("say hi", |_| {}, &mut approver)
            .await
            .expect("turn runs");

        assert!(
            matches!(outcome, AgentOutcome::Replied { .. }),
            "{outcome:?}"
        );
        assert_eq!(approver.asked.len(), 1, "one command, one question");
        assert!(
            approver.asked[0].reason.starts_with("shell"),
            "{:?}",
            approver.asked[0]
        );
        let log = provider.log.lock().unwrap();
        assert!(
            tool_results(&log[1]).contains("use printf instead"),
            "the human's words are what the model sees: {}",
            tool_results(&log[1])
        );
    }

    #[tokio::test]
    async fn yes_for_the_session_stops_asking_about_that_kind_of_action() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut session, _provider) = session_with(
            dir.path(),
            vec![
                Scripted::Tool("shell.run", serde_json::json!({"command": "echo one"})),
                Scripted::Tool("shell.run", serde_json::json!({"command": "echo two"})),
                Scripted::Text("ran both"),
            ],
        );
        session.set_mode(PermissionMode::Ask);
        let mut approver = CountingApprover {
            answer: ApprovalAnswer::YesForSession,
            asked: Vec::new(),
        };
        let outcome = session
            .run_turn_with("run two things", |_| {}, &mut approver)
            .await
            .expect("turn runs");

        assert_eq!(approver.asked.len(), 1, "asked once, then remembered");
        let completed = outcome
            .steps()
            .iter()
            .flat_map(|s| s.tool_calls.iter())
            .filter(|c| matches!(c.resolution, ToolCallResolution::Completed { .. }))
            .count();
        assert_eq!(completed, 2, "both commands ran");
    }

    #[tokio::test]
    async fn plan_mode_offers_only_read_tools_and_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut session, provider) = session_with(dir.path(), vec![Scripted::Text("a plan")]);
        session.set_mode(PermissionMode::Plan);
        session
            .run_turn_streaming("plan the refactor", |_| {}, |_| Ok(false))
            .await
            .expect("turn runs");

        let log = provider.log.lock().unwrap();
        let offered: Vec<&str> = log[0].tools.iter().map(|t| t.name.as_str()).collect();
        assert!(offered.contains(&"fs.read"), "{offered:?}");
        for write in [
            "edit.write_file",
            "edit.apply_patch",
            "shell.run",
            "git.commit",
        ] {
            assert!(
                !offered.contains(&write),
                "{write} must not be offered in plan mode"
            );
        }
        assert!(log[0]
            .system
            .as_deref()
            .unwrap_or_default()
            .contains("Plan mode is on"));
    }

    #[tokio::test]
    async fn interrupting_keeps_the_finished_steps_and_tells_the_model_next_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut session, provider) = session_with(
            dir.path(),
            vec![
                Scripted::Tool("fs.list", serde_json::json!({"path": "."})),
                Scripted::Hang,
                Scripted::Text("ok"),
            ],
        );
        let interrupter = session.interrupter();
        let outcome = session
            .run_turn_streaming(
                "look around",
                |event| {
                    if let TurnEvent::Steps(steps) = event {
                        if steps.len() == 1 {
                            interrupter.interrupt();
                        }
                    }
                },
                |_| Ok(false),
            )
            .await
            .expect("turn runs");

        match &outcome {
            AgentOutcome::Interrupted { steps } => assert_eq!(steps.len(), 1),
            other => panic!("expected Interrupted, got {other:?}"),
        }
        session
            .run_turn_streaming("never mind", |_| {}, |_| Ok(false))
            .await
            .expect("next turn runs");
        let log = provider.log.lock().unwrap();
        let last = request_text(log.last().expect("a request"));
        assert!(last.contains(INTERRUPTED_MARKER), "{last}");
        assert!(last.contains("never mind"));
    }

    #[tokio::test]
    async fn conversations_are_saved_and_resume_exactly_where_they_left_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut session, _) = session_with(dir.path(), vec![Scripted::Text("noted")]);
        session.set_mode(PermissionMode::Plan);
        session
            .run_turn_streaming("remember the word pelican", |_| {}, |_| Ok(false))
            .await
            .expect("turn runs");
        let id = session.session_id().clone();
        let project = session.project.clone();

        let sessions = list_sessions(&project).expect("list");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert_eq!(sessions[0].first_message, "remember the word pelican");

        let resumed = AgentSession::resume(
            project.clone(),
            Renderer::from_flags(false, true, true),
            &id,
        )
        .expect("resume");
        assert_eq!(resumed.session_id(), &id);
        assert_eq!(resumed.mode(), PermissionMode::Plan);
        assert_eq!(resumed.conversation().len(), 1);

        let (fabric, provider) =
            sequence_fabric(project.clock.clone(), vec![Scripted::Text("pelican")]);
        let mut resumed = resumed.with_fabric(fabric);
        resumed
            .run_turn_streaming("what was the word?", |_| {}, |_| Ok(false))
            .await
            .expect("resumed turn runs");
        let log = provider.log.lock().unwrap();
        assert!(request_text(&log[0]).contains("remember the word pelican"));
    }

    #[tokio::test]
    async fn a_direct_shell_command_joins_the_conversation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut session, provider) = session_with(dir.path(), vec![Scripted::Text("seen it")]);
        let run = session
            .run_shell("echo tm-shell-check")
            .await
            .expect("shell runs");
        assert_eq!(run.exit_code, 0);
        assert_eq!(run.stdout.trim(), "tm-shell-check");

        session
            .run_turn_streaming("what did that print?", |_| {}, |_| Ok(false))
            .await
            .expect("turn runs");
        let text = request_text(&provider.log.lock().unwrap()[0]);
        assert!(
            text.contains("<bash-input>echo tm-shell-check</bash-input>"),
            "{text}"
        );
        assert!(text.contains("tm-shell-check</bash-stdout>"), "{text}");
    }

    #[tokio::test]
    async fn bg_hands_the_latest_request_to_a_queued_ticket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut session, _) = session_with(dir.path(), vec![Scripted::Text("sure")]);
        assert!(session.background(None).is_err(), "nothing to hand off yet");
        session
            .run_turn_streaming("refactor the parser into modules", |_| {}, |_| Ok(false))
            .await
            .expect("turn runs");

        let ticket = session.background(None).expect("handed off");
        let view = session.project.store.view().expect("view");
        let queued = &view.tickets[&ticket];
        assert_eq!(queued.state, tm_core::TicketState::Ready);
        assert!(queued
            .objective
            .starts_with("refactor the parser into modules"));
        assert!(queued
            .objective
            .contains("Handed off from an interactive conversation"));
        assert_eq!(queued.authority, Authority::worker());
    }

    #[tokio::test]
    async fn a_turn_runs_against_the_attached_ticket_without_creating_another() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = Arc::new(open_test_project(dir.path()));
        let (fabric, provider) = scripted_fabric(project.clock.clone(), "on it");
        let mut session =
            AgentSession::new(project.clone(), Renderer::from_flags(false, true, true))
                .with_fabric(fabric);
        let ticket_id = create_scratch_ticket(&project.store, "original objective", &project.actor)
            .expect("create ticket");
        session.attach_ticket(ticket_id.clone()).expect("attach");

        session
            .run_turn_streaming("a different prompt", |_| {}, |_| Ok(false))
            .await
            .expect("turn runs");

        assert_eq!(project.store.view().expect("view").tickets.len(), 1);
        let first = request_text(&provider.call_log()[0]);
        assert!(first.contains(&format!("# Ticket {ticket_id}")), "{first}");
        assert!(first.contains("original objective"));
        assert!(first.contains("a different prompt"));
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
    fn process_executor_runs_ordinary_commands_with_the_users_path_but_no_credentials() {
        std::env::set_var("DEVPASS_API_KEY", "sk-must-not-leak");
        let spec = CommandSpec {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo \"path=${PATH:+set} key=${DEVPASS_API_KEY:-absent}\"".to_string(),
            ],
            cwd: ".".to_string(),
            env_allowlist: Vec::new(),
            declared_inputs: Vec::new(),
            cacheable: false,
            ticket: None,
            session: None,
        };
        let outcome = ProcessCommandExecutor.execute(&spec).expect("execute");
        std::env::remove_var("DEVPASS_API_KEY");
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout).trim(),
            "path=set key=absent"
        );
    }

    #[test]
    fn process_executor_names_the_program_it_could_not_start() {
        let spec = CommandSpec {
            argv: vec!["definitely-not-a-real-program-xyz".to_string()],
            cwd: ".".to_string(),
            env_allowlist: Vec::new(),
            declared_inputs: Vec::new(),
            cacheable: false,
            ticket: None,
            session: None,
        };
        let err = ProcessCommandExecutor
            .execute(&spec)
            .expect_err("no such program");
        assert!(
            err.to_string()
                .contains("definitely-not-a-real-program-xyz"),
            "{err}"
        );
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

    // ---- build_fabric: DevPass default preference ----
    //
    // Deliberately never touches `ANTHROPIC_API_KEY` (reading or setting it in test code is a
    // hard hygiene violation — `crates/xtask/src/hygiene.rs`'s `check_network_in_tests`, "no
    // real credential dependency in tests"). This test's assertion doesn't need to: `build_fabric`
    // must succeed once DevPass is configured *regardless* of whatever Anthropic credential
    // state happens to be ambient, and `AGENT_ROLE`'s route must prefer `devpass` either way,
    // since `default_table`'s DevPass-preferred table always puts it first for `coder.fast`.
    //
    // No other test in this crate touches `DEVPASS_*`, but this lock guards against a future one
    // racing this test's env mutation under `cargo test`'s default multi-threaded runner (the
    // same convention `tm-provider`'s `frontier.rs` tests use).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_background_runner_works_queued_tickets_without_anything_else_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = Arc::new(open_test_project(dir.path()));
        let ticket = crate::tickets::create_worker_ticket(&project, "write the changelog")
            .expect("create ticket");
        project
            .store
            .activate(&ticket, project.actor.clone())
            .expect("activate");

        let runner = {
            let _guard = devpass_build_fabric_env_lock()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::env::set_var(TEST_MOCK_PROVIDER_ENV, "1");
            let runner = crate::sched::spawn_background_runner(
                project.clone(),
                std::time::Duration::from_millis(20),
            );
            std::env::remove_var(TEST_MOCK_PROVIDER_ENV);
            runner.expect("runner starts")
        };

        // The mock model only ever replies in text, so the attempt ends without a submission
        // and is retried; what matters here is that it was leased and attempted at all.
        let mut attempts = 0;
        for _ in 0..250 {
            attempts = project.store.view().expect("view").tickets[&ticket].attempts;
            if attempts >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        runner.abort();
        assert!(attempts >= 1, "the queued ticket was never picked up");
    }

    fn devpass_build_fabric_env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn clear_devpass_env() {
        for var in ["DEVPASS_API_KEY", "DEVPASS_BASE_URL", "DEVPASS_MODEL"] {
            std::env::remove_var(var);
        }
    }

    #[test]
    fn build_fabric_prefers_devpass_when_configured() {
        let _guard = devpass_build_fabric_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_devpass_env();
        std::env::set_var("DEVPASS_API_KEY", "sk-test");
        std::env::set_var("DEVPASS_BASE_URL", "https://example.invalid/devpass");
        std::env::set_var("DEVPASS_MODEL", "devpass-test-model");

        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let result = build_fabric(clock.clone());
        clear_devpass_env();

        let fabric = result.expect(
            "build_fabric must succeed once DevPass is configured, regardless of any ambient \
             Anthropic credential state",
        );

        let need = tm_provider::Need {
            tolerance: AGENT_ROLE.default_tolerance(),
            estimated_tokens: 100,
            max_cost_micros: None,
        };
        match fabric.route(AGENT_ROLE, &need, clock.now()) {
            tm_provider::RouteDecision::Use(model_id) => {
                assert_eq!(model_id.provider, "devpass");
                assert_eq!(model_id.model, "devpass-test-model");
            }
            other => panic!("expected AGENT_ROLE to route to devpass, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_picks_the_model_turns_go_to_and_survives_resume() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut session, provider) = session_with(dir.path(), vec![Scripted::Text("hi")]);
        assert_eq!(session.model().unwrap(), Some(ModelId::new("mock", "m1")));

        let chosen = session
            .set_model("m2")
            .expect("a model of the configured provider");
        assert_eq!(chosen, Some(ModelId::new("mock", "m2")));
        assert_eq!(
            session.model_choices().unwrap(),
            [ModelId::new("mock", "m2"), ModelId::new("mock", "m1")]
        );
        let outcome = session
            .run_turn_streaming("hello", |_| {}, |_| Ok(false))
            .await
            .expect("turn runs");
        assert_eq!(outcome.steps()[0].served_by, "mock/m2");
        assert_eq!(provider.log.lock().unwrap()[0].model.as_deref(), Some("m2"));

        let err = session
            .set_model("nowhere/x")
            .expect_err("unconfigured provider");
        assert!(err.to_string().contains("nowhere"), "{err}");

        let resumed = AgentSession::resume(
            Arc::clone(&session.project),
            Renderer::from_flags(false, true, true),
            session.session_id(),
        )
        .expect("resumes");
        assert_eq!(resumed.model, Some(ModelId::new("mock", "m2")));

        assert_eq!(
            session.set_model("default").unwrap(),
            Some(ModelId::new("mock", "m1"))
        );
    }

    #[tokio::test]
    async fn compact_replaces_the_conversation_with_the_models_summary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut session, provider) = session_with(
            dir.path(),
            vec![
                Scripted::Text("first reply"),
                Scripted::Text("THE SUMMARY"),
                Scripted::Text("carrying on"),
            ],
        );
        assert!(
            session.compact(None).await.is_err(),
            "nothing to compact yet"
        );
        session
            .run_turn_streaming("question alpha", |_| {}, |_| Ok(false))
            .await
            .expect("turn runs");

        let compaction = session
            .compact(Some("the alpha question"))
            .await
            .expect("compacts");
        assert_eq!(compaction.turns, 1);
        assert_eq!(compaction.summary, "THE SUMMARY");
        let asked = request_text(&provider.log.lock().unwrap()[1]);
        assert!(asked.contains("User: question alpha"), "{asked}");
        assert!(asked.contains("Assistant: first reply"), "{asked}");
        assert!(asked.contains("focus on: the alpha question"), "{asked}");
        assert!(
            provider.log.lock().unwrap()[1].tools.is_empty(),
            "a summary needs no tools"
        );

        session
            .run_turn_streaming("next question", |_| {}, |_| Ok(false))
            .await
            .expect("turn runs");
        let next = request_text(&provider.log.lock().unwrap()[2]);
        assert!(next.contains("THE SUMMARY"), "{next}");
        assert!(
            !next.contains("question alpha"),
            "the old turns are gone: {next}"
        );
    }

    #[tokio::test]
    async fn a_turn_compacts_first_once_the_context_is_large() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut session, provider) = session_with(
            dir.path(),
            vec![
                Scripted::Text("first reply"),
                Scripted::Text("THE SUMMARY"),
                Scripted::Text("carrying on"),
            ],
        );
        session
            .run_turn_streaming("question alpha", |_| {}, |_| Ok(false))
            .await
            .expect("turn runs");
        assert!(session.context_tokens() < AUTO_COMPACT_TOKENS);
        if let Some(step) = session.conversation[0].steps.last_mut() {
            step.spend.tokens = AUTO_COMPACT_TOKENS;
        }

        let mut compacted = None;
        session
            .run_turn_streaming(
                "next question",
                |event| {
                    if let TurnEvent::Compacted(c) = event {
                        compacted = Some(c);
                    }
                },
                |_| Ok(false),
            )
            .await
            .expect("turn runs");
        assert_eq!(compacted.map(|c| c.turns), Some(1));
        let turn = request_text(&provider.log.lock().unwrap()[2]);
        assert!(turn.contains("THE SUMMARY"), "{turn}");
        assert!(!turn.contains("question alpha"), "{turn}");
    }

    #[test]
    fn init_improves_an_existing_instructions_file_or_creates_agents_md() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(init_prompt(dir.path()).starts_with("Create an AGENTS.md"));
        std::fs::write(dir.path().join("CLAUDE.md"), "# notes\n").expect("write");
        assert!(init_prompt(dir.path()).starts_with("Read the existing CLAUDE.md"));
    }
}
