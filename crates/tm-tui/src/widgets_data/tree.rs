//! An expandable/collapsible tree, e.g. a ticket's dependency chain or a project's milestone
//! breakdown rendered as an outline rather than a graph (see `widgets_viz::graph` for the
//! node-and-edge rendering of the same kind of data).

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, KeyBinding, KeyChord, Propagation};

/// One node in a [`Tree`]: a label and its children.
///
/// IMPL: this is a plain data node, not a `Component` itself — `Tree` owns all interaction state
/// (which paths are expanded, which row is selected) centrally rather than distributing it across
/// per-node components, since a node's identity within the tree is its path from the root, not a
/// `ComponentId` of its own.
#[derive(Debug, Clone)]
pub struct Node {
    /// The text shown for this node.
    pub label: String,
    /// This node's children, in display order.
    pub children: Vec<Node>,
}

impl Node {
    /// A leaf node with no children.
    pub fn leaf(label: impl Into<String>) -> Self {
        Node { label: label.into(), children: Vec::new() }
    }

    /// A node with children.
    pub fn with_children(label: impl Into<String>, children: Vec<Node>) -> Self {
        Node { label: label.into(), children }
    }
}

/// An expandable/collapsible tree over a forest of [`Node`]s.
///
/// IMPL:
/// - Track expansion as `expanded: std::collections::HashSet<Vec<usize>>`, where each key is a
///   path of child indices from a root (e.g. `[1, 0]` is the first child of the second root).
///   This avoids needing stable ids on `Node` while still surviving `set_roots` replacing the
///   data, as long as the shape does not change underneath an expanded path (acceptable for a
///   stub; note the limitation rather than solving stable-id diffing here).
/// - `render`: flatten the currently-visible nodes (roots, then each expanded node's children
///   recursively) into a `Vec<(depth, &Node)>` top to bottom, indent each line by `depth * 2`
///   columns, prefix expandable nodes with a `▸`/`▾` disclosure glyph from `ratatui_core::symbols`
///   (fall back to `>`/`v` when `ctx.caps.unicode` is `AsciiOnly`), and draw within `area` from
///   `self.scroll_offset` like `Table`/`List`. Style the selected visible row with
///   `ctx.theme.selection`.
/// - `handle_event`: Up/Down move `self.selected` over the same flattened visible list `render`
///   computes (recompute it here too, or cache it from the last render — a stub does not need to
///   solve that caching question, just move selection correctly). Right/`l`/Enter expands the
///   selected node (inserts its path into `expanded`) if it has children; Left/`h` collapses it if
///   expanded, else moves selection to its parent. Gate on focus like `Table`.
#[derive(Debug, Clone)]
pub struct Tree {
    id: ComponentId,
    roots: Vec<Node>,
    selected: usize,
    scroll_offset: usize,
}

impl Tree {
    /// An empty tree.
    pub fn new(id: ComponentId) -> Self {
        Tree {
            id,
            roots: Vec::new(),
            selected: 0,
            scroll_offset: 0,
        }
    }

    /// Replace the root nodes. Collapses everything (a stub-level simplification; see the IMPL
    /// note above about expansion-path stability across data changes).
    ///
    /// IMPL: `self.roots = roots; self.selected = 0; self.scroll_offset = 0;` and clear whatever
    /// expansion-tracking structure is chosen above.
    pub fn set_roots(&mut self, roots: Vec<Node>) {
        let _ = roots;
        todo!("store `roots` and reset selection/expansion per the IMPL note above")
    }

    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Right), "expand node"),
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Left), "collapse node"),
        ]
    }
}

impl Component for Tree {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let _ = (area, buf, ctx);
        todo!("flatten visible nodes and draw them per the IMPL note above")
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let _ = event;
        todo!("move selection / expand / collapse per the IMPL note above")
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            Tree::bindings()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_leaf_has_no_children() {
        let node = Node::leaf("T-1");
        assert_eq!(node.label, "T-1");
        assert!(node.children.is_empty());
    }

    #[test]
    fn new_tree_has_no_roots() {
        let tree = Tree::new(ComponentId::new("test.tree"));
        assert!(tree.roots.is_empty());
    }
}
