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
//! coding agent first, and tickets live in the background. `←` on an empty prompt, `/home`
//! (`/agents`, `/tickets`), or Ctrl+T opens the cross-session home ([`ScreenId::Home`],
//! `tm_tui::screens::home::Home`: sessions, background tickets, workers); from there `b` opens the
//! Kanban board and Enter on a ticket opens its detail. Esc walks back one level at a time
//! (`App::back_stack`); Ctrl+T from anywhere else returns straight to the chat.
//!
//! Nothing a human can type into the prompt quits: the only exits are Ctrl+C twice within
//! [`QUIT_WINDOW_MILLIS`], Ctrl+D on an empty prompt, and `/exit`.

mod steps;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use ratatui_core::style::Style;
use tm_tui::chat::commands::CommandId;
use tm_tui::chat::status::StatusInfo;
use tm_tui::chat::transcript::NoticeLevel;
use tm_tui::component::{Component, ComponentId, ComponentParent, FrameContext};
use tm_tui::event::{AppMessage, Event, InputEvent, KeyBinding, KeyChord, Propagation};
use tm_tui::runtime::{MessageSender, Runtime, RuntimeError};
use tm_tui::screens::chat::{ChatAction, ChatScreen, TurnUpdate};
use tm_tui::screens::home::{Home, HomeAction, HomeData, SessionRow, TicketRow, Tone, WorkerRow};
use tm_tui::screens::kanban::{Kanban, KanbanCard, KanbanColumn};
use tm_tui::screens::ticket_detail::TicketDetailScreen;
use tm_tui::theme::Theme;
use tm_tui::widgets_data::form::{Field, Form};
use tm_tui::widgets_data::list::List;
use tm_types::{Clock, SessionId, TicketId, Timestamp};
use tokio::sync::{Mutex, Notify};

use crate::agent::{self, AgentSession};
use crate::args::GlobalOpts;
use crate::project::{Project, Scope};
use crate::render::Renderer;

/// How close together two Ctrl+C presses must be to quit.
const QUIT_WINDOW_MILLIS: i64 = 1_200;

/// How often the home/board screens re-read project state while on screen, so background work
/// done by another process (a `tm sched run` worker) shows up without a keypress.
const REFRESH_MILLIS: i64 = 2_000;

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

    let chat = ChatScreen::new(
        ComponentId::new("tm.chat"),
        session_id.clone(),
        base_status(&project, &view, None),
    );
    let now = project.clock.now();
    let home = Home::new(
        ComponentId::new("tm.home"),
        build_home_data(&project, &view, &session_id, now),
    );
    let kanban = Kanban::new(ComponentId::new("tm.kanban"), build_kanban_columns(&view));

    // `sender` is held for the runtime's whole lifetime: `App::spawn_turn` clones it into each
    // background turn. Dropping it early would close `Runtime`'s message channel.
    let mut app = App {
        id: ComponentId::new("tm.app"),
        project,
        chat,
        home,
        kanban,
        detail: None,
        current: ScreenId::Chat,
        back_stack: Vec::new(),
        shutdown: runtime.shutdown_handle(),
        agent_session: Arc::new(Mutex::new(agent_session)),
        sender,
        attached: None,
        ctrl_c_at: None,
        last_refresh: now,
    };

    runtime.run(&mut app).await.map_err(runtime_error)
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
        tokens: 0,
    }
}

/// How a ticket state reads on the home screen.
fn tone_for(state: tm_core::TicketState) -> Tone {
    use tm_core::TicketState as S;
    match state {
        S::Escalated | S::Blocked | S::Recovery | S::Rework | S::Replan => Tone::Attention,
        S::Leased | S::Running | S::Submitted | S::Verifying | S::Auditing => Tone::Active,
        S::Draft | S::Ready => Tone::Queued,
        S::Closed => Tone::Done,
        S::Cancelled => Tone::Inactive,
    }
}

/// `just now`, `5m`, `3h`, `2d`.
fn ago(millis: i64) -> String {
    let secs = millis.max(0) / 1000;
    match secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", secs / 60),
        3_600..=86_399 => format!("{}h ago", secs / 3_600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// One past session as read from the event log.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LoggedSession {
    id: String,
    turns: usize,
    last: Option<Timestamp>,
}

/// The most recent sessions recorded in the project's event log, newest first: every event a
/// turn writes carries its session id, and each turn brackets itself with `session.started`, so
/// one grouped read-only query yields each session's turn count and last activity. Transcripts
/// themselves are not persisted, so these are history, not something to resume.
fn logged_sessions(project: &Project, limit: usize) -> Vec<LoggedSession> {
    let path = project.store.state_dir().join("project.db");
    let Ok(conn) = tm_events::schema::open_read_connection(&path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT session, SUM(CASE WHEN kind = 'session.started' THEN 1 ELSE 0 END), MAX(ts) \
         FROM events WHERE session IS NOT NULL GROUP BY session ORDER BY MAX(seq) DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map(rusqlite::params![limit as i64], |row| {
        Ok(LoggedSession {
            id: row.get::<_, String>(0)?,
            turns: row.get::<_, i64>(1)?.max(0) as usize,
            last: row
                .get::<_, Option<String>>(2)?
                .and_then(|ts| Timestamp::parse_rfc3339(&ts).ok()),
        })
    });
    match rows {
        Ok(rows) => rows.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

/// Everything the home screen shows, from real project state.
fn build_home_data(
    project: &Project,
    view: &tm_core::ProjectView,
    current: &SessionId,
    now: Timestamp,
) -> HomeData {
    let holder_of = |ticket: &TicketId| {
        view.leases
            .values()
            .find(|l| &l.ticket == ticket && !l.is_expired(now))
            .map(|l| l.holder.to_string())
    };
    let tickets = view
        .tickets
        .values()
        .map(|t| TicketRow {
            id: t.id.to_string(),
            state: format!("{:?}", t.state),
            tone: tone_for(t.state),
            objective: t.objective.lines().next().unwrap_or_default().to_string(),
            note: holder_of(&t.id).map(|h| format!("held by {h}")),
        })
        .collect();
    let workers = view
        .leases
        .values()
        .filter(|l| !l.is_expired(now))
        .map(|l| WorkerRow {
            holder: l.holder.to_string(),
            ticket: l.ticket.to_string(),
            age: ago(now.millis_since(l.acquired)).replace(" ago", ""),
        })
        .collect();

    let logged = logged_sessions(project, 8);
    let current_logged = logged.iter().find(|s| s.id == current.as_str());
    let mut sessions = vec![SessionRow {
        id: current.to_string(),
        turns: current_logged.map_or(0, |s| s.turns),
        when: None,
        current: true,
    }];
    sessions.extend(
        logged
            .iter()
            .filter(|s| s.id != current.as_str())
            .map(|s| SessionRow {
                id: s.id.clone(),
                turns: s.turns,
                when: s.last.map(|t| ago(now.millis_since(t))),
                current: false,
            }),
    );

    HomeData {
        place: place(project),
        tickets,
        workers,
        sessions,
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
    /// Sessions, background tickets, and workers.
    Home,
    /// The Kanban board, opened from `Home`.
    Kanban,
    /// One ticket's detail, opened from `Home` or a Kanban card.
    Detail,
}

fn is_ctrl(key: &crossterm::event::KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c) && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// True when `key` walks back one level on `current`. Esc always does; `Left` does on the detail
/// screen, but not on the board, where `Left`/`Right` move between columns. (`Home` handles its
/// own Esc/`→` as "back to chat" — see `tm_tui::screens::home`.)
fn is_back_chord(key: &crossterm::event::KeyEvent, current: ScreenId) -> bool {
    match key.code {
        KeyCode::Esc => true,
        KeyCode::Left => current == ScreenId::Detail,
        _ => false,
    }
}

/// The root component: owns every screen, routes input to the one on screen, runs chat turns,
/// executes slash commands, and keeps the domain-derived views (home, board, status bar) fresh.
struct App {
    id: ComponentId,
    project: Arc<Project>,
    chat: ChatScreen,
    home: Home,
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
    /// When the home/board data was last re-read.
    last_refresh: Timestamp,
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

    fn open_home(&mut self, now: Timestamp) {
        self.refresh(now);
        self.home.select_chat();
        self.back_stack.clear();
        self.back_stack.push(ScreenId::Chat);
        self.current = ScreenId::Home;
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
        let session = self.chat.session().clone();
        self.home
            .set_data(build_home_data(&self.project, &view, &session, now));
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
        self.ctrl_c_at = Some(now);
        self.chat.interrupt(now, QUIT_WINDOW_MILLIS);
    }

    fn quit_hint_active(&self, now: Timestamp) -> bool {
        self.ctrl_c_at
            .is_some_and(|at| now.millis_since(at) <= QUIT_WINDOW_MILLIS)
    }

    /// Replace the conversation with a fresh one (`/clear`, `/detach`), optionally attached to
    /// `ticket`. Refused while a turn holds the session.
    fn fresh_session(&mut self, ticket: Option<TicketId>) -> Result<SessionId, String> {
        if self.chat.is_turn_running() {
            return Err("A turn is running; wait for it to finish first.".to_string());
        }
        let Ok(mut session) = self.agent_session.try_lock() else {
            return Err("A turn is still finishing; try again in a moment.".to_string());
        };
        let mut fresh = AgentSession::new(
            self.project.clone(),
            Renderer::from_flags(false, true, true),
        );
        if let Some(ticket) = &ticket {
            fresh
                .attach_ticket(ticket.clone())
                .map_err(|e| format!("Could not re-attach {ticket}: {e}"))?;
        }
        let id = fresh.session_id().clone();
        *session = fresh;
        drop(session);
        self.attached = ticket;
        self.chat.reset(id.clone());
        self.chat.status_mut().ticket = self.attached.as_ref().map(|t| t.to_string());
        Ok(id)
    }

    fn run_command(&mut self, id: CommandId, arg: String, now: Timestamp) {
        match id {
            CommandId::Attach => self.attach(&arg),
            CommandId::Detach => match self.attached.clone() {
                None => self
                    .chat
                    .push_notice(NoticeLevel::Info, "No ticket is attached."),
                Some(ticket) => match self.fresh_session(None) {
                    Ok(_) => self.chat.push_notice(
                        NoticeLevel::Info,
                        format!(
                            "Detached from {ticket}. This is a fresh conversation: the session \
                             core cannot drop a ticket mid-conversation yet."
                        ),
                    ),
                    Err(e) => self.chat.push_notice(NoticeLevel::Warning, e),
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
            // The chat screen turns these three into their own actions before they get here;
            // handled anyway so the match stays exhaustive and honest.
            CommandId::Home => return self.open_home(now),
            CommandId::Help => return self.chat.toggle_help(),
            CommandId::Exit => return self.quit(),
        }
        self.refresh(now);
    }

    fn attach(&mut self, arg: &str) {
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

    fn handle_chat_actions(&mut self, now: Timestamp) {
        for action in self.chat.take_actions() {
            match action {
                ChatAction::Send(prompt) => self.spawn_turn(prompt),
                ChatAction::GoHome => self.open_home(now),
                ChatAction::Quit => self.quit(),
                ChatAction::Command { id, arg } => self.run_command(id, arg, now),
            }
        }
    }

    fn handle_home_action(&mut self, now: Timestamp) {
        match self.home.take_action() {
            Some(HomeAction::BackToChat) => self.back_to_chat(),
            Some(HomeAction::OpenTicket(id)) => self.open_detail(&id),
            Some(HomeAction::OpenBoard) => {
                self.refresh(now);
                self.push_screen(ScreenId::Kanban);
            }
            None => {}
        }
    }

    /// Run `prompt` as a turn on a background task, reporting its progress as
    /// [`AppMessage::Turn`]s so the event loop keeps drawing (and accepting input) while it runs.
    ///
    /// Calls the same `AgentSession::run_turn_streaming` the plain loop does. Approval is not
    /// interactive here yet: a suspension is denied, and the transcript says so plainly —
    /// prompting on stdin the way the plain loop does would race this runtime's own reader of the
    /// same terminal (the stdin-contention failure `runtime.rs`'s `Runtime::start` documents).
    fn spawn_turn(&self, prompt: String) {
        let agent_session = Arc::clone(&self.agent_session);
        let sender = self.sender.clone();
        let session_id = self.chat.session().clone();

        tokio::spawn(async move {
            let mut session = agent_session.lock().await;
            let mut latest: Vec<tm_agent::outcome::StepRecord> = Vec::new();
            let mut approvals: Vec<(usize, tm_tui::chat::transcript::Entry)> = Vec::new();
            let mut resolved_ticket: Option<TicketId> = None;

            let outcome = session
                .run_turn_streaming(
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
                            sender.send(AppMessage::Turn {
                                session: session_id.clone(),
                                update: steps::progress(&latest, &approvals),
                            });
                        }
                        agent::TurnEvent::AwaitingApproval(pending) => {
                            approvals.push((
                                latest.len(),
                                steps::approval_notice(&pending.tool_name, &pending.reason),
                            ));
                            sender.send(AppMessage::Turn {
                                session: session_id.clone(),
                                update: steps::progress(&latest, &approvals),
                            });
                        }
                    },
                    |_pending| Ok(false),
                )
                .await;
            drop(session);

            let (notice, failed) = match &outcome {
                Ok(outcome) => steps::outcome_notice(outcome),
                Err(e) => (
                    Some((NoticeLevel::Error, format!("The turn could not run: {e}"))),
                    true,
                ),
            };
            sender.send(AppMessage::Turn {
                session: session_id,
                update: TurnUpdate::Finished { notice, failed },
            });
            if let Some(id) = resolved_ticket {
                sender.send(AppMessage::TicketChanged { id });
            }
        });
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
            ScreenId::Home => self.home.render(area, buf, ctx),
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
                    self.on_ctrl_c(now);
                    return Propagation::Consumed;
                }
                if is_ctrl(key, 't') {
                    if self.current == ScreenId::Chat {
                        self.open_home(now);
                    } else {
                        self.back_to_chat();
                    }
                    return Propagation::Consumed;
                }
            }
            // Turn progress always reaches the chat, whichever screen is showing: a turn does not
            // pause because the human went to look at the board.
            Event::App(AppMessage::Turn { update, .. }) => {
                let finished = matches!(update, TurnUpdate::Finished { .. });
                let propagation = self.chat.handle_event(event, ctx);
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
                if matches!(self.current, ScreenId::Home | ScreenId::Kanban)
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
            ScreenId::Home => {
                let propagation = self.home.handle_event(event, ctx);
                self.handle_home_action(now);
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
                "sessions & tickets / back to chat",
            ),
        ];
        match self.current {
            ScreenId::Chat => bindings.extend(self.chat.keybindings(ctx)),
            ScreenId::Home => bindings.extend(self.home.keybindings(ctx)),
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
        // The chat and home screens take input straight from `App::handle_event`, not through the
        // focus tree; the board and detail screens still use it.
        match self.current {
            ScreenId::Chat | ScreenId::Home => Vec::new(),
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
            ScreenId::Home => (id == self.home.id()).then_some(&self.home as &dyn Component),
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
            ScreenId::Home if id == self.home.id() => Some(&mut self.home as &mut dyn Component),
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
    fn tones_cover_every_ticket_state() {
        for state in tm_core::TicketState::ALL {
            let _ = tone_for(*state);
        }
        assert_eq!(tone_for(tm_core::TicketState::Escalated), Tone::Attention);
        assert_eq!(tone_for(tm_core::TicketState::Running), Tone::Active);
        assert_eq!(tone_for(tm_core::TicketState::Draft), Tone::Queued);
        assert_eq!(tone_for(tm_core::TicketState::Closed), Tone::Done);
    }

    #[test]
    fn ago_is_compact() {
        assert_eq!(ago(5_000), "just now");
        assert_eq!(ago(5 * 60_000), "5m ago");
        assert_eq!(ago(3 * 3_600_000), "3h ago");
        assert_eq!(ago(2 * 86_400_000), "2d ago");
        assert_eq!(ago(-10), "just now");
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
