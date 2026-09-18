//! The verification ladder screen: a ticket's ordered verification steps and their status,
//! rendered as an outline.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use tm_types::TicketId;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, KeyBinding, Propagation};
use crate::widgets_data::tree::Tree;

/// The verification ladder for one ticket: its steps (and any sub-steps) as a [`Tree`], with a
/// heading naming the ticket.
///
/// IMPL:
/// - `render`: draw a heading line showing `self.ticket` (e.g. `"Verification: T-42"`), styled
///   with `ctx.theme.accent`, in the top row of `area`; render `self.steps` into the remainder
///   (`ratatui_core::layout::Rect` shrunk by one row — see `Dashboard`'s siblings for the
///   `theme::split_vertical` shape if a fixed-height heading slot reads more consistently with
///   the rest of this crate). Each step's pass/fail/pending status is expected to already be
///   encoded into its `Node::label` (e.g. a leading glyph or `"[pass]"` marker) by whoever builds
///   the `Tree` handed to `new`/`set_steps` — this screen does not itself know the domain
///   `VerificationStatus` type in `tm-core`, per this crate's dependency list.
/// - `handle_event`: this screen has exactly one focusable child (`self.steps`); forward every
///   event to it and return its `Propagation`, mirroring `TicketGraphScreen::handle_event`.
#[derive(Debug)]
pub struct VerificationLadderScreen {
    id: ComponentId,
    ticket: TicketId,
    steps: Tree,
}

impl VerificationLadderScreen {
    /// A verification ladder for `ticket`, over the given step tree.
    pub fn new(id: ComponentId, ticket: TicketId, steps: Tree) -> Self {
        VerificationLadderScreen { id, ticket, steps }
    }

    /// The ticket this screen is showing verification for.
    pub fn ticket(&self) -> &TicketId {
        &self.ticket
    }
}

impl Component for VerificationLadderScreen {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.height == 0 {
            return;
        }
        let heading = format!("Verification: {}", self.ticket.as_str());
        buf.set_stringn(
            area.x,
            area.y,
            heading,
            area.width as usize,
            ctx.theme.accent,
        );

        let steps_area = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        self.steps.render(steps_area, buf, ctx);
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        self.steps.handle_event(event, ctx)
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        self.steps.keybindings(ctx)
    }

    fn focusable_children(&self) -> Vec<ComponentId> {
        vec![self.steps.id()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_reports_the_step_tree_as_its_only_focusable_child() {
        let screen = VerificationLadderScreen::new(
            ComponentId::new("verification_ladder"),
            TicketId::new("V-1").expect("V-1 is a valid TicketId in this test"),
            Tree::new(ComponentId::new("verification_ladder.steps")),
        );
        assert_eq!(
            screen.focusable_children(),
            vec![ComponentId::new("verification_ladder.steps")]
        );
    }

    #[test]
    fn ladder_remembers_its_ticket() {
        let screen = VerificationLadderScreen::new(
            ComponentId::new("verification_ladder"),
            TicketId::new("V-1").expect("V-1 is a valid TicketId in this test"),
            Tree::new(ComponentId::new("verification_ladder.steps")),
        );
        assert_eq!(screen.ticket().as_str(), "V-1");
    }
}
