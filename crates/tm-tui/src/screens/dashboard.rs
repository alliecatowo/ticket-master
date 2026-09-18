//! The home screen: at a glance, what tickets need attention and what sessions are running.

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

/// The dashboard: a ticket table on the left, a session list on the right.
///
/// IMPL:
/// - `render`: `crate::theme::split_horizontal(area, &[("tickets", 3), ("sessions", 2)])`, render
///   `self.tickets` into the `"tickets"` slot and `self.sessions` into `"sessions"`. Draw a
///   border/heading per pane using `ctx.theme.muted` for the inactive pane's heading and
///   `ctx.theme.accent` for `self.active`'s.
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
}

impl Dashboard {
    /// A dashboard over the given ticket table and session list, starting with the ticket pane
    /// active.
    pub fn new(id: ComponentId, tickets: Table, sessions: List) -> Self {
        Dashboard {
            id,
            tickets,
            sessions,
            active: Pane::Tickets,
        }
    }
}

impl Component for Dashboard {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let slots = split_horizontal(area, &[("tickets", 3), ("sessions", 2)]);

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
    use crate::widgets_data::table::Column;

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
