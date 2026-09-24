//! The chat half of the TUI's root [`App`]: running turns and `!` commands against the
//! [`AgentSession`], answering permission prompts, slash commands, prompt history, `@` file
//! completion, and resuming saved conversations (`docs/decisions/D-019-claude-code-parity-
//! shell.md` §1).
//!
//! Kept out of `tui.rs` so the chat's wiring and the screen routing (the tickets view) can change
//! independently. Everything here is an `impl App` block plus [`ChatExt`], the chat's own state
//! on the root.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use tm_tui::chat::approval::ApprovalChoice;
use tm_tui::chat::commands::CommandId;
use tm_tui::chat::mention::FileIndex;
use tm_tui::chat::picker::{ConversationRow, ProviderRow};
use tm_tui::chat::status::PermissionMode as UiMode;
use tm_tui::chat::transcript::{Entry, NoticeLevel, ToolCallView, ToolStatus};
use tm_tui::event::AppMessage;
use tm_tui::screens::chat::{ChatAction, ChatScreen, TurnUpdate};
use tm_types::{SessionId, TicketId, Timestamp};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// Every sibling module under `tui/` (`config_cmd`, `steps`, `tickets_view`) is declared with
// `mod <name>;` in `tui.rs` instead. `slash_views` is declared here with an explicit `#[path]`
// instead, so this file's only consumer of it works without a `tui.rs` edit; an integrator should
// fold this into a plain `mod slash_views;` in `tui.rs` and drop this attribute.
#[path = "slash_views.rs"]
mod slash_views;

use super::{config_cmd, steps, App};
use crate::agent::{self, AgentSession, ApprovalAnswer, Approver, PermissionMode, TurnInterrupter};
use crate::auth::{env_setup_rows, format_auth_instructions};
use crate::project::Project;
use crate::render::Renderer;

/// The most files `@` completion indexes; past this a project is large enough that a prefix
/// narrows it better than a longer list would.
const MAX_INDEXED_FILES: usize = 50_000;

/// The most history entries read back at startup.
const HISTORY_LOAD_LIMIT: usize = 500;

/// The chat's state on the root [`App`].
pub(super) struct ChatExt {
    /// `<state_dir>/prompt-history.jsonl`: every prompt, one JSON object per line.
    history_path: PathBuf,
    /// Interrupts the running turn (Esc / Ctrl+C); replaced with the session.
    interrupter: TurnInterrupter,
    /// The running `!` command, so Esc can abandon it.
    shell_task: Option<JoinHandle<()>>,
    /// The running `/compact`, which cannot be abandoned midway.
    task: Option<JoinHandle<()>>,
    /// Tickets this conversation handed to the background (`/bg`), for the Ctrl+T checklist.
    backgrounded: Vec<TicketId>,
    /// Where the answer to the open permission prompt goes, and what it was about.
    approval: Arc<StdMutex<Option<PendingAnswer>>>,
}

/// The permission prompt waiting for the human.
struct PendingAnswer {
    reply: oneshot::Sender<ApprovalChoice>,
}

impl ChatExt {
    /// Set the chat up: load persisted prompt history, start indexing files for `@`, and show a
    /// resumed conversation's earlier turns and mode.
    pub(super) fn start(project: &Project, session: &AgentSession, chat: &mut ChatScreen) -> Self {
        let history_path = project.state_dir.join("prompt-history.jsonl");
        chat.load_history(read_history(&history_path, HISTORY_LOAD_LIMIT));
        chat.set_file_index(index_files(project.root.clone(), project.state_dir.clone()));
        show_conversation(chat, session);
        ChatExt {
            history_path,
            interrupter: session.interrupter(),
            shell_task: None,
            task: None,
            backgrounded: Vec::new(),
            approval: Arc::default(),
        }
    }
}

fn to_agent_mode(mode: UiMode) -> PermissionMode {
    match mode {
        UiMode::Auto => PermissionMode::Auto,
        UiMode::Plan => PermissionMode::Plan,
        UiMode::Ask => PermissionMode::Ask,
    }
}

fn to_ui_mode(mode: PermissionMode) -> UiMode {
    match mode {
        PermissionMode::Auto => UiMode::Auto,
        PermissionMode::Plan => UiMode::Plan,
        PermissionMode::Ask => UiMode::Ask,
    }
}

/// Put `session`'s saved turns and mode on screen (a resumed conversation).
fn show_conversation(chat: &mut ChatScreen, session: &AgentSession) {
    let entries = steps::conversation_entries(session.conversation());
    if !entries.is_empty() {
        for entry in entries {
            chat.push_entry(entry);
        }
        chat.push_notice(
            NoticeLevel::Info,
            format!("Resumed {}. Earlier turns are above.", session.session_id()),
        );
    }
    chat.set_mode(to_ui_mode(session.mode()));
    chat.status_mut().ticket = session.attached_ticket().map(|t| t.to_string());
    // The model turns will actually go to (a resumed conversation's `/model` choice included).
    if let Ok(Some(model)) = session.model() {
        chat.show_model(model.to_string());
    }
    chat.status_mut().context_tokens = session.context_tokens();
}

/// One persisted history line.
#[derive(serde::Serialize, serde::Deserialize)]
struct HistoryLine {
    text: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    at: Option<Timestamp>,
}

/// The newest `limit` prompts in `path`, oldest first. A missing or partly corrupt file yields
/// what can be read.
fn read_history(path: &Path, limit: usize) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut texts: Vec<String> = contents
        .lines()
        .filter_map(|line| serde_json::from_str::<HistoryLine>(line).ok())
        .map(|h| h.text)
        .collect();
    let skip = texts.len().saturating_sub(limit);
    texts.split_off(skip)
}

fn append_history(path: &Path, entries: &[String], session: &SessionId, at: Timestamp) {
    if entries.is_empty() {
        return;
    }
    let mut out = String::new();
    for text in entries {
        let line = HistoryLine {
            text: text.clone(),
            session: Some(session.to_string()),
            at: Some(at),
        };
        if let Ok(json) = serde_json::to_string(&line) {
            out.push_str(&json);
            out.push('\n');
        }
    }
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(out.as_bytes()));
    if let Err(e) = written {
        tracing::warn!(error = %e, path = %path.display(), "could not save prompt history");
    }
}

/// Walk `root` on a background thread (honouring `.gitignore`, skipping hidden files and the
/// project's own state directory) and fill the returned index when done.
fn index_files(root: PathBuf, state_dir: PathBuf) -> FileIndex {
    let index: FileIndex = Arc::new(OnceLock::new());
    let fill = Arc::clone(&index);
    std::thread::spawn(move || {
        let mut files = Vec::new();
        let walker = ignore::WalkBuilder::new(&root)
            .standard_filters(true)
            .require_git(false)
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            if path.starts_with(&state_dir) {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(&root) {
                files.push(rel.to_string_lossy().replace('\\', "/"));
            }
            if files.len() >= MAX_INDEXED_FILES {
                break;
            }
        }
        files.sort();
        let _ = fill.set(files);
    });
    index
}

/// `just now`, `5m ago`, `3h ago`, `2d ago`.
fn age(millis: i64) -> String {
    let secs = millis.max(0) / 1000;
    match secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", secs / 60),
        3_600..=86_399 => format!("{}h ago", secs / 3_600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Answers permission prompts by showing Claude Code's numbered prompt in the chat and waiting
/// for the human's choice.
struct UiApprover {
    sender: tm_tui::runtime::MessageSender,
    session: SessionId,
    slot: Arc<StdMutex<Option<PendingAnswer>>>,
    /// How many steps the turn had reported when each answer was given, and the transcript's
    /// record of it, so the answer shows where the question came up.
    answers: Arc<StdMutex<Vec<(usize, Entry)>>>,
    steps_seen: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Approver for UiApprover {
    async fn decide(
        &mut self,
        pending: &tm_agent::outcome::PendingApproval,
    ) -> tm_types::Result<ApprovalAnswer> {
        let (reply, answer) = oneshot::channel();
        let request = steps::approval_request(pending);
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(PendingAnswer { reply });
        }
        self.sender.send(AppMessage::Turn {
            session: self.session.clone(),
            update: TurnUpdate::AwaitingApproval(request.clone()),
        });
        // A dropped sender (the chat was torn down) is a "no".
        let choice = answer.await.unwrap_or(ApprovalChoice::No);
        if let Ok(mut answers) = self.answers.lock() {
            answers.push((
                self.steps_seen.load(std::sync::atomic::Ordering::SeqCst),
                steps::approval_notice(&request.tool, &request.target, choice),
            ));
        }
        Ok(match choice {
            ApprovalChoice::Yes => ApprovalAnswer::Yes,
            ApprovalChoice::YesForSession => ApprovalAnswer::YesForSession,
            ApprovalChoice::No => ApprovalAnswer::No { feedback: None },
        })
    }
}

impl App {
    /// Save prompts typed since the last call to the history file.
    pub(super) fn persist_history(&mut self) {
        let fresh = self.chat.take_unsaved_history();
        append_history(
            &self.chat_ext.history_path,
            &fresh,
            self.chat.session(),
            self.project.clock.now(),
        );
    }

    /// Every chat action `tui.rs`'s `handle_chat_actions` does not route itself.
    pub(super) fn on_chat_action(&mut self, action: ChatAction, now: Timestamp) {
        match action {
            ChatAction::Shell(command) => self.spawn_shell(command),
            ChatAction::Interrupt => self.interrupt_turn(now),
            ChatAction::SetMode(mode) => {
                // Applied now when the session is free, and at the start of every turn anyway
                // (`spawn_turn`), so a change made mid-turn is not lost.
                if let Ok(mut session) = self.agent_session.try_lock() {
                    session.set_mode(to_agent_mode(mode));
                }
            }
            ChatAction::OpenEditor(text) => {
                let edited = tm_tui::chat::editor::edit(&text);
                // The editor had the screen; whatever happened, repaint all of it.
                self.chat.force_full_repaint();
                match edited {
                    Ok(Some(edited)) => self.chat.set_input(edited),
                    Ok(None) => self.chat.show_hint(
                        "The editor exited with an error; the prompt is unchanged",
                        NoticeLevel::Warning,
                        false,
                        now.plus_millis(3_000),
                    ),
                    Err(e) => self.chat.push_notice(
                        NoticeLevel::Error,
                        format!(
                            "Could not open {}: {e}",
                            tm_tui::chat::editor::editor_command()
                        ),
                    ),
                }
            }
            ChatAction::Resume(id) => self.resume(&id),
            ChatAction::Approve(choice) => self.answer_approval(choice, now),
            ChatAction::ShowTasks => self.show_tasks(),
            ChatAction::Send(prompt) => self.spawn_turn(prompt),
            ChatAction::Command { id, arg } => self.run_command(id, arg, now),
            ChatAction::GoHome | ChatAction::Quit => {}
        }
    }

    /// Ctrl+T: the attached ticket's subtasks and what this conversation sent to the background,
    /// as a checklist.
    fn show_tasks(&mut self) {
        use tm_core::TicketState as S;
        use tm_tui::chat::tasks::{TaskItem, TaskState};
        let Ok(view) = self.project.store.view() else {
            return;
        };
        let mut ids: Vec<TicketId> = Vec::new();
        if let Some(ticket) = self.attached.as_ref().and_then(|t| view.tickets.get(t)) {
            ids.extend(ticket.children.iter().cloned());
        }
        for id in &self.chat_ext.backgrounded {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        let items = ids
            .iter()
            .filter_map(|id| view.tickets.get(id))
            .map(|t| TaskItem {
                state: match t.state {
                    S::Closed => TaskState::Done,
                    S::Cancelled => TaskState::Failed,
                    S::Leased
                    | S::Running
                    | S::Submitted
                    | S::Verifying
                    | S::Auditing
                    | S::Recovery
                    | S::Rework
                    | S::Replan => TaskState::Active,
                    S::Draft | S::Ready | S::Blocked | S::Escalated => TaskState::Pending,
                },
                text: format!(
                    "{} {}",
                    t.id,
                    t.objective.lines().next().unwrap_or_default()
                ),
            })
            .collect();
        self.chat.show_tasks(items);
    }

    /// `/context`: a table of token use by section (system prompt, instructions, tools,
    /// conversation, free space) from the last turn's own request, plus the attached ticket's
    /// prefetched context-pack sections when the last turn ran for that same ticket.
    fn show_context(&mut self) {
        let Ok(session) = self.agent_session.try_lock() else {
            return self.chat.push_notice(
                NoticeLevel::Warning,
                "A turn is running; check /context once it finishes.",
            );
        };
        let text = slash_views::context_table(
            session.last_context_report(),
            session.context_tokens(),
            self.attached.as_ref().map(TicketId::as_str),
        );
        drop(session);
        self.chat.push_notice(NoticeLevel::Info, text);
    }

    /// `/todos`: the same toggle Ctrl+T does (`ChatScreen::show_tasks`/`close_tasks`), so the two
    /// stay interchangeable ways to open or close the checklist.
    fn toggle_todos(&mut self) {
        if self.chat.is_tasks_open() {
            self.chat.close_tasks();
        } else {
            self.show_tasks();
        }
    }

    fn answer_approval(&mut self, choice: ApprovalChoice, _now: Timestamp) {
        let pending = self
            .chat_ext
            .approval
            .lock()
            .ok()
            .and_then(|mut s| s.take());
        let Some(pending) = pending else {
            return;
        };
        let _ = pending.reply.send(choice);
        // "No, and tell tm what to do differently": stop the turn so the human can say what.
        if choice == ApprovalChoice::No {
            self.chat_ext.interrupter.interrupt();
        }
    }

    /// Esc / Ctrl+C on a running turn: a `!` command is abandoned at once; a model turn stops
    /// after the step in flight (`TurnInterrupter`), keeping what it did.
    fn interrupt_turn(&mut self, now: Timestamp) {
        if let Some(task) = self.chat_ext.shell_task.take() {
            if !task.is_finished() {
                task.abort();
                self.chat.apply_turn_update(
                    TurnUpdate::ShellFinished(ToolCallView {
                        detail: Some("Interrupted".to_string()),
                        ..ToolCallView::new(ToolStatus::Failed, "shell", "")
                    }),
                    now,
                );
                self.handle_chat_actions(now);
                return;
            }
        }
        self.chat_ext.interrupter.interrupt();
    }

    /// Run a `!` command through the session (`AgentSession::run_shell`, which also records it
    /// into the conversation for the next turn).
    fn spawn_shell(&mut self, command: String) {
        let agent_session = Arc::clone(&self.agent_session);
        let sender = self.sender.clone();
        let session_id = self.chat.session().clone();
        let task = tokio::spawn(async move {
            let mut session = agent_session.lock().await;
            let result = session.run_shell(&command).await;
            drop(session);
            let view = match result {
                Ok(run) => ToolCallView::command(
                    "shell",
                    &command,
                    i64::from(run.exit_code),
                    &run.stdout,
                    &run.stderr,
                ),
                Err(e) => ToolCallView {
                    detail: Some(e.to_string()),
                    ..ToolCallView::new(ToolStatus::Failed, "shell", command.clone())
                },
            };
            sender.send(AppMessage::Turn {
                session: session_id,
                update: TurnUpdate::ShellFinished(view),
            });
        });
        self.chat_ext.shell_task = Some(task);
    }

    /// Run `prompt` as a turn on a background task, reporting its progress as
    /// [`AppMessage::Turn`]s so the event loop keeps drawing (and accepting input) while it runs.
    ///
    /// The same turn logic the plain loop uses (`AgentSession::run_turn_with`), under the chat's
    /// permission mode, with permission prompts answered in the chat ([`UiApprover`]).
    pub(super) fn spawn_turn(&self, prompt: String) {
        let agent_session = Arc::clone(&self.agent_session);
        let sender = self.sender.clone();
        let session_id = self.chat.session().clone();
        let mode = to_agent_mode(self.chat.mode());
        // Notes placed among the turn's steps where they happened: permission answers and an
        // automatic compaction.
        let notes: Arc<StdMutex<Vec<(usize, Entry)>>> = Arc::default();
        let steps_seen: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
        let mut approver = UiApprover {
            sender: sender.clone(),
            session: session_id.clone(),
            slot: Arc::clone(&self.chat_ext.approval),
            answers: Arc::clone(&notes),
            steps_seen: Arc::clone(&steps_seen),
        };
        let notes_for_read = Arc::clone(&notes);
        let answered = move || notes_for_read.lock().map(|a| a.clone()).unwrap_or_default();

        tokio::spawn(async move {
            let mut session = agent_session.lock().await;
            session.set_mode(mode);
            let mut latest: Vec<tm_agent::outcome::StepRecord> = Vec::new();
            let mut resolved_ticket: Option<TicketId> = None;

            let outcome = session
                .run_turn_with(
                    &prompt,
                    |event| match event {
                        agent::TurnEvent::TicketResolved(ticket) => {
                            resolved_ticket = Some(ticket.id.clone());
                            sender.send(AppMessage::TicketChanged {
                                id: ticket.id.clone(),
                            });
                        }
                        agent::TurnEvent::Steps(so_far) => {
                            latest = so_far;
                            steps_seen.store(latest.len(), std::sync::atomic::Ordering::SeqCst);
                            sender.send(AppMessage::Turn {
                                session: session_id.clone(),
                                update: steps::progress(&latest, &answered()),
                            });
                        }
                        // The approver puts the prompt on screen itself.
                        agent::TurnEvent::AwaitingApproval(_) => {}
                        agent::TurnEvent::Compacted(compaction) => {
                            if let Ok(mut notes) = notes.lock() {
                                notes.push((
                                    steps_seen.load(std::sync::atomic::Ordering::SeqCst),
                                    steps::auto_compacted_notice(&compaction),
                                ));
                            }
                        }
                    },
                    &mut approver,
                )
                .await;
            let context = session.context_tokens();
            drop(session);
            sender.send(AppMessage::Turn {
                session: session_id.clone(),
                update: TurnUpdate::Context(context),
            });

            let (notice, failed) = match &outcome {
                Ok(outcome) => steps::outcome_notice(outcome),
                Err(e) => (
                    Some((NoticeLevel::Error, format!("The turn could not run: {e}"))),
                    true,
                ),
            };
            if let Ok(outcome) = &outcome {
                // The final steps (an interrupted turn's, say) may not have been reported yet.
                sender.send(AppMessage::Turn {
                    session: session_id.clone(),
                    update: steps::progress(outcome.steps(), &answered()),
                });
            }
            sender.send(AppMessage::Turn {
                session: session_id,
                update: TurnUpdate::Finished { notice, failed },
            });
            if let Some(id) = resolved_ticket {
                sender.send(AppMessage::TicketChanged { id });
            }
        });
    }

    /// Swap in `fresh` as the conversation (after `/clear`, `/resume`), keeping the chat's
    /// permission mode unless the new session carries its own.
    fn install_session(
        &mut self,
        fresh: AgentSession,
        keep_mode: bool,
    ) -> Result<SessionId, String> {
        if self.chat.is_turn_running() {
            return Err("A turn is running; wait for it to finish first.".to_string());
        }
        let Ok(mut session) = self.agent_session.try_lock() else {
            return Err("A turn is still finishing; try again in a moment.".to_string());
        };
        let mut fresh = fresh;
        if keep_mode {
            fresh.set_mode(to_agent_mode(self.chat.mode()));
        }
        let id = fresh.session_id().clone();
        self.chat_ext.interrupter = fresh.interrupter();
        self.attached = fresh.attached_ticket().cloned();
        self.chat.reset(id.clone());
        show_conversation(&mut self.chat, &fresh);
        *session = fresh;
        drop(session);
        Ok(id)
    }

    /// Replace the conversation with a fresh one (`/clear`), optionally attached to `ticket`.
    fn fresh_session(&mut self, ticket: Option<TicketId>) -> Result<SessionId, String> {
        let mut fresh = AgentSession::new(
            self.project.clone(),
            Renderer::from_flags(false, true, true),
        );
        if let Some(ticket) = &ticket {
            fresh
                .attach_ticket(ticket.clone())
                .map_err(|e| format!("Could not re-attach {ticket}: {e}"))?;
        }
        self.install_session(fresh, true)
    }

    fn resume(&mut self, id: &str) {
        let Ok(id) = SessionId::new(id) else {
            return self
                .chat
                .push_notice(NoticeLevel::Warning, format!("{id} is not a session id."));
        };
        if &id == self.chat.session() {
            return self
                .chat
                .push_notice(NoticeLevel::Info, "That is this conversation.");
        }
        match AgentSession::resume(
            self.project.clone(),
            Renderer::from_flags(false, true, true),
            &id,
        ) {
            Ok(session) => {
                if let Err(e) = self.install_session(session, false) {
                    self.chat.push_notice(NoticeLevel::Warning, e);
                }
            }
            Err(e) => self
                .chat
                .push_notice(NoticeLevel::Error, format!("Could not resume {id}: {e}")),
        }
    }

    pub(super) fn run_command(&mut self, id: CommandId, arg: String, now: Timestamp) {
        match id {
            CommandId::Attach => self.attach(&arg),
            CommandId::Detach => match self.attached.clone() {
                None => self
                    .chat
                    .push_notice(NoticeLevel::Info, "No ticket is attached."),
                Some(ticket) => match self.agent_session.try_lock() {
                    Ok(mut session) => {
                        session.detach_ticket();
                        drop(session);
                        self.attached = None;
                        self.chat.status_mut().ticket = None;
                        self.chat.push_notice(
                            NoticeLevel::Info,
                            format!("Detached from {ticket}. The conversation carries on."),
                        );
                    }
                    Err(_) => self.chat.push_notice(
                        NoticeLevel::Warning,
                        "A turn is running; detach once it finishes.",
                    ),
                },
            },
            CommandId::Clear => match self.fresh_session(self.attached.clone()) {
                Ok(session) => self.chat.push_notice(
                    NoticeLevel::Info,
                    format!("Fresh conversation ({session})."),
                ),
                Err(e) => self.chat.push_notice(NoticeLevel::Warning, e),
            },
            CommandId::Decide => self.decide(&arg),
            CommandId::Resume => self.open_resume_picker(now),
            CommandId::Bg => self.background(&arg),
            CommandId::Init => {
                let prompt = agent::init_prompt(&self.project.root);
                self.chat.start_prompt("/init", prompt, now);
            }
            CommandId::Compact => self.compact(&arg, now),
            CommandId::Model => self.switch_model(&arg),
            CommandId::Connect => self.connect_provider(&arg),
            CommandId::Provider => self.show_provider(),
            CommandId::Config => self.config_cmd(&arg),
            CommandId::Context => self.show_context(),
            CommandId::Todos => self.toggle_todos(),
            // The chat screen answers these itself; handled anyway so the match stays exhaustive.
            CommandId::Status | CommandId::Cost => {}
            CommandId::Tickets => return self.open_tickets(now),
            CommandId::Help => return self.chat.toggle_help(),
            CommandId::Exit => return self.quit(),
        }
        self.handle_chat_actions(now);
        self.refresh(now);
    }

    fn open_resume_picker(&mut self, now: Timestamp) {
        match agent::list_sessions(&self.project) {
            Ok(sessions) => {
                let current = self.chat.session().clone();
                let rows = sessions
                    .into_iter()
                    .filter(|s| s.id != current)
                    .map(|s| ConversationRow {
                        id: s.id.to_string(),
                        first_message: if s.first_message.starts_with("<bash-input>") {
                            "(a shell command)".to_string()
                        } else {
                            s.first_message
                        },
                        age: age(now.millis_since(s.updated)),
                        turns: s.turns,
                    })
                    .collect();
                self.chat.open_resume_picker(rows);
            }
            Err(e) => self.chat.push_notice(
                NoticeLevel::Error,
                format!("Could not list saved conversations: {e}"),
            ),
        }
    }

    /// `/bg [task]`: hand work to a background worker as a ticket.
    fn background(&mut self, arg: &str) {
        let Ok(mut session) = self.agent_session.try_lock() else {
            return self.chat.push_notice(
                NoticeLevel::Warning,
                "A turn is running; hand work off once it finishes.",
            );
        };
        let instruction = (!arg.trim().is_empty()).then_some(arg.trim());
        match session.background(instruction) {
            Ok(ticket) => {
                drop(session);
                self.chat_ext.backgrounded.push(ticket.clone());
                self.chat.push_notice(
                    NoticeLevel::Success,
                    format!("Moved to the background as {ticket}. Press ← to watch it."),
                );
            }
            Err(e) => self
                .chat
                .push_notice(NoticeLevel::Warning, format!("Could not hand it off: {e}")),
        }
    }

    /// `/compact [focus]`: summarize the conversation so far into a fresh start
    /// (`AgentSession::compact`), shown as Claude Code does: "Compacting conversation…", then
    /// `⎿ Compacted N turns`.
    fn compact(&mut self, arg: &str, now: Timestamp) {
        if self.chat.is_turn_running() {
            return self.chat.push_notice(
                NoticeLevel::Warning,
                "A turn is running; compact once it finishes.",
            );
        }
        let focus = arg.trim().to_string();
        let shown = if focus.is_empty() {
            "/compact".to_string()
        } else {
            format!("/compact {focus}")
        };
        self.chat.begin_task(shown, "Compacting conversation", now);
        let agent_session = Arc::clone(&self.agent_session);
        let sender = self.sender.clone();
        let session_id = self.chat.session().clone();
        self.chat_ext.task = Some(tokio::spawn(async move {
            let mut session = agent_session.lock().await;
            let result = session
                .compact((!focus.is_empty()).then_some(focus.as_str()))
                .await;
            let context = session.context_tokens();
            drop(session);
            let entry = match result {
                Ok(compaction) => steps::compacted_entry(&compaction),
                Err(e) => Entry::Notice {
                    level: NoticeLevel::Warning,
                    text: format!("Could not compact: {e}"),
                },
            };
            sender.send(AppMessage::Turn {
                session: session_id.clone(),
                update: TurnUpdate::TaskFinished(entry),
            });
            sender.send(AppMessage::Turn {
                session: session_id,
                update: TurnUpdate::Context(context),
            });
        }));
    }

    /// `/model [name]`: with no name, a picker over the models this session can use (the current
    /// one marked); with one, switch to it (`AgentSession::set_model`).
    fn switch_model(&mut self, arg: &str) {
        let Ok(mut session) = self.agent_session.try_lock() else {
            return self.chat.push_notice(
                NoticeLevel::Warning,
                "A turn is running; switch models once it finishes.",
            );
        };
        if arg.trim().is_empty() {
            let current = session.model().ok().flatten().map(|m| m.to_string());
            match session.model_choices() {
                Ok(choices) => {
                    drop(session);
                    let choices = choices.iter().map(ToString::to_string).collect();
                    self.chat.open_model_picker(choices, current.as_deref());
                }
                Err(e) => self
                    .chat
                    .push_notice(NoticeLevel::Warning, format!("Could not list models: {e}")),
            }
            return;
        }
        match session.set_model(arg.trim()) {
            Ok(model) => {
                drop(session);
                let name = model.map_or_else(|| "the default model".to_string(), |m| m.to_string());
                self.chat.show_model(name.clone());
                self.chat
                    .push_notice(NoticeLevel::Success, format!("Set model to {name}."));
            }
            Err(e) => self.chat.push_notice(NoticeLevel::Warning, e.to_string()),
        }
    }

    /// The session's current `provider/model`, or `None` (with a notice) while a turn
    /// holds the session lock.
    fn session_model(&mut self) -> Option<String> {
        match self.agent_session.try_lock() {
            Ok(session) => session.model().ok().flatten().map(|m| m.to_string()),
            Err(_) => {
                self.chat.push_notice(
                    NoticeLevel::Warning,
                    "A turn is running; try again once it finishes.",
                );
                None
            }
        }
    }

    /// What the chat honestly claims about a backend without probing it: local backends
    /// are flagged unprobed (required-env presence means nothing there — see
    /// [`tm_provider::ProviderInfo::is_configured`]), the rest by required-env presence.
    /// Never a reachability claim: that needs a network probe the chat refuses to run on
    /// the UI thread.
    fn provider_env_status(info: &tm_provider::ProviderInfo) -> String {
        if tm_provider::LOCAL_PROVIDER_IDS.contains(&info.id) {
            "local, not checked".to_string()
        } else if info.is_configured() {
            "configured".to_string()
        } else {
            "not configured".to_string()
        }
    }

    /// `/connect [provider]`: bare, a picker over every known provider (current marked);
    /// named, that provider's `tm auth` setup instructions rendered in-chat. Keys are
    /// never shown, only present/absent — and newly set variables apply to the next turn,
    /// no restart needed (the fabric is built per turn).
    fn connect_provider(&mut self, arg: &str) {
        let slug = arg.trim();
        if slug.is_empty() {
            let current = self
                .session_model()
                .and_then(|m| m.split_once('/').map(|(p, _)| p.to_string()));
            let rows = tm_provider::Registry::known_providers()
                .into_iter()
                .map(|info| ProviderRow {
                    status: Self::provider_env_status(&info),
                    id: info.id.to_string(),
                    display_name: info.display_name.to_string(),
                })
                .collect();
            self.chat.open_provider_picker(rows, current.as_deref());
            return;
        }
        let known = tm_provider::Registry::known_providers();
        let Some(info) = known.iter().find(|i| i.id.eq_ignore_ascii_case(slug)) else {
            return self.chat.push_notice(
                NoticeLevel::Warning,
                format!(
                    "Unknown provider \"{slug}\" — /connect with no argument lists every known provider."
                ),
            );
        };
        let rows = env_setup_rows(info);
        let descriptions: Vec<(&'static str, &'static str)> = info
            .env_vars
            .iter()
            .map(|v| (v.name, v.description))
            .collect();
        self.chat.push_notice(
            NoticeLevel::Info,
            format_auth_instructions(info.display_name, &descriptions, &rows),
        );
        if rows.iter().filter(|r| r.required).all(|r| r.configured) {
            self.chat.push_notice(
                NoticeLevel::Success,
                format!(
                    "{} is already connected. Run `tm provider test {}` to check it \
                     (one small billed call).",
                    info.display_name, info.id
                ),
            );
        } else {
            self.chat.push_notice(
                NoticeLevel::Info,
                "Set the missing variable(s) above — the next turn picks them up, no restart needed.",
            );
        }
    }

    /// `/provider`: which `provider/model` answers this chat, which backends have their
    /// environment present, and how many more are known. Env presence only, never a probe
    /// and never a billed call.
    fn show_provider(&mut self) {
        let Some(model) = self.session_model() else {
            return;
        };
        self.chat.push_notice(
            NoticeLevel::Info,
            format!("This chat is answered by {model}."),
        );
        let known = tm_provider::Registry::known_providers();
        let mut configured: Vec<&str> = Vec::new();
        let mut local: Vec<&str> = Vec::new();
        let mut rest = 0;
        for info in &known {
            if tm_provider::LOCAL_PROVIDER_IDS.contains(&info.id) {
                if info.is_configured() {
                    local.push(info.id);
                } else {
                    rest += 1;
                }
            } else if info.is_configured() {
                configured.push(info.id);
            } else {
                rest += 1;
            }
        }
        if configured.is_empty() && local.is_empty() {
            self.chat.push_notice(
                NoticeLevel::Warning,
                "No provider is set up. Run /connect to add one.".to_string(),
            );
        } else {
            let mut line = String::new();
            if !configured.is_empty() {
                line.push_str(&format!("Configured: {}.", configured.join(", ")));
            }
            if !local.is_empty() {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(&format!(
                    "Local (not checked for a running server): {}.",
                    local.join(", ")
                ));
            }
            self.chat.push_notice(NoticeLevel::Info, line);
        }
        if rest > 0 {
            self.chat.push_notice(
                NoticeLevel::Info,
                format!("{rest} more known providers — /connect lists them all."),
            );
        }
    }

    /// `/config ...`: dashboard, dotted-key get, or `tm harness set`-identical set.
    fn config_cmd(&mut self, arg: &str) {
        match config_cmd::parse_config_args(arg) {
            config_cmd::ConfigAction::Show => {
                let session = self.chat.session().to_string();
                let model = self.session_model().unwrap_or_else(|| "?".to_string());
                let ticket = self.attached.as_ref().map(|t| t.to_string());
                for (level, text) in
                    config_cmd::dashboard(&self.project, &session, &model, ticket.as_deref())
                {
                    self.chat.push_notice(level, text);
                }
            }
            config_cmd::ConfigAction::Get(key) => {
                let (level, text) = config_cmd::config_get(&self.project, &key);
                self.chat.push_notice(level, text);
            }
            config_cmd::ConfigAction::Set(key, value) => {
                let (level, text) = config_cmd::config_set(&self.project, &key, &value);
                self.chat.push_notice(level, text);
            }
            config_cmd::ConfigAction::Help => self.chat.push_notice(
                NoticeLevel::Info,
                "Usage: /config [show] · /config get <dotted.key> · /config set <key> <toml-value>"
                    .to_string(),
            ),
        }
    }

    pub(super) fn attach(&mut self, arg: &str) {
        let Ok(ticket) = arg.trim().parse::<TicketId>() else {
            self.chat.push_notice(
                NoticeLevel::Warning,
                format!("\"{arg}\" is not a ticket id (they look like T-12)."),
            );
            return;
        };
        if self.chat.is_turn_running() {
            self.chat.push_notice(
                NoticeLevel::Warning,
                "A turn is running; attach once it finishes.",
            );
            return;
        }
        let Ok(mut session) = self.agent_session.try_lock() else {
            self.chat.push_notice(
                NoticeLevel::Warning,
                "A turn is still finishing; try again in a moment.",
            );
            return;
        };
        match session.attach_ticket(ticket.clone()) {
            Ok(()) => {
                drop(session);
                let objective = self
                    .project
                    .store
                    .view()
                    .ok()
                    .and_then(|v| v.tickets.get(&ticket).map(|t| t.objective.clone()))
                    .unwrap_or_default();
                self.attached = Some(ticket.clone());
                self.chat.status_mut().ticket = Some(ticket.to_string());
                let objective = objective.lines().next().unwrap_or_default().to_string();
                self.chat.push_notice(
                    NoticeLevel::Success,
                    if objective.is_empty() {
                        format!("Attached to {ticket}.")
                    } else {
                        format!("Attached to {ticket}: {objective}")
                    },
                );
            }
            Err(e) => self.chat.push_notice(
                NoticeLevel::Error,
                format!("Could not attach {ticket}: {e}"),
            ),
        }
    }

    /// `/decide <text>`: a project decision, scoped to the attached ticket if there is one — the
    /// same record the plain loop's `/decide` writes.
    fn decide(&mut self, text: &str) {
        let subject = self
            .attached
            .as_ref()
            .map(|t| t.to_string())
            .unwrap_or_else(|| "session".to_string());
        let affected = self.attached.clone().into_iter().collect();
        let result = self.project.store.record_decision(
            subject,
            text.to_string(),
            "recorded via the interactive tm session".to_string(),
            Vec::new(),
            affected,
            Vec::new(),
            self.project.actor.clone(),
        );
        match result {
            Ok(events) => {
                let id = events.iter().find_map(|e| {
                    e.payload
                        .as_decision_created()
                        .map(|p| p.decision.to_string())
                });
                self.chat.push_notice(
                    NoticeLevel::Success,
                    match id {
                        Some(id) => format!("Recorded decision {id}."),
                        None => "Recorded the decision.".to_string(),
                    },
                );
            }
            Err(e) => self.chat.push_notice(
                NoticeLevel::Error,
                format!("Could not record the decision: {e}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_round_trips_and_keeps_only_the_newest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("prompt-history.jsonl");
        let session = SessionId::new("S-1").expect("valid id");
        append_history(
            &path,
            &["one".to_string(), "two\nlines".to_string()],
            &session,
            Timestamp::EPOCH,
        );
        append_history(&path, &["three".to_string()], &session, Timestamp::EPOCH);
        assert_eq!(read_history(&path, 10), vec!["one", "two\nlines", "three"]);
        assert_eq!(read_history(&path, 2), vec!["two\nlines", "three"]);
        std::fs::write(&path, "not json\n{\"text\":\"ok\"}\n").expect("write");
        assert_eq!(read_history(&path, 10), vec!["ok"], "bad lines are skipped");
        assert!(read_history(&dir.path().join("missing"), 10).is_empty());
    }

    #[test]
    fn the_file_index_respects_gitignore_and_skips_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).expect("mkdir");
        std::fs::create_dir_all(root.join("build")).expect("mkdir");
        std::fs::create_dir_all(root.join("state")).expect("mkdir");
        std::fs::write(root.join(".gitignore"), "build/\n").expect("write");
        std::fs::write(root.join("src/main.rs"), "").expect("write");
        std::fs::write(root.join("build/out.o"), "").expect("write");
        std::fs::write(root.join("state/project.db"), "").expect("write");
        let index = index_files(root.to_path_buf(), root.join("state"));
        let mut files = None;
        for _ in 0..500 {
            if let Some(found) = index.get() {
                files = Some(found.clone());
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(files, Some(vec!["src/main.rs".to_string()]));
    }

    #[test]
    fn ages_are_compact() {
        assert_eq!(age(5_000), "just now");
        assert_eq!(age(5 * 60_000), "5m ago");
        assert_eq!(age(3 * 3_600_000), "3h ago");
        assert_eq!(age(2 * 86_400_000), "2d ago");
    }
}
