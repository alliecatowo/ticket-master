+++
[doc]
id = "wiki/architecture/tm-browser"
mode = "generated"
derived_from = ["crates/tm-browser/src/**"]
+++

# Architecture: tm-browser

## Module tree

- `crates/tm-browser/src/authority.rs`
- `crates/tm-browser/src/capability.rs`
- `crates/tm-browser/src/cdp.rs`
- `crates/tm-browser/src/config.rs`
- `crates/tm-browser/src/downloader.rs`
- `crates/tm-browser/src/launch.rs`
- `crates/tm-browser/src/lib.rs`
- `crates/tm-browser/src/managed.rs`
- `crates/tm-browser/src/provider.rs`
- `crates/tm-browser/src/remote_cdp.rs`
- `crates/tm-browser/src/session.rs`
- `crates/tm-browser/src/snapshot.rs`

## Public symbols

### `crates/tm-browser/src/authority.rs`

- `pub struct NavigationGuard<'a>`
- `impl<'a> NavigationGuard<'a>`
  - `pub fn new(authority: &'a Authority) -> Self`
  - `pub fn check_navigate(&self, url: &str) -> Result<()>`
  - `pub fn check_download(&self, url: &str) -> Result<()>`
- `pub enum TraceEvent`
- `pub enum ActionKind`
- `pub struct SessionTrace`
- `impl SessionTrace`
  - `pub fn new(session: SessionId) -> Self`
  - `pub fn record(&mut self, event: TraceEvent)`
  - `pub fn has_denials(&self) -> bool`
  - `pub fn to_evidence_json(&self) -> Result<serde_json::Value>`

### `crates/tm-browser/src/capability.rs`

- `pub struct SessionRegistry`
- `impl SessionRegistry`
  - `pub fn new(
        providers: Arc<ProviderRegistry>,
        sink: Arc<dyn ArtifactSink>,
        clock: Arc<dyn Clock>,
        artifact_threshold_bytes: usize,
    ) -> Self`
  - `pub async fn close(&self, ticket: &TicketId, session: &SessionId) -> Result<()>`
  - `pub async fn close_all(&self) -> Result<()>`
  - `pub async fn live_count(&self) -> usize`
- `pub struct BrowserCapability`
- `impl BrowserCapability`
  - `pub fn new(sessions: SessionRegistry) -> Self`
  - `pub async fn close_all(&self) -> Result<()>`

### `crates/tm-browser/src/cdp.rs`

- `pub type RequestId = u64;`
- `pub struct CdpRequest`
- `pub struct CdpErrorPayload`
- `pub struct CdpResponse`
- `pub struct CdpEvent`
- `pub enum CdpMessage`
- `pub enum CdpError`
- `pub fn encode_request(req: &CdpRequest) -> Result<String, CdpError>`
- `pub fn decode_message(text: &str) -> Result<CdpMessage, CdpError>`
- `pub struct TargetInfo`
- `pub struct CdpClient`
- `impl CdpClient`
  - `pub async fn connect(ws_url: &str) -> Result<Self, CdpError>`
  - `pub async fn call(&self, method: &str, params: Value) -> Result<Value, CdpError>`
  - `pub async fn call_in_session(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, CdpError>`
  - `pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent>`
  - `pub async fn list_targets(&self) -> Result<Vec<TargetInfo>, CdpError>`
  - `pub async fn create_target(&self, url: &str) -> Result<TargetInfo, CdpError>`
  - `pub async fn close_target(&self, target_id: &str) -> Result<(), CdpError>`
  - `pub async fn attach_to_target(&self, target_id: &str) -> Result<String, CdpError>`
  - `pub async fn detach_from_session(&self, session_id: &str) -> Result<(), CdpError>`
  - `pub async fn close(&self) -> Result<(), CdpError>`

### `crates/tm-browser/src/config.rs`

- `pub struct ManagedConfig`
- `pub struct RemoteCdpConfig`
- `pub struct BrowserToml`
- `impl BrowserToml`
  - `pub fn parse(source: &str) -> Result<Self>`
  - `pub fn validate(&self) -> Result<()>`

### `crates/tm-browser/src/downloader.rs`

- `pub trait BrowserDownloader: Send + Sync`
- `pub struct ReqwestDownloader`
- `impl ReqwestDownloader`
  - `pub fn new() -> Self`

### `crates/tm-browser/src/launch.rs`

- `pub struct LaunchConfig`
- `pub fn ephemeral_profile_dir(ids: &dyn IdSource) -> Result<PathBuf>`
- `pub fn build_argv(config: &LaunchConfig) -> Vec<String>`

### `crates/tm-browser/src/lib.rs`

- `pub mod authority;`
- `pub mod capability;`
- `pub mod cdp;`
- `pub mod config;`
- `pub mod downloader;`
- `pub mod launch;`
- `pub mod managed;`
- `pub mod provider;`
- `pub mod remote_cdp;`
- `pub mod session;`
- `pub mod snapshot;`

### `crates/tm-browser/src/managed.rs`

- `pub struct ManagedProvider`
- `impl ManagedProvider`
  - `pub fn new(
        config: ManagedConfig,
        downloader: Arc<dyn BrowserDownloader>,
        ids: Arc<dyn IdSource>,
    ) -> Self`

### `crates/tm-browser/src/provider.rs`

- `pub struct BrowserCapabilities`
- `pub struct SessionRequest`
- `pub struct BrowserEndpoint`
- `pub trait BrowserProvider: Send + Sync`
- `pub struct ProviderRegistry`
- `impl ProviderRegistry`
  - `pub fn new(
        providers: Vec<Arc<dyn BrowserProvider>>,
        fallback_order: Vec<String>,
    ) -> Result<Self>`
  - `pub fn fallback_order(&self) -> &[String]`
  - `pub fn get(&self, id: &str) -> Option<&Arc<dyn BrowserProvider>>`
  - `pub fn from_config(
        config: &BrowserToml,
        downloader: Arc<dyn BrowserDownloader>,
        ids: Arc<dyn IdSource>,
    ) -> Result<Self>`
  - `pub async fn acquire(
        &self,
        req: &SessionRequest,
    ) -> Result<(Arc<dyn BrowserProvider>, BrowserEndpoint)>`

### `crates/tm-browser/src/remote_cdp.rs`

- `pub struct RemoteCdpProvider`
- `impl RemoteCdpProvider`
  - `pub fn new(ws_url: impl Into<String>) -> Self`

### `crates/tm-browser/src/session.rs`

- `pub trait ArtifactSink: Send + Sync`
- `pub enum ArtifactRef`
- `pub struct TabId`
- `pub struct TabInfo`
- `pub enum WaitCondition`
- `pub struct ConsoleMessage`
- `pub struct NetworkEntry`
- `pub struct Cookie`
- `pub struct StorageState`
- `pub struct BrowserSessionConfig`
- `pub struct BrowserSession`
- `impl BrowserSession`
  - `pub async fn launch(
        provider: Arc<dyn BrowserProvider>,
        endpoint: BrowserEndpoint,
        config: BrowserSessionConfig,
        sink: Arc<dyn ArtifactSink>,
        clock: Arc<dyn Clock>,
        session_id: SessionId,
    ) -> Result<Self>`
  - `pub fn id(&self) -> &SessionId`
  - `pub fn trace(&self) -> &SessionTrace`
  - `pub async fn close(self) -> Result<()>`
  - `pub async fn open_tab(&mut self, url: &str) -> Result<TabId>`
  - `pub async fn list_tabs(&self) -> Result<Vec<TabInfo>>`
  - `pub async fn switch_tab(&mut self, tab: TabId) -> Result<()>`
  - `pub async fn navigate(&mut self, url: &str) -> Result<()>`
  - `pub async fn snapshot(&mut self) -> Result<Snapshot>`
  - `pub async fn click(&mut self, reference: &AxRef) -> Result<()>`
  - `pub async fn hover(&mut self, reference: &AxRef) -> Result<()>`
  - `pub async fn type_text(&mut self, reference: &AxRef, text: &str) -> Result<()>`
  - `pub async fn select(&mut self, reference: &AxRef, value: &str) -> Result<()>`
  - `pub async fn press(&mut self, key: &str) -> Result<()>`
  - `pub async fn eval(&mut self, js: &str) -> Result<Value>`
  - `pub async fn wait_for(&mut self, condition: WaitCondition, timeout: Duration) -> Result<()>`
  - `pub async fn console(&mut self) -> Result<Vec<ConsoleMessage>>`
  - `pub async fn network(&mut self) -> Result<Vec<NetworkEntry>>`
  - `pub async fn cookies(&mut self) -> Result<Vec<Cookie>>`
  - `pub async fn set_cookie(&mut self, cookie: Cookie) -> Result<()>`
  - `pub async fn storage_state(&mut self) -> Result<StorageState>`
  - `pub async fn screenshot(&mut self, full_page: bool) -> Result<ArtifactRef>`
  - `pub async fn pdf(&mut self) -> Result<ArtifactRef>`

### `crates/tm-browser/src/snapshot.rs`

- `pub struct AxRef`
- `impl AxRef`
  - `pub fn from_backend_node_id(backend_node_id: i64) -> AxRef`
- `pub struct AxNode`
- `pub struct Snapshot`
- `impl Snapshot`
  - `pub fn from_ax_tree(raw: &Value, include_ignored: bool) -> Result<Snapshot, SnapshotError>`
  - `pub fn find_ref(&self, r: &AxRef) -> Option<&AxNode>`
  - `pub fn render_text(&self) -> String`
  - `pub fn diff(&self, after: &Snapshot) -> SnapshotDiff`
- `pub struct SnapshotDiff`
- `impl SnapshotDiff`
  - `pub fn is_empty(&self) -> bool`
- `pub struct AxNodeChange`
- `pub enum SnapshotError`
