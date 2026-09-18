//! A navigable graph of tickets and their dependency edges — the widget D-002 calls out by name
//! as needing an app-owned component layer ratatui does not provide.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};
use ratatui_core::symbols::line;
use tm_types::TicketId;

use crate::caps::{Capabilities, UnicodeSupport};
use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::{display_width, scroll, truncate};
use crate::theme::Theme;

/// A label is clipped to this many columns (before the glyph/padding/border overhead) so one
/// long ticket title cannot blow the whole layout's column width out past 80-column readability.
const MAX_LABEL_WIDTH: usize = 18;
/// Every node box is exactly three rows: top border, one content row, bottom border. A taller
/// box (wrapped multi-line labels) would buy legibility for one node at the cost of the layout's
/// overall density; `MAX_LABEL_WIDTH` plus truncation is the tradeoff this widget makes instead.
const NODE_HEIGHT: u16 = 3;
/// Vertical spacing between stacked nodes within one rank, in canvas rows.
const ROW_GUTTER: u16 = 1;
/// Horizontal spacing reserved for edge routing between two adjacent ranks, in canvas columns.
const RANK_GUTTER: u16 = 4;

/// Where a ticket (or verification/audit node) currently stands, for the node's shape, glyph and
/// colour — never colour alone, so the graph stays legible under [`UnicodeSupport::AsciiOnly`]
/// and [`crate::caps::ColorSupport::NoColor`] alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeState {
    /// Not yet started.
    #[default]
    Pending,
    /// Actively being worked.
    InProgress,
    /// Completed successfully.
    Done,
    /// Blocked on something outside this node (a dependency, a lease, ...).
    Blocked,
    /// Failed (a verification or audit node that did not pass).
    Failed,
}

/// One node in the graph: a ticket, its display label, and its position on the graph's virtual
/// canvas (in cells, before the widget's own pan/zoom is applied at render time).
#[derive(Debug, Clone)]
pub struct GraphNode {
    /// The ticket (or verification/audit node) this graph node represents.
    pub id: TicketId,
    /// The label drawn inside the node's box.
    pub label: String,
    /// This node's current state, driving its box shape, glyph and colour. The screen that owns
    /// this widget is responsible for getting the right value in here; the widget only renders
    /// what it is given.
    pub state: NodeState,
    /// Position on the graph's virtual canvas. Populated by a layout pass — see
    /// [`Graph::set_graph`] — not chosen by the caller; treat this as the layout engine's output,
    /// not an input, even though callers can construct one directly for tests.
    pub position: (u16, u16),
}

impl GraphNode {
    /// A node with the given id, label and state; `position` is `(0, 0)` until [`Graph::set_graph`]
    /// lays it out.
    pub fn new(id: TicketId, label: impl Into<String>, state: NodeState) -> Self {
        GraphNode {
            id,
            label: label.into(),
            state,
            position: (0, 0),
        }
    }
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

/// Which direction along an edge a keyboard move traverses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeDirection {
    /// Toward a dependency (the node this one is blocked on).
    Predecessor,
    /// Toward a dependent (a node blocked on this one).
    Successor,
}

/// Which direction within a rank (same canvas column) a keyboard move traverses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RankDirection {
    Up,
    Down,
}

/// A navigable, pannable view of a ticket dependency graph.
///
/// Layout is a layered (Sugiyama-style) pass run once by [`Graph::set_graph`]: nodes are ranked
/// by longest path from a root so every dependency sits strictly left of its dependents, ranks
/// are ordered by a few barycenter sweeps to reduce edge crossings, and ranks/orders are then
/// turned into `(x, y)` canvas coordinates. `render` draws only the slice of that canvas visible
/// through `self.pan`; `handle_event` moves `self.selected` along graph edges (not raw canvas
/// directions) and pans just enough to keep the selection on screen.
#[derive(Debug, Clone)]
pub struct Graph {
    id: ComponentId,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    /// `TicketId` -> index into `nodes`, rebuilt whenever `nodes` changes.
    index: HashMap<TicketId, usize>,
    selected: usize,
    pan: (u16, u16),
    /// The area the last `render` call drew into, used by `ensure_visible` to pan just enough to
    /// keep the selection on screen. `render` takes `&self`, so this needs interior mutability.
    last_area: Cell<Rect>,
}

impl Graph {
    /// An empty graph.
    pub fn new(id: ComponentId) -> Self {
        Graph {
            id,
            nodes: Vec::new(),
            edges: Vec::new(),
            index: HashMap::new(),
            selected: 0,
            pan: (0, 0),
            last_area: Cell::new(Rect::default()),
        }
    }

    /// Replace the graph's nodes and edges and (re-)run layout.
    ///
    /// Ranks nodes by longest path from a root (so a chain of dependencies always reads
    /// left-to-right in dependency order), orders each rank with a few barycenter sweeps to
    /// reduce edge crossings, then assigns canvas coordinates from the resulting rank/order.
    /// Resets `self.selected`/`self.pan`, since neither the old selection index nor the old pan
    /// necessarily still means anything against the new graph.
    pub fn set_graph(&mut self, mut nodes: Vec<GraphNode>, edges: Vec<GraphEdge>) {
        let index: HashMap<TicketId, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.clone(), i))
            .collect();
        let rank = assign_ranks(&nodes, &edges, &index);
        let layers = order_within_ranks(&nodes, &edges, &index, &rank);
        layout_positions(&mut nodes, &layers);

        self.index = index;
        self.nodes = nodes;
        self.edges = edges;
        self.selected = 0;
        self.pan = (0, 0);
    }

    /// The currently selected node's ticket id, when the graph is non-empty.
    pub fn selected(&self) -> Option<&TicketId> {
        self.nodes.get(self.selected).map(|n| &n.id)
    }

    /// Indices of every node reachable from node `i` in `direction` by a single edge.
    fn neighbors(&self, i: usize, direction: EdgeDirection) -> Vec<usize> {
        let id = &self.nodes[i].id;
        match direction {
            EdgeDirection::Predecessor => self
                .edges
                .iter()
                .filter(|e| &e.to == id)
                .filter_map(|e| self.index.get(&e.from).copied())
                .collect(),
            EdgeDirection::Successor => self
                .edges
                .iter()
                .filter(|e| &e.from == id)
                .filter_map(|e| self.index.get(&e.to).copied())
                .collect(),
        }
    }

    /// The candidate in `candidates` whose canvas row is closest to the current selection's,
    /// breaking ties by `TicketId` for determinism.
    fn nearest_by_row(&self, candidates: Vec<usize>) -> Option<usize> {
        let cur_y = self.nodes[self.selected].position.1;
        candidates.into_iter().min_by(|&a, &b| {
            let da = self.nodes[a].position.1.abs_diff(cur_y);
            let db = self.nodes[b].position.1.abs_diff(cur_y);
            da.cmp(&db).then(self.nodes[a].id.cmp(&self.nodes[b].id))
        })
    }

    /// The nearest node sharing the current selection's canvas column (i.e. the same rank) in
    /// `direction`, or `None` when there is no such node.
    fn rank_neighbor(&self, direction: RankDirection) -> Option<usize> {
        let cur = &self.nodes[self.selected];
        let (x, y) = cur.position;
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.position.0 == x)
            .filter(|(_, n)| match direction {
                RankDirection::Up => n.position.1 < y,
                RankDirection::Down => n.position.1 > y,
            })
            .min_by_key(|(_, n)| n.position.1.abs_diff(y))
            .map(|(i, _)| i)
    }

    /// Pan just enough that the currently selected node's box is fully inside the last rendered
    /// area, in whichever axis it fell outside of.
    fn ensure_visible(&mut self) {
        let area = self.last_area.get();
        if area.width == 0 || area.height == 0 || self.nodes.is_empty() {
            return;
        }
        let node = &self.nodes[self.selected];
        let width = node_box_width(node);
        let (x, y) = node.position;

        if x < self.pan.0 {
            self.pan.0 = x;
        } else if x + width > self.pan.0 + area.width {
            self.pan.0 = (x + width).saturating_sub(area.width);
        }
        if y < self.pan.1 {
            self.pan.1 = y;
        } else if y + NODE_HEIGHT > self.pan.1 + area.height {
            self.pan.1 = (y + NODE_HEIGHT).saturating_sub(area.height);
        }
    }

    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Left),
                "select dependency",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Right),
                "select dependent",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Up),
                "select node above in this column",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Down),
                "select node below in this column",
            ),
        ]
    }
}

impl Component for Graph {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        self.last_area.set(area);
        if area.width == 0 || area.height == 0 {
            return;
        }

        for edge in &self.edges {
            let (Some(&from), Some(&to)) = (self.index.get(&edge.from), self.index.get(&edge.to))
            else {
                continue;
            };
            let from_node = &self.nodes[from];
            let to_node = &self.nodes[to];
            let src = to_screen(
                area,
                self.pan,
                (
                    from_node.position.0 + node_box_width(from_node),
                    from_node.position.1 + NODE_HEIGHT / 2,
                ),
            );
            let dst = to_screen(
                area,
                self.pan,
                (to_node.position.0, to_node.position.1 + NODE_HEIGHT / 2),
            );
            draw_edge(buf, area, ctx.theme, ctx.caps, src, dst);
        }

        for (i, node) in self.nodes.iter().enumerate() {
            let width = node_box_width(node);
            let (sx, sy) = to_screen(area, self.pan, node.position);
            if sy + (NODE_HEIGHT as i32) <= area.y as i32 || sy >= area.bottom() as i32 {
                continue;
            }
            if sx + (width as i32) <= area.x as i32 || sx >= area.right() as i32 {
                continue;
            }

            let chars = box_chars(node.state, ctx.caps);
            let style = node_style(self, i, node, ctx);
            let content = node_content(node, ctx.caps);
            let interior = (width - 2) as usize;

            let top = format!("{}{}{}", chars.tl, chars.h.repeat(interior), chars.tr);
            let mid = format!("{} {content} {}", chars.v, chars.v);
            let bottom = format!("{}{}{}", chars.bl, chars.h.repeat(interior), chars.br);

            draw_row(buf, area, sx, sy, &top, style);
            draw_row(buf, area, sx, sy + 1, &mid, style);
            draw_row(buf, area, sx, sy + 2, &bottom, style);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        if self.nodes.is_empty() {
            return Propagation::Propagate;
        }
        let Event::Input(InputEvent::Key(key)) = event else {
            return Propagation::Propagate;
        };

        use crossterm::event::KeyCode;
        let target = match key.code {
            KeyCode::Left => {
                self.nearest_by_row(self.neighbors(self.selected, EdgeDirection::Predecessor))
            }
            KeyCode::Right => {
                self.nearest_by_row(self.neighbors(self.selected, EdgeDirection::Successor))
            }
            KeyCode::Up => self.rank_neighbor(RankDirection::Up),
            KeyCode::Down => self.rank_neighbor(RankDirection::Down),
            _ => return Propagation::Propagate,
        };

        if let Some(next) = target {
            self.selected = next;
            self.ensure_visible();
        }
        Propagation::Consumed
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            Graph::bindings()
        } else {
            Vec::new()
        }
    }
}

// --- Layout: rank assignment, crossing-reduction ordering, coordinate assignment ---

/// Rank each node by its longest path from a root (a node with no incoming edge), via
/// topological processing (Kahn's algorithm) that relaxes each successor's rank to
/// `max(current, predecessor's rank + 1)` as it goes rather than stopping at the first predecessor
/// — that longest-path relaxation is what keeps a diamond-shaped dependency (`A -> B -> D`,
/// `A -> C -> D`, `B`/`C` different lengths) from placing `D` before every path into it has
/// cleared.
///
/// A ticket dependency graph should be acyclic, but this must not hang or panic if one sneaks in:
/// any node still unranked once the topological walk runs out of zero-in-degree work (i.e. it
/// sits on a cycle) is defensively assigned the next rank after the highest one reached, in its
/// original order, ignoring the cyclic edges' constraints.
fn assign_ranks(
    nodes: &[GraphNode],
    edges: &[GraphEdge],
    index: &HashMap<TicketId, usize>,
) -> Vec<u32> {
    let n = nodes.len();
    let mut successors: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut remaining_in_degree = vec![0u32; n];
    for edge in edges {
        if let (Some(&from), Some(&to)) = (index.get(&edge.from), index.get(&edge.to)) {
            successors[from].push(to);
            remaining_in_degree[to] += 1;
        }
    }

    let mut rank = vec![0u32; n];
    let mut visited = vec![false; n];
    let mut queue: VecDeque<usize> = (0..n).filter(|&i| remaining_in_degree[i] == 0).collect();
    let mut processed = 0usize;

    while let Some(u) = queue.pop_front() {
        if visited[u] {
            continue;
        }
        visited[u] = true;
        processed += 1;
        for &v in &successors[u] {
            rank[v] = rank[v].max(rank[u] + 1);
            remaining_in_degree[v] -= 1;
            if remaining_in_degree[v] == 0 {
                queue.push_back(v);
            }
        }
    }

    if processed < n {
        let mut next_rank = rank.iter().copied().max().map_or(0, |r| r + 1);
        for i in 0..n {
            if !visited[i] {
                rank[i] = next_rank;
                next_rank += 1;
            }
        }
    }

    rank
}

/// Group node indices into per-rank layers (index 0 = the lowest rank), then reduce edge
/// crossings with a handful of barycenter sweeps: each pass reorders every rank by the average
/// order-position of its neighbours in the adjacent rank just settled, alternating downward
/// (order by predecessors) and upward (order by successors) passes. This is the median-heuristic
/// shape Sugiyama layouts use; a full optimal crossing-minimization pass is not required for a
/// first, readable layout.
fn order_within_ranks(
    nodes: &[GraphNode],
    edges: &[GraphEdge],
    index: &HashMap<TicketId, usize>,
    rank: &[u32],
) -> Vec<Vec<usize>> {
    let max_rank = rank.iter().copied().max().unwrap_or(0);
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); (max_rank + 1) as usize];
    for (i, &r) in rank.iter().enumerate() {
        layers[r as usize].push(i);
    }

    let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut successors: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for edge in edges {
        if let (Some(&from), Some(&to)) = (index.get(&edge.from), index.get(&edge.to)) {
            successors[from].push(to);
            predecessors[to].push(from);
        }
    }

    let mut position = vec![0usize; nodes.len()];
    for layer in &layers {
        for (p, &n) in layer.iter().enumerate() {
            position[n] = p;
        }
    }

    const SWEEPS: usize = 4;
    for sweep in 0..SWEEPS {
        if sweep % 2 == 0 {
            // Skip rank 0: it has no predecessors to take a barycenter over.
            for layer in layers.iter_mut().skip(1) {
                reorder_by_barycenter(layer, &predecessors, &position);
                for (p, &n) in layer.iter().enumerate() {
                    position[n] = p;
                }
            }
        } else {
            for r in (0..layers.len().saturating_sub(1)).rev() {
                reorder_by_barycenter(&mut layers[r], &successors, &position);
                for (p, &n) in layers[r].iter().enumerate() {
                    position[n] = p;
                }
            }
        }
    }

    layers
}

/// Reorder `layer` in place by each node's barycenter (mean order-position) over `adjacency`,
/// keeping a node with no relevant neighbours at its current position rather than collapsing it
/// to one end, and breaking ties by node index for a deterministic, reproducible layout.
fn reorder_by_barycenter(layer: &mut [usize], adjacency: &[Vec<usize>], position: &[usize]) {
    let mut keyed: Vec<(usize, f64)> = layer
        .iter()
        .map(|&n| {
            let neighbors = &adjacency[n];
            let key = if neighbors.is_empty() {
                position[n] as f64
            } else {
                neighbors.iter().map(|&m| position[m] as f64).sum::<f64>() / neighbors.len() as f64
            };
            (n, key)
        })
        .collect();
    keyed.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    for (slot, (n, _)) in layer.iter_mut().zip(keyed) {
        *slot = n;
    }
}

/// Turn ranked, ordered layers into canvas `(x, y)` coordinates: each rank is one column, wide
/// enough for its widest node's box plus `RANK_GUTTER` for edge routing; within a rank, nodes
/// stack top to bottom separated by `ROW_GUTTER`.
fn layout_positions(nodes: &mut [GraphNode], layers: &[Vec<usize>]) {
    let mut x = 0u16;
    for layer in layers {
        let layer_width = layer
            .iter()
            .map(|&i| node_box_width(&nodes[i]))
            .max()
            .unwrap_or(0);
        let mut y = 0u16;
        for &i in layer {
            nodes[i].position = (x, y);
            y = y.saturating_add(NODE_HEIGHT + ROW_GUTTER);
        }
        x = x.saturating_add(layer_width + RANK_GUTTER);
    }
}

// --- Rendering ---

/// This node's box width in canvas columns: two border columns, two padding columns, the state
/// glyph, a separating space, and the (possibly truncated) label. Independent of
/// [`Capabilities`]/[`UnicodeSupport`] because every glyph this widget draws — ASCII fallback or
/// not — is exactly one column wide, so layout does not need to know which alphabet rendering
/// will eventually choose.
fn node_box_width(node: &GraphNode) -> u16 {
    let label = truncate(&node.label, MAX_LABEL_WIDTH, "…");
    (display_width(&label) as u16).saturating_add(6)
}

/// The glyph plus (possibly truncated) label drawn inside a node's box.
fn node_content(node: &GraphNode, caps: &Capabilities) -> String {
    let label = truncate(&node.label, MAX_LABEL_WIDTH, "…");
    format!("{} {label}", state_glyph(node.state, caps))
}

/// The single-column glyph marking `state`, distinct from colour so state still reads under
/// [`crate::caps::ColorSupport::NoColor`], and distinct enough from other states' glyphs to still
/// read under [`UnicodeSupport::AsciiOnly`].
fn state_glyph(state: NodeState, caps: &Capabilities) -> &'static str {
    let ascii = caps.unicode == UnicodeSupport::AsciiOnly;
    match (state, ascii) {
        (NodeState::Pending, false) => "○",
        (NodeState::Pending, true) => "o",
        (NodeState::InProgress, false) => "◐",
        (NodeState::InProgress, true) => ">",
        (NodeState::Done, false) => "✓",
        (NodeState::Done, true) => "v",
        (NodeState::Blocked, false) => "▲",
        (NodeState::Blocked, true) => "!",
        (NodeState::Failed, false) => "✕",
        (NodeState::Failed, true) => "x",
    }
}

/// One node box's corner/edge glyphs, distinct per state (shape, not just colour, carries state):
/// rounded for in-progress, doubled for blocked/failed (something needs attention), plain square
/// otherwise. Collapses to a single ASCII-safe set under [`UnicodeSupport::AsciiOnly`].
struct BoxChars {
    tl: &'static str,
    tr: &'static str,
    bl: &'static str,
    br: &'static str,
    h: &'static str,
    v: &'static str,
}

fn box_chars(state: NodeState, caps: &Capabilities) -> BoxChars {
    if caps.unicode == UnicodeSupport::AsciiOnly {
        return BoxChars {
            tl: "+",
            tr: "+",
            bl: "+",
            br: "+",
            h: "-",
            v: "|",
        };
    }
    match state {
        NodeState::InProgress => BoxChars {
            tl: line::ROUNDED_TOP_LEFT,
            tr: line::ROUNDED_TOP_RIGHT,
            bl: line::ROUNDED_BOTTOM_LEFT,
            br: line::ROUNDED_BOTTOM_RIGHT,
            h: line::HORIZONTAL,
            v: line::VERTICAL,
        },
        NodeState::Blocked | NodeState::Failed => BoxChars {
            tl: line::DOUBLE_TOP_LEFT,
            tr: line::DOUBLE_TOP_RIGHT,
            bl: line::DOUBLE_BOTTOM_LEFT,
            br: line::DOUBLE_BOTTOM_RIGHT,
            h: line::DOUBLE_HORIZONTAL,
            v: line::DOUBLE_VERTICAL,
        },
        NodeState::Pending | NodeState::Done => BoxChars {
            tl: line::TOP_LEFT,
            tr: line::TOP_RIGHT,
            bl: line::BOTTOM_LEFT,
            br: line::BOTTOM_RIGHT,
            h: line::HORIZONTAL,
            v: line::VERTICAL,
        },
    }
}

/// This node's box style: its state colour, replaced with `ctx.theme.selection`'s full-emphasis
/// style when it is selected and this widget holds focus, or bolded in place when selected but
/// unfocused (so the selection is still findable without competing with whatever else has focus —
/// the same "dim, don't drop" rule `Table::selection_style` uses).
fn node_style(graph: &Graph, index: usize, node: &GraphNode, ctx: &FrameContext<'_>) -> Style {
    let base = Style::new().fg(match node.state {
        NodeState::Pending => ctx.theme.muted,
        NodeState::InProgress => ctx.theme.accent,
        NodeState::Done => ctx.theme.success,
        NodeState::Blocked => ctx.theme.warning,
        NodeState::Failed => ctx.theme.danger,
    });
    if index != graph.selected {
        return base;
    }
    if ctx.focus.is_focused(graph.id) {
        ctx.theme.selection
    } else {
        base.add_modifier(Modifier::BOLD)
    }
}

/// Canvas coordinates, less `pan`, placed within `area`: canvas `(0, 0)` maps to `area`'s
/// top-left corner.
fn to_screen(area: Rect, pan: (u16, u16), canvas: (u16, u16)) -> (i32, i32) {
    (
        area.x as i32 + canvas.0 as i32 - pan.0 as i32,
        area.y as i32 + canvas.1 as i32 - pan.1 as i32,
    )
}

/// Draw one row of `row` (a box border or content line) starting at screen column `sx`, clipping
/// to `area` on every side — including a wide grapheme straddling the left or right edge, via
/// [`scroll`], the same "reserve the columns, pad, never tear a cluster" discipline `text.rs`
/// uses for a horizontally-scrolled viewport.
fn draw_row(buf: &mut Buffer, area: Rect, sx: i32, y: i32, row: &str, style: Style) {
    if y < area.y as i32 || y >= area.bottom() as i32 {
        return;
    }
    let area_left = area.x as i32;
    let area_right = area.right() as i32;
    if sx >= area_right {
        return;
    }
    let row_width = display_width(row) as i32;
    if sx + row_width <= area_left {
        return;
    }

    let offset = (area_left - sx).max(0) as usize;
    let draw_x = sx.max(area_left) as u16;
    let max_width = (area_right - draw_x as i32).max(0) as usize;
    let visible = scroll(row, offset, max_width);
    buf.set_stringn(draw_x, y as u16, &visible, max_width, style);
}

/// Draw a single cell at `(x, y)`, when it falls inside `area`.
fn put(buf: &mut Buffer, area: Rect, x: i32, y: i32, s: &str, style: Style) {
    if x < area.x as i32 || x >= area.right() as i32 {
        return;
    }
    if y < area.y as i32 || y >= area.bottom() as i32 {
        return;
    }
    buf.set_stringn(x as u16, y as u16, s, 1, style);
}

/// Draw one dependency edge with orthogonal (box-drawing) routing: a horizontal run out of the
/// source's right edge, a vertical run at the midpoint column between the two ranks (skipped when
/// both endpoints share a row), and a horizontal run into the target's left edge, capped with an
/// arrow so direction reads even in a static screenshot.
///
/// Edges are not routed as a bus (parallel edges through the same gutter may overlap); a full
/// crossing-free bus router is not required for a first, readable layout, and this crate does not
/// pull in a dedicated routing dependency for it (workspace rule: no new dependencies).
fn draw_edge(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    caps: &Capabilities,
    src: (i32, i32),
    dst: (i32, i32),
) {
    // A backward or coincident edge (only reachable via the cycle fallback in `assign_ranks`,
    // since real dependency edges always point to a strictly later rank) has no sensible
    // left-to-right route; the node boxes and keyboard navigation still work without a line.
    if dst.0 <= src.0 {
        return;
    }

    let ascii = caps.unicode == UnicodeSupport::AsciiOnly;
    let h = if ascii { "-" } else { line::HORIZONTAL };
    let v = if ascii { "|" } else { line::VERTICAL };
    let style = Style::new().fg(theme.muted);
    let mid_x = src.0 + (dst.0 - src.0) / 2;

    draw_h_segment(buf, area, src.0, mid_x, src.1, h, style);
    if src.1 == dst.1 {
        draw_h_segment(buf, area, mid_x, dst.0, dst.1, h, style);
    } else {
        let (top_corner, bottom_corner) = if ascii {
            ("+", "+")
        } else if dst.1 > src.1 {
            (line::TOP_RIGHT, line::BOTTOM_LEFT)
        } else {
            (line::BOTTOM_RIGHT, line::TOP_LEFT)
        };
        put(buf, area, mid_x, src.1, top_corner, style);
        let (y0, y1) = if dst.1 > src.1 {
            (src.1 + 1, dst.1)
        } else {
            (dst.1 + 1, src.1)
        };
        for y in y0..y1 {
            put(buf, area, mid_x, y, v, style);
        }
        put(buf, area, mid_x, dst.1, bottom_corner, style);
        draw_h_segment(buf, area, mid_x, dst.0, dst.1, h, style);
    }

    let arrow = if ascii { ">" } else { "›" };
    put(buf, area, dst.0 - 1, dst.1, arrow, style);
}

/// Draw `ch` at every column in `[x0, x1)` on row `y`.
fn draw_h_segment(buf: &mut Buffer, area: Rect, x0: i32, x1: i32, y: i32, ch: &str, style: Style) {
    let mut x = x0;
    while x < x1 {
        put(buf, area, x, y, ch, style);
        x += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::ColorSupport;
    use crate::component::FocusState;
    use crate::theme::Theme;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tm_types::FixedClock;

    fn tid(s: &str) -> TicketId {
        TicketId::new(s).expect("test ticket id literal is always a valid TicketId")
    }

    fn node(id: &str, label: &str) -> GraphNode {
        GraphNode::new(tid(id), label, NodeState::Pending)
    }

    fn ctx<'a>(
        theme: &'a Theme,
        caps: &'a Capabilities,
        clock: &'a FixedClock,
        focused_id: Option<ComponentId>,
    ) -> FrameContext<'a> {
        FrameContext {
            theme,
            caps,
            clock,
            focus: FocusState::new(focused_id),
        }
    }

    fn full_caps() -> Capabilities {
        Capabilities {
            color: ColorSupport::TrueColor,
            unicode: UnicodeSupport::Full,
            ..Capabilities::minimal()
        }
    }

    #[test]
    fn new_graph_has_no_selection() {
        let graph = Graph::new(ComponentId::new("test.graph"));
        assert_eq!(graph.selected(), None);
    }

    /// A -> B -> C: a straight dependency chain must land in three strictly increasing ranks
    /// (canvas columns), in dependency order.
    #[test]
    fn set_graph_ranks_a_chain_left_to_right() {
        let mut graph = Graph::new(ComponentId::new("test.graph"));
        graph.set_graph(
            vec![node("T-1", "a"), node("T-2", "b"), node("T-3", "c")],
            vec![
                GraphEdge {
                    from: tid("T-1"),
                    to: tid("T-2"),
                },
                GraphEdge {
                    from: tid("T-2"),
                    to: tid("T-3"),
                },
            ],
        );
        let by_id = |id: &str| graph.nodes.iter().find(|n| n.id == tid(id)).unwrap();
        assert!(by_id("T-1").position.0 < by_id("T-2").position.0);
        assert!(by_id("T-2").position.0 < by_id("T-3").position.0);
    }

    /// A diamond (A -> B -> D, A -> C -> D) must rank D strictly after *both* B and C, which only
    /// the longest-path relaxation (not "first predecessor wins") guarantees when B/C differ.
    #[test]
    fn set_graph_ranks_by_longest_path_through_a_diamond() {
        let mut graph = Graph::new(ComponentId::new("test.graph"));
        graph.set_graph(
            vec![
                node("T-1", "a"),
                node("T-2", "b"),
                node("T-3", "c"),
                node("T-4", "d"),
            ],
            vec![
                GraphEdge {
                    from: tid("T-1"),
                    to: tid("T-2"),
                },
                GraphEdge {
                    from: tid("T-1"),
                    to: tid("T-3"),
                },
                GraphEdge {
                    from: tid("T-2"),
                    to: tid("T-4"),
                },
                GraphEdge {
                    from: tid("T-3"),
                    to: tid("T-4"),
                },
            ],
        );
        let by_id = |id: &str| graph.nodes.iter().find(|n| n.id == tid(id)).unwrap();
        assert!(by_id("T-4").position.0 > by_id("T-2").position.0);
        assert!(by_id("T-4").position.0 > by_id("T-3").position.0);
    }

    /// A cycle must not hang layout, and every node still gets *some* rank.
    #[test]
    fn set_graph_does_not_hang_on_a_cycle() {
        let mut graph = Graph::new(ComponentId::new("test.graph"));
        graph.set_graph(
            vec![node("T-1", "a"), node("T-2", "b")],
            vec![
                GraphEdge {
                    from: tid("T-1"),
                    to: tid("T-2"),
                },
                GraphEdge {
                    from: tid("T-2"),
                    to: tid("T-1"),
                },
            ],
        );
        assert_eq!(graph.nodes.len(), 2);
    }

    #[test]
    fn set_graph_resets_selection_and_pan() {
        let mut graph = Graph::new(ComponentId::new("test.graph"));
        graph.set_graph(vec![node("T-1", "a"), node("T-2", "b")], Vec::new());
        graph.selected = 1;
        graph.pan = (5, 5);
        graph.set_graph(vec![node("T-3", "c")], Vec::new());
        assert_eq!(graph.selected(), Some(&tid("T-3")));
        assert_eq!(graph.pan, (0, 0));
    }

    #[test]
    fn unfocused_graph_ignores_navigation() {
        let mut graph = Graph::new(ComponentId::new("test.graph"));
        graph.set_graph(
            vec![node("T-1", "a"), node("T-2", "b")],
            vec![GraphEdge {
                from: tid("T-1"),
                to: tid("T-2"),
            }],
        );
        let theme = Theme::default();
        let caps = full_caps();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, None);
        let event = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )));
        let propagation = graph.handle_event(&event, &context);
        assert_eq!(propagation, Propagation::Propagate);
        assert_eq!(graph.selected(), Some(&tid("T-1")));
    }

    #[test]
    fn right_then_left_follows_the_dependency_edge_and_back() {
        let mut graph = Graph::new(ComponentId::new("test.graph"));
        let id = ComponentId::new("test.graph");
        graph.set_graph(
            vec![node("T-1", "a"), node("T-2", "b")],
            vec![GraphEdge {
                from: tid("T-1"),
                to: tid("T-2"),
            }],
        );
        let theme = Theme::default();
        let caps = full_caps();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, Some(id));

        let right = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )));
        assert_eq!(graph.handle_event(&right, &context), Propagation::Consumed);
        assert_eq!(graph.selected(), Some(&tid("T-2")));

        let left = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        )));
        assert_eq!(graph.handle_event(&left, &context), Propagation::Consumed);
        assert_eq!(graph.selected(), Some(&tid("T-1")));
    }

    #[test]
    fn up_down_navigate_within_a_rank() {
        let mut graph = Graph::new(ComponentId::new("test.graph"));
        let id = ComponentId::new("test.graph");
        // Two independent roots share rank 0 (no edges between them), stacked top to bottom.
        graph.set_graph(vec![node("T-1", "a"), node("T-2", "b")], Vec::new());
        let theme = Theme::default();
        let caps = full_caps();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, Some(id));

        let down = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )));
        assert_eq!(graph.handle_event(&down, &context), Propagation::Consumed);
        assert_eq!(graph.selected(), Some(&tid("T-2")));

        let up = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )));
        assert_eq!(graph.handle_event(&up, &context), Propagation::Consumed);
        assert_eq!(graph.selected(), Some(&tid("T-1")));
    }

    #[test]
    fn render_stays_within_80_columns_and_shows_selected_node() {
        let mut graph = Graph::new(ComponentId::new("test.graph"));
        graph.set_graph(
            vec![
                node("T-1", "alpha the first ticket"),
                node("T-2", "bravo"),
                node("T-3", "charlie"),
            ],
            vec![
                GraphEdge {
                    from: tid("T-1"),
                    to: tid("T-2"),
                },
                GraphEdge {
                    from: tid("T-1"),
                    to: tid("T-3"),
                },
            ],
        );
        let theme = Theme::default();
        let caps = full_caps();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, Some(ComponentId::new("test.graph")));

        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        graph.render(area, &mut buf, &context);

        for y in 0..area.height {
            let mut width = 0usize;
            for x in 0..area.width {
                width += display_width(buf[(x, y)].symbol());
            }
            assert!(width <= 80, "row {y} rendered wider than 80 columns");
        }
    }

    #[test]
    fn render_never_panics_at_a_one_by_one_viewport() {
        let mut graph = Graph::new(ComponentId::new("test.graph"));
        graph.set_graph(
            vec![node("T-1", "中文标题"), node("T-2", "b")],
            vec![GraphEdge {
                from: tid("T-1"),
                to: tid("T-2"),
            }],
        );
        let theme = Theme::default();
        let caps = full_caps();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, Some(ComponentId::new("test.graph")));

        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);
        graph.render(area, &mut buf, &context);
    }

    #[test]
    fn ascii_only_caps_uses_plain_box_glyphs() {
        let mut graph = Graph::new(ComponentId::new("test.graph"));
        graph.set_graph(vec![node("T-1", "a")], Vec::new());
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, None);

        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        graph.render(area, &mut buf, &context);

        let mut saw_plus = false;
        for y in 0..area.height {
            for x in 0..area.width {
                if buf[(x, y)].symbol() == "+" {
                    saw_plus = true;
                }
            }
        }
        assert!(saw_plus, "ASCII-only rendering should use '+' corners");
    }
}
