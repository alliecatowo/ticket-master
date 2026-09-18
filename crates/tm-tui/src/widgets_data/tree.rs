//! An expandable/collapsible tree, e.g. a ticket's dependency chain or a project's milestone
//! breakdown rendered as an outline rather than a graph (see `widgets_viz::graph` for the
//! node-and-edge rendering of the same kind of data).

use std::cell::Cell;
use std::collections::HashSet;

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::Modifier;

use crate::caps::UnicodeSupport;
use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::{display_width, truncate};

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
        Node {
            label: label.into(),
            children: Vec::new(),
        }
    }

    /// A node with children.
    pub fn with_children(label: impl Into<String>, children: Vec<Node>) -> Self {
        Node {
            label: label.into(),
            children,
        }
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
    /// Paths (child-index sequences from a root) that are currently expanded. See the struct's
    /// IMPL note above for why a path is used as the key rather than a stable id.
    expanded: HashSet<Vec<usize>>,
    /// Index into the flattened visible-row list (`visible_nodes`), not a path — the row the
    /// cursor is on, independent of how deep that row happens to be nested.
    selected: usize,
    scroll_offset: usize,
    /// The number of rows drawn by the last `render` call; see `Table::visible_rows`.
    visible_rows: Cell<u16>,
}

impl Tree {
    /// An empty tree.
    pub fn new(id: ComponentId) -> Self {
        Tree {
            id,
            roots: Vec::new(),
            expanded: HashSet::new(),
            selected: 0,
            scroll_offset: 0,
            visible_rows: Cell::new(0),
        }
    }

    /// Replace the root nodes. Collapses everything (a stub-level simplification; see the IMPL
    /// note above about expansion-path stability across data changes).
    pub fn set_roots(&mut self, roots: Vec<Node>) {
        self.roots = roots;
        self.selected = 0;
        self.scroll_offset = 0;
        self.expanded.clear();
    }

    /// Flatten the currently-visible nodes (roots, then each expanded node's children,
    /// recursively) into `(path, depth, node)` triples in display order.
    fn visible_nodes(&self) -> Vec<(Vec<usize>, usize, &Node)> {
        fn walk<'a>(
            nodes: &'a [Node],
            path: &mut Vec<usize>,
            depth: usize,
            expanded: &HashSet<Vec<usize>>,
            out: &mut Vec<(Vec<usize>, usize, &'a Node)>,
        ) {
            for (index, node) in nodes.iter().enumerate() {
                path.push(index);
                out.push((path.clone(), depth, node));
                if expanded.contains(path) {
                    walk(&node.children, path, depth + 1, expanded, out);
                }
                path.pop();
            }
        }

        let mut out = Vec::new();
        let mut path = Vec::new();
        walk(&self.roots, &mut path, 0, &self.expanded, &mut out);
        out
    }

    /// [`Tree::visible_nodes`], stripped to owned `(path, has_children)` pairs so the result does
    /// not keep `self` borrowed — needed in `handle_event`, which reads this list and then wants
    /// to mutate `self.expanded`/`self.selected` in the same call.
    fn visible_paths(&self) -> Vec<(Vec<usize>, bool)> {
        self.visible_nodes()
            .into_iter()
            .map(|(path, _depth, node)| (path, !node.children.is_empty()))
            .collect()
    }

    /// Keep `self.scroll_offset` a valid window containing `self.selected`, given `len` visible
    /// rows in total and `self.visible_rows` rows drawn at a time.
    fn clamp_scroll(&mut self, len: usize) {
        if len == 0 {
            self.scroll_offset = 0;
            return;
        }
        let visible = self.visible_rows.get().max(1) as usize;
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + visible {
            self.scroll_offset = self.selected + 1 - visible;
        }
        let max_offset = len.saturating_sub(visible);
        self.scroll_offset = self.scroll_offset.min(max_offset);
    }

    /// Move `self.selected` by `delta` rows over `len` visible rows, then re-clamp scroll.
    fn move_selection(&mut self, delta: isize, len: usize) {
        if len == 0 {
            return;
        }
        let last = len - 1;
        let next = (self.selected as isize)
            .saturating_add(delta)
            .clamp(0, last as isize);
        self.selected = next as usize;
        self.clamp_scroll(len);
    }

    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Right),
                "expand node",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Left),
                "collapse node / select parent",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Up),
                "move selection up",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Down),
                "move selection down",
            ),
        ]
    }
}

impl Component for Tree {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.width == 0 || area.height == 0 {
            self.visible_rows.set(0);
            return;
        }
        self.visible_rows.set(area.height);

        let rows = self.visible_nodes();
        let focused = ctx.focus.is_focused(self.id);
        let ascii_only = matches!(ctx.caps.unicode, UnicodeSupport::AsciiOnly);

        for (index, (path, depth, node)) in rows
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .take(area.height as usize)
        {
            let y = area.y + (index - self.scroll_offset) as u16;
            let indent = (*depth as u16) * 2;
            let has_children = !node.children.is_empty();
            let is_expanded = self.expanded.contains(path);
            let glyph = if !has_children {
                "  "
            } else if ascii_only {
                if is_expanded {
                    "v "
                } else {
                    "> "
                }
            } else if is_expanded {
                "▾ "
            } else {
                "▸ "
            };

            let style = if index == self.selected {
                selection_style(ctx, focused)
            } else {
                ratatui_core::style::Style::default().fg(ctx.theme.foreground)
            };
            buf.set_style(Rect::new(area.x, y, area.width, 1), style);

            if indent >= area.width {
                continue;
            }
            let glyph_x = area.x + indent;
            let remaining_after_indent = area.width - indent;
            buf.set_stringn(glyph_x, y, glyph, remaining_after_indent as usize, style);

            let glyph_width = display_width(glyph) as u16;
            let text_x = glyph_x.saturating_add(glyph_width);
            let content_width = area.width.saturating_sub(indent + glyph_width) as usize;
            let text = truncate(&node.label, content_width, "…");
            buf.set_stringn(text_x, y, &text, content_width, style);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let Event::Input(InputEvent::Key(key)) = event else {
            return Propagation::Propagate;
        };
        let rows = self.visible_paths();
        if rows.is_empty() {
            return Propagation::Propagate;
        }
        let len = rows.len();

        use crossterm::event::KeyCode;
        let page = self.visible_rows.get().max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1, len);
                Propagation::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1, len);
                Propagation::Consumed
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.move_selection(isize::MIN, len);
                Propagation::Consumed
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.move_selection(isize::MAX, len);
                Propagation::Consumed
            }
            KeyCode::PageUp => {
                self.move_selection(-page, len);
                Propagation::Consumed
            }
            KeyCode::PageDown => {
                self.move_selection(page, len);
                Propagation::Consumed
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                let (path, has_children) = &rows[self.selected];
                if *has_children {
                    self.expanded.insert(path.clone());
                }
                Propagation::Consumed
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let (path, _) = &rows[self.selected];
                if self.expanded.contains(path) {
                    let path = path.clone();
                    self.expanded.remove(&path);
                } else if path.len() > 1 {
                    let parent_path = path[..path.len() - 1].to_vec();
                    if let Some(parent_index) = rows.iter().position(|(p, _)| *p == parent_path) {
                        self.selected = parent_index;
                        self.clamp_scroll(len);
                    }
                }
                Propagation::Consumed
            }
            _ => Propagation::Propagate,
        }
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            Tree::bindings()
        } else {
            Vec::new()
        }
    }
}

/// The selected row's style: see `table::selection_style`, which this mirrors.
fn selection_style(ctx: &FrameContext<'_>, focused: bool) -> ratatui_core::style::Style {
    if focused {
        ctx.theme.selection
    } else {
        ratatui_core::style::Style::default()
            .fg(ctx.theme.foreground)
            .add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Capabilities;
    use crate::component::FocusState;
    use crate::event::InputEvent;
    use crate::theme::Theme;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tm_types::FixedClock;

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

    fn ctx<'a>(
        theme: &'a Theme,
        caps: &'a Capabilities,
        clock: &'a FixedClock,
        focused: bool,
    ) -> FrameContext<'a> {
        let id = ComponentId::new("test.tree");
        FrameContext {
            theme,
            caps,
            clock,
            focus: if focused {
                FocusState::new(Some(id))
            } else {
                FocusState::default()
            },
        }
    }

    fn sample_tree() -> Tree {
        let mut tree = Tree::new(ComponentId::new("test.tree"));
        tree.set_roots(vec![
            Node::with_children("T-1", vec![Node::leaf("T-1.1"), Node::leaf("T-1.2")]),
            Node::leaf("T-2"),
        ]);
        tree
    }

    #[test]
    fn collapsed_tree_shows_only_roots() {
        let tree = sample_tree();
        assert_eq!(tree.visible_nodes().len(), 2);
    }

    #[test]
    fn expanding_selected_root_reveals_children() {
        let mut tree = sample_tree();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let expand = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )));
        assert_eq!(tree.handle_event(&expand, &context), Propagation::Consumed);
        assert_eq!(tree.visible_nodes().len(), 4);
    }

    #[test]
    fn collapsing_an_expanded_node_hides_children_again() {
        let mut tree = sample_tree();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let expand = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )));
        let collapse = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        )));
        tree.handle_event(&expand, &context);
        assert_eq!(
            tree.handle_event(&collapse, &context),
            Propagation::Consumed
        );
        assert_eq!(tree.visible_nodes().len(), 2);
    }

    #[test]
    fn collapsing_a_selected_child_selects_its_parent() {
        let mut tree = sample_tree();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let expand = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )));
        let down = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )));
        let collapse = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        )));
        tree.handle_event(&expand, &context); // expand T-1
        tree.handle_event(&down, &context); // select T-1.1
        tree.handle_event(&collapse, &context); // T-1.1 has no children: select parent T-1
        assert_eq!(tree.selected, 0);
    }

    #[test]
    fn unfocused_tree_ignores_keys() {
        let mut tree = sample_tree();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, false);
        let expand = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )));
        assert_eq!(tree.handle_event(&expand, &context), Propagation::Propagate);
        assert_eq!(tree.visible_nodes().len(), 2);
    }

    #[test]
    fn render_does_not_panic_on_zero_sized_area() {
        let tree = sample_tree();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        tree.render(Rect::new(0, 0, 0, 0), &mut buf, &context);
    }
}
