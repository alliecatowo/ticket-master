+++
[doc]
id = "wiki/architecture/tm-mcp"
mode = "generated"
derived_from = ["crates/tm-mcp/src/**"]
+++

# Architecture: tm-mcp

## Module tree

- `crates/tm-mcp/src/bin/tm-mcp-server.rs`
- `crates/tm-mcp/src/capability.rs`
- `crates/tm-mcp/src/client.rs`
- `crates/tm-mcp/src/lib.rs`
- `crates/tm-mcp/src/protocol.rs`
- `crates/tm-mcp/src/server.rs`
- `crates/tm-mcp/src/transport.rs`

## Public symbols

### `crates/tm-mcp/src/capability.rs`

- `pub struct McpClientCapability`
- `impl McpClientCapability`
  - `pub fn new(server_id: String, client: McpClient, remote_tools: Vec<RemoteTool>) -> Self`
  - `pub fn has_tool(&self, tool: &str) -> bool`

### `crates/tm-mcp/src/client.rs`

- `pub struct RemoteTool`
- `pub struct McpClient`
- `impl McpClient`
  - `pub async fn connect(transport: Box<dyn Transport>) -> Result<(Self, Vec<RemoteTool>)>`
  - `pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value>`

### `crates/tm-mcp/src/lib.rs`

- `pub mod capability;`
- `pub mod client;`
- `pub mod protocol;`
- `pub mod server;`
- `pub mod transport;`

### `crates/tm-mcp/src/protocol.rs`

- `pub const JSONRPC_VERSION: &str = "2.0";`
- `pub enum RequestId`
- `pub struct JsonRpcRequest`
- `impl JsonRpcRequest`
  - `pub fn new(id: RequestId, method: impl Into<String>, params: Option<Value>) -> Self`
- `pub struct JsonRpcNotification`
- `impl JsonRpcNotification`
  - `pub fn new(method: impl Into<String>, params: Option<Value>) -> Self`
- `pub struct JsonRpcError`
- `pub mod error_codes`
  - `pub const METHOD_NOT_FOUND: i64 = -32601;`
  - `pub const INVALID_PARAMS: i64 = -32602;`
  - `pub const INVALID_REQUEST: i64 = -32600;`
  - `pub const INTERNAL_ERROR: i64 = -32603;`
- `impl JsonRpcError`
  - `pub fn new(code: i64, message: impl Into<String>) -> Self`
- `pub struct JsonRpcResponse`
- `impl JsonRpcResponse`
  - `pub fn ok(id: RequestId, result: Value) -> Self`
  - `pub fn err(id: RequestId, error: JsonRpcError) -> Self`
- `pub enum Message`
- `impl Message`
  - `pub fn to_bytes(&self) -> Result<Vec<u8>>`
  - `pub fn from_value(value: Value) -> Result<Message>`
  - `pub fn from_slice(bytes: &[u8]) -> Result<Message>`
- `pub enum Framing`
- `pub fn encode_frame(message: &Message, framing: Framing) -> Result<Vec<u8>>`
- `pub fn try_parse_frame(buf: &[u8], framing: Framing) -> Result<Option<(Message, usize)>>`

### `crates/tm-mcp/src/server.rs`

- `pub struct McpServer`
- `impl McpServer`
  - `pub fn new(project_root: PathBuf, state_dir: PathBuf) -> Result<Self>`
  - `pub fn handle_request(&self, req: JsonRpcRequest) -> JsonRpcResponse`
  - `pub async fn serve_transport(&self, transport: &mut dyn Transport) -> Result<()>`
  - `pub async fn serve<R, W>(&self, reader: R, writer: W, framing: Framing) -> Result<()>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,`
  - `pub async fn run_stdio(&self) -> Result<()>`

### `crates/tm-mcp/src/transport.rs`

- `pub trait Transport: Send`
- `pub struct FramedTransport<R, W>`
- `impl<R, W> FramedTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,`
  - `pub fn new(reader: R, writer: W, framing: Framing) -> Self`
- `pub struct StdioClientTransport`
- `impl StdioClientTransport`
  - `pub fn spawn(command: &[String], framing: Framing) -> Result<Self>`
- `pub struct SseEvent`
- `pub fn parse_sse_events(buf: &[u8]) -> (Vec<SseEvent>, usize)`
- `pub struct SseClientTransport`
- `impl SseClientTransport`
  - `pub async fn connect(sse_url: &str) -> Result<Self>`
