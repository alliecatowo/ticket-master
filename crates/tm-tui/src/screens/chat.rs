//! The chat screen: bare `tm`'s default view, built to be Claude Code's chat for anyone who
//! already knows it (`docs/decisions/D-019-claude-code-parity-shell.md` §1, extending D-018).
//!
//! A transcript (`⏺` tool calls with results under `⎿`, inline diffs), a prompt box, and a status
//! line that says `? for shortcuts` or which permission mode is on.
//!
//! # Keys
//!
//! Every printable key goes into the prompt; nothing a human can type is a shortcut. The keys a
//! Claude Code user expects:
//!
//! - **Input modes.** `/` opens the command popup; `!` on an empty prompt switches to shell mode
//!   (the command runs in the project root, and its output joins the conversation); `@` opens
//!   file-path completion; `?` on an empty prompt toggles the shortcuts panel. `←` on an empty
//!   prompt goes to tickets.
//! - **Editing.** Enter sends; Shift+Enter, Alt+Enter, Ctrl+J (or a trailing `\`) insert a
//!   newline. Ctrl+A/E/B/F move, Ctrl+K/U/W kill (Ctrl+Y yanks back), Alt+B/F/D work on words,
//!   Ctrl+_ undoes, Ctrl+G opens `$VISUAL`/`$EDITOR`. ↑/↓ walk history (persisted by the
//!   application), Ctrl+R searches it. Long pastes collapse to `[Pasted text #N +M lines]`.
//! - **Turns.** A message sent while a turn runs is queued (shown dimmed) and sent when it ends;
//!   ↑ takes queued messages back. Esc interrupts a running turn; Esc Esc clears the draft.
//!   Shift+Tab cycles the permission mode (auto → plan → ask). Ctrl+O opens the transcript
//!   viewer. A permission prompt takes 1/2/3.
//! - **Leaving.** Ctrl+C clears the prompt, and a second press quits (the application root owns
//!   the timing, see [`ChatScreen::on_ctrl_c`]); Ctrl+D on an empty prompt and `/exit` quit.
//!
//! # Domain boundary
//!
//! Like every screen in this crate, `ChatScreen` runs nothing. Submitting records a
//! [`ChatAction`] for the application to take ([`ChatScreen::take_actions`]); progress comes back
//! as [`TurnUpdate`]s inside [`crate::event::AppMessage::Turn`].

use std::collections::VecDeque;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};
use tm_types::{SessionId, Timestamp};

use crate::chat::approval::{ApprovalChoice, ApprovalPrompt, ApprovalRequest};
use crate::chat::commands::{self, Arg, CommandId, Parsed};
use crate::chat::glyphs::Glyphs;
use crate::chat::input::{BoxStyle, InputBox};
use crate::chat::lines::{clear, draw_box, draw_line, draw_spans, truncate_spans, Line, Span};
use crate::chat::mention::{self, FileIndex};
use crate::chat::picker::{ConversationRow, Picker, PickerOutcome, PickerPurpose};
use crate::chat::shortcuts;
use crate::chat::status::{self, PermissionMode, StatusInfo, TurnState};
use crate::chat::transcript::{Entry, NoticeLevel, ToolCallView, ToolStatus, Transcript};
use crate::chat::viewer::Viewer;
use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{AppMessage, Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::{display_width, truncate};
use crate::theme::{MotionPolicy, Spinner, Theme};

/// What the human asked the application to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatAction {
    /// Run a turn with this prompt (collapsed pastes already expanded). The screen has echoed it
    /// and marked a turn running.
    Send(String),
    /// Run a shell command typed in `!` mode, in the project root; its result comes back as
    /// [`TurnUpdate::ShellFinished`]. The screen has shown it as running.
    Shell(String),
    /// Run a slash command (every command except `/help` and the ones the screen answers
    /// itself: `/status`, `/cost`, `/exit`, `/tickets`).
    Command {
        /// Which command.
        id: CommandId,
        /// Its argument, trimmed (empty for commands that take none).
        arg: String,
    },
    /// Stop the running turn or shell command (Esc, or Ctrl+C while one runs).
    Interrupt,
    /// The permission mode changed (Shift+Tab).
    SetMode(PermissionMode),
    /// Edit this text (the whole prompt, pastes expanded) in the external editor, then hand the
    /// result back with [`ChatScreen::set_input`].
    OpenEditor(String),
    /// Resume this past conversation (chosen in the `/resume` picker).
    Resume(String),
    /// The answer to the open permission prompt.
    Approve(ApprovalChoice),
    /// Show the tickets view.
    GoHome,
    /// Quit tm.
    Quit,
}

/// Progress of a running turn, sent by the application as it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnUpdate {
    /// Everything the turn has done so far (cumulative, replacing the previous progress).
    Progress {
        /// The turn's entries so far: assistant text and tool calls, in order.
        entries: Vec<Entry>,
        /// The model that served the most recent step, if known.
        served_by: Option<String>,
        /// Tokens this turn has spent so far.
        tokens: u64,
        /// What the turn is doing now, for the spinner row ("Thinking", "Ran shell.run").
        activity: Option<String>,
    },
    /// The turn is waiting for the human to allow a tool call: show the permission prompt.
    AwaitingApproval(ApprovalRequest),
    /// The turn ended.
    Finished {
        /// A closing message (a failure summary, a budget stop), if the outcome warrants one.
        notice: Option<(NoticeLevel, String)>,
        /// Whether the turn failed (the status glyph turns red until the next turn).
        failed: bool,
    },
    /// A `!` command finished (or was interrupted).
    ShellFinished(ToolCallView),
    /// A background task the screen started with [`ChatScreen::begin_task`] (`/compact`)
    /// finished; `entry` records its result.
    TaskFinished(Entry),
    /// How many tokens the conversation's context now takes, for the status line.
    Context(u64),
}

/// How long a transient status-line hint stays up.
const HINT_MILLIS: i64 = 3_000;
/// How close two Esc presses must be to count as a double press.
const ESC_WINDOW_MILLIS: i64 = 800;
/// How many `@` completions the popup lists.
const MENTION_LIMIT: usize = 8;

/// A message in the status line's left slot that expires on its own.
#[derive(Debug, Clone)]
struct TransientHint {
    text: String,
    level: NoticeLevel,
    until: Timestamp,
}

/// Whether the running work is a model turn, a `!` command, or another task (`/compact`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnKind {
    Agent,
    Shell,
    Task,
}

/// The running turn's clock and current activity.
#[derive(Debug, Clone)]
struct RunningTurn {
    started: Timestamp,
    activity: String,
    kind: TurnKind,
    interrupting: bool,
}

/// Something typed while a turn ran, waiting to go out.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Queued {
    /// A prompt: as shown (pastes collapsed) and as sent (expanded).
    Prompt { shown: String, text: String },
    /// A `!` command.
    Shell(String),
}

/// A `/` or `@` popup's state.
#[derive(Debug, Default)]
struct Popup {
    selected: usize,
    /// The prompt text the popup was dismissed (Esc) at; it stays closed until the text changes.
    dismissed_at: Option<String>,
}

/// Ctrl+R: reverse search through history.
#[derive(Debug, Clone)]
struct Search {
    query: String,
    /// How many older matches to skip (each Ctrl+R skips one more).
    skip: usize,
    /// The prompt before the search, restored on cancel.
    original: String,
}

/// The chat screen.
#[derive(Debug)]
pub struct ChatScreen {
    id: ComponentId,
    session: SessionId,
    transcript: Transcript,
    input: InputBox,
    status: StatusInfo,
    served_by: Option<String>,
    settled_tokens: u64,
    live_tokens: u64,
    turn: Option<RunningTurn>,
    last_turn_failed: bool,
    popup: Popup,
    mention: Popup,
    shortcuts_open: bool,
    viewer: Option<Viewer>,
    search: Option<Search>,
    approval: Option<ApprovalPrompt>,
    picker: Option<Picker>,
    /// `!` mode: the prompt is a shell command.
    shell_mode: bool,
    mode: PermissionMode,
    queue: VecDeque<Queued>,
    last_esc: Option<Timestamp>,
    /// `(prompt, tokens)` for every finished turn, for `/cost`.
    turn_costs: Vec<(String, u64)>,
    /// The running turn's prompt, for `/cost`.
    running_prompt: Option<String>,
    files: FileIndex,
    hint: Option<TransientHint>,
    actions: VecDeque<ChatAction>,
}

/// The prompt box's placeholder.
pub const PLACEHOLDER: &str = "Ask tm anything… (? for shortcuts, / for commands)";
const PLACEHOLDER_ASCII: &str = "Ask tm anything... (? for shortcuts, / for commands)";

impl ChatScreen {
    /// An empty conversation for `session`, showing `status`.
    pub fn new(id: ComponentId, session: SessionId, status: StatusInfo) -> Self {
        ChatScreen {
            id,
            session,
            transcript: Transcript::new(),
            input: InputBox::new(),
            status,
            served_by: None,
            settled_tokens: 0,
            live_tokens: 0,
            turn: None,
            last_turn_failed: false,
            popup: Popup::default(),
            mention: Popup::default(),
            shortcuts_open: false,
            viewer: None,
            search: None,
            approval: None,
            picker: None,
            shell_mode: false,
            mode: PermissionMode::default(),
            queue: VecDeque::new(),
            last_esc: None,
            turn_costs: Vec::new(),
            running_prompt: None,
            files: FileIndex::default(),
            hint: None,
            actions: VecDeque::new(),
        }
    }

    /// The session this screen's turns belong to.
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The status-bar facts the application owns (cwd, branch, ticket, counts, configured model).
    pub fn status_mut(&mut self) -> &mut StatusInfo {
        &mut self.status
    }

    /// The status-bar facts, as last set.
    pub fn status(&self) -> &StatusInfo {
        &self.status
    }

    /// The transcript, read-only.
    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// The prompt's current text (collapsed pastes as placeholders; without the `!` in shell
    /// mode).
    pub fn input_text(&self) -> &str {
        self.input.text()
    }

    /// Replace the prompt's text (the external editor's result).
    pub fn set_input(&mut self, text: impl Into<String>) {
        self.input.set_text(text);
    }

    /// Seed prompt history with entries persisted by an earlier run, oldest first.
    pub fn load_history(&mut self, entries: Vec<String>) {
        self.input.load_history(entries);
    }

    /// History entries added since the last call (pastes expanded), for the application to
    /// persist.
    pub fn take_unsaved_history(&mut self) -> Vec<String> {
        self.input.take_unsaved_history()
    }

    /// Where `@` completion reads the project's files from.
    pub fn set_file_index(&mut self, files: FileIndex) {
        self.files = files;
    }

    /// Whether a turn (or a `!` command) is running.
    pub fn is_turn_running(&self) -> bool {
        self.turn.is_some()
    }

    /// Whether the prompt is in `!` shell mode.
    pub fn is_shell_mode(&self) -> bool {
        self.shell_mode
    }

    /// The permission mode shown in the status line.
    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Set the permission mode shown (a resumed conversation's), without reporting it back.
    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    /// Whether the shortcuts panel is open.
    pub fn is_help_open(&self) -> bool {
        self.shortcuts_open
    }

    /// Open or close the shortcuts panel.
    pub fn toggle_help(&mut self) {
        self.shortcuts_open = !self.shortcuts_open;
    }

    /// Whether the transcript viewer (Ctrl+O) is open.
    pub fn is_viewer_open(&self) -> bool {
        self.viewer.is_some()
    }

    /// Whether a permission prompt is waiting for an answer.
    pub fn is_awaiting_approval(&self) -> bool {
        self.approval.is_some()
    }

    /// How many messages are queued behind the running turn.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Drain every action recorded since the last call.
    pub fn take_actions(&mut self) -> Vec<ChatAction> {
        self.actions.drain(..).collect()
    }

    /// Add a message from tm to the conversation.
    pub fn push_notice(&mut self, level: NoticeLevel, text: impl Into<String>) {
        self.transcript.push(Entry::Notice {
            level,
            text: text.into(),
        });
        self.transcript.follow();
    }

    /// Send `text` as a turn on the human's behalf (a command that is really a prompt, such as
    /// `/init`), echoed as `shown`; queued if a turn is running.
    pub fn start_prompt(&mut self, shown: impl Into<String>, text: impl Into<String>, now: Timestamp) {
        let (shown, text) = (shown.into(), text.into());
        if self.turn.is_some() {
            self.queue.push_back(Queued::Prompt { shown, text });
        } else {
            self.start_turn(shown, text, now);
        }
    }

    /// Show a command the application runs itself (`/compact`) as running: `shown` is echoed as
    /// the human's message and the spinner says `activity` until
    /// [`TurnUpdate::TaskFinished`] arrives.
    pub fn begin_task(&mut self, shown: impl Into<String>, activity: impl Into<String>, now: Timestamp) {
        self.transcript.push(Entry::User(shown.into()));
        self.transcript.follow();
        self.turn = Some(RunningTurn {
            started: now,
            activity: activity.into(),
            kind: TurnKind::Task,
            interrupting: true,
        });
    }

    /// Name the model turns now go to (after `/model`), replacing the last one that answered.
    pub fn show_model(&mut self, model: impl Into<String>) {
        self.status.model = model.into();
        self.served_by = None;
    }

    /// Open the `/model` picker over `choices` (`provider/model`), `current` marked.
    pub fn open_model_picker(&mut self, choices: Vec<String>, current: Option<&str>) {
        self.shortcuts_open = false;
        self.picker = Some(Picker::models(choices, current));
    }

    /// Add an already-finished entry (an earlier turn of a resumed conversation).
    pub fn push_entry(&mut self, entry: Entry) {
        self.transcript.push(entry);
        self.transcript.follow();
    }

    /// Start a fresh conversation under `session`: the transcript, token counts, queue and
    /// failure mark reset; the prompt's history survives.
    pub fn reset(&mut self, session: SessionId) {
        self.session = session;
        self.transcript.clear();
        self.settled_tokens = 0;
        self.live_tokens = 0;
        self.turn = None;
        self.last_turn_failed = false;
        self.queue.clear();
        self.approval = None;
        self.turn_costs.clear();
        self.running_prompt = None;
    }

    /// Show `text` in the status line until `until`. (`urgent` is kept for callers; every hint
    /// now takes the left slot.)
    pub fn show_hint(
        &mut self,
        text: impl Into<String>,
        level: NoticeLevel,
        _urgent: bool,
        until: Timestamp,
    ) {
        self.hint = Some(TransientHint {
            text: text.into(),
            level,
            until,
        });
    }

    /// The first Ctrl+C outside the chat's own handling: close the shortcuts panel, stash and
    /// clear a half-written prompt (↑ brings it back), and warn that a second press quits.
    pub fn interrupt(&mut self, now: Timestamp, window_millis: i64) {
        self.shortcuts_open = false;
        if !self.input.is_empty() {
            let text = self.shown_prompt();
            self.input.remember(&text);
            self.input.clear();
        }
        self.shell_mode = false;
        self.show_hint(
            "Press Ctrl+C again to quit",
            NoticeLevel::Warning,
            true,
            now.plus_millis(window_millis),
        );
    }

    /// Ctrl+C on the chat, Claude Code's way. Returns whether a second press within the window
    /// should quit.
    ///
    /// An open dialog (permission prompt, viewer, picker, history search) closes first and does
    /// not arm quitting. A running turn is interrupted (and a second press quits, so a turn that
    /// will not stop never traps the human). Otherwise the prompt is cleared into history, as
    /// [`ChatScreen::interrupt`] does.
    pub fn on_ctrl_c(&mut self, now: Timestamp, window_millis: i64) -> bool {
        if self.approval.take().is_some() {
            self.actions
                .push_back(ChatAction::Approve(ApprovalChoice::No));
            return false;
        }
        if self.viewer.take().is_some() || self.picker.take().is_some() {
            return false;
        }
        if let Some(search) = self.search.take() {
            self.input.set_text(search.original);
            return false;
        }
        if let Some(turn) = &mut self.turn {
            if !turn.interrupting {
                turn.interrupting = true;
                turn.activity = "Interrupting".to_string();
                self.actions.push_back(ChatAction::Interrupt);
                self.show_hint(
                    "Interrupting · press Ctrl+C again to quit",
                    NoticeLevel::Warning,
                    true,
                    now.plus_millis(window_millis),
                );
                return true;
            }
        }
        self.interrupt(now, window_millis);
        true
    }

    fn record_turn_cost(&mut self) {
        if let Some(prompt) = self.running_prompt.take() {
            self.turn_costs.push((prompt, self.live_tokens));
        }
    }

    /// Apply a [`TurnUpdate`] for this screen's session. Finishing starts whatever was queued.
    pub fn apply_turn_update(&mut self, update: TurnUpdate, now: Timestamp) {
        match update {
            TurnUpdate::Progress {
                entries,
                served_by,
                tokens,
                activity,
            } => {
                self.transcript.set_live(entries);
                if let Some(model) = served_by {
                    self.served_by = Some(model);
                }
                self.live_tokens = tokens;
                if let (Some(turn), Some(activity)) = (&mut self.turn, activity) {
                    if !turn.interrupting {
                        turn.activity = activity;
                    }
                }
            }
            TurnUpdate::AwaitingApproval(request) => {
                self.shortcuts_open = false;
                self.search = None;
                self.approval = Some(ApprovalPrompt::new(request));
                if let Some(turn) = &mut self.turn {
                    turn.activity = "Waiting for your approval".to_string();
                }
            }
            TurnUpdate::Finished { notice, failed } => {
                self.transcript.commit_live();
                self.record_turn_cost();
                self.settled_tokens += self.live_tokens;
                self.live_tokens = 0;
                self.turn = None;
                self.approval = None;
                self.last_turn_failed = failed;
                if let Some((level, text)) = notice {
                    self.transcript.push(Entry::Notice { level, text });
                }
                self.start_next_queued(now);
            }
            TurnUpdate::TaskFinished(entry) => {
                self.transcript.push(entry);
                self.transcript.follow();
                if self.turn.as_ref().is_some_and(|t| t.kind == TurnKind::Task) {
                    self.turn = None;
                }
                self.start_next_queued(now);
            }
            TurnUpdate::Context(tokens) => self.status.context_tokens = tokens,
            TurnUpdate::ShellFinished(view) => {
                if !self.transcript.finish_shell(view.clone()) {
                    self.transcript.push(Entry::Shell(view));
                }
                if self.turn.as_ref().is_some_and(|t| t.kind == TurnKind::Shell) {
                    self.turn = None;
                }
                self.start_next_queued(now);
            }
        }
    }

    /// The prompt as typed, with the `!` shell mode hides put back.
    fn shown_prompt(&self) -> String {
        if self.shell_mode {
            format!("!{}", self.input.text())
        } else {
            self.input.text().to_string()
        }
    }

    /// Whether the prompt is a shell command (`!` mode, or a recalled/pasted `!…` line).
    fn in_shell(&self) -> bool {
        self.shell_mode || self.input.text().starts_with('!')
    }

    fn start_turn(&mut self, shown: String, text: String, now: Timestamp) {
        self.transcript.push(Entry::User(shown.clone()));
        self.transcript.follow();
        self.turn = Some(RunningTurn {
            started: now,
            activity: "Thinking".to_string(),
            kind: TurnKind::Agent,
            interrupting: false,
        });
        self.running_prompt = Some(shown);
        self.last_turn_failed = false;
        self.actions.push_back(ChatAction::Send(text));
    }

    fn start_shell(&mut self, command: String, now: Timestamp) {
        self.transcript.push(Entry::Shell(ToolCallView {
            input: command.clone(),
            ..ToolCallView::new(ToolStatus::Running, "shell", command.clone())
        }));
        self.transcript.follow();
        self.turn = Some(RunningTurn {
            started: now,
            activity: "Running".to_string(),
            kind: TurnKind::Shell,
            interrupting: false,
        });
        self.actions.push_back(ChatAction::Shell(command));
    }

    /// Send what was queued, if nothing is running: consecutive prompts go out together as one
    /// message; a shell command runs on its own.
    fn start_next_queued(&mut self, now: Timestamp) {
        if self.turn.is_some() {
            return;
        }
        match self.queue.pop_front() {
            Some(Queued::Shell(command)) => self.start_shell(command, now),
            Some(Queued::Prompt { shown, text }) => {
                let (mut shown, mut text) = (shown, text);
                while let Some(Queued::Prompt { .. }) = self.queue.front() {
                    if let Some(Queued::Prompt {
                        shown: more_shown,
                        text: more_text,
                    }) = self.queue.pop_front()
                    {
                        shown.push('\n');
                        shown.push_str(&more_shown);
                        text.push_str("\n\n");
                        text.push_str(&more_text);
                    }
                }
                self.start_turn(shown, text, now);
            }
            None => {}
        }
    }

    /// ↑ with messages queued: take them back into the prompt, one per line, ahead of the draft.
    fn take_back_queue(&mut self) {
        let mut lines: Vec<String> = Vec::new();
        let mut kept = VecDeque::new();
        for item in self.queue.drain(..) {
            match item {
                Queued::Prompt { shown, .. } => lines.push(shown),
                other => kept.push_back(other),
            }
        }
        self.queue = kept;
        if lines.is_empty() {
            return;
        }
        if !self.input.is_empty() {
            lines.push(self.input.text().to_string());
        }
        self.input.set_text(lines.join("\n"));
    }

    fn popup_query(&self) -> Option<&str> {
        if self.in_shell() {
            return None;
        }
        let text = self.input.text();
        let query = text.strip_prefix('/')?;
        if query.chars().any(char::is_whitespace) {
            return None;
        }
        if self.popup.dismissed_at.as_deref() == Some(text) {
            return None;
        }
        Some(query)
    }

    /// Whether the `/` command popup is showing.
    pub fn is_popup_open(&self) -> bool {
        self.popup_query().is_some()
    }

    fn popup_matches(&self) -> Vec<commands::Match> {
        self.popup_query().map(commands::filter).unwrap_or_default()
    }

    /// The `@` mention being typed: its start and query.
    fn mention_query(&self) -> Option<(usize, &str)> {
        if self.in_shell() || self.is_popup_open() {
            return None;
        }
        if self.mention.dismissed_at.as_deref() == Some(self.input.text()) {
            return None;
        }
        mention::token_at(self.input.text(), self.input.cursor())
    }

    /// Whether the `@` file popup is showing.
    pub fn is_mention_open(&self) -> bool {
        self.mention_query().is_some()
    }

    /// The `@` popup's rows, or `None` while the file index is still being built.
    fn mention_matches(&self) -> Option<Vec<String>> {
        let (_, query) = self.mention_query()?;
        let files = self.files.get()?;
        Some(
            mention::rank(files, query, MENTION_LIMIT)
                .into_iter()
                .map(str::to_string)
                .collect(),
        )
    }

    /// Put `/name ` (with a trailing space for commands that take an argument) in the prompt.
    fn complete(&mut self, command: &'static commands::SlashCommand) {
        let text = match command.arg {
            Arg::None => format!("/{}", command.name),
            Arg::Required(_) | Arg::Optional(_) => format!("/{} ", command.name),
        };
        self.input.set_text(text);
        self.popup.selected = 0;
    }

    /// Enter: newline after a trailing backslash, otherwise run a command, a shell command, or
    /// send (or queue) a prompt.
    fn submit(&mut self, now: Timestamp) {
        if self.input.text().ends_with('\\') && self.input.cursor() == self.input.text().len() {
            self.input.backspace();
            self.input.insert("\n");
            return;
        }
        let raw = self.input.text().to_string();
        if raw.trim().is_empty() {
            return;
        }
        if self.in_shell() {
            let command = if self.shell_mode {
                raw.trim().to_string()
            } else {
                raw.trim_start_matches('!').trim().to_string()
            };
            if command.is_empty() {
                return;
            }
            self.input.take_quiet();
            self.input.remember(&format!("!{command}"));
            self.shell_mode = false;
            if self.turn.is_some() {
                self.queue.push_back(Queued::Shell(command));
            } else {
                self.start_shell(command, now);
            }
            return;
        }
        if let Some(parsed) = commands::parse(&raw) {
            self.run_parsed(parsed, now);
            return;
        }
        let shown = self.input.take();
        let text = self.input.expand(&shown).trim_end().to_string();
        let shown = shown.trim_end().to_string();
        if self.turn.is_some() {
            self.queue.push_back(Queued::Prompt { shown, text });
            return;
        }
        self.start_turn(shown, text, now);
    }

    fn run_parsed(&mut self, parsed: Parsed, now: Timestamp) {
        match parsed {
            Parsed::Known { command, arg } => {
                self.input.take();
                self.popup = Popup::default();
                match command.id {
                    CommandId::Help => self.toggle_help(),
                    CommandId::Home => self.actions.push_back(ChatAction::GoHome),
                    CommandId::Exit => self.actions.push_back(ChatAction::Quit),
                    CommandId::Status => self.show_status(),
                    CommandId::Cost => self.show_cost(),
                    id => self.actions.push_back(ChatAction::Command { id, arg }),
                }
            }
            Parsed::MissingArg(command) => {
                self.complete(command);
                self.show_hint(
                    format!("usage: {}", commands::usage(command)),
                    NoticeLevel::Info,
                    false,
                    now.plus_millis(HINT_MILLIS),
                );
            }
            Parsed::Unknown(name) => {
                let text = self.input.take();
                self.input.remember(&text);
                self.push_notice(
                    NoticeLevel::Warning,
                    format!("Unknown command /{name}. Type / to see the commands."),
                );
            }
        }
    }

    fn total_tokens(&self) -> u64 {
        self.settled_tokens + self.live_tokens
    }

    /// `/status`: the current setup, as a notice.
    fn show_status(&mut self) {
        let model = self.model().to_string();
        let provider = model.split_once('/').map_or("?", |(p, _)| p).to_string();
        let mut place = self.status.cwd.clone();
        if let Some(branch) = &self.status.branch {
            place.push_str(&format!(" ({branch})"));
        }
        let scope = if self.status.global_scope {
            "global (state kept outside the repository)"
        } else {
            "repository"
        };
        let ticket = self.status.ticket.clone().unwrap_or_else(|| "none".into());
        let text = format!(
            "Status\n  Model: {model}\n  Provider: {provider}\n  Directory: {place}\n  \
             Scope: {scope}\n  Mode: {} (shift+tab to cycle)\n  Ticket: {ticket}\n  \
             Session: {}\n  Tokens: {}",
            self.mode.label(),
            self.session,
            status::format_tokens(self.total_tokens()),
        );
        self.push_notice(NoticeLevel::Info, text);
    }

    /// `/cost`: tokens by turn.
    fn show_cost(&mut self) {
        let mut text = format!(
            "Tokens this session: {}",
            status::format_tokens(self.total_tokens())
        );
        if self.turn_costs.is_empty() && self.live_tokens == 0 {
            text.push_str("\n  No turns yet.");
        }
        for (i, (prompt, tokens)) in self.turn_costs.iter().enumerate() {
            let first = prompt.lines().next().unwrap_or_default();
            text.push_str(&format!(
                "\n  {:>2}. {:>7}  {}",
                i + 1,
                status::format_tokens(*tokens),
                truncate(first, 50, "…")
            ));
        }
        if let Some(prompt) = &self.running_prompt {
            text.push_str(&format!(
                "\n  {:>2}. {:>7}  {} (running)",
                self.turn_costs.len() + 1,
                status::format_tokens(self.live_tokens),
                truncate(prompt.lines().next().unwrap_or_default(), 40, "…")
            ));
        }
        self.push_notice(NoticeLevel::Info, text);
    }

    /// Open the `/resume` picker over `rows` (newest first).
    pub fn open_resume_picker(&mut self, rows: Vec<ConversationRow>) {
        self.shortcuts_open = false;
        self.picker = Some(Picker::resume(rows));
    }

    /// Whether the `/resume` picker is open.
    pub fn is_picker_open(&self) -> bool {
        self.picker.is_some()
    }

    fn handle_popup_key(&mut self, key: &KeyEvent, now: Timestamp) -> bool {
        let matches = self.popup_matches();
        let count = matches.len();
        match key.code {
            KeyCode::Up if count > 0 => {
                self.popup.selected = (self.popup.selected + count - 1) % count;
                true
            }
            KeyCode::Down if count > 0 => {
                self.popup.selected = (self.popup.selected + 1) % count;
                true
            }
            KeyCode::Tab if count > 0 => {
                let chosen = matches[self.popup.selected.min(count - 1)].command;
                self.complete(chosen);
                true
            }
            KeyCode::Enter if key.modifiers.is_empty() && count > 0 => {
                let chosen = matches[self.popup.selected.min(count - 1)].command;
                match chosen.arg {
                    Arg::None | Arg::Optional(_) => self.run_parsed(
                        Parsed::Known {
                            command: chosen,
                            arg: String::new(),
                        },
                        now,
                    ),
                    Arg::Required(_) => self.run_parsed(Parsed::MissingArg(chosen), now),
                }
                true
            }
            KeyCode::Esc => {
                self.popup.dismissed_at = Some(self.input.text().to_string());
                true
            }
            _ => false,
        }
    }

    fn handle_mention_key(&mut self, key: &KeyEvent) -> bool {
        let Some((start, _)) = self.mention_query() else {
            return false;
        };
        let matches = self.mention_matches().unwrap_or_default();
        let count = matches.len();
        match key.code {
            KeyCode::Up if count > 0 => {
                self.mention.selected = (self.mention.selected + count - 1) % count;
                true
            }
            KeyCode::Down if count > 0 => {
                self.mention.selected = (self.mention.selected + 1) % count;
                true
            }
            KeyCode::Tab | KeyCode::Enter if count > 0 && !key.modifiers.contains(KeyModifiers::SHIFT) => {
                let path = &matches[self.mention.selected.min(count - 1)];
                self.input
                    .replace_before_cursor(start, &mention::completion(path));
                self.mention = Popup::default();
                true
            }
            KeyCode::Tab => true,
            KeyCode::Esc => {
                self.mention.dismissed_at = Some(self.input.text().to_string());
                true
            }
            _ => false,
        }
    }

    /// The history entries matching `query`, newest first, duplicates collapsed.
    fn search_matches(&self, query: &str) -> Vec<String> {
        let needle = query.to_lowercase();
        let mut seen = std::collections::HashSet::new();
        self.input
            .history()
            .iter()
            .rev()
            .filter(|h| needle.is_empty() || h.to_lowercase().contains(&needle))
            .filter(|h| seen.insert(h.as_str()))
            .cloned()
            .collect()
    }

    fn show_search_match(&mut self) {
        let Some(search) = &self.search else {
            return;
        };
        let matches = self.search_matches(&search.query);
        let shown = if search.query.is_empty() {
            search.original.clone()
        } else {
            matches
                .get(search.skip.min(matches.len().saturating_sub(1)))
                .cloned()
                .unwrap_or_else(|| search.original.clone())
        };
        self.input.set_text(shown);
    }

    fn handle_search_key(&mut self, key: &KeyEvent, now: Timestamp) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(search) = &mut self.search else {
            return;
        };
        match key.code {
            KeyCode::Char('r') if ctrl => search.skip += 1,
            KeyCode::Up => search.skip += 1,
            KeyCode::Down => search.skip = search.skip.saturating_sub(1),
            KeyCode::Char('c') | KeyCode::Char('g') if ctrl => {
                let original = search.original.clone();
                self.search = None;
                self.input.set_text(original);
                return;
            }
            KeyCode::Backspace if search.query.is_empty() => {
                let original = search.original.clone();
                self.search = None;
                self.input.set_text(original);
                return;
            }
            KeyCode::Backspace => {
                search.query.pop();
                search.skip = 0;
            }
            KeyCode::Tab | KeyCode::Esc => {
                self.search = None;
                return;
            }
            KeyCode::Enter => {
                self.search = None;
                self.submit(now);
                return;
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                search.query.push(c);
                search.skip = 0;
            }
            _ => return,
        }
        // Clamp the skip to the matches that exist.
        let count = self.search_matches(&self.search.as_ref().map_or(String::new(), |s| s.query.clone())).len();
        if let Some(search) = &mut self.search {
            search.skip = search.skip.min(count.saturating_sub(1));
        }
        self.show_search_match();
    }

    fn handle_viewer_key(&mut self, key: &KeyEvent) {
        let Some(viewer) = &self.viewer else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.viewer = None,
            KeyCode::Char('o') | KeyCode::Char('c') if ctrl => self.viewer = None,
            KeyCode::Up | KeyCode::Char('k') => viewer.scroll(-1),
            KeyCode::Down | KeyCode::Char('j') => viewer.scroll(1),
            KeyCode::PageUp | KeyCode::Char('b') => viewer.page(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => viewer.page(1),
            KeyCode::Home | KeyCode::Char('g') => viewer.home(),
            KeyCode::End | KeyCode::Char('G') => viewer.end(),
            _ => {}
        }
    }

    /// Esc, once nothing else wanted it: interrupt a running turn, else (twice) clear the draft,
    /// else leave shell mode, else jump back to the newest content.
    fn on_esc(&mut self, now: Timestamp) {
        if let Some(turn) = &mut self.turn {
            if !turn.interrupting {
                turn.interrupting = true;
                turn.activity = "Interrupting".to_string();
                self.actions.push_back(ChatAction::Interrupt);
            }
            return;
        }
        if !self.input.is_empty() {
            let double = self
                .last_esc
                .is_some_and(|at| now.millis_since(at) <= ESC_WINDOW_MILLIS);
            if double {
                let text = self.shown_prompt();
                self.input.remember(&text);
                self.input.clear();
                self.shell_mode = false;
                self.last_esc = None;
                self.show_hint(
                    "Draft cleared (↑ to bring it back)",
                    NoticeLevel::Info,
                    false,
                    now.plus_millis(HINT_MILLIS),
                );
            } else {
                self.last_esc = Some(now);
                self.show_hint(
                    "Esc again to clear",
                    NoticeLevel::Info,
                    false,
                    now.plus_millis(ESC_WINDOW_MILLIS),
                );
            }
            return;
        }
        if self.shell_mode {
            self.shell_mode = false;
            return;
        }
        self.transcript.follow();
    }

    fn handle_key(&mut self, key: &KeyEvent, now: Timestamp) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // Dialogs own every key while open.
        if let Some(prompt) = &mut self.approval {
            if let Some(choice) = prompt.handle_key(key) {
                self.approval = None;
                self.actions.push_back(ChatAction::Approve(choice));
            }
            return;
        }
        if self.viewer.is_some() {
            return self.handle_viewer_key(key);
        }
        if let Some(picker) = &mut self.picker {
            match picker.handle_key(key) {
                PickerOutcome::Open => {}
                PickerOutcome::Cancelled => self.picker = None,
                PickerOutcome::Chosen(id) => {
                    let purpose = picker.purpose();
                    self.picker = None;
                    self.actions.push_back(match purpose {
                        PickerPurpose::Resume => ChatAction::Resume(id),
                        PickerPurpose::Model => ChatAction::Command {
                            id: CommandId::Model,
                            arg: id,
                        },
                    });
                }
            }
            return;
        }
        if self.search.is_some() {
            return self.handle_search_key(key, now);
        }
        if self.shortcuts_open {
            match key.code {
                KeyCode::Esc => {
                    self.shortcuts_open = false;
                    return;
                }
                KeyCode::Char('?') if !ctrl && !alt => {
                    self.shortcuts_open = false;
                    return;
                }
                // Anything else closes the panel and then does what it normally does, so
                // starting to type is never blocked by it.
                _ => self.shortcuts_open = false,
            }
        }

        // Chords that work from anywhere in the prompt.
        match key.code {
            KeyCode::Char('o') if ctrl => {
                self.viewer = Some(Viewer::new());
                return;
            }
            KeyCode::BackTab => {
                self.mode = self.mode.next();
                self.actions.push_back(ChatAction::SetMode(self.mode));
                return;
            }
            KeyCode::Char('g') if ctrl => {
                let text = self.input.expand(self.input.text());
                self.actions.push_back(ChatAction::OpenEditor(text));
                return;
            }
            KeyCode::Char('r') if ctrl => {
                self.search = Some(Search {
                    query: String::new(),
                    skip: 0,
                    original: self.input.text().to_string(),
                });
                return;
            }
            // Ctrl+_ arrives as Ctrl+7 from most terminals (0x1F), Ctrl+- or Ctrl+_ from the rest.
            KeyCode::Char('_') | KeyCode::Char('7') | KeyCode::Char('-') if ctrl => {
                self.input.undo();
                return;
            }
            _ => {}
        }

        if self.is_popup_open() && self.handle_popup_key(key, now) {
            return;
        }
        if self.is_mention_open() && self.handle_mention_key(key) {
            return;
        }
        if key.code == KeyCode::Esc {
            return self.on_esc(now);
        }

        let before = self.input.text().to_string();
        match key.code {
            KeyCode::Enter if shift || alt || ctrl => self.input.insert("\n"),
            KeyCode::Enter => self.submit(now),
            KeyCode::Char('j') if ctrl => self.input.insert("\n"),
            KeyCode::Char('u') if ctrl => {
                if self.input.is_empty() {
                    self.shell_mode = false;
                } else {
                    self.input.kill_to_start();
                }
            }
            KeyCode::Char('k') if ctrl => self.input.kill_to_end(),
            KeyCode::Char('y') if ctrl => {
                self.input.yank();
            }
            KeyCode::Char('w') if ctrl => self.input.delete_word_back(),
            KeyCode::Char('h') if ctrl => self.input.backspace(),
            KeyCode::Char('a') if ctrl => self.input.home(),
            KeyCode::Char('e') if ctrl => self.input.end(),
            KeyCode::Char('b') if ctrl => self.input.left(),
            KeyCode::Char('f') if ctrl => self.input.right(),
            KeyCode::Char('p') if ctrl => {
                self.input.history_prev();
            }
            KeyCode::Char('n') if ctrl => {
                self.input.history_next();
            }
            KeyCode::Char('d') if ctrl => {
                if self.input.is_empty() {
                    self.actions.push_back(ChatAction::Quit);
                } else {
                    self.input.delete();
                }
            }
            KeyCode::Char('b') if alt => self.input.word_left(),
            KeyCode::Char('f') if alt => self.input.word_right(),
            KeyCode::Char('d') if alt => self.input.delete_word_forward(),
            // Any other chord is swallowed rather than typed or propagated.
            KeyCode::Char(_) if ctrl || alt => {}
            KeyCode::Char('?') if self.input.is_empty() && !self.shell_mode => self.toggle_help(),
            KeyCode::Char('!') if self.input.is_empty() && !self.shell_mode => {
                self.shell_mode = true;
            }
            KeyCode::Char(c) => self.input.insert_char(c),
            KeyCode::Backspace if self.input.is_empty() && self.shell_mode => {
                self.shell_mode = false;
            }
            KeyCode::Backspace if alt || ctrl => self.input.delete_word_back(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left if self.input.is_empty() && !self.shell_mode => {
                self.actions.push_back(ChatAction::GoHome)
            }
            KeyCode::Left if ctrl || alt => self.input.word_left(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right if ctrl || alt => self.input.word_right(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Up if shift => self.transcript.scroll_up(1),
            KeyCode::Down if shift => self.transcript.scroll_down(1),
            KeyCode::Up => {
                if !self.queue.is_empty() && (self.input.is_empty() || self.input.on_first_row())
                {
                    self.take_back_queue();
                } else if self.input.is_empty() || self.input.on_first_row() {
                    self.input.history_prev();
                } else {
                    self.input.move_vertical(-1);
                }
            }
            KeyCode::Down => {
                if self.input.is_browsing() && self.input.on_last_row() {
                    self.input.history_next();
                } else {
                    self.input.move_vertical(1);
                }
            }
            KeyCode::PageUp => self.transcript.page_up(),
            KeyCode::PageDown => self.transcript.page_down(),
            _ => {}
        }

        if self.input.text() != before {
            // A new query reopens a dismissed popup and restarts its selection.
            self.popup.selected = 0;
            self.mention.selected = 0;
            if self.popup.dismissed_at.as_deref() != Some(self.input.text()) {
                self.popup.dismissed_at = None;
            }
            if self.mention.dismissed_at.as_deref() != Some(self.input.text()) {
                self.mention.dismissed_at = None;
            }
            self.last_esc = None;
        }
    }

    fn on_paste(&mut self, text: &str) {
        self.shortcuts_open = false;
        if self.search.is_some() || self.approval.is_some() || self.viewer.is_some() {
            return;
        }
        // Pasting a `!command` into an empty prompt enters shell mode, as typing `!` does.
        if self.input.is_empty() && !self.shell_mode {
            if let Some(rest) = text.strip_prefix('!') {
                self.shell_mode = true;
                self.input.insert_paste(rest);
                return;
            }
        }
        self.input.insert_paste(text);
    }

    fn model(&self) -> &str {
        self.served_by.as_deref().unwrap_or(&self.status.model)
    }

    fn turn_state(&self) -> TurnState {
        if self.turn.is_some() {
            TurnState::Running
        } else if self.last_turn_failed {
            TurnState::Failed
        } else {
            TurnState::Idle
        }
    }

    fn spinner_frame(&self, glyphs: &Glyphs, now: Timestamp) -> &'static str {
        let started = self.turn.as_ref().map_or(now, |t| t.started);
        Spinner {
            frames: glyphs.spinner,
            frame_duration: Duration::from_millis(120),
        }
        .frame(started, now, MotionPolicy::enabled())
    }

    /// The rows under the transcript: the running turn's spinner (Claude Code's
    /// `✻ Thinking… (12s · 1.2k tokens · esc to interrupt)`), then anything queued, dimmed.
    fn tail_lines(&self, width: usize, theme: &Theme, glyphs: &Glyphs, now: Timestamp) -> Vec<Line> {
        let muted = Style::default().fg(theme.muted);
        let mut rows = Vec::new();
        if let Some(turn) = &self.turn {
            if turn.kind != TurnKind::Shell {
                let elapsed = now.millis_since(turn.started).max(0) / 1000;
                let clock = if elapsed >= 60 {
                    format!("{}m {:02}s", elapsed / 60, elapsed % 60)
                } else {
                    format!("{elapsed}s")
                };
                let mut facts = clock;
                if self.live_tokens > 0 {
                    facts.push_str(&format!(
                        "{}{} tokens",
                        glyphs.sep,
                        status::format_tokens(self.live_tokens)
                    ));
                }
                if !turn.interrupting {
                    facts.push_str(&format!("{}esc to interrupt", glyphs.sep));
                }
                if !self.transcript.is_empty() {
                    rows.push(Line::blank());
                }
                rows.push(Line::from_spans(truncate_spans(
                    &[
                        Span::new(
                            self.spinner_frame(glyphs, now),
                            Style::default().fg(theme.accent),
                        ),
                        Span::new(" ", Style::default()),
                        Span::new(
                            format!("{}{}", turn.activity, glyphs.ellipsis),
                            Style::default().fg(theme.accent),
                        ),
                        Span::new(format!(" ({facts})"), muted),
                    ],
                    width,
                    glyphs.ellipsis,
                )));
            }
        }
        if !self.queue.is_empty() {
            rows.push(Line::blank());
            for item in &self.queue {
                let (mark, text) = match item {
                    Queued::Prompt { shown, .. } => (glyphs.prompt, shown.as_str()),
                    Queued::Shell(command) => ("!", command.as_str()),
                };
                let first = text.lines().next().unwrap_or_default();
                rows.push(Line::from_spans(truncate_spans(
                    &[
                        Span::new(format!(" {mark} "), muted),
                        Span::new(first.to_string(), muted),
                        Span::new("  (queued)", muted.add_modifier(Modifier::ITALIC)),
                    ],
                    width,
                    glyphs.ellipsis,
                )));
            }
        }
        rows
    }

    /// The status line's left slot: a live hint, the mode being typed in, the permission mode,
    /// or `? for shortcuts`.
    fn status_left(&self, theme: &Theme, glyphs: &Glyphs, now: Timestamp) -> Vec<Span> {
        let muted = Style::default().fg(theme.muted);
        if let Some(hint) = &self.hint {
            if now.millis_since(hint.until) < 0 {
                let color = match hint.level {
                    NoticeLevel::Info => theme.muted,
                    NoticeLevel::Success => theme.success,
                    NoticeLevel::Warning => theme.warning,
                    NoticeLevel::Error => theme.danger,
                };
                let text = if glyphs.unicode {
                    hint.text.clone()
                } else {
                    hint.text.replace('↑', "up").replace('·', "-")
                };
                return vec![Span::new(text, Style::default().fg(color))];
            }
        }
        let sep = glyphs.sep;
        if let Some(search) = &self.search {
            return vec![
                Span::new(
                    format!("search history: {}", search.query),
                    Style::default().fg(theme.accent),
                ),
                Span::new(
                    format!("{sep}ctrl+r older{sep}tab accept{sep}enter send{sep}ctrl+c cancel"),
                    muted,
                ),
            ];
        }
        if self.is_popup_open() {
            return vec![Span::new(
                format!(
                    "{} select{sep}tab complete{sep}enter run{sep}esc close",
                    glyphs.updown
                ),
                muted,
            )];
        }
        if self.is_mention_open() {
            return vec![Span::new(
                format!(
                    "{} select{sep}tab/enter insert{sep}esc close",
                    glyphs.updown
                ),
                muted,
            )];
        }
        if self.in_shell() {
            return vec![Span::new(
                "! for bash mode",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            )];
        }
        if let Some(indicator) = self.mode.indicator(theme, glyphs) {
            return indicator;
        }
        if !self.transcript.is_following() {
            return vec![Span::new(
                format!("pgup/pgdn scroll{sep}esc jump to latest"),
                muted,
            )];
        }
        vec![Span::new("? for shortcuts", muted)]
    }

    /// Claude Code's welcome box, then its tips.
    fn render_welcome(&self, area: Rect, buf: &mut Buffer, theme: &Theme, glyphs: &Glyphs) {
        if area.height < 3 || area.width < 12 {
            return;
        }
        let muted = Style::default().fg(theme.muted);
        let accent = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let mut place = self.status.cwd.clone();
        if let Some(branch) = &self.status.branch {
            place.push_str(&format!(" ({branch})"));
        }
        let card: Vec<Line> = vec![
            Line::from_spans(vec![
                Span::new(format!("{} ", glyphs.star), accent),
                Span::new("Welcome to ", Style::default().add_modifier(Modifier::BOLD)),
                Span::new("tm", accent),
                Span::new("!", Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::blank(),
            Line::from_spans(vec![
                Span::new("  /help", Style::default()),
                Span::new(" for help, ", muted),
                Span::new("/status", Style::default()),
                Span::new(" for your current setup", muted),
            ]),
            Line::blank(),
            Line::from_spans(vec![
                Span::new("  cwd: ", muted),
                Span::new(place, Style::default()),
            ]),
            Line::from_spans(vec![
                Span::new("  model: ", muted),
                Span::new(self.model().to_string(), Style::default()),
            ]),
        ];
        let content_width = card.iter().map(Line::width).max().unwrap_or(0);
        let box_width = ((content_width + 4).max(50) as u16).min(area.width);
        let inner = box_width.saturating_sub(4);
        if area.height < card.len() as u16 + 2 {
            // Too short for the card: just the greeting.
            draw_spans(
                buf,
                area.x,
                area.y,
                area.width,
                &truncate_spans(&card[0].spans, area.width as usize, glyphs.ellipsis),
            );
            return;
        }
        let outer = Rect::new(area.x, area.y, box_width, card.len() as u16 + 2);
        draw_box(buf, outer, &glyphs.border, Style::default().fg(theme.accent));
        for (i, line) in card.iter().enumerate() {
            draw_spans(
                buf,
                outer.x + 2,
                outer.y + 1 + i as u16,
                inner,
                &truncate_spans(&line.spans, inner as usize, glyphs.ellipsis),
            );
        }

        let left = glyphs.left;
        let tips = [
            "Ask tm to explain, fix, or build something in this project".to_string(),
            "Type ! to run a shell command, @ to mention a file".to_string(),
            format!("Press {left} on an empty prompt for tickets: work tm does in the background"),
            "Be as specific as you would with another engineer for the best results".to_string(),
        ];
        let mut y = outer.y + outer.height + 1;
        let bottom = area.y + area.height;
        if y < bottom {
            buf.set_stringn(
                area.x + 1,
                y,
                "Tips for getting started:",
                area.width.saturating_sub(1) as usize,
                muted,
            );
            y += 2;
        }
        for (i, tip) in tips.iter().enumerate() {
            if y >= bottom {
                break;
            }
            let line = format!("{}. {tip}", i + 1);
            let line = truncate(&line, area.width.saturating_sub(1) as usize, glyphs.ellipsis);
            buf.set_stringn(
                area.x + 1,
                y,
                &line,
                area.width.saturating_sub(1) as usize,
                muted,
            );
            y += 1;
        }
    }

    /// A popup anchored above the input box: `rows` of `(label, description, selected)`.
    #[allow(clippy::too_many_arguments)]
    fn render_list_popup(
        &self,
        rows: &[(String, String, bool)],
        empty: &str,
        input_area: Rect,
        bounds: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        glyphs: &Glyphs,
    ) {
        let shown = rows.len().clamp(1, 8);
        let height = shown as u16 + 2;
        if input_area.y < bounds.y + height || bounds.width < 20 {
            return;
        }
        let width = bounds.width.min(76);
        let area = Rect::new(input_area.x, input_area.y - height, width, height);
        clear(buf, area);
        let muted = Style::default().fg(theme.muted);
        draw_box(buf, area, &glyphs.border, muted);
        let inner_x = area.x + 2;
        let inner_width = width.saturating_sub(4) as usize;
        if rows.is_empty() {
            buf.set_stringn(inner_x, area.y + 1, empty, inner_width, muted);
            return;
        }
        let selected = rows.iter().position(|r| r.2).unwrap_or(0);
        let first = selected.saturating_sub(shown - 1);
        let label_width = rows
            .iter()
            .map(|r| display_width(&r.0))
            .max()
            .unwrap_or(0)
            .min(inner_width / 2);
        for (row, (label, description, is_selected)) in
            rows.iter().skip(first).take(shown).enumerate()
        {
            let y = area.y + 1 + row as u16;
            let label = truncate(label, label_width, glyphs.ellipsis);
            let pad = " ".repeat(label_width.saturating_sub(display_width(&label)) + 3);
            let (pointer, label_style) = if *is_selected {
                (
                    glyphs.pointer,
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                (" ", Style::default())
            };
            let spans = vec![
                Span::new(pointer, Style::default().fg(theme.accent)),
                Span::new(" ", Style::default()),
                Span::new(label, label_style),
                Span::new(pad, Style::default()),
                Span::new(
                    description.clone(),
                    if *is_selected { Style::default() } else { muted },
                ),
            ];
            draw_spans(
                buf,
                inner_x,
                y,
                inner_width as u16,
                &truncate_spans(&spans, inner_width, glyphs.ellipsis),
            );
        }
    }

    fn command_rows(&self, glyphs: &Glyphs) -> Vec<(String, String, bool)> {
        let matches = self.popup_matches();
        let selected = self.popup.selected.min(matches.len().saturating_sub(1));
        matches
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mut description = m.command.description.to_string();
                if let Some(alias) = m.via_alias {
                    description.push_str(&format!("{}/{alias}", glyphs.sep));
                }
                (commands::usage(m.command), description, i == selected)
            })
            .collect()
    }

    fn mention_rows(&self) -> (Vec<(String, String, bool)>, &'static str) {
        match self.mention_matches() {
            None => (Vec::new(), "Indexing files…"),
            Some(paths) => {
                let selected = self.mention.selected.min(paths.len().saturating_sub(1));
                let rows = paths
                    .into_iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let (dir, name) = match p.rsplit_once('/') {
                            Some((dir, name)) => (format!("{dir}/"), name.to_string()),
                            None => (String::new(), p.clone()),
                        };
                        (name, dir, i == selected)
                    })
                    .collect();
                (rows, "No matching files")
            }
        }
    }
}

impl Component for ChatScreen {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.width < 4 || area.height == 0 {
            return;
        }
        let theme = ctx.theme;
        let glyphs = Glyphs::for_caps(ctx.caps);
        let now = ctx.clock.now();

        if let Some(viewer) = &self.viewer {
            let version = self.transcript.version();
            viewer.render(
                area,
                buf,
                self.transcript
                    .entries()
                    .iter()
                    .chain(self.transcript.live().iter()),
                version,
                theme,
                &glyphs,
            );
            return;
        }

        // Bottom up: the status line (or the shortcuts panel in its place), the input box (or the
        // permission prompt in its place), and the transcript over the rest.
        let panel: Vec<Line> = if self.shortcuts_open {
            shortcuts::lines(area.width as usize, theme, &glyphs)
        } else {
            Vec::new()
        };
        let footer_height = if panel.is_empty() {
            1
        } else {
            (panel.len() as u16).min(area.height / 3).max(1)
        };
        let footer_area = Rect::new(
            area.x,
            area.y + area.height - footer_height.min(area.height),
            area.width,
            footer_height.min(area.height),
        );
        let rest = Rect::new(
            area.x,
            area.y,
            area.width,
            area.height - footer_area.height,
        );

        let input_height = match &self.approval {
            Some(prompt) => prompt
                .height(rest.width, theme, &glyphs)
                .min(rest.height.saturating_sub(2).max(3)),
            None => {
                let max_input = (rest.height / 3).clamp(3, 10);
                self.input.height(rest.width, max_input)
            }
        }
        .min(rest.height);
        let input_area = Rect::new(
            rest.x,
            rest.y + rest.height - input_height,
            rest.width,
            input_height,
        );
        let transcript_area = Rect::new(
            rest.x + 1,
            rest.y,
            rest.width.saturating_sub(2),
            rest.height - input_height,
        );

        if transcript_area.height > 0 {
            if self.transcript.is_empty() && self.turn.is_none() && self.queue.is_empty() {
                self.render_welcome(transcript_area, buf, theme, &glyphs);
            } else {
                let tail = self.tail_lines(transcript_area.width as usize, theme, &glyphs, now);
                self.transcript
                    .render(transcript_area, buf, theme, &glyphs, &tail);
            }
        }

        if let Some(prompt) = &self.approval {
            prompt.render(input_area, buf, theme, &glyphs);
        } else {
            let placeholder = if glyphs.unicode {
                PLACEHOLDER
            } else {
                PLACEHOLDER_ASCII
            };
            let search_title;
            let style = if let Some(search) = &self.search {
                search_title = format!("search history: {}", search.query);
                BoxStyle {
                    marker: glyphs.prompt,
                    color: theme.accent,
                    tinted: true,
                    title: Some(&search_title),
                }
            } else if self.in_shell() {
                BoxStyle {
                    marker: "!",
                    color: theme.warning,
                    tinted: true,
                    title: None,
                }
            } else {
                BoxStyle {
                    marker: glyphs.prompt,
                    color: theme.accent,
                    tinted: false,
                    title: None,
                }
            };
            let placeholder = if self.shell_mode {
                "Run a shell command in the project root"
            } else if self.turn.is_some() {
                "Type to queue a message for when this turn ends"
            } else {
                placeholder
            };
            self.input.render_styled(
                input_area,
                buf,
                theme,
                &glyphs,
                placeholder,
                self.turn.is_some(),
                style,
            );
        }

        if panel.is_empty() {
            let segments = status::segments(
                &StatusInfo {
                    model: self.model().to_string(),
                    tokens: self.total_tokens(),
                    ..self.status.clone()
                },
                self.turn_state(),
                self.spinner_frame(&glyphs, now),
                theme,
                &glyphs,
            );
            let left = self.status_left(theme, &glyphs, now);
            status::render(buf, footer_area, &left, &segments, theme, &glyphs);
        } else {
            for (i, line) in panel.iter().take(footer_area.height as usize).enumerate() {
                draw_line(buf, footer_area.x, footer_area.y + i as u16, footer_area.width, line);
            }
        }

        if self.approval.is_none() {
            if self.is_popup_open() {
                let rows = self.command_rows(&glyphs);
                self.render_list_popup(
                    &rows,
                    "No matching commands",
                    input_area,
                    rest,
                    buf,
                    theme,
                    &glyphs,
                );
            } else if self.is_mention_open() {
                let (rows, empty) = self.mention_rows();
                self.render_list_popup(&rows, empty, input_area, rest, buf, theme, &glyphs);
            }
        }
        if let Some(picker) = &self.picker {
            picker.render(rest, buf, theme, &glyphs);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        let now = ctx.clock.now();
        match event {
            Event::App(AppMessage::Turn { session, update }) => {
                if session != &self.session {
                    return Propagation::Propagate;
                }
                self.apply_turn_update(update.clone(), now);
                Propagation::Consumed
            }
            Event::Input(InputEvent::Key(key)) => {
                if key.kind == crossterm::event::KeyEventKind::Release {
                    return Propagation::Consumed;
                }
                self.handle_key(key, now);
                Propagation::Consumed
            }
            Event::Input(InputEvent::Paste(text)) => {
                self.on_paste(text);
                Propagation::Consumed
            }
            Event::Input(InputEvent::Mouse(mouse)) => {
                let scroll = |up: bool, chat: &mut ChatScreen| match &chat.viewer {
                    Some(viewer) => viewer.scroll(if up { -3 } else { 3 }),
                    None if up => chat.transcript.scroll_up(3),
                    None => chat.transcript.scroll_down(3),
                };
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        scroll(true, self);
                        Propagation::Consumed
                    }
                    MouseEventKind::ScrollDown => {
                        scroll(false, self);
                        Propagation::Consumed
                    }
                    _ => Propagation::Propagate,
                }
            }
            _ => Propagation::Propagate,
        }
    }

    fn keybindings(&self, _ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        let ctrl = |c: char, what: &'static str| {
            KeyBinding::new(
                KeyChord {
                    code: KeyCode::Char(c),
                    modifiers: KeyModifiers::CONTROL,
                },
                what,
            )
        };
        vec![
            KeyBinding::new(KeyChord::plain(KeyCode::Enter), "send"),
            ctrl('j', "new line"),
            KeyBinding::new(KeyChord::plain(KeyCode::Esc), "interrupt / clear"),
            KeyBinding::new(
                KeyChord {
                    code: KeyCode::BackTab,
                    modifiers: KeyModifiers::SHIFT,
                },
                "cycle permission mode",
            ),
            ctrl('o', "transcript viewer"),
            ctrl('r', "search history"),
            ctrl('g', "edit in $EDITOR"),
            KeyBinding::new(KeyChord::plain(KeyCode::PageUp), "scroll up"),
            KeyBinding::new(KeyChord::plain(KeyCode::PageDown), "scroll down"),
        ]
    }
}

#[cfg(test)]
mod tests;
