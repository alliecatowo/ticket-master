+++
[doc]
id = "wiki/architecture/tm-tui"
mode = "generated"
derived_from = ["crates/tm-tui/src/**"]
+++

# Architecture: tm-tui

## Module tree

- `crates/tm-tui/src/caps.rs`
- `crates/tm-tui/src/component.rs`
- `crates/tm-tui/src/event.rs`
- `crates/tm-tui/src/lib.rs`
- `crates/tm-tui/src/runtime.rs`
- `crates/tm-tui/src/screens/command_palette.rs`
- `crates/tm-tui/src/screens/dashboard.rs`
- `crates/tm-tui/src/screens/diff_viewer.rs`
- `crates/tm-tui/src/screens/home.rs`
- `crates/tm-tui/src/screens/kanban.rs`
- `crates/tm-tui/src/screens/mod.rs`
- `crates/tm-tui/src/screens/session_stream.rs`
- `crates/tm-tui/src/screens/ticket_detail.rs`
- `crates/tm-tui/src/screens/ticket_graph.rs`
- `crates/tm-tui/src/screens/verification_ladder.rs`
- `crates/tm-tui/src/testing.rs`
- `crates/tm-tui/src/text.rs`
- `crates/tm-tui/src/theme.rs`
- `crates/tm-tui/src/widgets_data/form.rs`
- `crates/tm-tui/src/widgets_data/list.rs`
- `crates/tm-tui/src/widgets_data/mod.rs`
- `crates/tm-tui/src/widgets_data/table.rs`
- `crates/tm-tui/src/widgets_data/tree.rs`
- `crates/tm-tui/src/widgets_viz/diff.rs`
- `crates/tm-tui/src/widgets_viz/graph.rs`
- `crates/tm-tui/src/widgets_viz/mod.rs`
- `crates/tm-tui/src/widgets_viz/stream.rs`

## Public symbols

### `crates/tm-tui/src/caps.rs`

- `pub enum ColorSupport`
- `pub enum UnicodeSupport`
- `pub struct Capabilities`
- `impl Capabilities`
  - `pub fn minimal() -> Self`
- `pub type Environment = HashMap<String, String>;`
- `pub struct ProbeReply`
- `pub const PROBE_REQUEST: &[u8] = b"\x1b]11;?\x07\x1b[?u\x1b[c";`
- `pub const PROBE_TIMEOUT: Duration = Duration::from_millis(200);`
- `pub async fn probe<R, W>(reader: &mut R, writer: &mut W, timeout: Duration) -> ProbeReply
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,`
- `pub fn parse_probe_reply(bytes: &[u8]) -> ProbeReply`
- `pub fn detect(env: &Environment, synchronized_output: bool, mouse: bool) -> Capabilities`
- `pub fn detect_with_probe(
    env: &Environment,
    probe: ProbeReply,
    synchronized_output: bool,
    mouse: bool,
) -> Capabilities`
- `pub fn probe_synchronized_output(env: &Environment) -> bool`
- `pub fn degrade_color(caps: &Capabilities, color: Color) -> Color`

### `crates/tm-tui/src/component.rs`

- `pub struct ComponentId`
- `impl ComponentId`
  - `pub const fn new(path: &'static str) -> Self`
  - `pub fn as_str(&self) -> &'static str`
- `pub struct FocusState`
- `impl FocusState`
  - `pub fn new(focused: Option<ComponentId>) -> Self`
  - `pub fn focused(&self) -> Option<ComponentId>`
  - `pub fn is_focused(&self, id: ComponentId) -> bool`
- `pub struct FrameContext<'a>`
- `pub trait Component: std::fmt::Debug`
- `pub trait ComponentParent: Component`
- `pub struct FocusTree`
- `impl FocusTree`
  - `pub fn new() -> Self`
  - `pub fn rebuild(&mut self, root: &dyn ComponentParent)`
  - `pub fn focused(&self) -> Option<ComponentId>`
  - `pub fn state(&self) -> FocusState`
  - `pub fn focus_next(&mut self)`
  - `pub fn focus_prev(&mut self)`
  - `pub fn push_trap(&mut self, modal_id: ComponentId)`
  - `pub fn pop_trap(&mut self)`
  - `pub fn dispatch(
        &mut self,
        root: &mut dyn Component,
        event: &Event,
        ctx: &FrameContext<'_>,
    ) -> Propagation`
  - `pub fn keybindings(
        &self,
        root: &dyn ComponentParent,
        ctx: &FrameContext<'_>,
    ) -> Vec<KeyBinding>`
  - `pub fn begin_frame(&mut self)`
  - `pub fn record_rect(&mut self, id: ComponentId, rect: Rect)`
  - `pub fn hit_test(&self, column: u16, row: u16) -> Option<ComponentId>`

### `crates/tm-tui/src/event.rs`

- `pub enum InputEvent`
- `pub struct KeyChord`
- `impl KeyChord`
  - `pub fn plain(code: crossterm::event::KeyCode) -> Self`
  - `pub fn matches(&self, event: &crossterm::event::KeyEvent) -> bool`
- `pub struct KeyBinding`
- `impl KeyBinding`
  - `pub fn new(chord: KeyChord, description: &'static str) -> Self`
- `pub enum ToastLevel`
- `pub enum AppMessage`
- `pub enum Event`
- `pub enum Propagation`
- `impl Propagation`
  - `pub fn is_consumed(self) -> bool`
  - `pub fn or_else(self, next: impl FnOnce() -> Propagation) -> Propagation`

### `crates/tm-tui/src/lib.rs`

- `pub mod caps;`
- `pub mod component;`
- `pub mod event;`
- `pub mod runtime;`
- `pub mod screens;`
- `pub mod testing;`
- `pub mod text;`
- `pub mod theme;`
- `pub mod widgets_data;`
- `pub mod widgets_viz;`

### `crates/tm-tui/src/runtime.rs`

- `pub enum RuntimeError`
- `pub struct MessageSender`
- `impl MessageSender`
  - `pub fn send(&self, message: AppMessage)`
- `pub struct Runtime`
- `impl Runtime`
  - `pub fn capabilities(&self) -> &Capabilities`
  - `pub fn theme(&self) -> &Theme`
  - `pub fn shutdown_handle(&self) -> Arc<Notify>`
  - `pub async fn start(
        clock: Arc<dyn Clock>,
        theme: Theme,
    ) -> Result<(Runtime, MessageSender), RuntimeError>`
  - `pub async fn run(&mut self, root: &mut dyn ComponentParent) -> Result<(), RuntimeError>`
- `pub async fn install_signal_handlers(
    shutdown: Arc<Notify>,
    resume: Arc<Notify>,
    mouse_enabled: bool,
) -> Result<(), RuntimeError>`

### `crates/tm-tui/src/screens/command_palette.rs`

- `pub struct Action`
- `impl Action`
  - `pub const fn new(label: &'static str, id: &'static str) -> Self`
- `pub struct CommandPaletteScreen`
- `impl CommandPaletteScreen`
  - `pub fn new(id: ComponentId, actions: Vec<Action>) -> Self`
  - `pub fn filter(&mut self)`
  - `pub fn take_resolved(&mut self) -> Option<&'static str>`
  - `pub fn take_closed(&mut self) -> bool`

### `crates/tm-tui/src/screens/dashboard.rs`

- `pub struct Dashboard`
- `impl Dashboard`
  - `pub fn new(id: ComponentId, tickets: Table, sessions: List) -> Self`
  - `pub fn set_goal(&mut self, goal: Option<String>)`

### `crates/tm-tui/src/screens/diff_viewer.rs`

- `pub struct DiffViewerScreen`
- `impl DiffViewerScreen`
  - `pub fn new(id: ComponentId, paths: List, diff: Diff) -> Self`

### `crates/tm-tui/src/screens/home.rs`

- `pub struct Home`
- `impl Home`
  - `pub fn new(id: ComponentId, dashboard: Dashboard, session: SessionId) -> Self`
  - `pub fn set_status_line(&mut self, line: Option<String>)`
  - `pub fn set_dashboard(&mut self, dashboard: Dashboard)`
  - `pub fn take_submission(&mut self) -> Option<String>`
  - `pub fn is_turn_running(&self) -> bool`

### `crates/tm-tui/src/screens/kanban.rs`

- `pub struct KanbanCard`
- `impl KanbanCard`
  - `pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self`
- `pub struct KanbanColumn`
- `impl KanbanColumn`
  - `pub fn new(title: impl Into<String>, cards: Vec<KanbanCard>) -> Self`
- `pub struct Kanban`
- `impl Kanban`
  - `pub fn new(id: ComponentId, columns: Vec<KanbanColumn>) -> Self`
  - `pub fn set_columns(&mut self, columns: Vec<KanbanColumn>)`
  - `pub fn selected_column(&self) -> usize`
  - `pub fn selected_card_id(&self) -> Option<&str>`
  - `pub fn take_activation(&mut self) -> Option<String>`

### `crates/tm-tui/src/screens/mod.rs`

- `pub mod command_palette;`
- `pub mod dashboard;`
- `pub mod diff_viewer;`
- `pub mod home;`
- `pub mod kanban;`
- `pub mod session_stream;`
- `pub mod ticket_detail;`
- `pub mod ticket_graph;`
- `pub mod verification_ladder;`

### `crates/tm-tui/src/screens/session_stream.rs`

- `pub struct SessionStreamScreen`
- `impl SessionStreamScreen`
  - `pub fn new(id: ComponentId, session: SessionId, pane: StreamPane) -> Self`
  - `pub fn session(&self) -> &SessionId`

### `crates/tm-tui/src/screens/ticket_detail.rs`

- `pub struct TicketDetailScreen`
- `impl TicketDetailScreen`
  - `pub fn new(id: ComponentId, ticket: TicketId, fields: Form, activity: List) -> Self`
  - `pub fn ticket(&self) -> &TicketId`

### `crates/tm-tui/src/screens/ticket_graph.rs`

- `pub struct TicketGraphScreen`
- `impl TicketGraphScreen`
  - `pub fn new(id: ComponentId, graph: Graph) -> Self`

### `crates/tm-tui/src/screens/verification_ladder.rs`

- `pub struct VerificationLadderScreen`
- `impl VerificationLadderScreen`
  - `pub fn new(id: ComponentId, ticket: TicketId, steps: Tree) -> Self`
  - `pub fn ticket(&self) -> &TicketId`

### `crates/tm-tui/src/testing.rs`

- `pub struct Harness`
- `impl Harness`
  - `pub fn new(width: u16, height: u16) -> Self`
  - `pub fn render_lines(
        &mut self,
        component: &dyn Component,
        ctx: &FrameContext<'_>,
    ) -> Vec<String>`
  - `pub fn render_styled(
        &mut self,
        component: &dyn Component,
        ctx: &FrameContext<'_>,
    ) -> Vec<Vec<(String, ratatui_core::style::Style)>>`
  - `pub fn send_and_render(
        &mut self,
        component: &mut dyn Component,
        event: &Event,
        ctx: &FrameContext<'_>,
    ) -> Vec<String>`
  - `pub fn send_and_render_with_propagation(
        &mut self,
        component: &mut dyn Component,
        event: &Event,
        ctx: &FrameContext<'_>,
    ) -> (crate::event::Propagation, Vec<String>)`

### `crates/tm-tui/src/text.rs`

- `pub struct Grapheme<'a>`
- `pub fn graphemes(s: &str) -> Vec<Grapheme<'_>>`
- `pub fn display_width(s: &str) -> usize`
- `pub fn wrap(s: &str, max_width: usize) -> Vec<String>`
- `pub fn truncate(s: &str, max_width: usize, ellipsis: &str) -> String`
- `pub fn scroll(s: &str, offset: usize, max_width: usize) -> String`
- `pub struct StyledSpan<'a>`
- `impl<'a> StyledSpan<'a>`
  - `pub fn new(text: &'a str, style: ratatui_core::style::Style) -> Self`
- `pub fn wrap_spans<'a>(spans: &[StyledSpan<'a>], max_width: usize) -> Vec<Vec<StyledSpan<'a>>>`

### `crates/tm-tui/src/theme.rs`

- `pub struct Theme`
- `impl Theme`
  - `pub fn dark() -> Self`
  - `pub fn light() -> Self`
  - `pub fn degraded(&self, caps: &Capabilities) -> Theme`
- `pub struct Slots`
- `impl Slots`
  - `pub fn get(&self, name: &str) -> Rect`
- `pub fn split_vertical(area: Rect, names: &[(&'static str, u32)]) -> Slots`
- `pub fn split_horizontal(area: Rect, names: &[(&'static str, u32)]) -> Slots`
- `pub enum Easing`
- `pub struct Animation`
- `impl Animation`
  - `pub fn new(from: f64, to: f64, started_at: Timestamp, duration: Duration) -> Self`
  - `pub fn with_easing(self, easing: Easing) -> Self`
  - `pub fn progress(&self, now: Timestamp) -> f64`
  - `pub fn value_at(&self, now: Timestamp) -> f64`
  - `pub fn is_finished(&self, now: Timestamp) -> bool`
- `pub struct MotionPolicy`
- `impl MotionPolicy`
  - `pub fn enabled() -> Self`
  - `pub fn motion_enabled(&self) -> bool`
- `impl Animation`
  - `pub fn settle(&self, now: Timestamp, policy: MotionPolicy) -> f64`
- `pub fn blend(from: Color, to: Color, t: f64) -> Color`
- `pub fn slide_rect(from: Rect, to: Rect, t: f64) -> Rect`
- `pub struct Spinner`
- `impl Spinner`
  - `pub const BRAILLE: &'static [&'static str] =
        &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];`
  - `pub fn frame(
        &self,
        started_at: Timestamp,
        now: Timestamp,
        policy: MotionPolicy,
    ) -> &'static str`
- `pub fn pad(area: Rect, horizontal: u16, vertical: u16) -> Rect`
- `pub enum Track`
- `pub fn flex_vertical(area: Rect, spacing: u16, tracks: &[(&'static str, Track)]) -> Slots`
- `pub fn flex_horizontal(area: Rect, spacing: u16, tracks: &[(&'static str, Track)]) -> Slots`

### `crates/tm-tui/src/widgets_data/form.rs`

- `pub enum FieldKind`
- `pub struct Field`
- `impl Field`
  - `pub fn text(label: impl Into<String>) -> Self`
  - `pub fn read_only(label: impl Into<String>, value: impl Into<String>) -> Self`
- `pub struct Form`
- `impl Form`
  - `pub fn new(id: ComponentId, fields: Vec<Field>) -> Self`
  - `pub fn values(&self) -> Vec<&str>`

### `crates/tm-tui/src/widgets_data/list.rs`

- `pub struct List`
- `impl List`
  - `pub fn new(id: ComponentId) -> Self`
  - `pub fn set_items(&mut self, items: Vec<String>)`
  - `pub fn selected(&self) -> Option<&str>`

### `crates/tm-tui/src/widgets_data/mod.rs`

- `pub mod form;`
- `pub mod list;`
- `pub mod table;`
- `pub mod tree;`

### `crates/tm-tui/src/widgets_data/table.rs`

- `pub struct Column`
- `impl Column`
  - `pub fn new(title: impl Into<String>, weight: u16) -> Self`
- `pub struct Table`
- `impl Table`
  - `pub fn new(id: ComponentId, columns: Vec<Column>) -> Self`
  - `pub fn set_rows(&mut self, rows: Vec<Vec<String>>)`
  - `pub fn selected(&self) -> Option<usize>`

### `crates/tm-tui/src/widgets_data/tree.rs`

- `pub struct Node`
- `impl Node`
  - `pub fn leaf(label: impl Into<String>) -> Self`
  - `pub fn with_children(label: impl Into<String>, children: Vec<Node>) -> Self`
- `pub struct Tree`
- `impl Tree`
  - `pub fn new(id: ComponentId) -> Self`
  - `pub fn set_roots(&mut self, roots: Vec<Node>)`

### `crates/tm-tui/src/widgets_viz/diff.rs`

- `pub enum DiffLineKind`
- `pub struct DiffLine`
- `pub struct Diff`
- `impl Diff`
  - `pub fn new(id: ComponentId, path: impl Into<String>) -> Self`
  - `pub fn path(&self) -> &str`
  - `pub fn set_diff(&mut self, path: impl Into<String>, lines: Vec<DiffLine>)`

### `crates/tm-tui/src/widgets_viz/graph.rs`

- `pub enum NodeState`
- `pub struct GraphNode`
- `impl GraphNode`
  - `pub fn new(id: TicketId, label: impl Into<String>, state: NodeState) -> Self`
- `pub struct GraphEdge`
- `pub struct Graph`
- `impl Graph`
  - `pub fn new(id: ComponentId) -> Self`
  - `pub fn set_graph(&mut self, mut nodes: Vec<GraphNode>, edges: Vec<GraphEdge>)`
  - `pub fn selected(&self) -> Option<&TicketId>`

### `crates/tm-tui/src/widgets_viz/mod.rs`

- `pub mod diff;`
- `pub mod graph;`
- `pub mod stream;`

### `crates/tm-tui/src/widgets_viz/stream.rs`

- `pub struct StreamPane`
- `impl StreamPane`
  - `pub fn new(id: ComponentId, max_lines: usize) -> Self`
  - `pub fn push_chunk(&mut self, chunk: &str)`
  - `pub fn len(&self) -> usize`
  - `pub fn is_empty(&self) -> bool`
  - `pub fn is_following(&self) -> bool`
