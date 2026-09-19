//! The home screen: at a glance, what tickets need attention, what sessions are running, and
//! (`SPEC.md` §29, `docs/audit-2026-09-18-fable.md` B-09) the active ticket's current durable
//! goal, if it has one.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;

use crate::component::{Component, ComponentId, ComponentParent, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, Propagation};
use crate::theme::split_horizontal;
use crate::widgets_data::list::List;
use crate::widgets_data::table::Table;

/// Draw `heading` in the top row of `area`, styled `accent` when `active` (this pane holds
/// internal focus) or `muted` otherwise, and return the remaining area below it for the pane's
/// own widget to render into.
fn heading(
    area: Rect,
    buf: &mut Buffer,
    ctx: &FrameContext<'_>,
    heading: &str,
    active: bool,
) -> Rect {
    if area.height == 0 {
        return area;
    }
    let style = if active {
        ctx.theme.accent
    } else {
        ctx.theme.muted
    };
    buf.set_stringn(area.x, area.y, heading, area.width as usize, style);
    Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    }
}

/// Which of the dashboard's two panes currently has internal focus.
///
/// IMPL: this screen manages focus between exactly two children itself rather than going through
/// `component::FocusTree` — a fixed two-pane layout does not need general tab-order computation,
/// and screens are free to make that call independently of whether `FocusTree` exists yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Tickets,
    Sessions,
}

/// One row reserved above the ticket/session panes for [`Dashboard::goal`]'s "Goal: <text>" line,
/// when set. Mirrors `screens::home::Home`'s own `INPUT_HEIGHT` convention: a single row is
/// enough for one line of text, and this screen owns no domain logic (`tm-tui` has no `tm_core`
/// dependency — see this module's own top-level doc comment) so it never wraps or truncates the
/// text itself beyond what `Buffer::set_stringn` already does at the area's width.
const GOAL_HEIGHT: u16 = 1;

/// The dashboard: an optional current-goal line on top, a ticket table on the left below it, a
/// session list on the right.
///
/// IMPL:
/// - `render`: if `self.goal` is `Some`, carve [`GOAL_HEIGHT`] rows off the top of `area` for a
///   `"Goal: <text>"` line (styled `ctx.theme.accent`, mirroring `heading`'s active style — a
///   goal is always worth drawing attention to, regardless of which pane has focus) before
///   `crate::theme::split_horizontal(area, &[("tickets", 3), ("sessions", 2)])` runs on what's
///   left; with no goal set, the full `area` goes to the split exactly as before this field
///   existed. Render `self.tickets` into the `"tickets"` slot and `self.sessions` into
///   `"sessions"`. Draw a border/heading per pane using `ctx.theme.muted` for the inactive pane's
///   heading and `ctx.theme.accent` for `self.active`'s.
/// - `handle_event`: Tab switches `self.active` between `Pane::Tickets`/`Pane::Sessions`, gated
///   on `ctx.focus.is_focused(self.id())` — the dashboard only owns internal pane focus while it
///   is itself the focused component. Otherwise forward the event to whichever child
///   `self.active` names (`self.tickets.handle_event(..)` or `self.sessions.handle_event(..)`)
///   and return its `Propagation`, falling back to `Propagation::Propagate` for the Tab case
///   itself once handled (a screen consuming Tab to switch panes should still stop it bubbling
///   further, per the `Component::handle_event` contract).
#[derive(Debug)]
pub struct Dashboard {
    id: ComponentId,
    tickets: Table,
    sessions: List,
    active: Pane,
    /// The active ticket's current durable goal text (`SPEC.md` §29), if any — a plain `String`,
    /// not a `tm_core::GoalState`: this crate has no `tm_core` dependency (see the module doc
    /// comment), so the caller (`tm-cli`'s `tui.rs`, which does depend on it) reads
    /// `tm_core::Store::goal_state` and hands over already-rendered text via
    /// [`Dashboard::set_goal`].
    goal: Option<String>,
}

impl Dashboard {
    /// A dashboard over the given ticket table and session list, starting with the ticket pane
    /// active and no goal line shown.
    pub fn new(id: ComponentId, tickets: Table, sessions: List) -> Self {
        Dashboard {
            id,
            tickets,
            sessions,
            active: Pane::Tickets,
            goal: None,
        }
    }

    /// Set (or clear, with `None`) the goal line shown above the ticket/session panes.
    pub fn set_goal(&mut self, goal: Option<String>) {
        self.goal = goal;
    }
}

impl Component for Dashboard {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let panes_area = match &self.goal {
            Some(text) if area.height > 0 => {
                let goal_height = GOAL_HEIGHT.min(area.height);
                buf.set_stringn(
                    area.x,
                    area.y,
                    format!("Goal: {text}"),
                    area.width as usize,
                    ctx.theme.accent,
                );
                Rect {
                    y: area.y + goal_height,
                    height: area.height.saturating_sub(goal_height),
                    ..area
                }
            }
            _ => area,
        };
        let slots = split_horizontal(panes_area, &[("tickets", 3), ("sessions", 2)]);

        let tickets_area = heading(
            slots.get("tickets"),
            buf,
            ctx,
            "Tickets",
            self.active == Pane::Tickets,
        );
        self.tickets.render(tickets_area, buf, ctx);

        let sessions_area = heading(
            slots.get("sessions"),
            buf,
            ctx,
            "Sessions",
            self.active == Pane::Sessions,
        );
        self.sessions.render(sessions_area, buf, ctx);
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if ctx.focus.is_focused(self.id) {
            if let Event::Input(InputEvent::Key(key)) = event {
                if key.code == crossterm::event::KeyCode::Tab {
                    self.active = match self.active {
                        Pane::Tickets => Pane::Sessions,
                        Pane::Sessions => Pane::Tickets,
                    };
                    return Propagation::Consumed;
                }
            }
        }

        match self.active {
            Pane::Tickets => self.tickets.handle_event(event, ctx),
            Pane::Sessions => self.sessions.handle_event(event, ctx),
        }
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        match self.active {
            Pane::Tickets => self.tickets.keybindings(ctx),
            Pane::Sessions => self.sessions.keybindings(ctx),
        }
    }

    fn focusable_children(&self) -> Vec<ComponentId> {
        vec![self.tickets.id(), self.sessions.id()]
    }
}

/// Resolves this dashboard's own id and its two panes' ids, so a root above it (`tm-cli`'s `App`)
/// can be a [`crate::component::ComponentParent`] over the whole tree without knowing the
/// dashboard's internals — see `Table`/`List`'s IMPL notes, which are leaves and never need this
/// themselves.
impl ComponentParent for Dashboard {
    fn resolve(&self, id: ComponentId) -> Option<&dyn Component> {
        if id == self.id {
            Some(self)
        } else if id == self.tickets.id() {
            Some(&self.tickets)
        } else if id == self.sessions.id() {
            Some(&self.sessions)
        } else {
            None
        }
    }

    fn resolve_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component> {
        if id == self.id {
            Some(self)
        } else if id == self.tickets.id() {
            Some(&mut self.tickets)
        } else if id == self.sessions.id() {
            Some(&mut self.sessions)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Capabilities;
    use crate::component::FocusState;
    use crate::theme::Theme;
    use crate::widgets_data::table::Column;
    use tm_types::FixedClock;

    fn dashboard() -> Dashboard {
        Dashboard::new(
            ComponentId::new("dashboard"),
            Table::new(
                ComponentId::new("dashboard.tickets"),
                vec![Column::new("Title", 1)],
            ),
            List::new(ComponentId::new("dashboard.sessions")),
        )
    }

    fn ctx<'a>(
        theme: &'a Theme,
        caps: &'a Capabilities,
        clock: &'a FixedClock,
    ) -> FrameContext<'a> {
        FrameContext {
            theme,
            caps,
            clock,
            focus: FocusState::new(None),
        }
    }

    fn row_text(buf: &Buffer, area: Rect, y: u16) -> String {
        (0..area.width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    #[test]
    fn a_fresh_dashboard_has_no_goal_line() {
        let d = dashboard();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock);
        let area = Rect::new(0, 0, 40, 6);
        let mut buf = Buffer::empty(area);
        d.render(area, &mut buf, &context);

        // With no goal set, row 0 belongs to the tickets/sessions pane headings, exactly as
        // before this field existed -- not a "Goal:" line.
        assert!(row_text(&buf, area, 0).contains("Tickets"));
    }

    #[test]
    fn set_goal_draws_a_goal_line_above_the_panes() {
        let mut d = dashboard();
        d.set_goal(Some("ship the login fix".to_string()));
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock);
        let area = Rect::new(0, 0, 40, 6);
        let mut buf = Buffer::empty(area);
        d.render(area, &mut buf, &context);

        assert!(row_text(&buf, area, 0).contains("Goal: ship the login fix"));
        // The panes still render, just shifted down by one row.
        assert!(row_text(&buf, area, 1).contains("Tickets"));
    }

    #[test]
    fn set_goal_none_clears_a_previously_set_goal() {
        let mut d = dashboard();
        d.set_goal(Some("temporary goal".to_string()));
        d.set_goal(None);
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock);
        let area = Rect::new(0, 0, 40, 6);
        let mut buf = Buffer::empty(area);
        d.render(area, &mut buf, &context);

        assert!(!row_text(&buf, area, 0).contains("Goal:"));
        assert!(row_text(&buf, area, 0).contains("Tickets"));
    }

    #[test]
    fn a_zero_height_area_with_a_goal_set_does_not_panic() {
        let mut d = dashboard();
        d.set_goal(Some("goal".to_string()));
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock);
        let area = Rect::new(0, 0, 40, 0);
        let mut buf = Buffer::empty(area);
        d.render(area, &mut buf, &context);
    }

    #[test]
    fn dashboard_reports_both_panes_as_focusable() {
        let dashboard = Dashboard::new(
            ComponentId::new("dashboard"),
            Table::new(
                ComponentId::new("dashboard.tickets"),
                vec![Column::new("Title", 1)],
            ),
            List::new(ComponentId::new("dashboard.sessions")),
        );
        assert_eq!(
            dashboard.focusable_children(),
            vec![
                ComponentId::new("dashboard.tickets"),
                ComponentId::new("dashboard.sessions")
            ]
        );
    }

    #[test]
    fn resolve_finds_self_and_both_panes_but_not_an_unknown_id() {
        let dashboard = Dashboard::new(
            ComponentId::new("dashboard"),
            Table::new(
                ComponentId::new("dashboard.tickets"),
                vec![Column::new("Title", 1)],
            ),
            List::new(ComponentId::new("dashboard.sessions")),
        );
        assert!(dashboard.resolve(ComponentId::new("dashboard")).is_some());
        assert!(dashboard
            .resolve(ComponentId::new("dashboard.tickets"))
            .is_some());
        assert!(dashboard
            .resolve(ComponentId::new("dashboard.sessions"))
            .is_some());
        assert!(dashboard.resolve(ComponentId::new("nope")).is_none());
    }

    #[test]
    fn resolve_mut_finds_the_tickets_pane() {
        let mut dashboard = Dashboard::new(
            ComponentId::new("dashboard"),
            Table::new(
                ComponentId::new("dashboard.tickets"),
                vec![Column::new("Title", 1)],
            ),
            List::new(ComponentId::new("dashboard.sessions")),
        );
        assert!(dashboard
            .resolve_mut(ComponentId::new("dashboard.tickets"))
            .is_some());
    }
}
