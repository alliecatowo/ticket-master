//! A single ticket's full detail view: its fields plus a scrollable list of related activity
//! (comments, status changes, linked tickets).

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use tm_types::TicketId;

use crate::component::{Component, ComponentId, ComponentParent, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, Propagation};
use crate::theme::split_vertical;
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
        TicketDetailScreen {
            id,
            ticket,
            fields,
            activity,
            active: Pane::Fields,
        }
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
        if area.height == 0 {
            return;
        }
        let heading_row = Rect { height: 1, ..area };
        let heading = format!("Ticket {}", self.ticket.as_str());
        buf.set_stringn(
            heading_row.x,
            heading_row.y,
            heading,
            heading_row.width as usize,
            ctx.theme.accent,
        );

        let body = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        let slots = split_vertical(body, &[("fields", 1), ("activity", 2)]);
        self.fields.render(slots.get("fields"), buf, ctx);
        self.activity.render(slots.get("activity"), buf, ctx);
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if ctx.focus.is_focused(self.id) {
            if let Event::Input(InputEvent::Key(key)) = event {
                if key.code == crossterm::event::KeyCode::Tab {
                    self.active = match self.active {
                        Pane::Fields => Pane::Activity,
                        Pane::Activity => Pane::Fields,
                    };
                    return Propagation::Consumed;
                }
            }
        }

        match self.active {
            Pane::Fields => self.fields.handle_event(event, ctx),
            Pane::Activity => self.activity.handle_event(event, ctx),
        }
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

/// Resolves this screen's own id and its two panes' ids — added for `tm-cli`'s navigation shell
/// (`tui.rs`'s `App`), which composes this screen as a drill-down target the same way it already
/// composes `screens::home::Home` and needs a uniform way to reach into either one's children
/// without knowing their internals. Mirrors `screens::dashboard::Dashboard`'s identical impl
/// exactly (same two-child shape: a form/list pair instead of a table/list pair).
impl ComponentParent for TicketDetailScreen {
    fn resolve(&self, id: ComponentId) -> Option<&dyn Component> {
        if id == self.id {
            Some(self)
        } else if id == self.fields.id() {
            Some(&self.fields)
        } else if id == self.activity.id() {
            Some(&self.activity)
        } else {
            None
        }
    }

    fn resolve_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component> {
        if id == self.id {
            Some(self)
        } else if id == self.fields.id() {
            Some(&mut self.fields)
        } else if id == self.activity.id() {
            Some(&mut self.activity)
        } else {
            None
        }
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
            Form::new(
                ComponentId::new("ticket_detail.fields"),
                vec![Field::text("Title")],
            ),
            List::new(ComponentId::new("ticket_detail.activity")),
        );
        assert_eq!(screen.ticket().as_str(), "T-42");
    }

    fn screen() -> TicketDetailScreen {
        TicketDetailScreen::new(
            ComponentId::new("ticket_detail"),
            TicketId::new("T-42").expect("T-42 is a valid TicketId in this test"),
            Form::new(
                ComponentId::new("ticket_detail.fields"),
                vec![Field::text("Title")],
            ),
            List::new(ComponentId::new("ticket_detail.activity")),
        )
    }

    #[test]
    fn resolve_finds_self_and_both_panes_but_not_an_unknown_id() {
        let screen = screen();
        assert!(screen.resolve(ComponentId::new("ticket_detail")).is_some());
        assert!(screen
            .resolve(ComponentId::new("ticket_detail.fields"))
            .is_some());
        assert!(screen
            .resolve(ComponentId::new("ticket_detail.activity"))
            .is_some());
        assert!(screen.resolve(ComponentId::new("nope")).is_none());
    }

    #[test]
    fn resolve_mut_finds_the_fields_pane() {
        let mut screen = screen();
        assert!(screen
            .resolve_mut(ComponentId::new("ticket_detail.fields"))
            .is_some());
    }
}
