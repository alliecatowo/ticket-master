//! The ticket dependency graph screen: the navigable graph plus a detail sidebar for whichever
//! node is selected.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use tm_types::TicketId;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, KeyBinding, Propagation};
use crate::theme::split_horizontal;
use crate::widgets_viz::graph::{Graph, GraphEdge, GraphNode};

/// The ticket graph screen: [`Graph`] on the left, a plain-text detail panel on the right naming
/// whatever node is currently selected. This screen has no `tm_core` dependency and does not know
/// ticket domain details beyond a `TicketId` — the caller (`tm-cli`'s `App::build_graph`) is
/// responsible for building the labelled [`GraphNode`]/[`GraphEdge`] data via [`Self::set_graph`]
/// and for reacting to [`Self::selected`] (e.g. Enter opening that ticket's own detail screen);
/// this screen only renders and navigates what it is given.
///
/// `handle_event` forwards every event straight to `self.graph`, the screen's one focusable
/// child, and returns its `Propagation` — there is no pane-switching logic needed, since the
/// detail panel is not independently interactive.
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

    /// Replace the graph's nodes and edges (e.g. after a project-state refresh). Forwards
    /// straight to [`Graph::set_graph`], including its reset of selection and pan.
    pub fn set_graph(&mut self, nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) {
        self.graph.set_graph(nodes, edges);
    }

    /// The currently selected node's ticket id, for the caller to act on (e.g. Enter opening
    /// that ticket's detail screen).
    pub fn selected(&self) -> Option<&TicketId> {
        self.graph.selected()
    }
}

impl Component for TicketGraphScreen {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let slots = split_horizontal(area, &[("graph", 3), ("detail", 2)]);

        self.graph.render(slots.get("graph"), buf, ctx);

        let detail_area = slots.get("detail");
        if detail_area.height > 0 {
            let summary = match self.graph.selected() {
                Some(id) => format!("Selected: {}", id.as_str()),
                None => "No node selected".to_string(),
            };
            buf.set_stringn(
                detail_area.x,
                detail_area.y,
                summary,
                detail_area.width as usize,
                ctx.theme.foreground,
            );
        }
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
        assert_eq!(
            screen.focusable_children(),
            vec![ComponentId::new("ticket_graph.graph")]
        );
    }
}
