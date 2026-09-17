//! A navigable graph of tickets and their dependency edges — the widget D-002 calls out by name
//! as needing an app-owned component layer ratatui does not provide.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use tm_types::TicketId;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, KeyBinding, KeyChord, Propagation};

/// One node in the graph: a ticket, its display label, and its position on the graph's virtual
/// canvas (in cells, before the widget's own pan/zoom is applied at render time).
#[derive(Debug, Clone)]
pub struct GraphNode {
    /// The ticket (or verification/audit node) this graph node represents.
    pub id: TicketId,
    /// The label drawn inside the node's box.
    pub label: String,
    /// Position on the graph's virtual canvas. `IMPL`: populated by a layout pass — see
    /// [`Graph::set_graph`] — not chosen by the caller; treat this as the layout engine's output,
    /// not an input, even though callers can construct one directly for tests.
    pub position: (u16, u16),
}

/// A directed dependency edge between two nodes, referenced by [`TicketId`] rather than index so
/// edges survive `set_graph` reordering nodes.
#[derive(Debug, Clone)]
pub struct GraphEdge {
    /// The dependency (the ticket that must complete first).
    pub from: TicketId,
    /// The dependent (the ticket blocked on `from`).
    pub to: TicketId,
}

/// A navigable, pannable view of a ticket dependency graph.
///
/// IMPL:
/// - Layout: `set_graph` receives nodes *without* trustworthy positions and must assign them —
///   a layered/Sugiyama-style layout (rank nodes by longest path from a root, place each rank in
///   a column, order within a rank to minimize edge crossings) suits a DAG of dependencies well
///   and does not require a new dependency (it is plain graph traversal over `edges`). A full
///   crossing-minimization pass is not required for a first implementation; correct ranking is
///   the part that matters for readability.
/// - `render`: draw each node as a box (`label` inside, sized to fit it, clipped/truncated via
///   `crate::text::truncate` if the graph is zoomed out past legibility) positioned by
///   `node.position` minus `self.pan`, and draw edges between box edges using box-drawing
///   characters from `ratatui_core::symbols::line` (fall back to `-`/`|`/`+` when
///   `ctx.caps.unicode` is `AsciiOnly`). Style the node at `self.selected` with
///   `ctx.theme.selection`; style nodes representing failed/blocked verification state with
///   `ctx.theme.danger`/`ctx.theme.warning` (the screen owning this widget is responsible for
///   getting that status into `GraphNode` — this widget only renders what it is given).
/// - `handle_event`: arrow keys move `self.selected` to the nearest node in that direction that
///   shares an edge with the current selection (directional graph navigation, not directional
///   *canvas* navigation — moving "right" jumps along a dependency edge, it does not pan). Pan
///   the canvas (`self.pan`) as needed to keep the newly selected node visible within the last
///   rendered `area`. Gate on focus like the widgets in `widgets_data`.
#[derive(Debug, Clone)]
pub struct Graph {
    id: ComponentId,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    selected: usize,
    pan: (u16, u16),
}

impl Graph {
    /// An empty graph.
    pub fn new(id: ComponentId) -> Self {
        Graph {
            id,
            nodes: Vec::new(),
            edges: Vec::new(),
            selected: 0,
            pan: (0, 0),
        }
    }

    /// Replace the graph's nodes and edges and (re-)run layout.
    ///
    /// IMPL: run the layered layout described above to assign `position` on each of `nodes`
    /// before storing them; reset `self.selected`/`self.pan`.
    pub fn set_graph(&mut self, nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) {
        let _ = (nodes, edges);
        todo!("lay out `nodes` per `edges` and store both per the IMPL note above")
    }

    /// The currently selected node's ticket id, when the graph is non-empty.
    pub fn selected(&self) -> Option<&TicketId> {
        self.nodes.get(self.selected).map(|n| &n.id)
    }

    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Left), "select dependency"),
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Right), "select dependent"),
        ]
    }
}

impl Component for Graph {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let _ = (area, buf, ctx);
        todo!("draw nodes and edges per the IMPL note above")
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let _ = event;
        todo!("navigate along edges and pan to keep the selection visible per the IMPL note above")
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            Graph::bindings()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_graph_has_no_selection() {
        let graph = Graph::new(ComponentId::new("test.graph"));
        assert_eq!(graph.selected(), None);
    }
}
