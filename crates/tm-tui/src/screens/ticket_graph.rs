//! The ticket dependency graph screen: the navigable graph plus a detail sidebar for whichever
//! node is selected.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, KeyBinding, Propagation};
use crate::widgets_viz::graph::Graph;

/// The ticket graph screen: [`Graph`] on the left, a plain-text detail panel on the right for
/// whatever node is currently selected.
///
/// IMPL:
/// - `render`: `crate::theme::split_horizontal(area, &[("graph", 3), ("detail", 2)])`. Render
///   `self.graph` into `"graph"`. For `"detail"`, look up `self.graph.selected()` and draw
///   whatever summary text `self.detail_for` (below) returns — this screen does not know ticket
///   domain details beyond a `TicketId`, so the actual summary content is supplied by the caller
///   via `set_detail_provider` or similar (add whichever shape fits once the wiring from
///   `tm-cli`/`tm-core` data into this screen is decided; a stub should not guess that contract).
/// - `handle_event`: this screen has exactly one focusable child (`self.graph`); forward every
///   event to it and return its `Propagation` — there is no pane-switching logic needed the way
///   `Dashboard` has, since the detail panel is not independently interactive.
#[derive(Debug)]
pub struct TicketGraphScreen {
    id: ComponentId,
    graph: Graph,
}

impl TicketGraphScreen {
    /// A screen wrapping the given graph widget.
    pub fn new(id: ComponentId, graph: Graph) -> Self {
        TicketGraphScreen { id, graph }
    }
}

impl Component for TicketGraphScreen {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let _ = (area, buf, ctx);
        todo!("split into graph/detail panes and render each per the IMPL note above")
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        self.graph.handle_event(event, ctx)
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        self.graph.keybindings(ctx)
    }

    fn focusable_children(&self) -> Vec<ComponentId> {
        vec![self.graph.id()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_reports_the_graph_as_its_only_focusable_child() {
        let screen = TicketGraphScreen::new(
            ComponentId::new("ticket_graph"),
            Graph::new(ComponentId::new("ticket_graph.graph")),
        );
        assert_eq!(screen.focusable_children(), vec![ComponentId::new("ticket_graph.graph")]);
    }
}
