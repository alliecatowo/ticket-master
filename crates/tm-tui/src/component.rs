//! The component tree: the `Component` trait, focus, hit testing and event bubbling.
//!
//! This file has two halves with different guarantees.
//!
//! The **contract** — [`Component`], [`ComponentId`], [`FrameContext`], [`FocusState`] — is one
//! of the two finished, tested pieces every other file in this crate codes against (see the
//! crate root docs and `event.rs`). Its shape is settled; do not change these signatures without
//! updating every implementor.
//!
//! The **tree machinery** below it — [`FocusTree`] — is the part of ratatui's missing "retained
//! widget tree, focus management, event bubbling and hit testing" (D-002) that this crate exists
//! to build.

use std::collections::HashMap;

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Position, Rect};
use tm_types::Clock;

use crate::caps::Capabilities;
use crate::event::{Event, KeyBinding, Propagation};
use crate::theme::Theme;

/// A component's stable identity, used for focus routing, hit testing, and matching a component
/// to itself across frames.
///
/// Ids are literal, author-assigned paths (`"dashboard.sidebar.ticket_list"`), not allocated at
/// runtime: allocating them (a counter, a random suffix) would make component identity depend on
/// construction order or on `tm_types::IdSource`, neither of which a screen author should have to
/// thread through just to name a widget. Two components sharing an id is a bug the author
/// controls directly, the same way two HTML elements sharing a `key` prop would be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentId(&'static str);

impl ComponentId {
    /// Name a component. Conventionally a dotted path from the screen root, e.g.
    /// `"ticket_graph.node_list"`.
    pub const fn new(path: &'static str) -> Self {
        ComponentId(path)
    }

    /// The id's literal path.
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for ComponentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// A read-only snapshot of which component currently holds keyboard focus, threaded through
/// [`FrameContext`] so a component can render itself differently (a highlighted border, a
/// visible cursor) exactly when it is the one the user is typing into.
///
/// This is the value type a renderer reads. The mutable machinery that computes it — tab order
/// across a tree, focus-in/focus-out transitions — is [`FocusTree`], below.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FocusState {
    focused: Option<ComponentId>,
}

impl FocusState {
    /// A state with `focused` holding keyboard focus (or none, when `focused` is `None`).
    pub fn new(focused: Option<ComponentId>) -> Self {
        FocusState { focused }
    }

    /// The currently focused component, if any.
    pub fn focused(&self) -> Option<ComponentId> {
        self.focused
    }

    /// True when `id` is the focused component.
    pub fn is_focused(&self, id: ComponentId) -> bool {
        self.focused == Some(id)
    }
}

/// Everything rendering and event handling need that is not the component's own state: the
/// active theme, what the terminal can actually display, a clock for anything time-based
/// (animation, relative timestamps — never `std::time::Instant::now()`, per the workspace
/// determinism rule), and the current focus snapshot.
///
/// Built fresh once per frame by the runtime and passed down by shared reference; components
/// never own a `FrameContext`, they only borrow one for the duration of a `render` or
/// `handle_event` call.
#[derive(Clone, Copy)]
pub struct FrameContext<'a> {
    /// The active theme: palette, semantic styles, layout helpers.
    pub theme: &'a Theme,
    /// What this terminal can be trusted to render (colour depth, Unicode support, ...).
    pub caps: &'a Capabilities,
    /// The injected clock. Read via `ctx.clock.now()`; never call a wall-clock API directly.
    pub clock: &'a dyn Clock,
    /// Which component currently holds keyboard focus.
    pub focus: FocusState,
}

impl std::fmt::Debug for FrameContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameContext")
            .field("theme", &self.theme)
            .field("caps", &self.caps)
            .field("focus", &self.focus)
            .finish_non_exhaustive()
    }
}

/// A node in the app-owned component tree: something that can render into a slice of the
/// terminal buffer, react to an event, declare the keybindings it currently exposes, and report
/// which of its children can receive focus.
///
/// ratatui gives us a cell buffer and a diffing backend; it gives us no retained widget tree, no
/// focus management, no event bubbling and no hit testing (D-002). `Component` is the interface
/// that lets `FocusTree` provide all four uniformly, whether the concrete type is a leaf widget
/// (`widgets_data`, `widgets_viz`) or a screen composing several of them (`screens`).
///
/// Every method has a default appropriate for a pure-display leaf that never takes focus or
/// input; implementors override only what they need.
pub trait Component: std::fmt::Debug {
    /// This component's stable identity.
    fn id(&self) -> ComponentId;

    /// Render this component (and, for a container, its children) into `area` of `buf`.
    ///
    /// Implementations draw with `ratatui_core` types only (`Buffer`, `Rect`, `Style`, the text
    /// primitives) — never by writing escape sequences or touching a terminal handle directly,
    /// which belongs solely to `runtime.rs`. A container computes its children's sub-`Rect`s
    /// (typically via `theme::split_vertical`/`split_horizontal`) and calls their `render` in
    /// turn.
    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>);

    /// Offer `event` to this component.
    ///
    /// Return [`Propagation::Consumed`] when no ancestor should see the event again, or
    /// [`Propagation::Propagate`] to keep bubbling toward the root. The default always
    /// propagates, which is correct for widgets that only display and never take input.
    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        let _ = (event, ctx);
        Propagation::Propagate
    }

    /// The keybindings this component currently exposes, for a help overlay or the command
    /// palette. Empty by default. A component whose bindings change with its own state (e.g. a
    /// list that only accepts "delete" once something is selected) should reflect that here
    /// rather than advertising bindings it would not actually act on.
    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        let _ = ctx;
        Vec::new()
    }

    /// This component's immediate focusable children, in tab order.
    ///
    /// Leaf widgets return the empty default. Containers return the ids of the children they
    /// render that can themselves take focus (which may be a subset of all rendered children,
    /// and is not necessarily every id returned transitively — `FocusTree` calls this at each
    /// level and recurses itself rather than requiring a flattened list).
    fn focusable_children(&self) -> Vec<ComponentId> {
        Vec::new()
    }
}

/// A [`Component`] that owns focusable children and can hand one back by id.
///
/// `Component` alone cannot support this: it is not `Any` (no downcasting), and
/// `focusable_children` returns only ids, never a reference. Any component whose
/// `focusable_children()` is non-empty implements `ComponentParent` so [`FocusTree`] can resolve
/// those ids back to something it can call `render`/`handle_event`/`keybindings` on; a pure leaf
/// never needs to.
///
/// `resolve`/`resolve_mut` are defined over the *whole* subtree rooted at `self`, not just
/// immediate children — including `self` when `id == self.id()`. That is what lets `FocusTree`
/// resolve any id in one call from the tree root, rather than needing `ComponentParent` at every
/// intermediate level (which the type system cannot express without downcasting: a child handed
/// back as `&dyn Component` has no way to be recognised as also implementing `ComponentParent`).
/// A container's own implementation typically checks `id == self.id()`, then tries each child in
/// turn — the same per-child match arms `focusable_children()` already needs.
pub trait ComponentParent: Component {
    /// Resolve `id` to the component that owns it, if `id` names `self` or a descendant.
    fn resolve(&self, id: ComponentId) -> Option<&dyn Component>;

    /// Resolve `id` to the component that owns it, mutably.
    fn resolve_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component>;
}

/// Computes and owns tab order and focus transitions across a [`Component`] tree, and dispatches
/// an [`Event`] by bubbling it from the focused leaf up to the root.
///
/// This is the stub half of this file (see the module docs): the contract above is finished, and
/// this is the tree walk built on top of it, plus [`ComponentParent`], the resolver trait the
/// struct docs on the previous version of this stub called for.
///
/// - Tab order (`order`) is a pre-order walk of `focusable_children()` starting under the root
///   (the root itself is never tabbable — only what it exposes as focusable is). `parents` records
///   each visited id's immediate parent (the root's id for top-level children), which `keybindings`
///   uses to climb back toward the root. `spans` records each visited id's contiguous `order` range
///   (start, end) covering itself and every descendant, which is what makes `push_trap` an O(1)
///   slice rather than a second walk: a pre-order traversal always lays a subtree out as one
///   contiguous run.
/// - `dispatch` does **not** walk the tree itself. A container `Component` already owns its
///   children as typed struct fields (the same reason `render` recurses per the trait docs above,
///   not through a generic id lookup), so bubbling is ordinary recursive function calls: a
///   container's own `handle_event` checks `ctx.focus` against each child's id, offers the event
///   to whichever one is focused first, and only runs its own handling once that returns
///   `Propagate` (see `screens::Dashboard`'s `handle_event` for the pattern). `FocusTree`'s part is
///   supplying the `ctx.focus` snapshot (`state()`) those checks read; `dispatch` itself is a thin
///   `root.handle_event(event, ctx)`. This is also why `dispatch` takes `&mut dyn Component`, not
///   `&mut dyn ComponentParent` like `rebuild`/`keybindings` — it never needs to resolve an id back
///   to a component, only the root the runtime already holds directly.
/// - Mouse hit testing is separate from focus: `record_rect` remembers where each component
///   rendered this frame (last call for a point wins, so an overlay recorded after its backdrop is
///   found first) and `hit_test` answers "what's at (column, row)". Wiring `record_rect` into an
///   actual render pass needs `Component::render` to report its own area, which is a frozen-
///   contract change coordinated elsewhere (see the crate root docs) — this type only owns the
///   bookkeeping once that call happens.
#[derive(Debug, Default)]
pub struct FocusTree {
    order: Vec<ComponentId>,
    current: usize,
    parents: HashMap<ComponentId, ComponentId>,
    spans: HashMap<ComponentId, (usize, usize)>,
    trap_stack: Vec<(usize, usize)>,
    rects: Vec<(ComponentId, Rect)>,
}

impl FocusTree {
    /// An empty tree with nothing focused yet.
    pub fn new() -> Self {
        FocusTree::default()
    }

    /// Recompute tab order, parent links, and subtree spans from `root`'s `focusable_children()`,
    /// recursively. Drops any active focus traps (`push_trap`): a rebuilt tree may not contain the
    /// same ids at the same spans, so a stale trap range could silently confine focus to the wrong
    /// components.
    pub fn rebuild(&mut self, root: &dyn ComponentParent) {
        self.order.clear();
        self.parents.clear();
        self.spans.clear();
        self.trap_stack.clear();
        self.current = 0;
        for child in root.focusable_children() {
            Self::visit(
                root,
                child,
                root.id(),
                &mut self.order,
                &mut self.parents,
                &mut self.spans,
            );
        }
    }

    /// Pre-order visit of `id` and its descendants, per the struct-level docs.
    fn visit(
        root: &dyn ComponentParent,
        id: ComponentId,
        parent: ComponentId,
        order: &mut Vec<ComponentId>,
        parents: &mut HashMap<ComponentId, ComponentId>,
        spans: &mut HashMap<ComponentId, (usize, usize)>,
    ) {
        let start = order.len();
        order.push(id);
        parents.insert(id, parent);

        let children = if id == root.id() {
            root.focusable_children()
        } else {
            root.resolve(id)
                .map(Component::focusable_children)
                .unwrap_or_default()
        };
        for child in children {
            Self::visit(root, child, id, order, parents, spans);
        }

        spans.insert(id, (start, order.len()));
    }

    /// The currently focused component, if the tree has any focusable components.
    pub fn focused(&self) -> Option<ComponentId> {
        self.order.get(self.current).copied()
    }

    /// A [`FocusState`] snapshot suitable for this frame's [`FrameContext`].
    pub fn state(&self) -> FocusState {
        FocusState::new(self.focused())
    }

    /// The `order` range tab traversal is currently confined to: the innermost active trap
    /// (`push_trap`), or the whole tree when none is active.
    fn active_range(&self) -> (usize, usize) {
        self.trap_stack
            .last()
            .copied()
            .unwrap_or((0, self.order.len()))
    }

    /// Move focus to the next component in tab order, wrapping around within the innermost
    /// active focus trap (`push_trap`), or the whole tree when there is none.
    pub fn focus_next(&mut self) {
        let (start, end) = self.active_range();
        if end > start {
            let len = end - start;
            let offset = self.current.saturating_sub(start);
            self.current = start + (offset + 1) % len;
        }
    }

    /// Move focus to the previous component in tab order, wrapping around within the innermost
    /// active focus trap (`push_trap`), or the whole tree when there is none.
    pub fn focus_prev(&mut self) {
        let (start, end) = self.active_range();
        if end > start {
            let len = end - start;
            let offset = self.current.saturating_sub(start);
            self.current = start + (offset + len - 1) % len;
        }
    }

    /// Trap tab traversal inside `modal_id`'s own subtree (as recorded by the last `rebuild`)
    /// until the matching `pop_trap`, and move focus to the first component inside it — the
    /// standard "open a modal, focus lands inside it, tab cannot escape it" behaviour.
    ///
    /// `modal_id` must have been visited by `rebuild` (reachable via some ancestor's
    /// `focusable_children()`, directly or transitively) for the trap to confine anything; an
    /// unknown id traps focus into an empty range, making `focus_next`/`focus_prev` no-ops until
    /// `pop_trap` — the safe failure mode for a modal that turned out not to be focusable itself.
    pub fn push_trap(&mut self, modal_id: ComponentId) {
        let span = self.spans.get(&modal_id).copied().unwrap_or((0, 0));
        self.trap_stack.push(span);
        if span.0 < span.1 {
            self.current = span.0;
        }
    }

    /// Release the innermost focus trap pushed by `push_trap`, restoring the enclosing scope's
    /// tab order (the whole tree, or the next trap out for nested modals).
    pub fn pop_trap(&mut self) {
        self.trap_stack.pop();
    }

    /// Deliver `event` to `root`, letting it bubble from whichever component `ctx.focus` names as
    /// focused up to `root` itself, per the struct-level docs: `root`'s own `handle_event` (and,
    /// transitively, each container it forwards to) is where the actual walk and the
    /// consume-or-propagate decision at each level happen. `ctx.focus` should come from
    /// `self.state()` for this to route anywhere below the root.
    pub fn dispatch(
        &mut self,
        root: &mut dyn Component,
        event: &Event,
        ctx: &FrameContext<'_>,
    ) -> Propagation {
        root.handle_event(event, ctx)
    }

    /// The keybindings visible from the currently focused component up to the root, most specific
    /// first — the list a help overlay or command palette shows. When two components in the chain
    /// declare the same chord, the more specific (closer to focus) one wins and the root's is
    /// dropped, which is what "per-component override" means in practice: a focused text input
    /// binding "q" shadows a screen-level "q" quit binding without either side having to know
    /// about the other.
    pub fn keybindings(
        &self,
        root: &dyn ComponentParent,
        ctx: &FrameContext<'_>,
    ) -> Vec<KeyBinding> {
        let root_id = root.id();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut current = self.focused();

        loop {
            let id = current.unwrap_or(root_id);
            let bindings = if id == root_id {
                root.keybindings(ctx)
            } else if let Some(component) = root.resolve(id) {
                component.keybindings(ctx)
            } else {
                Vec::new()
            };
            for binding in bindings {
                if seen.insert(binding.chord) {
                    out.push(binding);
                }
            }
            if id == root_id {
                return out;
            }
            current = Some(self.parents.get(&id).copied().unwrap_or(root_id));
        }
    }

    /// Forget every rect recorded by `record_rect`. Call once per frame before re-rendering: a
    /// component that stopped rendering (collapsed panel, closed modal) must stop being hit-
    /// testable, and a resize changes every rect anyway.
    pub fn begin_frame(&mut self) {
        self.rects.clear();
    }

    /// Record that `id` rendered into `rect` this frame. Call order is z-order: a later call
    /// (e.g. an overlay drawn after its backdrop) is preferred by `hit_test` on overlap.
    pub fn record_rect(&mut self, id: ComponentId, rect: Rect) {
        self.rects.push((id, rect));
    }

    /// The topmost component whose last-recorded rect contains `(column, row)`, if any — how a
    /// click, drag, or scroll routes to a component: hit-test first, then feed the same point's
    /// owner to `dispatch` (typically after moving focus there for a click).
    pub fn hit_test(&self, column: u16, row: u16) -> Option<ComponentId> {
        let point = Position { x: column, y: row };
        self.rects
            .iter()
            .rev()
            .find(|(_, rect)| rect.contains(point))
            .map(|(id, _)| *id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{Capabilities, ColorSupport, UnicodeSupport};
    use crate::event::{InputEvent, KeyChord};
    use crate::theme::Theme;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::cell::Cell;
    use tm_types::FixedClock;

    const CHILD_A: ComponentId = ComponentId::new("test.child_a");
    const CHILD_B: ComponentId = ComponentId::new("test.child_b");
    const ROOT: ComponentId = ComponentId::new("test.root");

    #[derive(Debug)]
    struct Leaf {
        id: ComponentId,
    }

    impl Component for Leaf {
        fn id(&self) -> ComponentId {
            self.id
        }

        fn render(&self, _area: Rect, _buf: &mut Buffer, _ctx: &FrameContext<'_>) {}
    }

    /// A leaf that records how many times it was offered an event and either consumes or
    /// propagates every one, for asserting on `FocusTree::dispatch`'s bubbling order.
    #[derive(Debug)]
    struct RecordingLeaf {
        id: ComponentId,
        consume: bool,
        calls: Cell<u32>,
        bindings: Vec<KeyBinding>,
    }

    impl RecordingLeaf {
        fn new(id: ComponentId, consume: bool) -> Self {
            RecordingLeaf {
                id,
                consume,
                calls: Cell::new(0),
                bindings: Vec::new(),
            }
        }
    }

    impl Component for RecordingLeaf {
        fn id(&self) -> ComponentId {
            self.id
        }

        fn render(&self, _area: Rect, _buf: &mut Buffer, _ctx: &FrameContext<'_>) {}

        fn handle_event(&mut self, _event: &Event, _ctx: &FrameContext<'_>) -> Propagation {
            self.calls.set(self.calls.get() + 1);
            if self.consume {
                Propagation::Consumed
            } else {
                Propagation::Propagate
            }
        }

        fn keybindings(&self, _ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
            self.bindings.clone()
        }
    }

    /// A container of [`RecordingLeaf`]s, standing in for a screen: `handle_event` forwards to
    /// whichever child `ctx.focus` names first (the same pattern `screens::Dashboard`'s IMPL note
    /// describes), and it resolves its children by id via [`ComponentParent`] for `rebuild`.
    #[derive(Debug)]
    struct Container {
        id: ComponentId,
        consume: bool,
        calls: Cell<u32>,
        bindings: Vec<KeyBinding>,
        children: Vec<RecordingLeaf>,
    }

    impl Container {
        fn new(id: ComponentId, consume: bool, children: Vec<RecordingLeaf>) -> Self {
            Container {
                id,
                consume,
                calls: Cell::new(0),
                bindings: Vec::new(),
                children,
            }
        }
    }

    impl Component for Container {
        fn id(&self) -> ComponentId {
            self.id
        }

        fn render(&self, _area: Rect, _buf: &mut Buffer, _ctx: &FrameContext<'_>) {}

        fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
            for child in &mut self.children {
                if ctx.focus.is_focused(child.id()) && child.handle_event(event, ctx).is_consumed()
                {
                    return Propagation::Consumed;
                }
            }
            self.calls.set(self.calls.get() + 1);
            if self.consume {
                Propagation::Consumed
            } else {
                Propagation::Propagate
            }
        }

        fn keybindings(&self, _ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
            self.bindings.clone()
        }

        fn focusable_children(&self) -> Vec<ComponentId> {
            self.children.iter().map(RecordingLeaf::id).collect()
        }
    }

    impl ComponentParent for Container {
        fn resolve(&self, id: ComponentId) -> Option<&dyn Component> {
            if id == self.id {
                return Some(self);
            }
            self.children
                .iter()
                .find(|child| child.id == id)
                .map(|child| child as &dyn Component)
        }

        fn resolve_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component> {
            if id == self.id {
                return Some(self);
            }
            self.children
                .iter_mut()
                .find(|child| child.id == id)
                .map(|child| child as &mut dyn Component)
        }
    }

    fn test_ctx<'a>(
        theme: &'a Theme,
        caps: &'a Capabilities,
        clock: &'a FixedClock,
        focus: FocusState,
    ) -> FrameContext<'a> {
        FrameContext {
            theme,
            caps,
            clock,
            focus,
        }
    }

    fn key_event(event: crossterm::event::KeyEvent) -> Event {
        Event::Input(InputEvent::Key(event))
    }

    fn test_capabilities() -> Capabilities {
        Capabilities {
            color: ColorSupport::NoColor,
            unicode: UnicodeSupport::AsciiOnly,
            synchronized_output: false,
            mouse: false,
            kitty_keyboard: false,
            bracketed_paste: false,
        }
    }

    #[test]
    fn component_id_round_trips_its_path() {
        assert_eq!(CHILD_A.as_str(), "test.child_a");
        assert_eq!(CHILD_A.to_string(), "test.child_a");
        assert_ne!(CHILD_A, CHILD_B);
    }

    #[test]
    fn focus_state_reports_only_the_focused_id() {
        let state = FocusState::new(Some(CHILD_A));
        assert!(state.is_focused(CHILD_A));
        assert!(!state.is_focused(CHILD_B));
        assert_eq!(state.focused(), Some(CHILD_A));
    }

    #[test]
    fn focus_state_default_focuses_nothing() {
        let state = FocusState::default();
        assert_eq!(state.focused(), None);
        assert!(!state.is_focused(CHILD_A));
    }

    #[test]
    fn default_component_methods_are_inert() {
        let mut leaf = Leaf { id: CHILD_A };
        let theme = Theme::default();
        let caps = test_capabilities();
        let clock = FixedClock::epoch();
        let ctx = FrameContext {
            theme: &theme,
            caps: &caps,
            clock: &clock,
            focus: FocusState::default(),
        };

        let event = Event::Resize {
            width: 80,
            height: 24,
        };
        assert_eq!(leaf.handle_event(&event, &ctx), Propagation::Propagate);
        assert!(leaf.keybindings(&ctx).is_empty());
        assert!(leaf.focusable_children().is_empty());
        assert_eq!(leaf.id(), CHILD_A);
    }

    #[test]
    fn focus_tree_wraps_tab_order() {
        let mut tree = FocusTree::new();
        tree.order = vec![CHILD_A, CHILD_B];
        assert_eq!(tree.focused(), Some(CHILD_A));
        tree.focus_next();
        assert_eq!(tree.focused(), Some(CHILD_B));
        tree.focus_next();
        assert_eq!(tree.focused(), Some(CHILD_A), "tab order must wrap forward");
        tree.focus_prev();
        assert_eq!(
            tree.focused(),
            Some(CHILD_B),
            "tab order must wrap backward"
        );
    }

    #[test]
    fn focus_tree_with_no_components_focuses_nothing() {
        let tree = FocusTree::new();
        assert_eq!(tree.focused(), None);
        assert_eq!(tree.state(), FocusState::default());
    }

    #[test]
    fn rebuild_walks_focusable_children_in_pre_order_and_skips_the_root() {
        let root = Container::new(
            ROOT,
            false,
            vec![
                RecordingLeaf::new(CHILD_A, false),
                RecordingLeaf::new(CHILD_B, false),
            ],
        );
        let mut tree = FocusTree::new();
        tree.rebuild(&root);

        assert_eq!(tree.order, vec![CHILD_A, CHILD_B]);
        assert_eq!(
            tree.focused(),
            Some(CHILD_A),
            "tab order starts at the first child"
        );
    }

    #[test]
    fn dispatch_consumed_at_the_focused_leaf_never_reaches_the_root() {
        let mut root = Container::new(ROOT, true, vec![RecordingLeaf::new(CHILD_A, true)]);
        let mut tree = FocusTree::new();
        tree.rebuild(&root);

        let theme = Theme::default();
        let caps = test_capabilities();
        let clock = FixedClock::epoch();
        let ctx = test_ctx(&theme, &caps, &clock, tree.state());
        let event = key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

        let propagation = tree.dispatch(&mut root, &event, &ctx);

        assert_eq!(propagation, Propagation::Consumed);
        assert_eq!(root.children[0].calls.get(), 1);
        assert_eq!(
            root.calls.get(),
            0,
            "root must not see an event its child consumed"
        );
    }

    #[test]
    fn dispatch_bubbles_a_propagated_event_up_to_the_root() {
        let mut root = Container::new(ROOT, true, vec![RecordingLeaf::new(CHILD_A, false)]);
        let mut tree = FocusTree::new();
        tree.rebuild(&root);

        let theme = Theme::default();
        let caps = test_capabilities();
        let clock = FixedClock::epoch();
        let ctx = test_ctx(&theme, &caps, &clock, tree.state());
        let event = key_event(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));

        let propagation = tree.dispatch(&mut root, &event, &ctx);

        assert_eq!(propagation, Propagation::Consumed);
        assert_eq!(
            root.children[0].calls.get(),
            1,
            "the leaf must be offered the event first"
        );
        assert_eq!(
            root.calls.get(),
            1,
            "the root must see it after the leaf propagated"
        );
    }

    #[test]
    fn dispatch_propagates_past_the_root_when_nothing_consumes() {
        let mut root = Container::new(ROOT, false, vec![RecordingLeaf::new(CHILD_A, false)]);
        let mut tree = FocusTree::new();
        tree.rebuild(&root);

        let theme = Theme::default();
        let caps = test_capabilities();
        let clock = FixedClock::epoch();
        let ctx = test_ctx(&theme, &caps, &clock, tree.state());
        let event = key_event(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));

        let propagation = tree.dispatch(&mut root, &event, &ctx);

        assert_eq!(
            propagation,
            Propagation::Propagate,
            "an unhandled event is the runtime's cue to try a global fallback"
        );
    }

    #[test]
    fn push_trap_confines_tab_order_and_pop_trap_releases_it() {
        let child_c = ComponentId::new("test.child_c");
        let mut tree = FocusTree::new();
        tree.order = vec![CHILD_A, CHILD_B, child_c];
        tree.spans.insert(CHILD_B, (1, 2));

        tree.push_trap(CHILD_B);
        assert_eq!(
            tree.focused(),
            Some(CHILD_B),
            "opening a trap must focus inside it"
        );
        tree.focus_next();
        assert_eq!(
            tree.focused(),
            Some(CHILD_B),
            "a trap of one component must not escape to its siblings"
        );
        tree.focus_prev();
        assert_eq!(tree.focused(), Some(CHILD_B));

        tree.pop_trap();
        tree.focus_next();
        assert_eq!(
            tree.focused(),
            Some(child_c),
            "after the trap is released, tab order covers the whole tree again"
        );
    }

    #[test]
    fn push_trap_with_an_unknown_id_is_a_safe_no_op() {
        let mut tree = FocusTree::new();
        tree.order = vec![CHILD_A, CHILD_B];

        tree.push_trap(ComponentId::new("test.never_visited"));
        let before = tree.focused();
        tree.focus_next();
        assert_eq!(
            tree.focused(),
            before,
            "an empty trap range must not move focus anywhere"
        );
    }

    #[test]
    fn keybindings_prefers_the_focused_component_over_the_root_on_a_shared_chord() {
        let shared = KeyChord::plain(KeyCode::Char('q'));
        let mut root = Container::new(ROOT, false, vec![RecordingLeaf::new(CHILD_A, false)]);
        root.bindings = vec![KeyBinding::new(shared, "root: quit")];
        root.children[0].bindings = vec![
            KeyBinding::new(shared, "leaf: shadow root's quit"),
            KeyBinding::new(KeyChord::plain(KeyCode::Char('d')), "leaf: delete"),
        ];

        let mut tree = FocusTree::new();
        tree.rebuild(&root);

        let theme = Theme::default();
        let caps = test_capabilities();
        let clock = FixedClock::epoch();
        let ctx = test_ctx(&theme, &caps, &clock, tree.state());

        let bindings = tree.keybindings(&root, &ctx);
        let descriptions: Vec<&str> = bindings.iter().map(|b| b.description).collect();

        assert_eq!(
            descriptions,
            vec!["leaf: shadow root's quit", "leaf: delete"],
            "the focused leaf's binding must win on a shared chord, and the root's distinct \
             binding must not appear once its chord is already claimed"
        );
    }

    #[test]
    fn hit_test_prefers_the_most_recently_recorded_rect_on_overlap() {
        let mut tree = FocusTree::new();
        tree.begin_frame();
        tree.record_rect(CHILD_A, Rect::new(0, 0, 10, 10));
        tree.record_rect(CHILD_B, Rect::new(5, 5, 10, 10));

        assert_eq!(
            tree.hit_test(2, 2),
            Some(CHILD_A),
            "only child_a's rect covers this point"
        );
        assert_eq!(
            tree.hit_test(6, 6),
            Some(CHILD_B),
            "on overlap, the later-recorded (topmost) rect wins"
        );
        assert_eq!(tree.hit_test(50, 50), None, "outside every rect");
    }

    #[test]
    fn begin_frame_forgets_rects_from_a_previous_frame() {
        let mut tree = FocusTree::new();
        tree.record_rect(CHILD_A, Rect::new(0, 0, 10, 10));
        tree.begin_frame();
        assert_eq!(
            tree.hit_test(1, 1),
            None,
            "a stale rect must not survive begin_frame"
        );
    }
}
