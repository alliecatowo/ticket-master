//! The home screen: bare `tm`'s single default view once the ratatui TUI launches.
//!
//! This is the answer to "you should be able to just code without looking at tickets ... I'm not
//! seeing a chat box or anything" — the chat input at the bottom is always focused the moment
//! `tm` starts (no keybinding to discover first), the running turn's output streams into the pane
//! above it, and the ticket/session dashboard stays visible alongside so "internal state" (the
//! scratch ticket a typed prompt creates) is something a human can glance at rather than dig for.
//!
//! Like every other screen in this crate, `Home` owns no domain logic: it has no idea what a
//! `tm_agent::AgentLoop` or a `tm_core::Store` is. Submitting the input bar only records that a
//! submission happened ([`Home::take_submission`]); the caller (`tm-cli`'s `tui.rs`, which does
//! depend on `tm_agent`/`tm_core`) is what actually runs a turn and feeds progress back in as
//! [`crate::event::AppMessage`]s.

use crossterm::event::KeyCode;
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use tm_types::SessionId;

use crate::component::{Component, ComponentId, ComponentParent, FrameContext};
use crate::event::{AppMessage, Event, InputEvent, KeyBinding, Propagation};
use crate::screens::dashboard::Dashboard;
use crate::theme::split_vertical;
use crate::widgets_data::form::{Field, Form};
use crate::widgets_viz::stream::StreamPane;

/// Rows reserved at the bottom of the screen for the chat input bar. One row is enough for
/// `Form`'s single-field rendering (see `widgets_data::form`'s `render`, one row per field); a
/// full-height text box is not needed for a single always-focused prompt line.
const INPUT_HEIGHT: u16 = 1;

/// Rows reserved at the top of the screen for [`Home::status_line`], when set. Mirrors
/// `screens::dashboard::Dashboard`'s own `GOAL_HEIGHT` convention (and, at the bottom,
/// [`INPUT_HEIGHT`] above): a single row is enough for one line of text.
const STATUS_HEIGHT: u16 = 1;

/// The label `Home`'s chat input field renders, doubling as the "type here" affordance a human
/// sees the instant `tm` starts.
const INPUT_LABEL: &str = "tm›";

/// The home screen: the ticket/session dashboard, a stream pane for the running turn's output,
/// and a chat input bar, composed vertically.
///
/// IMPL:
/// - `render`: if `self.status_line` is `Some`, carve [`STATUS_HEIGHT`] rows off the *top* of
///   `area` for it first (mirrors `screens::dashboard::Dashboard`'s own `goal` line); then carve
///   [`INPUT_HEIGHT`] rows off the bottom of what remains for `self.input`; split what remains
///   between `self.dashboard` and `self.stream` via `split_vertical` (dashboard gets enough room
///   to read ticket state at a glance; stream gets the larger share, since watching the agent
///   work is this screen's main draw).
/// - `handle_event`: the chat input is the one component actually wired into
///   [`crate::component::FocusTree`]'s tab order (`focusable_children`, first entry) — see this
///   struct's `id()`/`focusable_children` docs for why that is what makes it focused the instant
///   the app starts. Enter, while focused, is intercepted here (not forwarded to `self.input`,
///   whose own `handle_event` never claims `Enter` — see `widgets_data::form`) to record a
///   submission and reset the field. An `Event::App(AppMessage::StreamChunk)` whose `session`
///   matches `self.session` is this screen's own concern (mirrors
///   `screens::session_stream::SessionStreamScreen`): push it into `self.stream` directly, and
///   track `final_chunk` so [`Home::is_turn_running`] flips back to `false` once the caller's
///   turn actually finishes. Everything else forwards to `self.dashboard`.
#[derive(Debug)]
pub struct Home {
    id: ComponentId,
    input: Form,
    dashboard: Dashboard,
    stream: StreamPane,
    /// The session this screen's turn output belongs to — matched against
    /// `AppMessage::StreamChunk::session` the same way `SessionStreamScreen` matches its own.
    session: SessionId,
    /// Set by `handle_event` on a non-empty Enter submission, cleared by
    /// [`Home::take_submission`]. Not itself the input's *current* text (which lives in
    /// `self.input` until submission resets it) — this is specifically "a submission happened
    /// and here is what was submitted".
    pending_submission: Option<String>,
    /// Whether a turn this screen submitted is still running, i.e. `AppMessage::StreamChunk`'s
    /// `final_chunk` has not yet arrived for `self.session` since the last submission. Gates
    /// `take_submission` so pressing Enter mid-turn does not start a second, overlapping turn
    /// against the same `tm-cli::agent::AgentSession`.
    turn_running: bool,
    /// An optional status line drawn in the top row, above the dashboard/stream split, when set
    /// (`tm-cli`'s D-003 project-scope line, via [`Home::set_status_line`]). `tm-tui` has no
    /// opinion on what the text means — same convention as `screens::dashboard::Dashboard`'s own
    /// `goal` field.
    status_line: Option<String>,
}

impl Home {
    /// A home screen over `dashboard`'s current ticket/session state, an empty stream pane, an
    /// empty chat input, and no status line, for turns that will be attributed to `session`.
    pub fn new(id: ComponentId, dashboard: Dashboard, session: SessionId) -> Self {
        Home {
            id,
            input: fresh_input(),
            dashboard,
            stream: StreamPane::new(ComponentId::new("tm.home.stream"), 2000),
            session,
            pending_submission: None,
            turn_running: false,
            status_line: None,
        }
    }

    /// Set (or clear, with `None`) the status line drawn in the top row, above the
    /// dashboard/stream split.
    pub fn set_status_line(&mut self, line: Option<String>) {
        self.status_line = line;
    }

    /// Replace this screen's dashboard (fresh ticket/session rows), e.g. after `tm-cli`'s `tui.rs`
    /// re-reads `tm_core::ProjectView` on an `AppMessage::TicketChanged`. Domain state itself
    /// (`tm_core::ProjectView`) is deliberately never read by this crate — the caller builds the
    /// replacement `Dashboard` and hands it over already-populated.
    pub fn set_dashboard(&mut self, dashboard: Dashboard) {
        self.dashboard = dashboard;
    }

    /// Take the most recent submitted prompt, if any, clearing it. `None` once already taken (or
    /// before the human ever submits anything) — a caller polls this once per event, exactly
    /// mirroring how `tm-cli`'s `App` already polls a quit keybinding's effect today.
    pub fn take_submission(&mut self) -> Option<String> {
        self.pending_submission.take()
    }

    /// Whether a turn is currently running (see `turn_running`'s field docs).
    pub fn is_turn_running(&self) -> bool {
        self.turn_running
    }

    /// Read, trim, and — if non-empty and no turn is already running — record the input field's
    /// current text as a pending submission, then reset the input to an empty field.
    fn try_submit(&mut self) {
        if self.turn_running {
            return;
        }
        let prompt = self
            .input
            .values()
            .first()
            .copied()
            .unwrap_or("")
            .trim()
            .to_string();
        self.input = fresh_input();
        if prompt.is_empty() {
            return;
        }
        self.pending_submission = Some(prompt);
        self.turn_running = true;
    }
}

/// A fresh chat input: one always-editable text field, no leftover state from a previous
/// submission. `widgets_data::form::Form` has no field-clearing mutator (nothing else in this
/// crate has needed one yet), so a submitted input is reset by replacing the whole `Form` rather
/// than adding one just for this call site.
fn fresh_input() -> Form {
    Form::new(
        ComponentId::new("tm.home.input"),
        vec![Field::text(INPUT_LABEL)],
    )
}

impl Component for Home {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.height == 0 {
            return;
        }
        let area = match &self.status_line {
            Some(text) => {
                let status_height = STATUS_HEIGHT.min(area.height);
                buf.set_stringn(area.x, area.y, text, area.width as usize, ctx.theme.muted);
                Rect {
                    y: area.y + status_height,
                    height: area.height.saturating_sub(status_height),
                    ..area
                }
            }
            None => area,
        };
        if area.height == 0 {
            return;
        }
        let input_height = INPUT_HEIGHT.min(area.height);
        let body_height = area.height - input_height;
        let body_area = Rect {
            height: body_height,
            ..area
        };
        let input_area = Rect {
            y: area.y + body_height,
            height: input_height,
            ..area
        };

        // The dashboard gets enough room to read ticket/session state at a glance; the stream
        // pane gets the larger share, since watching the turn run is this screen's main draw
        // (see the struct-level IMPL note).
        let slots = split_vertical(body_area, &[("dashboard", 2), ("stream", 3)]);
        self.dashboard.render(slots.get("dashboard"), buf, ctx);
        self.stream.render(slots.get("stream"), buf, ctx);
        self.input.render(input_area, buf, ctx);
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if let Event::App(AppMessage::StreamChunk {
            session,
            text,
            final_chunk,
        }) = event
        {
            if session == &self.session {
                self.stream.push_chunk(text);
                if *final_chunk {
                    self.turn_running = false;
                }
                return Propagation::Consumed;
            }
            // A chunk for some other session is not this screen's concern; fall through so an
            // ancestor (or, once one exists, a different open session view) can claim it.
        }

        if ctx.focus.is_focused(self.input.id()) {
            if let Event::Input(InputEvent::Key(key)) = event {
                if key.code == KeyCode::Enter {
                    self.try_submit();
                    return Propagation::Consumed;
                }
            }
            let consumed = self.input.handle_event(event, ctx);
            if consumed.is_consumed() {
                return consumed;
            }
        }

        let stream_consumed = self.stream.handle_event(event, ctx);
        if stream_consumed.is_consumed() {
            return stream_consumed;
        }
        self.dashboard.handle_event(event, ctx)
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        let mut bindings = self.input.keybindings(ctx);
        bindings.extend(self.stream.keybindings(ctx));
        bindings.extend(self.dashboard.keybindings(ctx));
        bindings
    }

    fn focusable_children(&self) -> Vec<ComponentId> {
        // The input is listed first deliberately: `component::FocusTree::rebuild` walks
        // `focusable_children()` in pre-order and starts focus at the very first entry
        // (`FocusTree::new`'s `current: 0`), and nothing in this crate's `Runtime` yet advances
        // focus at runtime (no `Tab`-router is wired at that layer for any screen — the same gap
        // `screens::dashboard::Dashboard`'s own IMPL notes call out for its two panes). Listing
        // the input first is what makes it *the* focused, reactive component for this screen's
        // whole lifetime, satisfying "typing goes somewhere real with zero extra keystrokes" —
        // not a workaround, the intended behavior for this screen specifically.
        let mut ids = vec![self.input.id(), self.stream.id()];
        ids.extend(self.dashboard.focusable_children());
        ids
    }
}

impl ComponentParent for Home {
    fn resolve(&self, id: ComponentId) -> Option<&dyn Component> {
        if id == self.id {
            Some(self)
        } else if id == self.input.id() {
            Some(&self.input)
        } else if id == self.stream.id() {
            Some(&self.stream)
        } else {
            self.dashboard.resolve(id)
        }
    }

    fn resolve_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component> {
        if id == self.id {
            Some(self)
        } else if id == self.input.id() {
            Some(&mut self.input)
        } else if id == self.stream.id() {
            Some(&mut self.stream)
        } else {
            self.dashboard.resolve_mut(id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Capabilities;
    use crate::component::FocusState;
    use crate::theme::Theme;
    use crate::widgets_data::list::List;
    use crate::widgets_data::table::{Column, Table};
    use crossterm::event::{KeyEvent, KeyModifiers};
    use tm_types::FixedClock;

    fn home() -> Home {
        let dashboard = Dashboard::new(
            ComponentId::new("test.home.dashboard"),
            Table::new(
                ComponentId::new("test.home.dashboard.tickets"),
                vec![Column::new("ID", 1)],
            ),
            List::new(ComponentId::new("test.home.dashboard.sessions")),
        );
        Home::new(
            ComponentId::new("test.home"),
            dashboard,
            SessionId::new("S-1").expect("S-1 is a valid SessionId in this test"),
        )
    }

    fn ctx<'a>(
        theme: &'a Theme,
        caps: &'a Capabilities,
        clock: &'a FixedClock,
        focused: ComponentId,
    ) -> FrameContext<'a> {
        FrameContext {
            theme,
            caps,
            clock,
            focus: FocusState::new(Some(focused)),
        }
    }

    fn char_event(c: char) -> Event {
        Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
    }

    fn enter_event() -> Event {
        Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
    }

    /// Read row `y` of `buf` back as a plain string, for asserting on rendered text — the same
    /// convention `screens::dashboard`'s own tests use for its `goal` line.
    fn row_text(buf: &Buffer, area: Rect, y: u16) -> String {
        (0..area.width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    #[test]
    fn the_input_is_first_in_tab_order_so_it_is_focused_on_launch() {
        let home = home();
        assert_eq!(home.focusable_children().first(), Some(&home.input.id()));
    }

    #[test]
    fn a_fresh_home_screen_has_no_status_line() {
        let home = home();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, home.input.id());
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        home.render(area, &mut buf, &context);

        assert!(!row_text(&buf, area, 0).contains("project:"));
    }

    #[test]
    fn set_status_line_draws_a_status_line_above_the_panes() {
        let mut home = home();
        home.set_status_line(Some("project: /tmp/foo (repo scope)".to_string()));
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, home.input.id());
        let area = Rect::new(0, 0, 60, 10);
        let mut buf = Buffer::empty(area);
        home.render(area, &mut buf, &context);

        assert!(row_text(&buf, area, 0).contains("project: /tmp/foo (repo scope)"));
    }

    #[test]
    fn set_status_line_none_clears_a_previously_set_status_line() {
        let mut home = home();
        home.set_status_line(Some("temporary status".to_string()));
        home.set_status_line(None);
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, home.input.id());
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        home.render(area, &mut buf, &context);

        assert!(!row_text(&buf, area, 0).contains("temporary status"));
    }

    #[test]
    fn typing_and_pressing_enter_produces_a_submission_and_clears_the_field() {
        let mut home = home();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, home.input.id());

        for c in "fix the build".chars() {
            home.handle_event(&char_event(c), &context);
        }
        assert_eq!(home.take_submission(), None, "no submission before Enter");

        let propagation = home.handle_event(&enter_event(), &context);
        assert_eq!(propagation, Propagation::Consumed);
        assert_eq!(home.take_submission(), Some("fix the build".to_string()));
        assert_eq!(home.take_submission(), None, "take_submission clears it");
        assert_eq!(
            home.input.values(),
            vec![""],
            "the input resets after submitting"
        );
    }

    #[test]
    fn pressing_enter_on_an_empty_input_does_not_submit() {
        let mut home = home();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, home.input.id());

        home.handle_event(&enter_event(), &context);
        assert_eq!(home.take_submission(), None);
    }

    #[test]
    fn submitting_marks_a_turn_running_and_blocks_a_second_submission() {
        let mut home = home();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, home.input.id());

        home.handle_event(&char_event('a'), &context);
        home.handle_event(&enter_event(), &context);
        assert!(home.is_turn_running());
        assert_eq!(home.take_submission(), Some("a".to_string()));

        home.handle_event(&char_event('b'), &context);
        home.handle_event(&enter_event(), &context);
        assert_eq!(
            home.take_submission(),
            None,
            "a second Enter while a turn is running must not start another"
        );
    }

    #[test]
    fn a_stream_chunk_for_this_screens_session_is_pushed_into_the_pane() {
        let mut home = home();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, home.input.id());

        home.turn_running = true;
        let event = Event::App(AppMessage::StreamChunk {
            session: SessionId::new("S-1").unwrap(),
            text: "hello from the turn".to_string(),
            final_chunk: false,
        });
        let propagation = home.handle_event(&event, &context);
        assert_eq!(propagation, Propagation::Consumed);
        assert!(!home.stream.is_empty());
        assert!(home.is_turn_running(), "not the final chunk yet");
    }

    #[test]
    fn a_final_stream_chunk_clears_turn_running() {
        let mut home = home();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, home.input.id());

        home.turn_running = true;
        let event = Event::App(AppMessage::StreamChunk {
            session: SessionId::new("S-1").unwrap(),
            text: "done".to_string(),
            final_chunk: true,
        });
        home.handle_event(&event, &context);
        assert!(!home.is_turn_running());
    }

    #[test]
    fn a_stream_chunk_for_a_different_session_is_ignored() {
        let mut home = home();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, home.input.id());

        let event = Event::App(AppMessage::StreamChunk {
            session: SessionId::new("S-2").unwrap(),
            text: "not for us".to_string(),
            final_chunk: false,
        });
        home.handle_event(&event, &context);
        assert!(home.stream.is_empty());
    }

    #[test]
    fn resolve_finds_the_input_the_stream_and_the_dashboards_children() {
        let home = home();
        assert!(home.resolve(home.input.id()).is_some());
        assert!(home.resolve(home.stream.id()).is_some());
        assert!(home
            .resolve(ComponentId::new("test.home.dashboard.tickets"))
            .is_some());
        assert!(home.resolve(ComponentId::new("nope")).is_none());
    }

    #[test]
    fn set_dashboard_replaces_it() {
        let mut home = home();
        let replacement = Dashboard::new(
            ComponentId::new("test.home.dashboard"),
            Table::new(
                ComponentId::new("test.home.dashboard.tickets"),
                vec![Column::new("ID", 1), Column::new("Objective", 2)],
            ),
            List::new(ComponentId::new("test.home.dashboard.sessions")),
        );
        home.set_dashboard(replacement);
        assert_eq!(
            format!("{:?}", home.dashboard).matches("Objective").count(),
            1
        );
    }
}
