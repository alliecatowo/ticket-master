//! The home screen: at a glance, what tickets need attention and what sessions are running.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, KeyBinding, Propagation};
use crate::widgets_data::table::Table;
use crate::widgets_data::list::List;

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
        Dashboard { id, tickets, sessions, active: Pane::Tickets }
    }
}

impl Component for Dashboard {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let _ = (area, buf, ctx);
        todo!("split into ticket/session panes and render each per the IMPL note above")
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        let _ = (event, ctx);
        todo!("switch panes on Tab or forward to the active pane per the IMPL note above")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets_data::table::Column;

    #[test]
    fn dashboard_reports_both_panes_as_focusable() {
        let dashboard = Dashboard::new(
            ComponentId::new("dashboard"),
            Table::new(ComponentId::new("dashboard.tickets"), vec![Column::new("Title", 1)]),
            List::new(ComponentId::new("dashboard.sessions")),
        );
        assert_eq!(
            dashboard.focusable_children(),
            vec![ComponentId::new("dashboard.tickets"), ComponentId::new("dashboard.sessions")]
        );
    }
}
