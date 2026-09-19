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
use tm_tui::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use tm_tui::runtime::{Runtime, RuntimeError};
use tm_tui::screens::dashboard::Dashboard;
use tm_tui::theme::Theme;
use tm_tui::widgets_data::list::List;
use tm_tui::widgets_data::table::{Column, Table};
use tokio::sync::Notify;

use crate::args::GlobalOpts;
use crate::project::Project;

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

/// Open a dashboard over `project`'s current state and drive it until the human quits or a
/// termination signal arrives.
pub async fn run(project: Arc<Project>) -> tm_types::Result<()> {
    let view = project
        .store
        .view()
        .map_err(|e| tm_types::TmError::storage(e.to_string()))?;
    let dashboard = build_dashboard(&view);

    let (mut runtime, _messages) = Runtime::start(project.clock.clone(), Theme::dark())
        .await
        .map_err(runtime_error)?;
    // Held for the runtime's whole lifetime: a background task (none exist yet for the dashboard
    // alone, but any future streaming source needs one) sends through this. Dropping it early
    // would close `Runtime`'s message channel and turn `messages.recv()` into an immediate
    // `RuntimeError::ChannelClosed` on the very first event-loop iteration.
    let mut app = App::new(dashboard, runtime.shutdown_handle());

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
fn build_dashboard(view: &tm_core::ProjectView) -> Dashboard {
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

    Dashboard::new(ComponentId::new("tm.dashboard"), tickets, sessions)
}

/// This crate's root component: wraps `tm-tui`'s [`Dashboard`] and adds the one thing every
/// screen needs that no individual screen should own itself — a quit keybinding that ends
/// [`Runtime::run`]'s event loop the same way a termination signal does.
#[derive(Debug)]
struct App {
    id: ComponentId,
    dashboard: Dashboard,
    /// Notified on `q`/`ctrl-c`, mirroring `Runtime`'s own `SIGTERM`/`SIGHUP` shutdown path
    /// (`runtime.rs`'s `install_signal_handlers`) rather than inventing a second exit mechanism.
    shutdown: Arc<Notify>,
}

impl App {
    fn new(dashboard: Dashboard, shutdown: Arc<Notify>) -> Self {
        App {
            id: ComponentId::new("tm.app"),
            dashboard,
            shutdown,
        }
    }

    /// True when `key` is this app's quit chord: plain `q`, or ctrl-c (honoured here rather than
    /// left to the terminal, since raw mode disables the kernel's own SIGINT-on-ctrl-c handling).
    fn is_quit(key: &crossterm::event::KeyEvent) -> bool {
        key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
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
        self.dashboard.render(area, buf, ctx);
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
        self.dashboard.handle_event(event, ctx)
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        let mut bindings = vec![KeyBinding::new(KeyChord::plain(KeyCode::Char('q')), "quit")];
        bindings.extend(self.dashboard.keybindings(ctx));
        bindings
    }

    fn focusable_children(&self) -> Vec<ComponentId> {
        self.dashboard.focusable_children()
    }
}

impl ComponentParent for App {
    fn resolve(&self, id: ComponentId) -> Option<&dyn Component> {
        if id == self.id {
            Some(self)
        } else {
            self.dashboard.resolve(id)
        }
    }

    fn resolve_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component> {
        if id == self.id {
            Some(self)
        } else {
            self.dashboard.resolve_mut(id)
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
        let dashboard = build_dashboard(&view);
        // `Dashboard` does not expose its rows directly; a full render assertion is `tm-tui`'s
        // own insta-snapshot layer's job. What this crate owns is that real project state
        // actually made it into the widget, which the debug repr is enough to prove.
        assert!(format!("{dashboard:?}").contains("wire the tui"));
    }

    #[test]
    fn build_dashboard_over_an_empty_project_has_no_fake_rows() {
        let view = tm_core::ProjectView::empty();
        let dashboard = build_dashboard(&view);
        let rendered = format!("{dashboard:?}");
        assert!(
            !rendered.contains("placeholder") && !rendered.contains("example"),
            "an empty project must render an honestly empty dashboard, not a fabricated row"
        );
    }
}
