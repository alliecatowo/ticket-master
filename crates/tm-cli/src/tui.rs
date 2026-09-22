//! The bare-`tm` ratatui TUI mode (D-002): entered on the bare-`tm` TTY path, alongside (never
//! instead of) the plain [`crate::agent::AgentSession`] loop `--json`/`--quiet`/`--plain`, a
//! non-tty stdout/stdin, or `TERM=dumb` still gets.
//!
//! This module owns exactly the seam between `tm-cli` and `tm-tui`: [`should_launch`] decides
//! which of the two bare-`tm` paths a given invocation takes, [`App`] is the small root
//! [`tm_tui::component::ComponentParent`] this crate wires around `tm-tui`'s screens, and [`run`]
//! drives `tm_tui::runtime::Runtime` against an already-open [`crate::project::Project`]'s real
//! state.
//!
//! `App` is a real navigation shell, not a single permanently-visible screen: [`ScreenId`] names
//! which of [`tm_tui::screens::home::Home`] (the bare-`tm` default, unchanged), the Kanban board
//! (`tm_tui::screens::kanban::Kanban`, entered from `Home` via [`is_tickets_chord`]), and a ticket
//! drill-down (`tm_tui::screens::ticket_detail::TicketDetailScreen`, entered from a Kanban card's
//! Enter) is currently on screen, and `App::back_stack` is a real back-stack `Left`/`Esc`
//! ([`is_back_chord`]) unwinds one level at a time. `ticket_graph`, `diff_viewer`,
//! `session_stream`, `verification_ladder`, and `command_palette` remain unwired past this crate
//! for now — see this module's tests and the caller's commit message for why that scope line was
//! drawn where it was.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyModifiers};
use tm_tui::component::{Component, ComponentId, ComponentParent, FrameContext};
use tm_tui::event::{AppMessage, Event, InputEvent, KeyBinding, KeyChord, Propagation};
use tm_tui::runtime::{MessageSender, Runtime, RuntimeError};
use tm_tui::screens::dashboard::Dashboard;
use tm_tui::screens::home::Home;
use tm_tui::screens::kanban::{Kanban, KanbanCard, KanbanColumn};
use tm_tui::screens::ticket_detail::TicketDetailScreen;
use tm_tui::theme::Theme;
use tm_tui::widgets_data::form::{Field, Form};
use tm_tui::widgets_data::list::List;
use tm_tui::widgets_data::table::{Column, Table};
use tm_types::{SessionId, TicketId};
use tokio::sync::{Mutex, Notify};

use crate::agent::{self, AgentSession};
use crate::args::GlobalOpts;
use crate::project::Project;
use crate::render::Renderer;

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

/// Open the home screen over `project`'s current state and drive it until the human quits or a
/// termination signal arrives.
///
/// The chat input the home screen wires in (D-002's "you should be able to just code without
/// looking at tickets") drives the exact same turn-running logic (`crate::agent`'s
/// `AgentSession::run_turn_streaming`) the plain `tm`/`tm -p` loop uses — see [`App::spawn_turn`]
/// for how a submitted prompt gets from a key event to a running [`tm_agent::agent_loop::AgentLoop`].
pub async fn run(project: Arc<Project>) -> tm_types::Result<()> {
    let view = project
        .store
        .view()
        .map_err(|e| tm_types::TmError::storage(e.to_string()))?;
    // No ticket is active yet at launch, so there is nothing to show a goal for.
    let dashboard = build_dashboard(&view, None);
    let kanban = Kanban::new(ComponentId::new("tm.kanban"), build_kanban_columns(&view));

    let (mut runtime, sender) = Runtime::start(project.clock.clone(), Theme::dark())
        .await
        .map_err(runtime_error)?;

    // `--quiet`/`--no-color`: the TUI never renders through `Renderer` (its own widgets own
    // presentation), so these flags are inert here, but `AgentSession::new` still needs some
    // `Renderer` to construct — see `App::spawn_turn`, which drives this session through
    // `run_turn_streaming` directly rather than through `AgentSession::run_turn`, the one method
    // that would actually read it.
    let agent_session = AgentSession::new(project.clone(), Renderer::from_flags(false, true, true));
    let session_id = agent_session.session_id().clone();
    let agent_session = Arc::new(Mutex::new(agent_session));

    let mut home = Home::new(ComponentId::new("tm.home"), dashboard, session_id.clone());
    // D-003: the same one-line scope note the plain loop prints once before its first prompt
    // (`agent.rs::run_interactive`), shown here as a persistent status row instead.
    home.set_status_line(Some(project.scope_line()));
    // Held for the runtime's whole lifetime: `App::spawn_turn` clones this into each background
    // turn it spawns. Dropping it early would close `Runtime`'s message channel and turn
    // `messages.recv()` into an immediate `RuntimeError::ChannelClosed` on the very next
    // event-loop iteration.
    let mut app = App::new(
        project,
        home,
        kanban,
        runtime.shutdown_handle(),
        agent_session,
        sender,
        session_id,
    );

    runtime.run(&mut app).await.map_err(runtime_error)
}

/// Map a [`RuntimeError`] (terminal I/O, a closed channel, a closed input stream) to the
/// `tm_types::Result` every `tm-cli` execution module already returns, so `main.rs`'s single
/// `TmError`-to-exit-code path handles a TUI failure exactly like any other command's.
fn runtime_error(err: RuntimeError) -> tm_types::TmError {
    tm_types::TmError::storage(format!("tm-tui runtime error: {err}"))
}

/// Build the dashboard's ticket table and session list from `view`'s real, already-materialized
/// project state — no placeholder rows. The session list reads live leases (who currently holds
/// which ticket): `tm_core::ProjectView` has no separate "session" collection, and a lease *is*
/// the closest existing notion of "a worker session in progress" (see `agent.rs`'s `SessionId`,
/// which is not persisted in `ProjectView` at all).
///
/// `goal` is the active ticket's current durable goal text (`SPEC.md` §29,
/// `docs/audit-2026-09-18-fable.md` B-09), already read via `tm_core::Store::goal_state` by the
/// caller (`App::refresh`) — `view` alone carries no goal state (`goal_state` reads the
/// `goals` materialized table directly, not `ProjectView`), and there being no active ticket yet
/// (the initial dashboard `run` builds before any prompt is submitted) is exactly `None`.
fn build_dashboard(view: &tm_core::ProjectView, goal: Option<String>) -> Dashboard {
    let mut tickets = Table::new(
        ComponentId::new("tm.dashboard.tickets"),
        vec![
            Column::new("ID", 1),
            Column::new("State", 1),
            Column::new("Objective", 3),
        ],
    );
    tickets.set_rows(
        view.tickets
            .values()
            .map(|t| {
                vec![
                    t.id.to_string(),
                    format!("{:?}", t.state),
                    t.objective.clone(),
                ]
            })
            .collect(),
    );

    let mut sessions = List::new(ComponentId::new("tm.dashboard.sessions"));
    sessions.set_items(
        view.leases
            .values()
            .map(|lease| format!("{} holds {}", lease.holder, lease.ticket))
            .collect(),
    );

    let mut dashboard = Dashboard::new(ComponentId::new("tm.dashboard"), tickets, sessions);
    dashboard.set_goal(goal);
    dashboard
}

/// Build the Kanban board's columns from `view`'s real ticket state: one column per real
/// `tm_core::TicketState` variant, in `TicketState::ALL`'s declaration order, each holding every
/// ticket currently in that state.
///
/// Deliberately one column per raw state rather than a hand-curated "phase" grouping with
/// invented labels (e.g. "In Progress") — the backlog's own ask was a board that names the real
/// state machine, not a generic simplification of it, and a board built directly from
/// `TicketState::ALL` never drifts out of sync with the enum the way a hardcoded grouping would
/// (see this repo's `CLAUDE.md` on exactly that failure mode for enum-derived constants). Not
/// every column fitting on screen at once is `tm_tui::screens::kanban::Kanban`'s own problem to
/// solve (horizontal scrolling), not this function's.
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
/// `view` (e.g. the board went stale between the card being drawn and Enter being pressed — a
/// race this function resolves by simply declining to open anything, rather than panicking or
/// showing a screen for a ticket that is not really there).
///
/// The activity feed is built entirely from data `view` already carries (dependencies, children,
/// a live lease if one is held, and recorded failures) rather than a dedicated per-ticket
/// event-log query, which this crate does not have wired up yet — every line is still real
/// project state, not a placeholder.
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

/// Which of this app's screens is currently on screen. `Home` is the permanent default (D-002:
/// bare `tm` opens straight into chat, zero prompts); `Kanban` and `Detail` are reachable on
/// demand and returned from via [`App`]'s back-stack (`App::back_stack`) — the "compare 1:1 with
/// `claude agents`" ask: a navigable list/board you enter and leave, not a second screen
/// permanently glued alongside the first the way `Home`'s own embedded `Dashboard` already is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenId {
    /// The default chat screen. Never pushed onto `App::back_stack` as a *target* (nothing is
    /// "before" it) but always the fallback `App::pop_screen` lands on if the stack is ever
    /// unexpectedly empty.
    Home,
    /// The Kanban board.
    Kanban,
    /// A single ticket's drill-down detail, opened from a Kanban card's Enter action.
    Detail,
}

/// True when `key` is the chord that opens the Kanban/Tickets view from `Home`.
///
/// A modifier chord, not a bare letter: `Home`'s chat input is always focused the instant `tm`
/// starts (see `screens::home::Home`'s own module doc) and claims every plain keystroke as text a
/// human is typing into a prompt, so a bare `t` would just be typed into the chat box instead of
/// navigating anywhere. `Ctrl+T` ("Tickets") is not claimed by this terminal's raw-mode input in
/// the way e.g. `Ctrl+S`/`Ctrl+Q` risk colliding with a terminal's own XON/XOFF flow control, and
/// reads mnemonically for what it opens.
fn is_tickets_chord(key: &crossterm::event::KeyEvent) -> bool {
    key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// A typed alternative to [`is_tickets_chord`]: `/tickets` (or bare `tickets`, forgiving the slash
/// a human typing fast might drop), submitted through the same chat input every prompt goes
/// through. `Ctrl+T` alone is not discoverable the way a named command is — nothing on screen
/// hints it exists, unlike `claude agents`, which a human finds by typing a plausible word. This
/// is the same navigation, reached the way that command actually reads: typed, not a hidden chord.
/// Checked before treating a submission as a real prompt, so it never reaches the model.
fn is_tickets_command(prompt: &str) -> bool {
    matches!(prompt.trim(), "/tickets" | "tickets")
}

/// True when `key` is a back-navigation chord for a screen currently showing `current`.
///
/// `Esc` always means "back" on any non-`Home` screen (nothing in this crate ever binds `Esc` to
/// anything else). `Left` means "back" everywhere except [`ScreenId::Kanban`], where it is
/// already `Kanban`'s own column-navigation key (`Left`/`Right` move between columns) — binding
/// it to *both* "previous column" and "back" would make the two indistinguishable the moment a
/// human is in the leftmost column and presses `Left` again, so Kanban's back path is `Esc` only.
/// This is a deliberate, narrower reading of "Left/Esc from any non-Home screen returns to the
/// previous screen" for the one screen where `Left` already means something else.
fn is_back_chord(key: &crossterm::event::KeyEvent, current: ScreenId) -> bool {
    match key.code {
        KeyCode::Esc => true,
        KeyCode::Left => current != ScreenId::Kanban,
        _ => false,
    }
}

/// This crate's root component: wraps `tm-tui`'s [`Home`] screen and adds the two things every
/// screen needs that no individual screen should own itself — a quit keybinding that ends
/// [`Runtime::run`]'s event loop the same way a termination signal does, and the domain-aware
/// wiring `Home` deliberately can't own itself (`tm-tui` has no `tm_core`/`tm_agent` dependency —
/// see `tm_tui::screens::home`'s module docs): reading a fresh `tm_core::ProjectView` when a
/// ticket changes, and actually running a turn when the chat input is submitted.
struct App {
    id: ComponentId,
    /// The open project, read fresh on every `AppMessage::TicketChanged` so the dashboard
    /// reflects the scratch ticket a submitted prompt just created (or any other change) without
    /// the human needing to do anything to see it — D-002's "internal state ... a glance, not
    /// forced".
    project: Arc<Project>,
    home: Home,
    /// The Kanban board, reachable from `home` via [`is_tickets_chord`]. Persistent for the whole
    /// process lifetime (like `home`), not rebuilt on every navigation, so a human's place in it
    /// (which column, which card, how far scrolled) survives leaving and returning — refreshed in
    /// place by [`App::refresh`] on every `AppMessage::TicketChanged`, the same trigger that keeps
    /// `home`'s dashboard current.
    kanban: Kanban,
    /// The ticket detail drill-down, present only once a Kanban card has actually been opened.
    /// Rebuilt fresh (not reused) every time [`App::open_detail`] runs, so it is never possible
    /// to observe stale ticket data by re-entering a previously-visited detail screen.
    detail: Option<TicketDetailScreen>,
    /// Which screen is currently on screen. `Home` is `App::new`'s only starting value — the bare
    /// `tm` default this task must not change.
    current: ScreenId,
    /// The back-stack `App::pop_screen` unwinds: every screen navigated *away from* to reach
    /// `self.current`, most recent last. Popping this (not "always go home") is what makes
    /// `Home -> Kanban -> Detail -> back -> back` land on `Kanban` then `Home`, rather than
    /// jumping straight to `Home` from two levels deep.
    back_stack: Vec<ScreenId>,
    /// Notified on `q`/`ctrl-c`, mirroring `Runtime`'s own `SIGTERM`/`SIGHUP` shutdown path
    /// (`runtime.rs`'s `install_signal_handlers`) rather than inventing a second exit mechanism.
    shutdown: Arc<Notify>,
    /// The one `AgentSession` this TUI process drives every submitted prompt through — the exact
    /// same type (and, via `run_turn_streaming`, the exact same turn-running logic) `tm`/`tm -p`
    /// drive via `AgentSession::run_turn`. Behind a `tokio::sync::Mutex` (not a plain field)
    /// because [`App::spawn_turn`] runs a turn on a background task so the event loop stays
    /// responsive while it runs; the mutex also means a second submission mid-turn simply waits
    /// for the lock rather than running concurrently against the same session state — belt and
    /// suspenders alongside `Home`'s own `turn_running` gate, which is what actually stops a
    /// second submission from being accepted in the first place.
    agent_session: Arc<Mutex<AgentSession>>,
    /// Cloned into every spawned turn so it can push `AppMessage`s back into this runtime's event
    /// loop as the turn progresses.
    sender: MessageSender,
    /// `self.agent_session`'s session id, cached here (rather than re-locking the mutex) so
    /// `spawn_turn` can stamp every `AppMessage::StreamChunk` it sends without an `.await`.
    session_id: SessionId,
}

// Manual, not `#[derive(Debug)]`: `Component: std::fmt::Debug` requires *some* impl, but neither
// `project::Project` nor `agent::AgentSession` derives `Debug` themselves (an open `tm_core::Store`
// handle and a live session are not meaningfully "printable" state), so this reports just the
// fields a debugger actually cares about — shape and identity, not a dump of everything reachable
// through two more `Arc`s.
impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("id", &self.id)
            .field("home", &self.home)
            .field("current", &self.current)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        project: Arc<Project>,
        home: Home,
        kanban: Kanban,
        shutdown: Arc<Notify>,
        agent_session: Arc<Mutex<AgentSession>>,
        sender: MessageSender,
        session_id: SessionId,
    ) -> Self {
        App {
            id: ComponentId::new("tm.app"),
            project,
            home,
            kanban,
            detail: None,
            current: ScreenId::Home,
            back_stack: Vec::new(),
            shutdown,
            agent_session,
            sender,
            session_id,
        }
    }

    /// True when `key` is this app's quit chord: plain `q`, or ctrl-c (honoured here rather than
    /// left to the terminal, since raw mode disables the kernel's own SIGINT-on-ctrl-c handling).
    fn is_quit(key: &crossterm::event::KeyEvent) -> bool {
        key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    }

    /// Navigate forward to `next`, remembering `self.current` on the back-stack so
    /// [`App::pop_screen`] can return to it later. The one and only way `self.current` ever
    /// moves away from wherever it started.
    fn push_screen(&mut self, next: ScreenId) {
        self.back_stack.push(self.current);
        self.current = next;
    }

    /// Navigate back to whatever screen `self.current` was entered from, or [`ScreenId::Home`] if
    /// the stack is unexpectedly already empty (defensive: nothing in this module's own
    /// navigation logic should ever pop more times than it pushed, but landing on `Home` rather
    /// than panicking is the honest fallback if that invariant is ever wrong).
    fn pop_screen(&mut self) {
        self.current = self.back_stack.pop().unwrap_or(ScreenId::Home);
    }

    /// Open `ticket_id`'s detail screen and push [`ScreenId::Detail`], if `ticket_id` parses as a
    /// real [`TicketId`] and still exists in a freshly-read `tm_core::ProjectView`. Both failure
    /// modes (a malformed id, which should never happen since `ticket_id` always originates from
    /// a `TicketId::to_string()` in [`build_kanban_columns`]; or a ticket that existed when the
    /// Kanban card was drawn but is gone by the time Enter is processed) simply decline to
    /// navigate anywhere, rather than opening a screen for a ticket that is not really there.
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

    /// Re-read `self.project`'s store and hand both `self.home` a freshly built dashboard *and*
    /// `self.kanban` freshly built columns — the `AppMessage::TicketChanged` handler's whole job.
    /// Both are refreshed unconditionally, regardless of `self.current` (which screen is actually
    /// on screen right now): a turn can change ticket state while the human is looking at either
    /// screen, and each must show current data the moment it is (re)selected rather than whatever
    /// was true when it was last on screen. A read failure is swallowed (the previous dashboard/
    /// board just stay on screen one more frame) rather than tearing down the TUI over a
    /// transient store error; `tm-cli`'s other commands already treat a `view()` failure as fatal
    /// where that is the right call, which driving a live UI is not.
    ///
    /// `ticket` is `changed`'s own id: the ticket whose state just moved is also the one whose
    /// goal (`SPEC.md` §29) is worth showing — `Store::goal_state` failing or finding nothing set
    /// yet is not an error here either, it just means no goal line renders this frame. An empty
    /// `text` is filtered out the same way: `tm_core::materialize`'s `goal.step_added`/
    /// `goal.reoriented`/`goal.claimed_complete` arms can (defensibly, per their own doc
    /// comments) leave a `goals` row with `text = ""` if the log somehow ever saw one of those
    /// before a `goal.set` — real `AgentLoop` usage never produces that ordering, but a bare
    /// `Goal: ` line with nothing after it would be a visible artifact of that edge case if it
    /// ever did happen, so it renders as "no goal" instead.
    fn refresh(&mut self, ticket: &TicketId) {
        if let Ok(view) = self.project.store.view() {
            let goal = self
                .project
                .store
                .goal_state(ticket)
                .ok()
                .flatten()
                .map(|g| g.text)
                .filter(|text| !text.is_empty());
            self.home.set_dashboard(build_dashboard(&view, goal));
            self.kanban.set_columns(build_kanban_columns(&view));
            // If the ticket currently open in the detail drill-down is the one that just
            // changed, rebuild it too — otherwise a human sitting on `ScreenId::Detail` while a
            // background turn moves that exact ticket's state would keep looking at a stale
            // snapshot from whenever they opened it, the one screen `refresh` would otherwise
            // leave behind (unlike `home`/`kanban`, which this function already always keeps
            // current regardless of `self.current`).
            if self.detail.as_ref().map(TicketDetailScreen::ticket) == Some(ticket) {
                if let Some(screen) = build_detail_screen(&view, ticket) {
                    self.detail = Some(screen);
                }
            }
        }
    }

    /// Run `prompt` as a turn on a background task, translating [`agent::TurnEvent`]s and the
    /// final [`tm_agent::outcome::AgentOutcome`] into [`AppMessage`]s as they happen, so
    /// [`Runtime::run`]'s event loop keeps redrawing (scrolling the stream pane, honouring quit)
    /// while the turn is in flight rather than blocking on it.
    ///
    /// Calls [`AgentSession::run_turn_streaming`] directly — the same shared method
    /// `AgentSession::run_turn` (the plain `tm`/`tm -p` loop) calls — rather than
    /// `AgentSession::run_turn` itself, since that method's own rendering goes through
    /// `Renderer::note`/`error` (plain stdout), not this runtime's message channel.
    ///
    /// Approval handling is deliberately minimal for this first TUI turn driver: an
    /// `AwaitingApproval` suspension is always auto-denied (`Ok(false)`), and a note explaining
    /// why is streamed into the pane instead. Prompting for approval the way the plain loop does
    /// (a blocking stdin read via `prompt_approval_decision`) would race the same real terminal
    /// input this runtime's own `crossterm::event::EventStream` reads from — precisely the
    /// stdin-contention failure mode `runtime.rs`'s `Runtime::start` doc comment already found
    /// and fixed once for the capability probe; deliberately not reintroducing an instance of it
    /// here. A real interactive approval UI (a modal, a dedicated keybinding) is future work.
    fn spawn_turn(&self, prompt: String) {
        let agent_session = Arc::clone(&self.agent_session);
        let sender = self.sender.clone();
        let session_id = self.session_id.clone();

        tokio::spawn(async move {
            let mut session = agent_session.lock().await;

            let mut steps_sent = 0usize;
            let mut resolved_ticket: Option<tm_types::TicketId> = None;

            let send_chunk = |sender: &MessageSender, text: String| {
                if !text.is_empty() {
                    sender.send(AppMessage::StreamChunk {
                        session: session_id.clone(),
                        text,
                        final_chunk: false,
                    });
                }
            };

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
                        agent::TurnEvent::Steps(steps) => {
                            for step in steps.iter().skip(steps_sent) {
                                send_chunk(&sender, format!("{}\n", agent::format_step(step)));
                            }
                            steps_sent = steps.len();
                        }
                        agent::TurnEvent::AwaitingApproval(pending) => {
                            send_chunk(
                                &sender,
                                format!(
                                    "{}\n(auto-denied: the TUI does not support interactive \
                                     approval yet)\n",
                                    agent::format_pending_approval(&pending)
                                ),
                            );
                        }
                    },
                    |_pending| Ok(false),
                )
                .await;

            let summary = match &outcome {
                Ok(outcome) => agent::format_outcome_summary(outcome),
                Err(e) => format!("turn failed to run: {e}"),
            };
            sender.send(AppMessage::StreamChunk {
                session: session_id,
                text: if summary.is_empty() {
                    String::new()
                } else {
                    format!("{summary}\n")
                },
                final_chunk: true,
            });
            // The ticket's own state may have moved (e.g. a submitted turn transitions it), so
            // the dashboard is refreshed again now, not just once at resolution time.
            if let Some(id) = resolved_ticket {
                sender.send(AppMessage::TicketChanged { id });
            }
        });
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
        // Only the screen actually on screen renders — `home`/`kanban`/`detail` otherwise keep
        // their state alive in memory (see this struct's own field docs) without taking any
        // frame time, the same "inactive but not destroyed" property a real windowing system
        // gives a backgrounded window.
        match self.current {
            ScreenId::Home => self.home.render(area, buf, ctx),
            ScreenId::Kanban => self.kanban.render(area, buf, ctx),
            ScreenId::Detail => {
                if let Some(detail) = &self.detail {
                    detail.render(area, buf, ctx);
                }
            }
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if let Event::Input(InputEvent::Key(key)) = event {
            if App::is_quit(key) {
                // `notify_one`, not `notify_waiters`: this runs synchronously, inside the very
                // `select!` iteration of `Runtime::run`'s event loop that delivered this key, on
                // the same task. That iteration's `shutdown.notified()` listener is about to be
                // dropped (a different arm — this one — won the `select!`), and the loop
                // reconstructs a brand new listener next iteration; `notify_waiters` only wakes
                // *currently registered* listeners and stores nothing for that next one, so a
                // notification racing the listener's own reconstruction like this one can be
                // silently lost. `notify_one` stores a permit when nobody is registered yet, so
                // the next iteration's fresh listener consumes it immediately instead — the
                // correctness property this path actually needs, not just what happened to work
                // in manual testing.
                self.shutdown.notify_one();
                return Propagation::Consumed;
            }
        }

        // Domain-aware (needs `tm_core::ProjectView`, which `tm-tui` deliberately never depends
        // on): handled here rather than forwarded into `self.home`/`self.kanban`, which only ever
        // see the already-built `Dashboard`/columns this hands them via `refresh`. Runs
        // regardless of `self.current` (see `refresh`'s own doc comment on why both are always
        // kept current).
        if let Event::App(AppMessage::TicketChanged { id }) = event {
            self.refresh(id);
            return Propagation::Consumed;
        }

        // A turn's streaming output must keep reaching `home`'s stream pane (and its
        // `turn_running` bookkeeping) even while some other screen is the one on screen — the
        // turn `App::spawn_turn` started does not pause just because the human navigated away to
        // browse the Kanban board mid-turn. Routed here, unconditionally, rather than only when
        // `self.current == ScreenId::Home`; `home`'s own `handle_event` already ignores a chunk
        // for a session that is not its own, so this is safe to always offer.
        if let Event::App(AppMessage::StreamChunk { .. }) = event {
            return self.home.handle_event(event, ctx);
        }

        if let Event::Input(InputEvent::Key(key)) = event {
            if self.current == ScreenId::Home && is_tickets_chord(key) {
                self.push_screen(ScreenId::Kanban);
                return Propagation::Consumed;
            }
            if self.current != ScreenId::Home && is_back_chord(key, self.current) {
                self.pop_screen();
                return Propagation::Consumed;
            }
        }

        match self.current {
            ScreenId::Home => {
                let propagation = self.home.handle_event(event, ctx);
                if let Some(prompt) = self.home.take_submission() {
                    if is_tickets_command(&prompt) {
                        self.push_screen(ScreenId::Kanban);
                    } else {
                        self.spawn_turn(prompt);
                    }
                }
                propagation
            }
            ScreenId::Kanban => {
                let propagation = self.kanban.handle_event(event, ctx);
                if let Some(ticket_id) = self.kanban.take_activation() {
                    self.open_detail(&ticket_id);
                }
                propagation
            }
            ScreenId::Detail => match &mut self.detail {
                Some(detail) => detail.handle_event(event, ctx),
                None => Propagation::Propagate,
            },
        }
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        let mut bindings = vec![KeyBinding::new(KeyChord::plain(KeyCode::Char('q')), "quit")];
        match self.current {
            ScreenId::Home => {
                bindings.push(KeyBinding::new(
                    KeyChord {
                        code: KeyCode::Char('t'),
                        modifiers: KeyModifiers::CONTROL,
                    },
                    "open tickets (kanban board)",
                ));
                bindings.extend(self.home.keybindings(ctx));
            }
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
        // Only the screen actually on screen is reachable by focus — see `runtime.rs`'s
        // `Runtime::run`, which rebuilds `FocusTree` from this every iteration specifically so a
        // navigation change like this one takes effect on the very next frame rather than
        // leaving focus pointed at a screen that is no longer even rendered.
        match self.current {
            ScreenId::Home => self.home.focusable_children(),
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
            ScreenId::Home => self.home.resolve(id),
            ScreenId::Kanban => (id == self.kanban.id()).then_some(&self.kanban as &dyn Component),
            ScreenId::Detail => self.detail.as_ref().and_then(|detail| detail.resolve(id)),
        }
    }

    fn resolve_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component> {
        if id == self.id {
            return Some(self);
        }
        match self.current {
            ScreenId::Home => self.home.resolve_mut(id),
            ScreenId::Kanban => {
                if id == self.kanban.id() {
                    Some(&mut self.kanban as &mut dyn Component)
                } else {
                    None
                }
            }
            ScreenId::Detail => self
                .detail
                .as_mut()
                .and_then(|detail| detail.resolve_mut(id)),
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

    /// A real ticket, created the same way `agent.rs::create_scratch_ticket` does, rather than a
    /// hand-built `Ticket` literal — `Ticket` has fields this test does not own the shape of, and
    /// going through `Store::create_ticket` means it can never drift out of sync with them.
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

    #[test]
    fn build_dashboard_reflects_a_real_ticket_from_project_state() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).expect("open store");
        create_real_ticket(&store, "wire the tui");

        let view = store.view().expect("view");
        let dashboard = build_dashboard(&view, None);
        // `Dashboard` does not expose its rows directly; a full render assertion is `tm-tui`'s
        // own insta-snapshot layer's job. What this crate owns is that real project state
        // actually made it into the widget, which the debug repr is enough to prove.
        assert!(format!("{dashboard:?}").contains("wire the tui"));
    }

    #[test]
    fn build_dashboard_over_an_empty_project_has_no_fake_rows() {
        let view = tm_core::ProjectView::empty();
        let dashboard = build_dashboard(&view, None);
        let rendered = format!("{dashboard:?}");
        assert!(
            !rendered.contains("placeholder") && !rendered.contains("example"),
            "an empty project must render an honestly empty dashboard, not a fabricated row"
        );
    }

    #[test]
    fn build_dashboard_with_no_goal_carries_none() {
        let view = tm_core::ProjectView::empty();
        let dashboard = build_dashboard(&view, None);
        // `Dashboard`'s derived `Debug` always names the `goal` field; `None` here is what
        // distinguishes "no active goal" from `build_dashboard_with_a_goal_carries_its_text`
        // below, since `tm-tui` has no render-to-string assertion this crate can reach for.
        assert!(format!("{dashboard:?}").contains("goal: None"));
    }

    #[test]
    fn build_dashboard_with_a_goal_carries_its_text() {
        let view = tm_core::ProjectView::empty();
        let dashboard = build_dashboard(&view, Some("ship the login fix".to_string()));
        // `Dashboard` does not expose its `goal` field directly; the debug repr is enough to
        // prove the text this function was given actually made it into the widget, the same
        // convention `build_dashboard_reflects_a_real_ticket_from_project_state` above uses for
        // ticket rows.
        assert!(format!("{dashboard:?}").contains("ship the login fix"));
    }

    #[test]
    fn goal_state_read_for_a_changed_ticket_carries_through_to_build_dashboard() {
        // `App::refresh` reads `Store::goal_state` for the changed ticket and passes
        // its text to `build_dashboard`; this exercises that exact composition (not just
        // `build_dashboard` in isolation), so a regression that drops the goal along the way
        // would actually be caught here.
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).expect("open store");
        let ticket = create_real_ticket(&store, "wire the tui");
        store
            .set_goal(
                &ticket,
                "make the dashboard show this".to_string(),
                ParticipantId::new("human:tester").unwrap(),
            )
            .expect("set_goal");

        let view = store.view().expect("view");
        let goal = store
            .goal_state(&ticket)
            .expect("goal_state")
            .map(|g| g.text);
        let dashboard = build_dashboard(&view, goal);
        assert!(format!("{dashboard:?}").contains("make the dashboard show this"));
    }

    fn plain_key(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn tickets_chord_requires_control_t_not_a_bare_t() {
        assert!(is_tickets_chord(&ctrl_key(KeyCode::Char('t'))));
        assert!(
            !is_tickets_chord(&plain_key(KeyCode::Char('t'))),
            "a bare 't' must reach the chat input as a typed character, not navigate"
        );
        assert!(!is_tickets_chord(&ctrl_key(KeyCode::Char('k'))));
    }

    #[test]
    fn esc_is_a_back_chord_on_every_non_home_screen_including_kanban() {
        assert!(is_back_chord(&plain_key(KeyCode::Esc), ScreenId::Kanban));
        assert!(is_back_chord(&plain_key(KeyCode::Esc), ScreenId::Detail));
    }

    #[test]
    fn left_is_a_back_chord_everywhere_except_kanban() {
        assert!(is_back_chord(&plain_key(KeyCode::Left), ScreenId::Detail));
        assert!(
            !is_back_chord(&plain_key(KeyCode::Left), ScreenId::Kanban),
            "Left is Kanban's own column-navigation key, not its back chord"
        );
    }

    #[test]
    fn build_kanban_columns_has_one_column_per_real_ticket_state_and_no_more() {
        let view = tm_core::ProjectView::empty();
        let columns = build_kanban_columns(&view);
        // Deliberately not hardcoding a literal count: this test must keep passing (with no edit
        // needed here) if `TicketState` ever gains or loses a variant, per this repo's own
        // convention on deriving cardinality-based expectations from the enum rather than a
        // hardcoded number (see `CLAUDE.md`'s "Parallel tracks against a moving `main`").
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
        // A freshly created ticket starts in `TicketState::Draft` (see `tm_core::ticket`'s own
        // machine docs), so its card must land in exactly that column and no other.
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
    }

    #[test]
    fn build_detail_screen_activity_falls_back_to_a_plain_message_when_nothing_happened_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).expect("open store");
        let ticket_id = create_real_ticket(&store, "a brand new ticket");

        let view = store.view().expect("view");
        let screen = build_detail_screen(&view, &ticket_id).expect("ticket exists");
        // `TicketDetailScreen` does not expose its activity list directly; the debug repr is
        // enough to prove real (non-fabricated) content reached the widget, the same convention
        // `build_dashboard`'s own tests use.
        assert!(format!("{screen:?}").contains("no recorded activity yet"));
    }
}
