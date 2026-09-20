+++
[doc]
id = "wiki/architecture/tm-computer"
mode = "generated"
derived_from = ["crates/tm-computer/src/**"]
+++

# Architecture: tm-computer

## Module tree

- `crates/tm-computer/src/backend.rs`
- `crates/tm-computer/src/capability.rs`
- `crates/tm-computer/src/input.rs`
- `crates/tm-computer/src/lib.rs`
- `crates/tm-computer/src/linux.rs`
- `crates/tm-computer/src/macos.rs`
- `crates/tm-computer/src/session.rs`

## Public symbols

### `crates/tm-computer/src/backend.rs`

- `pub enum BackendKind`
- `impl BackendKind`
  - `pub fn as_str(&self) -> &'static str`
  - `pub fn parse(s: &str) -> Option<BackendKind>`
- `pub struct SelectionEnv`
- `impl SelectionEnv`
  - `pub fn from_process() -> SelectionEnv`
- `pub fn select_backend(env: &SelectionEnv) -> Result<BackendKind, ComputerError>`
- `pub fn open_selected(env: &SelectionEnv) -> TmResult<Box<dyn Backend>>`
- `pub struct Capabilities`
- `pub struct DisplayInfo`
- `pub struct WindowInfo`
- `pub struct ElementNode`
- `pub struct Screenshot`
- `pub trait Backend: Send + Sync`

### `crates/tm-computer/src/capability.rs`

- `pub struct SessionRegistry`
- `impl SessionRegistry`
  - `pub fn new(env: SelectionEnv, headless: bool, panic_stop_config: PanicStopConfig) -> Self`
  - `pub async fn close(&self, ticket: &TicketId, session: &SessionId) -> Result<()>`
  - `pub async fn close_all(&self) -> Result<()>`
  - `pub async fn live_count(&self) -> usize`
- `pub struct ComputerCapability`
- `impl ComputerCapability`
  - `pub fn new(sessions: SessionRegistry) -> Self`
  - `pub async fn close_all(&self) -> Result<()>`

### `crates/tm-computer/src/input.rs`

- `pub struct Point`
- `impl Point`
  - `pub fn new(x: f64, y: f64) -> Self`
- `pub struct Rect`
- `impl Rect`
  - `pub fn new(origin: Point, width: f64, height: f64) -> Self`
  - `pub fn contains(&self, p: Point) -> bool`
- `pub fn clamp_point(p: Point, bounds: Rect) -> Point`
- `pub fn lerp_drag_path(from: Point, to: Point, steps: usize) -> Vec<Point>`
- `pub enum MouseButton`
- `pub enum Modifier`
- `pub struct KeyChord`
- `impl KeyChord`
  - `pub fn parse(s: &str) -> Result<KeyChord, ComputerError>`
  - `pub fn to_canonical_string(&self) -> String`
- `pub struct ScrollDelta`
- `pub enum InputTarget`
- `pub enum InputAction`

### `crates/tm-computer/src/lib.rs`

- `pub mod backend;`
- `pub mod capability;`
- `pub mod input;`
- `pub mod session;`
- `pub mod macos;`
- `pub mod linux;`
- `pub enum ComputerError`

### `crates/tm-computer/src/linux.rs`

- `pub struct X11Backend`
- `impl X11Backend`
  - `pub fn connect(display: Option<&str>) -> TmResult<X11Backend>`
  - `pub fn has_xtest_extension(&self) -> TmResult<bool>`
- `pub struct WaylandBackend`
- `impl WaylandBackend`
  - `pub fn connect() -> TmResult<WaylandBackend>`
- `pub struct XvfbSession`
- `impl XvfbSession`
  - `pub fn start(ids: &dyn IdSource) -> TmResult<XvfbSession>`
  - `pub fn display(&self) -> &str`
  - `pub fn backend(&self) -> TmResult<X11Backend>`
- `pub async fn headless<F, Fut, T>(ids: &dyn IdSource, body: F) -> TmResult<T>
where
    F: FnOnce(X11Backend) -> Fut,
    Fut: std::future::Future<Output = TmResult<T>>,`
- `pub fn wayland_portal_reachable() -> TmResult<bool>`

### `crates/tm-computer/src/macos.rs`

- `pub enum TccPermission`
- `impl TccPermission`
  - `pub fn settings_path(&self) -> &'static str`
  - `pub fn label(&self) -> &'static str`
- `mod sys`
  - `pub struct OpaqueAxUiElement`
  - `pub type AXUIElementRef = *const OpaqueAxUiElement;`
  - `pub const K_AX_VALUE_CG_POINT_TYPE: u32 = 1;`
  - `pub const K_AX_VALUE_CG_SIZE_TYPE: u32 = 2;`
  - `pub const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1 << 0;`
  - `pub const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;`
  - `pub const K_CG_NULL_WINDOW_ID: u32 = 0;`
- `pub fn check_accessibility_permission() -> TmResult<bool>`
- `pub fn check_screen_recording_permission() -> TmResult<bool>`
- `pub fn permission_missing_error(permission: TccPermission) -> ComputerError`
- `pub struct MacosBackend`
- `impl MacosBackend`
  - `pub fn new() -> Self`

### `crates/tm-computer/src/session.rs`

- `pub enum SessionMode`
- `pub struct PanicStopConfig`
- `pub struct PanicStop`
- `impl PanicStop`
  - `pub fn new(config: PanicStopConfig) -> Self`
  - `pub fn mouse_moved(&self, agent_last_point: Point, observed_point: Point) -> bool`
  - `pub fn is_abort_chord(&self, pressed: &KeyChord) -> bool`
- `pub struct Snapshot`
- `pub struct ComputerSession`
- `impl ComputerSession`
  - `pub fn new(backend: Box<dyn Backend>, mode: SessionMode, panic_stop: PanicStop) -> Self`
  - `pub fn mode(&self) -> &SessionMode`
  - `pub fn approve(&mut self, at: Timestamp)`
  - `pub fn is_approved(&self) -> bool`
  - `pub async fn capabilities(&self) -> TmResult<Capabilities>`
  - `pub async fn snapshot(&self, at: Timestamp, force_screenshot: bool) -> TmResult<Snapshot>`
  - `pub async fn act(&mut self, action: InputAction) -> TmResult<()>`
  - `pub fn panic_stop_check(&mut self, observed: Point) -> TmResult<()>`
  - `pub async fn windows(&self) -> TmResult<Vec<WindowInfo>>`
  - `pub async fn displays(&self) -> TmResult<Vec<DisplayInfo>>`
  - `pub async fn focus(&self, window_id: &str) -> TmResult<()>`
  - `pub async fn move_window(&self, window_id: &str, to: Point) -> TmResult<()>`
  - `pub async fn resize_window(&self, window_id: &str, width: f64, height: f64) -> TmResult<()>`
  - `pub async fn clipboard_get(&self) -> TmResult<Option<String>>`
  - `pub async fn clipboard_set(&self, text: &str) -> TmResult<()>`
  - `pub async fn launch(&self, app: &str) -> TmResult<()>`
  - `pub async fn quit(&self, app: &str) -> TmResult<()>`
