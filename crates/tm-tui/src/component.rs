//! The component tree: the `Component` trait, focus, hit testing and event bubbling.
//!
//! This file has two halves with different guarantees.
//!
//! The **contract** — [`Component`], [`ComponentId`], [`FrameContext`], [`FocusState`] — is one
//! of the two finished, tested pieces every other file in this crate codes against (see the
//! crate root docs and `event.rs`). Its shape is settled; do not change these signatures without
//! updating every implementor.
//!
//! The **tree machinery** below it — [`FocusTree`] — is a stub, todo!()-bodied, and is one of the
//! nine parts scaffolded for parallel implementation. It is the part of ratatui's missing
//! "retained widget tree, focus management, event bubbling and hit testing" (D-002) that this
//! crate exists to build.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
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

/// Computes and owns tab order and focus transitions across a [`Component`] tree, and dispatches
/// an [`Event`] by bubbling it from the focused leaf up to the root.
///
/// This is the stub half of this file (see the module docs): the contract above is finished, but
/// the tree walk itself is todo!()-bodied for the implementing agent.
///
/// IMPL notes for the whole type:
/// - Tab order is computed by a pre-order walk calling `focusable_children()` at each level;
///   there is no reflection/downcasting available (`Component` is not `Any`), so `FocusTree` must
///   ask each component for its children by id and separately ask the tree owner (whoever holds
///   the actual child components, e.g. a screen's struct fields) to resolve an id to a
///   `&dyn Component` / `&mut dyn Component`. The cleanest shape is likely a second trait,
///   `ComponentParent`, that screens implement to hand back children by id — add it here if so;
///   do not smuggle a lookup table into this struct that duplicates state the screen already
///   owns.
/// - `dispatch` bubbles: start at `self.focused()`, call `handle_event`, and on `Propagate` move
///   to that component's parent (which requires the walk to have recorded parent links, not just
///   a flat focus order) until `Consumed` or the root is reached and still propagates (meaning no
///   one handled it — the runtime may have a final fallback, e.g. a global quit key).
/// - Mouse events additionally need hit testing: given a `(column, row)` and the `Rect` each
///   component rendered into last frame, find the deepest component containing the point. This
///   requires `FocusTree` to remember last-rendered rects per id (populate this from `render`,
///   e.g. by having `Component::render` take a `&mut FocusTree` to report its own `area` — that
///   is a contract change and must be coordinated rather than done unilaterally, since it would
///   touch the frozen `Component::render` signature above).
#[derive(Debug, Default)]
pub struct FocusTree {
    order: Vec<ComponentId>,
    current: usize,
}

impl FocusTree {
    /// An empty tree with nothing focused yet.
    pub fn new() -> Self {
        FocusTree::default()
    }

    /// Recompute tab order from `root`'s `focusable_children()`, recursively.
    ///
    /// IMPL: pre-order walk; see the struct-level note about resolving a child id back to a
    /// component to recurse into.
    pub fn rebuild(&mut self, root: &dyn Component) {
        let _ = root;
        todo!("recompute `self.order` by walking `root.focusable_children()` per the struct docs")
    }

    /// The currently focused component, if the tree has any focusable components.
    pub fn focused(&self) -> Option<ComponentId> {
        self.order.get(self.current).copied()
    }

    /// A [`FocusState`] snapshot suitable for this frame's [`FrameContext`].
    pub fn state(&self) -> FocusState {
        FocusState::new(self.focused())
    }

    /// Move focus to the next component in tab order, wrapping around.
    pub fn focus_next(&mut self) {
        if !self.order.is_empty() {
            self.current = (self.current + 1) % self.order.len();
        }
    }

    /// Move focus to the previous component in tab order, wrapping around.
    pub fn focus_prev(&mut self) {
        if !self.order.is_empty() {
            self.current = (self.current + self.order.len() - 1) % self.order.len();
        }
    }

    /// Bubble `event` from the focused leaf up to the root, per the struct-level IMPL note.
    pub fn dispatch(&mut self, root: &mut dyn Component, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        let _ = (root, event, ctx);
        todo!("bubble `event` from the focused leaf to the root per the struct docs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{Capabilities, ColorSupport, UnicodeSupport};
    use crate::theme::Theme;
    use tm_types::FixedClock;

    const CHILD_A: ComponentId = ComponentId::new("test.child_a");
    const CHILD_B: ComponentId = ComponentId::new("test.child_b");

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

    fn test_capabilities() -> Capabilities {
        Capabilities {
            color: ColorSupport::NoColor,
            unicode: UnicodeSupport::AsciiOnly,
            synchronized_output: false,
            mouse: false,
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

        let event = Event::Resize { width: 80, height: 24 };
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
        assert_eq!(tree.focused(), Some(CHILD_B), "tab order must wrap backward");
    }

    #[test]
    fn focus_tree_with_no_components_focuses_nothing() {
        let tree = FocusTree::new();
        assert_eq!(tree.focused(), None);
        assert_eq!(tree.state(), FocusState::default());
    }
}
