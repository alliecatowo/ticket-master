//! A single ticket's full detail view: its fields plus a scrollable list of related activity
//! (comments, status changes, linked tickets).

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use tm_types::TicketId;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, KeyBinding, Propagation};
use crate::widgets_data::form::Form;
use crate::widgets_data::list::List;

/// Which of the two panes currently has internal focus; see `Dashboard`'s identical field for
/// why this is screen-local rather than routed through `component::FocusTree`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Fields,
    Activity,
}

/// A ticket's detail view: an editable [`Form`] of its fields above a scrollable [`List`] of
/// activity.
///
/// IMPL:
/// - `render`: `crate::theme::split_vertical(area, &[("fields", 1), ("activity", 2)])`. A heading
///   line above the form showing `self.ticket` (e.g. `"T-42"`) styled with `ctx.theme.accent`.
/// - `handle_event`: Tab switches `self.active` between the two panes when this screen itself is
///   focused (mirrors `Dashboard::handle_event`); otherwise forwards to whichever child
///   `self.active` names.
#[derive(Debug)]
pub struct TicketDetailScreen {
    id: ComponentId,
    ticket: TicketId,
    fields: Form,
    activity: List,
    active: Pane,
}

impl TicketDetailScreen {
    /// A detail screen for `ticket`, with the given field form and activity list.
    pub fn new(id: ComponentId, ticket: TicketId, fields: Form, activity: List) -> Self {
        TicketDetailScreen { id, ticket, fields, activity, active: Pane::Fields }
    }

    /// The ticket this screen is showing.
    pub fn ticket(&self) -> &TicketId {
        &self.ticket
    }
}

impl Component for TicketDetailScreen {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let _ = (area, buf, ctx);
        todo!("split into fields/activity panes, draw the heading, and render each per the IMPL note above")
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        let _ = (event, ctx);
        todo!("switch panes on Tab or forward to the active pane per the IMPL note above")
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        match self.active {
            Pane::Fields => self.fields.keybindings(ctx),
            Pane::Activity => self.activity.keybindings(ctx),
        }
    }

    fn focusable_children(&self) -> Vec<ComponentId> {
        vec![self.fields.id(), self.activity.id()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets_data::form::Field;

    #[test]
    fn detail_screen_remembers_its_ticket() {
        let screen = TicketDetailScreen::new(
            ComponentId::new("ticket_detail"),
            TicketId::new("T-42").expect("T-42 is a valid TicketId in this test"),
            Form::new(ComponentId::new("ticket_detail.fields"), vec![Field::text("Title")]),
            List::new(ComponentId::new("ticket_detail.activity")),
        );
        assert_eq!(screen.ticket().as_str(), "T-42");
    }
}
