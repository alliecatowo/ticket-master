//! The bare-`tm` ratatui TUI mode (D-002): entered on the bare-`tm` TTY path, alongside (never
//! instead of) the plain [`crate::agent::AgentSession`] loop `--json`/`--quiet`/`--plain`, a
//! non-tty stdout/stdin, or `TERM=dumb` still gets.
//!
//! This module owns exactly the seam between `tm-cli` and `tm-tui`: [`should_launch`] decides
//! which of the two bare-`tm` paths a given invocation takes, [`App`] is the root
//! [`tm_tui::component::ComponentParent`] wired around `tm-tui`'s screens, and [`run`] drives
//! `tm_tui::runtime::Runtime` against an already-open [`crate::project::Project`]'s real state.
//!
//! # Navigation (`docs/decisions/D-018-tui-chat-first-shell.md`)
//!
//! `tm` opens on the chat ([`ScreenId::Chat`], `tm_tui::screens::chat::ChatScreen`) — tm is a
//! coding agent first, and tickets live in the background. `←` twice on an empty prompt (the
//! first press shows "Press ← again to open tickets", as Claude Code does), `/tickets`
//! (`/home`, `/agents`) opens the tickets screen ([`ScreenId::Tickets`],
//! `tm_tui::screens::tickets::TicketsScreen`, Claude Code's agent view with tickets as rows;
//! `docs/decisions/D-019-claude-code-parity-shell.md` §2); `tm tickets` opens straight onto it.
//! From there Enter attaches the chat to a ticket, and `b` opens the Kanban board. Esc walks back
//! one level at a time (`App::back_stack`); Ctrl+T from anywhere else returns straight to the
//! chat.
//!
//! While the TUI is open, a scheduler runs inside this process
//! ([`crate::sched::spawn_background_runner`]), so tickets dispatched from the tickets screen are
//! actually worked (D-019 §4).
//!
//! Nothing a human can type into the prompt quits: the only exits are Ctrl+C twice within
//! [`QUIT_WINDOW_MILLIS`], Ctrl+D on an empty prompt, and `/exit`.

mod chat_ops;
mod steps;
mod tickets_view;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use ratatui_core::style::Style;
use tm_tui::chat::status::StatusInfo;
use tm_tui::chat::transcript::NoticeLevel;
use tm_tui::component::{Component, ComponentId, ComponentParent, FrameContext};
use tm_tui::event::{AppMessage, Event, InputEvent, KeyBinding, KeyChord, Propagation};
use tm_tui::runtime::{MessageSender, Runtime, RuntimeError};
use tm_tui::screens::chat::{ChatAction, ChatScreen, TurnUpdate};
use tm_tui::screens::kanban::{Kanban, KanbanCard, KanbanColumn};
use tm_tui::screens::ticket_detail::TicketDetailScreen;
use tm_tui::screens::tickets::{FlashTone, TicketsAction, TicketsScreen};
use tm_tui::theme::Theme;
use tm_tui::widgets_data::form::{Field, Form};
use tm_tui::widgets_data::list::List;
use tm_types::{Clock, TicketId, Timestamp};
use tokio::sync::{Mutex, Notify};

use crate::agent::{self, AgentSession};
use crate::args::GlobalOpts;
use crate::project::{Project, Scope};
use crate::render::Renderer;

/// How close together two Ctrl+C presses must be to quit.
const QUIT_WINDOW_MILLIS: i64 = 1_200;

/// How often the tickets/board screens re-read project state while on screen, so background
/// work (this process's own scheduler, or a separate `tm sched run`) shows up without a keypress.
const REFRESH_MILLIS: i64 = 2_000;

/// How long the first `←` on an empty chat prompt waits for the second one that opens tickets.
const LEFT_WINDOW_MILLIS: i64 = 2_000;

/// How often the in-process scheduler ticks while the TUI is open.
const SCHEDULER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Which screen the TUI opens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartOn {
    Chat,
    Tickets,
}

/// Whether a bare `tm` invocation (`cli.command.is_none()`, `cli.prompt.is_none()`) should open
/// the ratatui TUI ([`run`]) rather than [`crate::agent::AgentSession::run_interactive`].
///
/// Every one of these forces the plain loop, per D-002's "graceful degradation" and the backlog's
/// `--plain` requirement:
/// - `--plain`, the explicit opt-out.
/// - `--json`, since a TUI has no JSON output to emit and D-002 requires `--json` to keep working
///   untouched, not be silently overridden into an interactive screen.
/// - `--quiet`, which asks for *less* ceremony, the opposite of an alternate-screen UI.
/// - `TERM=dumb`, the traditional "no cursor addressing at all" signal.
/// - stdout or stdin not being a real tty (piped, redirected, CI): `Runtime::start` needs to
///   write escape sequences to stdout and `Runtime::run`'s `EventStream` reads raw key events
///   from stdin, so either being non-interactive makes the TUI unusable, not merely undesired.
pub fn should_launch(global: &GlobalOpts) -> bool {
    use std::io::IsTerminal;

    if global.plain || global.json || global.quiet {
        return false;
    }
    if std::env::var("TERM")
        .map(|term| term == "dumb")
        .unwrap_or(false)
    {
        return false;
    }
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

/// Open the chat over `project` and drive it until the human quits or a termination signal
/// arrives.
///
/// Every submitted prompt runs through the exact same turn logic the plain `tm`/`tm -p` loop uses
/// (`AgentSession::run_turn_streaming`) — see [`App::spawn_turn`].
pub async fn run(project: Arc<Project>, resumed: Option<AgentSession>) -> tm_types::Result<()> {
    run_on(project, resumed, StartOn::Chat).await
}

/// `tm tickets`: the same TUI, opened on the tickets screen (Claude Code's `claude agents`); Esc
/// goes to a fresh chat.
pub async fn run_tickets(project: Arc<Project>) -> tm_types::Result<()> {
    run_on(project, None, StartOn::Tickets).await
}

async fn run_on(
    project: Arc<Project>,
    resumed: Option<AgentSession>,
    start: StartOn,
) -> tm_types::Result<()> {
    let view = project
        .store
        .view()
        .map_err(|e| tm_types::TmError::storage(e.to_string()))?;

    let (mut runtime, sender) = Runtime::start(project.clock.clone(), Theme::dark())
        .await
        .map_err(runtime_error)?;

    // `--quiet`/`--no-color`: the TUI never renders through `Renderer` (its own widgets own
    // presentation), but `AgentSession::new` still needs one to construct.
    let agent_session = resumed.unwrap_or_else(|| {
        AgentSession::new(project.clone(), Renderer::from_flags(false, true, true))
    });
    let session_id = agent_session.session_id().clone();

    let mut chat = ChatScreen::new(
        ComponentId::new("tm.chat"),
        session_id.clone(),
        base_status(&project, &view, None),
    );
    let chat_ext = chat_ops::ChatExt::start(&project, &agent_session, &mut chat);
    let attached = agent_session.attached_ticket().cloned();
    let now = project.clock.now();

    // D-019 §4: dispatched tickets get worked while `tm` is open, without anyone having to know
    // `tm sched run` exists. If the scheduler cannot start (typically no provider configured),
    // the tickets header says so instead of the Queued group waiting silently forever.
    let (scheduler, worker_notice) =
        match crate::sched::spawn_background_runner(project.clone(), SCHEDULER_INTERVAL) {
            Ok(handle) => (Some(handle), None),
            Err(e) => {
                tracing::warn!(error = %e, "background scheduler did not start");
                (None, Some(format!("background worker off: {e}")))
            }
        };

    let activity = tickets_view_index(&project);
    let tickets = TicketsScreen::new(
        ComponentId::new("tm.tickets"),
        tickets_view::build(
            &view,
            &activity,
            now,
            scheduler.is_some(),
            tickets_header(&project),
            worker_notice.clone(),
        ),
    );
    let kanban = Kanban::new(ComponentId::new("tm.kanban"), build_kanban_columns(&view));

    // `sender` is held for the runtime's whole lifetime: `App::spawn_turn` clones it into each
    // background turn. Dropping it early would close `Runtime`'s message channel.
    let mut app = App {
        id: ComponentId::new("tm.app"),
        project,
        chat,
        tickets,
        activity,
        local_worker: scheduler.is_some(),
        worker_notice,
        left_at: None,
        kanban,
        detail: None,
        current: ScreenId::Chat,
        back_stack: Vec::new(),
        shutdown: runtime.shutdown_handle(),
        agent_session: Arc::new(Mutex::new(agent_session)),
        sender,
        attached,
        ctrl_c_at: None,
        last_refresh: now,
        chat_ext,
    };

    if start == StartOn::Tickets {
        app.open_tickets(now);
    }

    let result = runtime.run(&mut app).await.map_err(runtime_error);
    if let Some(handle) = scheduler {
        handle.abort();
    }
    result
}

/// A fresh activity index for `project`, already caught up with its event log.
fn tickets_view_index(project: &Project) -> crate::tickets::overview::ActivityIndex {
    let mut index = crate::tickets::overview::ActivityIndex::new();
    if let Err(e) = index.refresh(project.store.state_dir()) {
        tracing::debug!(error = %e, "could not read ticket activity");
    }
    index
}

/// The tickets header's model and place.
fn tickets_header(project: &Project) -> (String, String) {
    (configured_model(), place(project))
}

/// Map a [`RuntimeError`] to the `tm_types::Result` every `tm-cli` execution module returns.
fn runtime_error(err: RuntimeError) -> tm_types::TmError {
    tm_types::TmError::storage(format!("tm-tui runtime error: {err}"))
}

/// The model the status bar names before any turn has reported who actually answered — the same
/// provider selection `agent::build_fabric` makes, so the name does not jump after the first turn.
fn configured_model() -> String {
    if std::env::var_os("TM_TEST_MOCK_PROVIDER").is_some() {
        return "mock/m1".to_string();
    }
    match tm_provider::DevPassProvider::preferred_model() {
        Some(model) => format!("devpass/{model}"),
        None => format!("anthropic/{}", agent::AGENT_MODEL),
    }
}

/// `path` with the home directory shortened to `~`.
fn display_path(path: &Path) -> String {
    let shown = path.display().to_string();
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if !home.as_os_str().is_empty() => match path.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => shown,
        },
        _ => shown,
    }
}

/// The checked-out branch of the git repository containing `root` (or a short commit id when
/// HEAD is detached), read straight from `.git/HEAD` — no subprocess on the render path.
fn git_branch(root: &Path) -> Option<String> {
    let dir = root.ancestors().find(|d| d.join(".git").exists())?;
    let dot_git = dir.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        // A worktree or submodule: `.git` is a file pointing at the real git dir.
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let target = PathBuf::from(pointer.trim().strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target
        } else {
            dir.join(target)
        }
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(reference) => Some(
            reference
                .strip_prefix("refs/heads/")
                .unwrap_or(reference)
                .to_string(),
        ),
        None => head.get(..7).map(str::to_string),
    }
}

/// `~/src/app (main)`.
fn place(project: &Project) -> String {
    let mut place = display_path(&project.root);
    if let Some(branch) = git_branch(&project.root) {
        place.push_str(&format!(" ({branch})"));
    }
    place
}

fn is_open(state: tm_core::TicketState) -> bool {
    !matches!(
        state,
        tm_core::TicketState::Closed | tm_core::TicketState::Cancelled
    )
}

/// The status-bar facts `App` owns (the chat screen overlays the served model and its tokens).
fn base_status(
    project: &Project,
    view: &tm_core::ProjectView,
    ticket: Option<&TicketId>,
) -> StatusInfo {
    StatusInfo {
        model: configured_model(),
        cwd: display_path(&project.root),
        branch: git_branch(&project.root),
        global_scope: project.scope == Scope::Global,
        ticket: ticket.map(|t| t.to_string()),
        open_tickets: view.tickets.values().filter(|t| is_open(t.state)).count(),
        ..StatusInfo::default()
    }
}

/// Build the Kanban board's columns from `view`'s real ticket state: one column per real
/// `tm_core::TicketState` variant, in `TicketState::ALL`'s declaration order, each holding every
/// ticket currently in that state.
///
/// Deliberately one column per raw state rather than a hand-curated "phase" grouping with
/// invented labels — the board names the real state machine, and a board built directly from
/// `TicketState::ALL` never drifts out of sync with the enum the way a hardcoded grouping would.
fn build_kanban_columns(view: &tm_core::ProjectView) -> Vec<KanbanColumn> {
    tm_core::TicketState::ALL
        .iter()
        .map(|state| {
            let cards = view
                .tickets
                .values()
                .filter(|ticket| &ticket.state == state)
                .map(|ticket| KanbanCard::new(ticket.id.to_string(), ticket.objective.clone()))
                .collect();
            KanbanColumn::new(format!("{state:?}"), cards)
        })
        .collect()
}

/// Build a read-only drill-down screen for `ticket_id`, or `None` if it no longer exists in
/// `view` (the list went stale between drawing and Enter — declining to open anything is the
/// honest resolution).
fn build_detail_screen(
    view: &tm_core::ProjectView,
    ticket_id: &TicketId,
) -> Option<TicketDetailScreen> {
    let ticket = view.tickets.get(ticket_id)?;

    let fields = Form::new(
        ComponentId::new("tm.detail.fields"),
        vec![
            Field::read_only("ID", ticket.id.to_string()),
            Field::read_only("State", format!("{:?}", ticket.state)),
            Field::read_only("Kind", format!("{:?}", ticket.kind)),
            Field::read_only("Objective", ticket.objective.clone()),
            Field::read_only("Priority", ticket.priority.to_string()),
            Field::read_only("Attempts", ticket.attempts.to_string()),
        ],
    );

    let mut activity = Vec::new();
    if !ticket.dependencies.is_empty() {
        let deps: Vec<String> = ticket.dependencies.iter().map(|d| d.to_string()).collect();
        activity.push(format!("depends on: {}", deps.join(", ")));
    }
    if !ticket.children.is_empty() {
        let children: Vec<String> = ticket.children.iter().map(|c| c.to_string()).collect();
        activity.push(format!("children: {}", children.join(", ")));
    }
    if let Some(lease) = view
        .leases
        .values()
        .find(|lease| &lease.ticket == ticket_id)
    {
        activity.push(format!(
            "leased by {} since {}",
            lease.holder, lease.acquired
        ));
    }
    for failure in &ticket.failures {
        activity.push(format!(
            "attempt {}: {:?} — {}",
            failure.attempt, failure.class, failure.detail
        ));
    }
    if activity.is_empty() {
        activity.push("no recorded activity yet".to_string());
    }
    let mut activity_list = List::new(ComponentId::new("tm.detail.activity"));
    activity_list.set_items(activity);

    Some(TicketDetailScreen::new(
        ComponentId::new("tm.detail"),
        ticket.id.clone(),
        fields,
        activity_list,
    ))
}

/// Which screen is on screen. `Chat` is the root: it is never pushed onto `App::back_stack`, and
/// it is where every back-navigation eventually lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenId {
    /// The conversation (the default).
    Chat,
    /// The tickets screen (Claude Code's agent view, with tickets as rows).
    Tickets,
    /// The Kanban board, opened from `Tickets`.
    Kanban,
    /// One ticket's detail, opened from a Kanban card.
    Detail,
}

/// Run a store command against ticket `id`, as a message for the tickets screen's footer.
fn with_ticket(
    id: &str,
    f: impl FnOnce(&TicketId) -> tm_types::Result<String>,
) -> Result<String, String> {
    let ticket = TicketId::new(id).map_err(|e| format!("{id}: {e}"))?;
    f(&ticket).map_err(|e| format!("{id}: {e}"))
}

fn is_ctrl(key: &crossterm::event::KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c) && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// True when `key` walks back one level on `current`. Esc always does; `Left` does on the detail
/// screen, but not on the board, where `Left`/`Right` move between columns. (`Tickets` handles its
/// own Esc as "back to chat" — see `tm_tui::screens::tickets`.)
fn is_back_chord(key: &crossterm::event::KeyEvent, current: ScreenId) -> bool {
    match key.code {
        KeyCode::Esc => true,
        KeyCode::Left => current == ScreenId::Detail,
        _ => false,
    }
}

/// The root component: owns every screen, routes input to the one on screen, runs chat turns,
/// executes slash commands, and keeps the domain-derived views (tickets, board, status bar) fresh.
struct App {
    id: ComponentId,
    project: Arc<Project>,
    chat: ChatScreen,
    tickets: TicketsScreen,
    /// Per-ticket facts from the event log the tickets screen summarizes, read incrementally.
    activity: crate::tickets::overview::ActivityIndex,
    /// Whether this process's own scheduler is running (so queued tickets will be picked up).
    local_worker: bool,
    /// Why the background worker is not running, if it could not start.
    worker_notice: Option<String>,
    /// When `←` was last pressed on an empty chat prompt, for "press ← again to open tickets".
    left_at: Option<Timestamp>,
    kanban: Kanban,
    /// Rebuilt fresh on every open, so it never shows stale ticket data.
    detail: Option<TicketDetailScreen>,
    current: ScreenId,
    /// Every screen navigated away from to reach `current`, most recent last.
    back_stack: Vec<ScreenId>,
    /// Notified to quit, the same path `Runtime`'s own `SIGTERM`/`SIGHUP` handling takes.
    shutdown: Arc<Notify>,
    /// The one conversation this process drives. Behind a `tokio::sync::Mutex` because turns run
    /// on a background task; commands that need it (`/attach`, `/clear`) `try_lock` and refuse
    /// while a turn holds it rather than blocking the event loop.
    agent_session: Arc<Mutex<AgentSession>>,
    /// Cloned into every spawned turn so it can report progress back into the event loop.
    sender: MessageSender,
    /// The ticket the conversation is attached to, mirrored here for the status bar and `/decide`.
    attached: Option<TicketId>,
    /// When Ctrl+C was last pressed, for the double-press quit.
    ctrl_c_at: Option<Timestamp>,
    /// When the tickets/board data was last re-read.
    last_refresh: Timestamp,
    /// The chat's own wiring: history, `@` files, interrupts, permission prompts
    /// (`tui/chat_ops.rs`).
    chat_ext: chat_ops::ChatExt,
}

// Manual: neither `Project` nor `AgentSession` implements `Debug`.
impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("id", &self.id)
            .field("current", &self.current)
            .field("session", self.chat.session())
            .finish_non_exhaustive()
    }
}

impl App {
    fn quit(&self) {
        // `notify_one`, not `notify_waiters`: this runs inside the very `select!` iteration of
        // `Runtime::run` that delivered the key, whose `shutdown.notified()` listener is about to
        // be dropped and rebuilt; `notify_one` stores a permit for that next listener instead of
        // being lost in the gap.
        self.shutdown.notify_one();
    }

    fn push_screen(&mut self, next: ScreenId) {
        if self.current != next {
            self.back_stack.push(self.current);
            self.current = next;
        }
    }

    fn pop_screen(&mut self) {
        self.current = self.back_stack.pop().unwrap_or(ScreenId::Chat);
    }

    fn back_to_chat(&mut self) {
        self.back_stack.clear();
        self.current = ScreenId::Chat;
    }

    fn open_tickets(&mut self, now: Timestamp) {
        self.refresh(now);
        self.left_at = None;
        // Like Claude Code's `←`, land on the row the conversation came from.
        if let Some(ticket) = self.attached.clone() {
            self.tickets.select_ticket(ticket.as_str());
        }
        self.back_stack.clear();
        self.back_stack.push(ScreenId::Chat);
        self.current = ScreenId::Tickets;
    }

    fn open_detail(&mut self, ticket_id: &str) {
        let Ok(id) = TicketId::new(ticket_id) else {
            return;
        };
        let Ok(view) = self.project.store.view() else {
            return;
        };
        if let Some(screen) = build_detail_screen(&view, &id) {
            self.detail = Some(screen);
            self.push_screen(ScreenId::Detail);
        }
    }

    /// Re-read project state into every domain-derived view. A read failure keeps the previous
    /// data on screen rather than tearing the UI down over a transient store error.
    fn refresh(&mut self, now: Timestamp) {
        self.last_refresh = now;
        let Ok(view) = self.project.store.view() else {
            return;
        };
        if let Err(e) = self.activity.refresh(self.project.store.state_dir()) {
            tracing::debug!(error = %e, "could not read ticket activity");
        }
        self.tickets.set_data(tickets_view::build(
            &view,
            &self.activity,
            now,
            self.local_worker,
            tickets_header(&self.project),
            self.worker_notice.clone(),
        ));
        self.kanban.set_columns(build_kanban_columns(&view));
        if let Some(open) = self.detail.as_ref().map(|d| d.ticket().clone()) {
            if let Some(screen) = build_detail_screen(&view, &open) {
                self.detail = Some(screen);
            }
        }
        let fresh = base_status(&self.project, &view, self.attached.as_ref());
        let status = self.chat.status_mut();
        status.open_tickets = fresh.open_tickets;
        status.branch = fresh.branch;
        status.ticket = fresh.ticket;
    }

    fn on_ctrl_c(&mut self, now: Timestamp) {
        if let Some(at) = self.ctrl_c_at {
            if now.millis_since(at) <= QUIT_WINDOW_MILLIS {
                self.quit();
                return;
            }
        }
        if self.current == ScreenId::Chat {
            // The chat closes a dialog, interrupts a turn, or clears the prompt (D-019); only
            // the last two arm the second press.
            let arm = self.chat.on_ctrl_c(now, QUIT_WINDOW_MILLIS);
            self.ctrl_c_at = arm.then_some(now);
            self.handle_chat_actions(now);
            return;
        }
        self.ctrl_c_at = Some(now);
        self.chat.interrupt(now, QUIT_WINDOW_MILLIS);
    }

    fn quit_hint_active(&self, now: Timestamp) -> bool {
        self.ctrl_c_at
            .is_some_and(|at| now.millis_since(at) <= QUIT_WINDOW_MILLIS)
    }

    fn handle_chat_actions(&mut self, now: Timestamp) {
        self.persist_history();
        for action in self.chat.take_actions() {
            match action {
                ChatAction::Send(prompt) => self.spawn_turn(prompt),
                ChatAction::GoHome => self.open_tickets(now),
                ChatAction::Quit => self.quit(),
                ChatAction::Command { id, arg } => self.run_command(id, arg, now),
                // `!`, Esc, Shift+Tab, Ctrl+G, /resume, permission answers: `tui/chat_ops.rs`.
                other => self.on_chat_action(other, now),
            }
        }
    }

    /// Execute what the tickets screen asked for.
    fn handle_tickets_actions(&mut self, now: Timestamp) {
        const FLASH_MILLIS: i64 = 4_000;
        for action in self.tickets.take_actions() {
            let outcome: Result<String, String> = match action {
                TicketsAction::BackToChat => {
                    self.back_to_chat();
                    continue;
                }
                TicketsAction::OpenBoard => {
                    self.refresh(now);
                    self.push_screen(ScreenId::Kanban);
                    continue;
                }
                TicketsAction::Attach(id) => {
                    self.attach_from_tickets(&id, now);
                    continue;
                }
                TicketsAction::Dispatch(task) => {
                    match crate::tickets::create_and_queue(&self.project, &task) {
                        Ok(id) => {
                            self.refresh(now);
                            self.tickets.select_ticket(id.as_str());
                            Ok(if self.local_worker {
                                format!("Dispatched {id} to a background worker.")
                            } else {
                                format!("Queued {id}. No worker is running here: tm sched run")
                            })
                        }
                        Err(e) => Err(format!("Could not dispatch: {e}")),
                    }
                }
                TicketsAction::Cancel(id) => with_ticket(&id, |t| {
                    self.project
                        .store
                        .cancel(
                            t,
                            Some("cancelled from tm tickets".to_string()),
                            self.project.actor.clone(),
                        )
                        .map(|_| format!("Cancelled {t}."))
                }),
                TicketsAction::Accept(id) => with_ticket(&id, |t| {
                    self.project
                        .store
                        .accept(t, None, self.project.actor.clone())
                        .map(|_| format!("Accepted {t}: closed (tm ticket reopen {t} undoes it)."))
                }),
                TicketsAction::Reject { id, reason } => with_ticket(&id, |t| {
                    self.project
                        .store
                        .reject(t, reason.clone(), self.project.actor.clone())
                        .map(|_| format!("Rejected {t}: the next attempt sees your reason."))
                }),
            };
            self.refresh(now);
            let until = now.plus_millis(FLASH_MILLIS);
            match outcome {
                Ok(text) => self.tickets.flash(text, FlashTone::Success, until),
                Err(text) => self.tickets.flash(text, FlashTone::Error, until),
            }
        }
    }

    /// Enter/→ on a ticket row: attach the conversation to it and go back to the chat, with a
    /// one-line recap of where the ticket stands (Claude Code posts one when you attach). If the
    /// attach is refused (a turn is running), the chat says why.
    fn attach_from_tickets(&mut self, id: &str, now: Timestamp) {
        self.attach(id);
        self.back_to_chat();
        if self.attached.as_ref().map(TicketId::as_str) != Some(id) {
            return;
        }
        let Ok(view) = self.project.store.view() else {
            return;
        };
        let recap = crate::tickets::overview::overviews(
            &view,
            &self.activity,
            now,
            self.local_worker,
            true,
        )
        .into_iter()
        .find(|o| o.id.as_str() == id)
        .map(|o| tickets_view::recap(&o));
        if let Some(recap) = recap {
            self.chat.push_notice(NoticeLevel::Info, recap);
        }
    }

    /// Draw the "press Ctrl+C again" warning on screens that have no status bar of their own.
    fn render_quit_hint(
        &self,
        area: ratatui_core::layout::Rect,
        buf: &mut ratatui_core::buffer::Buffer,
        ctx: &FrameContext<'_>,
    ) {
        let text = " Press Ctrl+C again to quit ";
        let width = text.chars().count() as u16;
        if area.width <= width || area.height == 0 {
            return;
        }
        buf.set_string(
            area.x + area.width - width - 1,
            area.y + area.height - 1,
            text,
            Style::default()
                .fg(ctx.theme.background)
                .bg(ctx.theme.warning),
        );
    }
}

impl Component for App {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(
        &self,
        area: ratatui_core::layout::Rect,
        buf: &mut ratatui_core::buffer::Buffer,
        ctx: &FrameContext<'_>,
    ) {
        match self.current {
            ScreenId::Chat => self.chat.render(area, buf, ctx),
            ScreenId::Tickets => self.tickets.render(area, buf, ctx),
            ScreenId::Kanban => self.kanban.render(area, buf, ctx),
            ScreenId::Detail => {
                if let Some(detail) = &self.detail {
                    detail.render(area, buf, ctx);
                }
                // The detail screen has no key hints of its own; say how to leave it.
                if area.height > 2 && area.width > 30 {
                    buf.set_stringn(
                        area.x + 1,
                        area.y + area.height - 1,
                        "esc back   ctrl+t chat   ctrl+c twice quit",
                        (area.width - 2) as usize,
                        Style::default().fg(ctx.theme.muted),
                    );
                }
            }
        }
        if self.current != ScreenId::Chat && self.quit_hint_active(ctx.clock.now()) {
            self.render_quit_hint(area, buf, ctx);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        let now = ctx.clock.now();

        match event {
            Event::Input(InputEvent::Key(key)) => {
                if key.kind == KeyEventKind::Release {
                    return Propagation::Consumed;
                }
                // The only two global chords. Neither is a printable key.
                if is_ctrl(key, 'c') {
                    // On the tickets screen the first Ctrl+C clears a half-typed task, as in
                    // Claude Code's agent view; only on an empty input does it count toward quit.
                    if self.current == ScreenId::Tickets && self.tickets.clear_input() {
                        return Propagation::Consumed;
                    }
                    self.on_ctrl_c(now);
                    return Propagation::Consumed;
                }
                // Ctrl+T is the chat's task checklist (D-019 §1, as in Claude Code); from any
                // other screen it still returns to the chat.
                if is_ctrl(key, 't') && self.current != ScreenId::Chat {
                    self.back_to_chat();
                    return Propagation::Consumed;
                }
                // `←` on an empty chat prompt with nothing open over it: the first press says
                // what a second one does, and the second opens tickets (Claude Code's "Press ←
                // again to open agents").
                if self.current == ScreenId::Chat
                    && key.code == KeyCode::Left
                    && key.modifiers.is_empty()
                    && self.chat.left_opens_tickets()
                {
                    match self.left_at {
                        Some(at) if now.millis_since(at) <= LEFT_WINDOW_MILLIS => {
                            self.open_tickets(now);
                        }
                        _ => {
                            self.left_at = Some(now);
                            self.chat.show_hint(
                                "Press ← again to open tickets",
                                NoticeLevel::Info,
                                true,
                                now.plus_millis(LEFT_WINDOW_MILLIS),
                            );
                        }
                    }
                    return Propagation::Consumed;
                }
                self.left_at = None;
            }
            // Turn progress always reaches the chat, whichever screen is showing: a turn does not
            // pause because the human went to look at the board.
            Event::App(AppMessage::Turn { update, .. }) => {
                let finished = matches!(update, TurnUpdate::Finished { .. });
                let propagation = self.chat.handle_event(event, ctx);
                // Finishing may have started what was queued behind the turn.
                self.handle_chat_actions(now);
                // A turn waiting for permission must be seen, whichever screen is showing.
                if self.chat.is_awaiting_approval() && self.current != ScreenId::Chat {
                    self.back_to_chat();
                }
                if finished {
                    self.refresh(now);
                }
                return if propagation.is_consumed() {
                    propagation
                } else {
                    // A stale session's update (after /clear) is dropped, not an error.
                    Propagation::Consumed
                };
            }
            Event::App(AppMessage::TicketChanged { .. }) => {
                self.refresh(now);
                return Propagation::Consumed;
            }
            Event::Tick { .. } => {
                if matches!(self.current, ScreenId::Tickets | ScreenId::Kanban)
                    && now.millis_since(self.last_refresh) >= REFRESH_MILLIS
                {
                    self.refresh(now);
                }
                return Propagation::Propagate;
            }
            _ => {}
        }

        match self.current {
            ScreenId::Chat => {
                let propagation = self.chat.handle_event(event, ctx);
                self.handle_chat_actions(now);
                propagation
            }
            ScreenId::Tickets => {
                let propagation = self.tickets.handle_event(event, ctx);
                self.handle_tickets_actions(now);
                propagation
            }
            ScreenId::Kanban => {
                if let Event::Input(InputEvent::Key(key)) = event {
                    if is_back_chord(key, ScreenId::Kanban) {
                        self.pop_screen();
                        return Propagation::Consumed;
                    }
                }
                let propagation = self.kanban.handle_event(event, ctx);
                if let Some(ticket_id) = self.kanban.take_activation() {
                    self.open_detail(&ticket_id);
                }
                propagation
            }
            ScreenId::Detail => {
                if let Event::Input(InputEvent::Key(key)) = event {
                    if is_back_chord(key, ScreenId::Detail) {
                        self.pop_screen();
                        return Propagation::Consumed;
                    }
                }
                match &mut self.detail {
                    Some(detail) => detail.handle_event(event, ctx),
                    None => Propagation::Propagate,
                }
            }
        }
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        let mut bindings = vec![
            KeyBinding::new(
                KeyChord {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                },
                "quit (press twice)",
            ),
            KeyBinding::new(
                KeyChord {
                    code: KeyCode::Char('t'),
                    modifiers: KeyModifiers::CONTROL,
                },
                "task checklist (chat) / back to chat",
            ),
        ];
        match self.current {
            ScreenId::Chat => bindings.extend(self.chat.keybindings(ctx)),
            ScreenId::Tickets => bindings.extend(self.tickets.keybindings(ctx)),
            ScreenId::Kanban => {
                bindings.push(KeyBinding::new(KeyChord::plain(KeyCode::Esc), "back"));
                bindings.extend(self.kanban.keybindings(ctx));
            }
            ScreenId::Detail => {
                bindings.push(KeyBinding::new(KeyChord::plain(KeyCode::Esc), "back"));
                bindings.push(KeyBinding::new(KeyChord::plain(KeyCode::Left), "back"));
                if let Some(detail) = &self.detail {
                    bindings.extend(detail.keybindings(ctx));
                }
            }
        }
        bindings
    }

    fn focusable_children(&self) -> Vec<ComponentId> {
        // The chat and tickets screens take input straight from `App::handle_event`, not through
        // the focus tree; the board and detail screens still use it.
        match self.current {
            ScreenId::Chat | ScreenId::Tickets => Vec::new(),
            ScreenId::Kanban => vec![self.kanban.id()],
            ScreenId::Detail => match &self.detail {
                Some(detail) => {
                    let mut ids = vec![detail.id()];
                    ids.extend(detail.focusable_children());
                    ids
                }
                None => Vec::new(),
            },
        }
    }
}

impl ComponentParent for App {
    fn resolve(&self, id: ComponentId) -> Option<&dyn Component> {
        if id == self.id {
            return Some(self);
        }
        match self.current {
            ScreenId::Chat => (id == self.chat.id()).then_some(&self.chat as &dyn Component),
            ScreenId::Tickets => {
                (id == self.tickets.id()).then_some(&self.tickets as &dyn Component)
            }
            ScreenId::Kanban => (id == self.kanban.id()).then_some(&self.kanban as &dyn Component),
            ScreenId::Detail => self.detail.as_ref().and_then(|detail| detail.resolve(id)),
        }
    }

    fn resolve_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component> {
        if id == self.id {
            return Some(self);
        }
        match self.current {
            ScreenId::Chat if id == self.chat.id() => Some(&mut self.chat as &mut dyn Component),
            ScreenId::Tickets if id == self.tickets.id() => {
                Some(&mut self.tickets as &mut dyn Component)
            }
            ScreenId::Kanban if id == self.kanban.id() => {
                Some(&mut self.kanban as &mut dyn Component)
            }
            ScreenId::Detail => self
                .detail
                .as_mut()
                .and_then(|detail| detail.resolve_mut(id)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::{ExecutorRequirements, RetryPolicy, Store, TicketKind, VerificationPolicy};
    use tm_types::{Authority, Budget, ParticipantId, Role, Tolerance};

    fn global(plain: bool, json: bool, quiet: bool) -> GlobalOpts {
        GlobalOpts {
            json,
            quiet,
            no_color: false,
            plain,
            project: None,
        }
    }

    #[test]
    fn should_launch_is_false_when_plain_is_set() {
        assert!(!should_launch(&global(true, false, false)));
    }

    #[test]
    fn should_launch_is_false_when_json_is_set() {
        assert!(!should_launch(&global(false, true, false)));
    }

    #[test]
    fn should_launch_is_false_when_quiet_is_set() {
        assert!(!should_launch(&global(false, false, true)));
    }

    /// A real ticket, created through `Store::create_ticket` rather than a hand-built literal.
    fn create_real_ticket(store: &Store, objective: &str) -> tm_types::TicketId {
        let actor = ParticipantId::new("human:tester").unwrap();
        let events = store
            .create_ticket(
                TicketKind::Investigation,
                objective.to_string(),
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
                actor,
            )
            .expect("create_ticket");
        events
            .iter()
            .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.clone()))
            .expect("create_ticket emits ticket.created")
    }

    fn plain_key(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn only_modifier_chords_are_global() {
        assert!(is_ctrl(&ctrl_key(KeyCode::Char('c')), 'c'));
        assert!(is_ctrl(&ctrl_key(KeyCode::Char('t')), 't'));
        for c in ['q', 't', 'c', '?', '/'] {
            assert!(
                !is_ctrl(&plain_key(KeyCode::Char(c)), 'c')
                    && !is_ctrl(&plain_key(KeyCode::Char(c)), 't'),
                "a bare {c:?} must never be a global chord"
            );
        }
    }

    #[test]
    fn esc_is_back_everywhere_and_left_only_on_detail() {
        assert!(is_back_chord(&plain_key(KeyCode::Esc), ScreenId::Kanban));
        assert!(is_back_chord(&plain_key(KeyCode::Esc), ScreenId::Detail));
        assert!(is_back_chord(&plain_key(KeyCode::Left), ScreenId::Detail));
        assert!(
            !is_back_chord(&plain_key(KeyCode::Left), ScreenId::Kanban),
            "Left is Kanban's own column-navigation key, not its back chord"
        );
    }

    #[test]
    fn display_path_shortens_home() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        assert_eq!(display_path(&home.join("src").join("app")), "~/src/app");
        assert_eq!(display_path(&home), "~");
        assert_eq!(
            display_path(Path::new("/definitely/elsewhere")),
            "/definitely/elsewhere"
        );
    }

    #[test]
    fn git_branch_reads_head_including_worktree_pointers() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(
            repo.join(".git").join("HEAD"),
            "ref: refs/heads/feature/x\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        assert_eq!(git_branch(&repo.join("sub")).as_deref(), Some("feature/x"));

        let wt = tmp.path().join("wt");
        let gitdir = tmp.path().join("gitdirs").join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        std::fs::write(gitdir.join("HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(git_branch(&wt).as_deref(), Some("0123456"));

        assert_eq!(git_branch(&tmp.path().join("nowhere")), None);
    }

    #[test]
    fn build_kanban_columns_has_one_column_per_real_ticket_state_and_no_more() {
        let view = tm_core::ProjectView::empty();
        let columns = build_kanban_columns(&view);
        assert_eq!(columns.len(), tm_core::TicketState::ALL.len());
        let titles: Vec<&str> = columns.iter().map(|c| c.title.as_str()).collect();
        assert!(titles.contains(&"Running"));
        assert!(titles.contains(&"Closed"));
    }

    #[test]
    fn build_kanban_columns_places_a_real_ticket_in_its_own_states_column() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).expect("open store");
        create_real_ticket(&store, "wire the kanban board");

        let view = store.view().expect("view");
        let columns = build_kanban_columns(&view);
        let draft = columns
            .iter()
            .find(|c| c.title == "Draft")
            .expect("a Draft column always exists");
        assert!(draft
            .cards
            .iter()
            .any(|c| c.title == "wire the kanban board"));
        for other in columns.iter().filter(|c| c.title != "Draft") {
            assert!(
                !other
                    .cards
                    .iter()
                    .any(|c| c.title == "wire the kanban board"),
                "a ticket must appear in exactly one state's column, not {}",
                other.title
            );
        }
    }

    #[test]
    fn build_kanban_columns_over_an_empty_project_has_no_fake_cards() {
        let view = tm_core::ProjectView::empty();
        let columns = build_kanban_columns(&view);
        assert!(columns.iter().all(|c| c.cards.is_empty()));
    }

    #[test]
    fn build_detail_screen_for_an_unknown_ticket_is_none() {
        let view = tm_core::ProjectView::empty();
        let id = TicketId::new("T-999").expect("T-999 is a valid TicketId shape");
        assert!(build_detail_screen(&view, &id).is_none());
    }

    #[test]
    fn build_detail_screen_for_a_real_ticket_carries_its_real_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).expect("open store");
        let ticket_id = create_real_ticket(&store, "investigate the flaky test");

        let view = store.view().expect("view");
        let screen = build_detail_screen(&view, &ticket_id)
            .expect("a ticket that was just created must still be in a freshly-read view");
        assert_eq!(screen.ticket(), &ticket_id);
        assert!(format!("{screen:?}").contains("no recorded activity yet"));
    }
}
