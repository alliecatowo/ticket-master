+++
[doc]
id = "wiki/architecture/tm-acp"
mode = "generated"
derived_from = ["crates/tm-acp/src/**"]
+++

# Architecture: tm-acp

## Module tree

- `crates/tm-acp/src/client.rs`
- `crates/tm-acp/src/connection.rs`
- `crates/tm-acp/src/error.rs`
- `crates/tm-acp/src/executor.rs`
- `crates/tm-acp/src/jsonrpc.rs`
- `crates/tm-acp/src/lib.rs`
- `crates/tm-acp/src/permission.rs`
- `crates/tm-acp/src/protocol.rs`
- `crates/tm-acp/src/server.rs`

## Public symbols

### `crates/tm-acp/src/client.rs`

- `pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);`
- `pub struct AcpClient`
- `impl AcpClient`
  - `pub async fn spawn(
        command: &[String],
        cwd: &Path,
        authority: Authority,
        timeout: Duration,
    ) -> Result<Self, AcpError>`
  - `pub fn connect(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
        authority: Authority,
        cwd: PathBuf,
        timeout: Duration,
    ) -> Self`
  - `pub async fn initialize(&self) -> Result<InitializeResponse, AcpError>`
  - `pub async fn new_session(&self) -> Result<NewSessionResponse, AcpError>`
  - `pub async fn prompt(
        &self,
        session_id: &str,
        prompt: Vec<ContentBlock>,
    ) -> Result<PromptResponse, AcpError>`
  - `pub async fn drain_transcript(&self) -> Vec<String>`
  - `pub async fn kill(&mut self)`

### `crates/tm-acp/src/connection.rs`

- `pub trait RequestHandler: Send + Sync`
- `pub enum ConnError`
- `pub struct Outbound<W>`
- `impl<W> Outbound<W>
where
    W: AsyncWrite + Unpin + Send + 'static,`
  - `pub fn new(writer: W) -> Self`
  - `pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ConnError>`
  - `pub async fn notify(&self, method: &str, params: Value) -> Result<(), ConnError>`
- `pub struct Connection<W>`
- `impl<W> Connection<W>
where
    W: AsyncWrite + Unpin + Send + 'static,`
  - `pub fn spawn<R, H>(reader: R, writer: W, handler: Arc<H>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        H: RequestHandler + 'static,`
  - `pub fn from_parts<R, H>(outbound: Outbound<W>, reader: R, handler: Arc<H>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        H: RequestHandler + 'static,`
  - `pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ConnError>`
  - `pub async fn notify(&self, method: &str, params: Value) -> Result<(), ConnError>`
  - `pub fn outbound(&self) -> Outbound<W>`

### `crates/tm-acp/src/error.rs`

- `pub enum AcpError`

### `crates/tm-acp/src/executor.rs`

- `pub struct AcpAgentConfig`
- `pub struct AcpExecutor`
- `impl AcpExecutor`
  - `pub fn new(id: impl Into<String>, config: AcpAgentConfig, store: Arc<Store>) -> Self`

### `crates/tm-acp/src/jsonrpc.rs`

- `pub const JSONRPC_VERSION: &str = "2.0";`
- `pub enum RequestId`
- `pub struct RpcRequest`
- `pub struct RpcNotification`
- `pub struct RpcError`
- `pub mod error_codes`
  - `pub const METHOD_NOT_FOUND: i64 = -32601;`
  - `pub const INVALID_PARAMS: i64 = -32602;`
  - `pub const INTERNAL_ERROR: i64 = -32603;`
- `impl RpcError`
  - `pub fn method_not_found(method: &str) -> Self`
  - `pub fn invalid_params(detail: impl std::fmt::Display) -> Self`
  - `pub fn internal(detail: impl std::fmt::Display) -> Self`
- `pub struct RpcResponse`
- `impl RpcResponse`
  - `pub fn success(id: RequestId, result: Value) -> Self`
  - `pub fn failure(id: RequestId, error: RpcError) -> Self`
- `impl RpcRequest`
  - `pub fn new(id: RequestId, method: impl Into<String>, params: Value) -> Self`
- `impl RpcNotification`
  - `pub fn new(method: impl Into<String>, params: Value) -> Self`
- `pub enum IncomingMessage`
- `pub enum FramingError`
- `pub fn parse_line(line: &str) -> Result<IncomingMessage, FramingError>`
- `pub async fn write_message<W, T>(writer: &mut W, value: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,`
- `pub async fn read_line<R>(reader: &mut R) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,`

### `crates/tm-acp/src/lib.rs`

- `pub mod client;`
- `pub mod connection;`
- `pub mod error;`
- `pub mod executor;`
- `pub mod jsonrpc;`
- `pub mod permission;`
- `pub mod protocol;`
- `pub mod server;`

### `crates/tm-acp/src/permission.rs`

- `pub enum PermissionDecision`
- `pub fn evaluate(
    authority: &Authority,
    tool_call: &ToolCallUpdate,
    cwd: &Path,
) -> PermissionDecision`
- `pub fn choose_option(
    decision: &PermissionDecision,
    options: &[PermissionOption],
) -> RequestPermissionOutcome`

### `crates/tm-acp/src/protocol.rs`

- `pub mod methods`
  - `pub const INITIALIZE: &str = "initialize";`
  - `pub const SESSION_NEW: &str = "session/new";`
  - `pub const SESSION_PROMPT: &str = "session/prompt";`
  - `pub const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";`
  - `pub const SESSION_UPDATE: &str = "session/update";`
- `pub const PROTOCOL_VERSION: u16 = 1;`
- `pub struct Implementation`
- `pub struct FileSystemCapabilities`
- `pub struct ClientCapabilities`
- `pub struct PromptCapabilities`
- `pub struct AgentCapabilities`
- `pub struct InitializeRequest`
- `pub struct InitializeResponse`
- `pub struct NewSessionRequest`
- `pub struct NewSessionResponse`
- `pub struct ContentBlock`
- `impl ContentBlock`
  - `pub fn text(text: impl Into<String>) -> Self`
- `pub struct PromptRequest`
- `pub enum StopReason`
- `pub struct PromptResponse`
- `pub enum ToolKind`
- `pub enum ToolCallStatus`
- `pub struct ToolCallLocation`
- `pub struct ToolCallUpdate`
- `pub enum PermissionOptionKind`
- `impl PermissionOptionKind`
  - `pub fn is_allow(self) -> bool`
- `pub struct PermissionOption`
- `pub struct RequestPermissionRequest`
- `pub enum RequestPermissionOutcome`
- `pub struct RequestPermissionResponse`
- `pub struct ContentChunk`
- `pub enum SessionUpdate`
- `pub struct SessionNotification`

### `crates/tm-acp/src/server.rs`

- `pub trait AgentBackend: Send + Sync`
- `pub struct ProjectAgentBackend`
- `impl ProjectAgentBackend`
  - `pub fn new(store: Arc<Store>, ids: Arc<dyn IdSource>) -> Self`
- `pub struct AcpServer<B>`
- `impl<B: AgentBackend + 'static> AcpServer<B>`
  - `pub fn new(backend: Arc<B>) -> Arc<Self>`
  - `pub fn attach(
        self: &Arc<Self>,
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Connection<BoxedWriter>`
