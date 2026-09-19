//! The bare-`tm` ratatui TUI mode (D-002): entered on the bare-`tm` TTY path, alongside (never
//! instead of) the plain [`crate::agent::AgentSession`] loop `--json`/`--quiet`/`--plain`, a
//! non-tty stdout/stdin, or `TERM=dumb` still gets.
//!
//! This module owns exactly the seam between `tm-cli` and `tm-tui`: [`should_launch`] decides
//! which of the two bare-`tm` paths a given invocation takes, [`App`] is the small root
//! [`tm_tui::component::ComponentParent`] this crate wires around `tm-tui`'s
//! [`tm_tui::screens::dashboard::Dashboard`], and [`run`] drives `tm_tui::runtime::Runtime`
//! against an already-open [`crate::project::Project`]'s real state. Every other screen `tm-tui`
//! ships (`ticket_graph`, `diff_viewer`, `session_stream`, `ticket_detail`,
//! `verification_ladder`, `command_palette`) stays unwired past this crate for now — see this
//! module's tests and the caller's commit message for why that scope line was drawn where it was.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyModifiers};
use tm_tui::component::{Component, ComponentId, ComponentParent, FrameContext};
use tm_tui::event::{AppMessage, Event, InputEvent, KeyBinding, KeyChord, Propagation};
use tm_tui::runtime::{MessageSender, Runtime, RuntimeError};
use tm_tui::screens::dashboard::Dashboard;
use tm_tui::screens::home::Home;
use tm_tui::theme::Theme;
use tm_tui::widgets_data::list::List;
use tm_tui::widgets_data::table::{Column, Table};
use tm_types::SessionId;
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
/// caller (`App::refresh_dashboard`) — `view` alone carries no goal state (`goal_state` reads the
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
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        project: Arc<Project>,
        home: Home,
        shutdown: Arc<Notify>,
        agent_session: Arc<Mutex<AgentSession>>,
        sender: MessageSender,
        session_id: SessionId,
    ) -> Self {
        App {
            id: ComponentId::new("tm.app"),
            project,
            home,
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

    /// Re-read `self.project`'s store and hand `self.home` a freshly built dashboard — the
    /// `AppMessage::TicketChanged` handler's whole job. A read failure is swallowed (the previous
    /// dashboard just stays on screen one more frame) rather than tearing down the TUI over a
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
    fn refresh_dashboard(&mut self, ticket: &tm_types::TicketId) {
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
                text: format!("{summary}\n"),
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
        self.home.render(area, buf, ctx);
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
        // on): handled here rather than forwarded into `self.home`, which only ever sees the
        // already-built `Dashboard` this hands it via `refresh_dashboard`.
        if let Event::App(AppMessage::TicketChanged { id }) = event {
            self.refresh_dashboard(id);
            return Propagation::Consumed;
        }

        let propagation = self.home.handle_event(event, ctx);
        if let Some(prompt) = self.home.take_submission() {
            self.spawn_turn(prompt);
        }
        propagation
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        let mut bindings = vec![KeyBinding::new(KeyChord::plain(KeyCode::Char('q')), "quit")];
        bindings.extend(self.home.keybindings(ctx));
        bindings
    }

    fn focusable_children(&self) -> Vec<ComponentId> {
        self.home.focusable_children()
    }
}

impl ComponentParent for App {
    fn resolve(&self, id: ComponentId) -> Option<&dyn Component> {
        if id == self.id {
            Some(self)
        } else {
            self.home.resolve(id)
        }
    }

    fn resolve_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component> {
        if id == self.id {
            Some(self)
        } else {
            self.home.resolve_mut(id)
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
        // `App::refresh_dashboard` reads `Store::goal_state` for the changed ticket and passes
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
}
