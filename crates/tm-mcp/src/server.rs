//! [`McpServer`]: exposes one already-identified Ticketmaster project as a real, read-only MCP
//! server over stdio — `ticket.*` (from [`tm_core::Store::view`]), `search.*` and `symbol.*`
//! (from `tm_codeintel::CodeIntel`, the same facade `tm-cli`'s own `search`/`symbol` subcommands
//! call — see `crates/tm-cli/src/search.rs`).
//!
//! # Opening a project: no second discovery mechanism
//!
//! [`McpServer::new`] takes an already-resolved `project_root`/`state_dir` pair and opens
//! [`tm_core::Store::open_at`] directly over `state_dir` — the same shape
//! `tm_server::state::AppState::open`'s `ServerConfig` already uses (accept resolved paths,
//! open `Store` yourself; never re-derive `<root>/.tm` by hand). `tm-cli::project`'s
//! `open_at`/`resolve_scope` is the sanctioned place that turns a bare directory into a
//! `(root, state_dir)` pair (`docs/decisions/D-003-project-scope.md`); depending on `tm-cli`
//! itself from here would pull in axum/crossterm/ratatui/tm-tui/tm-browser/tm-computer/tm-server
//! transitively for a binary that needs none of them, so this crate does not do that.
//! `src/bin/tm-mcp-server.rs` instead takes `--project-root`/`--state-dir` directly as flags —
//! resolving *which* directory those are (bare `tm`'s scope logic) is left to whatever launches
//! this binary (a future `tm mcp serve` verb in `tm-cli`, or a hand-written MCP host config)
//! rather than reinvented here. No `.join(".tm")` appears anywhere in this crate as a result.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tm_types::{MilestoneId, Result, TicketId, TmError};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::protocol::{
    error_codes, Framing, JsonRpcError, JsonRpcRequest, JsonRpcResponse, Message,
};
use crate::transport::{FramedTransport, Transport};

/// The MCP protocol version this server declares in `initialize`'s response. Matches
/// [`crate::client::McpClient`]'s own `PROTOCOL_VERSION` (kept as a separate private constant
/// there rather than shared, since a client and a server are free to evolve independently and
/// this crate's two halves happen to target the same spec revision today).
const PROTOCOL_VERSION: &str = "2024-11-05";

/// The six read-only tools this server exposes: `(name, description, input JSON Schema)`. A
/// plain function rather than a `const` because `serde_json::json!` allocates.
fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "ticket.list",
            "description": "List tickets in the project, optionally filtered by state, milestone, or parent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "state": {"type": "string", "description": "One of: draft, blocked, ready, leased, running, submitted, verifying, auditing, rework, replan, recovery, escalated, closed, cancelled."},
                    "milestone": {"type": "string"},
                    "parent": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1}
                }
            }
        }),
        json!({
            "name": "ticket.show",
            "description": "Show the full record for one ticket by id.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": {"type": "string"} },
                "required": ["id"]
            }
        }),
        json!({
            "name": "search.exact",
            "description": "Literal substring search over the project's tracked files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1}
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "search.hybrid",
            "description": "Fused semantic + lexical + symbol-graph search over the project's code index.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1}
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "symbol.def",
            "description": "Resolve a symbol name to its definition site.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "from": {"type": "string", "description": "Path to resolve the name from; defaults to the project root."}
                },
                "required": ["name"]
            }
        }),
        json!({
            "name": "symbol.outline",
            "description": "Rendered top-level symbol outline for one file.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": {"type": "string"} },
                "required": ["path"]
            }
        }),
    ]
}

const TOOL_NAMES: &[&str] = &[
    "ticket.list",
    "ticket.show",
    "search.exact",
    "search.hybrid",
    "symbol.def",
    "symbol.outline",
];

/// The default result cap for a search/list tool call that does not specify `limit`.
const DEFAULT_LIMIT: usize = 20;

fn get_limit(args: &Value) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_LIMIT)
}

fn get_str<'a>(args: &'a Value, field: &str) -> Result<&'a str> {
    args.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| TmError::parse(format!("missing or non-string field `{field}`")))
}

/// One opened Ticketmaster project, exposed as an MCP server.
pub struct McpServer {
    store: Arc<tm_core::Store>,
    project_root: PathBuf,
    state_dir: PathBuf,
}

impl McpServer {
    /// Open the project at `state_dir` (already resolved by the caller — see this module's doc
    /// comment) and serve reads over it. `project_root` is the workspace `tm-codeintel` walks;
    /// it may differ from `state_dir` (D-003: global-scope state lives outside the workspace).
    pub fn new(project_root: PathBuf, state_dir: PathBuf) -> Result<Self> {
        let store = Arc::new(tm_core::Store::open_at(&state_dir)?);
        Ok(McpServer {
            store,
            project_root,
            state_dir,
        })
    }

    fn code_intel(&self) -> Result<tm_codeintel::CodeIntel> {
        tm_codeintel::CodeIntel::open_at(&self.state_dir, &self.project_root)
    }

    fn ticket_summary(t: &tm_core::Ticket) -> Value {
        json!({
            "id": t.id.to_string(),
            "kind": t.kind,
            "state": t.state,
            "priority": t.priority,
            "objective": t.objective,
        })
    }

    fn ticket_list(&self, args: &Value) -> Result<Value> {
        let view = self.store.view()?;
        let mut tickets: Vec<&tm_core::Ticket> = view.tickets.values().collect();
        tickets.sort_by_key(|t| &t.id);

        if let Some(state_str) = args.get("state").and_then(Value::as_str) {
            let target: tm_core::TicketState =
                serde_json::from_value(Value::String(state_str.to_string())).map_err(|_| {
                    TmError::parse(format!("unrecognized ticket state {state_str:?}"))
                })?;
            tickets.retain(|t| t.state == target);
        }
        if let Some(milestone) = args.get("milestone").and_then(Value::as_str) {
            let milestone_id = MilestoneId::new(milestone)?;
            tickets.retain(|t| t.milestone.as_ref() == Some(&milestone_id));
        }
        if let Some(parent) = args.get("parent").and_then(Value::as_str) {
            let parent_id = TicketId::new(parent)?;
            tickets.retain(|t| t.parent.as_ref() == Some(&parent_id));
        }

        let limit = get_limit(args);
        let summaries: Vec<Value> = tickets
            .into_iter()
            .take(limit)
            .map(Self::ticket_summary)
            .collect();
        Ok(json!({ "tickets": summaries }))
    }

    fn ticket_show(&self, args: &Value) -> Result<Value> {
        let id = TicketId::new(get_str(args, "id")?)?;
        let view = self.store.view()?;
        let ticket = view
            .tickets
            .get(&id)
            .ok_or_else(|| TmError::not_found("ticket", &id))?;
        serde_json::to_value(ticket).map_err(TmError::from)
    }

    fn search_exact(&self, args: &Value) -> Result<Value> {
        let query = get_str(args, "query")?;
        let limit = get_limit(args);
        let code_intel = self.code_intel()?;
        let result = code_intel.search_exact(query)?;
        let hits: Vec<Value> = result
            .hits
            .iter()
            .take(limit)
            .map(|h| {
                json!({
                    "path": h.path,
                    "line": h.line,
                    "col": h.col,
                    "line_text": h.line_text,
                })
            })
            .collect();
        Ok(json!({ "hits": hits, "truncated": result.truncated }))
    }

    fn search_hybrid(&self, args: &Value) -> Result<Value> {
        let query_text = get_str(args, "query")?;
        let limit = get_limit(args);
        let code_intel = self.code_intel()?;
        let query = tm_codeintel::hybrid::Query {
            text: query_text.to_string(),
            seed_symbols: Vec::new(),
            seed_paths: Vec::new(),
        };
        let ctx = tm_codeintel::hybrid::RetrievalContext {
            claimed_paths: Vec::new(),
            recently_edited: Vec::new(),
        };
        let ranked = code_intel.search_hybrid(
            &query,
            &ctx,
            tm_codeintel::hybrid::SignalWeights::default(),
        )?;
        let hits: Vec<Value> = ranked
            .iter()
            .take(limit)
            .map(|h| {
                json!({
                    "path": h.path,
                    "line_start": h.line_start,
                    "line_end": h.line_end,
                    "snippet": h.snippet,
                    "fused_score": h.fused_score,
                })
            })
            .collect();
        Ok(json!({ "hits": hits }))
    }

    fn symbol_def(&self, args: &Value) -> Result<Value> {
        let name = get_str(args, "name")?;
        let from = args.get("from").and_then(Value::as_str).unwrap_or(".");
        let code_intel = self.code_intel()?;
        match code_intel.definition(name, from)? {
            Some(sym) => Ok(json!({
                "name": sym.name,
                "kind": format!("{:?}", sym.kind),
                "path": sym.path,
                "line_start": sym.range.line_start,
                "line_end": sym.range.line_end,
            })),
            None => Ok(Value::Null),
        }
    }

    fn symbol_outline(&self, args: &Value) -> Result<Value> {
        let path = get_str(args, "path")?;
        let code_intel = self.code_intel()?;
        let entries = code_intel.outline(path)?;
        let rendered: Vec<Value> = entries
            .iter()
            .map(|e| json!({ "depth": e.depth, "rendered": e.rendered }))
            .collect();
        Ok(json!({ "entries": rendered }))
    }

    fn execute_tool(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "ticket.list" => self.ticket_list(args),
            "ticket.show" => self.ticket_show(args),
            "search.exact" => self.search_exact(args),
            "search.hybrid" => self.search_hybrid(args),
            "symbol.def" => self.symbol_def(args),
            "symbol.outline" => self.symbol_outline(args),
            other => Err(TmError::invariant(format!(
                "execute_tool called with unvalidated tool name `{other}`"
            ))),
        }
    }

    /// Handle one `tools/call`. Distinguishes protocol-level errors (unknown tool, missing
    /// `name`/`params` — a real `JsonRpcError`) from a *known* tool's own execution failure (a
    /// ticket that doesn't exist, an unreadable path — reported as `isError: true` inside a
    /// successful JSON-RPC response, matching real MCP servers' convention that a tool failing
    /// is a normal outcome of calling it, not a transport-level fault).
    fn dispatch_tool_call(
        &self,
        params: Option<Value>,
    ) -> std::result::Result<Value, JsonRpcError> {
        let params = params.ok_or_else(|| {
            JsonRpcError::new(error_codes::INVALID_PARAMS, "tools/call requires `params`")
        })?;
        let name = params.get("name").and_then(Value::as_str).ok_or_else(|| {
            JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                "tools/call params missing `name`",
            )
        })?;
        if !TOOL_NAMES.contains(&name) {
            return Err(JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                format!("unknown tool `{name}`"),
            ));
        }
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        Ok(match self.execute_tool(name, &arguments) {
            Ok(value) => json!({
                "content": [{"type": "text", "text": value.to_string()}],
                "isError": false,
            }),
            Err(e) => json!({
                "content": [{"type": "text", "text": e.to_string()}],
                "isError": true,
            }),
        })
    }

    /// Handle one JSON-RPC request, producing exactly the response to send back.
    pub fn handle_request(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        match req.method.as_str() {
            "initialize" => JsonRpcResponse::ok(
                req.id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "tm-mcp", "version": env!("CARGO_PKG_VERSION")},
                }),
            ),
            "ping" => JsonRpcResponse::ok(req.id, json!({})),
            "tools/list" => JsonRpcResponse::ok(req.id, json!({ "tools": tool_definitions() })),
            "tools/call" => match self.dispatch_tool_call(req.params) {
                Ok(value) => JsonRpcResponse::ok(req.id, value),
                Err(err) => JsonRpcResponse::err(req.id, err),
            },
            other => JsonRpcResponse::err(
                req.id,
                JsonRpcError::new(
                    error_codes::METHOD_NOT_FOUND,
                    format!("method not found: {other}"),
                ),
            ),
        }
    }

    /// Serve requests over an already-framed [`Transport`] until the peer disconnects cleanly.
    /// `serve` (below) is the usual entry point; this exists so a caller that already has a
    /// [`Transport`] (e.g. a test driving both ends of one `tokio::io::duplex`) does not need to
    /// go through a fresh [`FramedTransport`].
    pub async fn serve_transport(&self, transport: &mut dyn Transport) -> Result<()> {
        loop {
            match transport.recv().await? {
                None => return Ok(()),
                Some(Message::Request(req)) => {
                    let response = self.handle_request(req);
                    transport.send(&Message::Response(response)).await?;
                }
                // A client notification (e.g. `notifications/initialized`) needs no reply; a
                // stray `Response` (this process is never itself a JSON-RPC client on this
                // channel) is likewise not an error, just ignored.
                Some(Message::Notification(_)) | Some(Message::Response(_)) => continue,
            }
        }
    }

    /// Serve requests read from `reader`/written to `writer`, framed with `framing`, until the
    /// peer disconnects cleanly.
    pub async fn serve<R, W>(&self, reader: R, writer: W, framing: Framing) -> Result<()>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        let mut transport = FramedTransport::new(reader, writer, framing);
        self.serve_transport(&mut transport).await
    }

    /// Serve over this process's real stdin/stdout, [`Framing::ContentLength`]-framed (the
    /// framing the task that produced this crate specified for MCP stdio — see
    /// `crate::protocol`'s module doc comment for why that diverges from the real-world MCP
    /// stdio spec, and why [`Framing::LineDelimited`] exists on the client side for talking to
    /// an actual external server).
    pub async fn run_stdio(&self) -> Result<()> {
        self.serve(
            tokio::io::stdin(),
            tokio::io::stdout(),
            Framing::ContentLength,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_server() -> (McpServer, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir creation should not fail in a test sandbox");
        // Root and state dir are intentionally the same bare tempdir path here (no `.tm`
        // subdirectory involved anywhere) — see `crates/xtask/src/hygiene.rs`'s
        // `check_dot_tm_literals`, which this sidesteps entirely rather than working around.
        let server = McpServer::new(dir.path().to_path_buf(), dir.path().to_path_buf())
            .expect("opening a fresh tempdir project should not fail");
        (server, dir)
    }

    #[test]
    fn tool_definitions_names_match_the_dispatch_table() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs
            .iter()
            .map(|d| {
                d.get("name")
                    .and_then(Value::as_str)
                    .expect("every tool def has a name")
            })
            .collect();
        assert_eq!(names, TOOL_NAMES);
    }

    #[test]
    fn handle_request_initialize_reports_the_declared_protocol_version() {
        let (server, _dir) = open_test_server();
        let resp = server.handle_request(JsonRpcRequest::new(
            crate::protocol::RequestId::Number(1),
            "initialize",
            None,
        ));
        let result = resp.result.expect("initialize succeeds");
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn handle_request_unknown_method_is_a_json_rpc_error() {
        let (server, _dir) = open_test_server();
        let resp = server.handle_request(JsonRpcRequest::new(
            crate::protocol::RequestId::Number(1),
            "totally/bogus",
            None,
        ));
        let err = resp.error.expect("unknown method is an error");
        assert_eq!(err.code, error_codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn tools_call_with_unknown_tool_name_is_a_json_rpc_error_not_a_tool_result() {
        let (server, _dir) = open_test_server();
        let resp = server.handle_request(JsonRpcRequest::new(
            crate::protocol::RequestId::Number(1),
            "tools/call",
            Some(json!({"name": "not.a.real.tool", "arguments": {}})),
        ));
        let err = resp
            .error
            .expect("unknown tool name is a protocol-level error");
        assert_eq!(err.code, error_codes::INVALID_PARAMS);
    }

    #[test]
    fn ticket_show_on_a_missing_ticket_reports_is_error_not_a_json_rpc_error() {
        let (server, _dir) = open_test_server();
        let resp = server.handle_request(JsonRpcRequest::new(
            crate::protocol::RequestId::Number(1),
            "tools/call",
            Some(json!({"name": "ticket.show", "arguments": {"id": "T-999"}})),
        ));
        let result = resp
            .result
            .expect("a known tool's own failure is still a JSON-RPC success");
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn ticket_list_on_an_empty_project_returns_an_empty_list_not_an_error() {
        let (server, _dir) = open_test_server();
        let resp = server.handle_request(JsonRpcRequest::new(
            crate::protocol::RequestId::Number(1),
            "tools/call",
            Some(json!({"name": "ticket.list", "arguments": {}})),
        ));
        let result = resp
            .result
            .expect("listing tickets on an empty project succeeds");
        assert_eq!(result["isError"], false);
    }
}
