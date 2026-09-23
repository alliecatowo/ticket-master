//! [`McpServer`]: exposes one already-identified Ticketmaster project as a real MCP server over
//! stdio — `ticket_list`/`ticket_show` (from [`tm_core::Store::view`]), `search_*` and `symbol_*`
//! (from `tm_codeintel::CodeIntel`, the same facade `tm-cli`'s own `search`/`symbol` subcommands
//! call — see `crates/tm-cli/src/search.rs`), and one write: `ticket_dispatch`, which creates a
//! work ticket with `tm ticket new`'s defaults and activates it, so an MCP host (Claude Code, via
//! `tm mcp`) can hand work to tm's workers.
//!
//! # Tool names
//!
//! Underscored, not dotted: Claude Code (and the Anthropic and OpenAI APIs behind most MCP hosts)
//! only accept tool names matching `^[a-zA-Z0-9_-]{1,64}$`, so `ticket.list` would be rejected by
//! the host before a model ever saw it.
//!
//! # Deliberately absent: accept, reject, retry
//!
//! A submitted ticket's accept/reject and an escalated ticket's retry are human-only decisions
//! (`tm ticket accept|reject|retry`, the tickets screen, `tm serve`'s transition route). An MCP
//! host is an agent, so this server never offers them.
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
//! `src/bin/tm-mcp-server.rs` instead takes `--project-root`/`--state-dir` directly as flags.
//! `tm mcp` (in `tm-cli`) resolves the project itself and uses [`McpServer::with_store`] to share
//! its already-open [`tm_core::Store`] with the in-process scheduler, so both mint ids from one
//! counter. No `.join(".tm")` appears anywhere in this crate as a result.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tm_types::{Authority, Budget, MilestoneId, ParticipantId, Result, TicketId, TmError};
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

/// The tools this server exposes: `(name, description, input JSON Schema)`. A
/// plain function rather than a `const` because `serde_json::json!` allocates.
fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "ticket_list",
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
            "name": "ticket_show",
            "description": "Show the full record for one ticket by id.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": {"type": "string"} },
                "required": ["id"]
            }
        }),
        json!({
            "name": "search_exact",
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
            "name": "search_hybrid",
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
            "name": "symbol_def",
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
            "name": "symbol_outline",
            "description": "Rendered top-level symbol outline for one file.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": {"type": "string"} },
                "required": ["path"]
            }
        }),
        json!({
            "name": "ticket_dispatch",
            "description": "Hand a task to tm's background workers: creates a work ticket for the objective (the same defaults as `tm ticket new`) and queues it. Returns the ticket id and state; follow it with ticket_show. A worker picks it up when a tm scheduler is running for this project (`tm mcp` runs one unless started with --no-workers). A finished ticket waits for a human to accept or reject it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "objective": {"type": "string", "description": "What the worker should do, stated as a complete task."}
                },
                "required": ["objective"]
            }
        }),
    ]
}

const TOOL_NAMES: &[&str] = &[
    "ticket_list",
    "ticket_show",
    "search_exact",
    "search_hybrid",
    "symbol_def",
    "symbol_outline",
    "ticket_dispatch",
];

/// The actor `ticket_dispatch` records when the host didn't say who it is in `initialize`.
const DEFAULT_DISPATCH_ACTOR: &str = "agent:mcp/client";

/// The actor for tickets dispatched by the MCP host that named itself `client_name` in
/// `initialize`'s `clientInfo`: `agent:mcp/<name>`, with every character outside
/// `[A-Za-z0-9._-]` replaced by `-`. A `ParticipantId` for an agent must be
/// `agent:<provider>/<id>`, so a bare `agent:mcp` would not parse.
fn dispatch_actor(client_name: Option<&str>) -> ParticipantId {
    let cleaned: String = client_name
        .unwrap_or("")
        .trim()
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-');
    let default = || {
        // `DEFAULT_DISPATCH_ACTOR` is a valid `agent:<provider>/<id>`; `system` is unreachable.
        ParticipantId::new(DEFAULT_DISPATCH_ACTOR).unwrap_or_else(|_| ParticipantId::system())
    };
    if cleaned.is_empty() {
        return default();
    }
    ParticipantId::new(format!("agent:mcp/{cleaned}")).unwrap_or_else(|_| default())
}

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
        .ok_or_else(|| TmError::parse(format!("`{field}` is required and must be a string")))
}

/// One opened Ticketmaster project, exposed as an MCP server.
pub struct McpServer {
    store: Arc<tm_core::Store>,
    project_root: PathBuf,
    state_dir: PathBuf,
    /// `clientInfo.name` from the host's `initialize`, for `ticket_dispatch`'s actor.
    client_name: Mutex<Option<String>>,
}

impl McpServer {
    /// Open the project at `state_dir` (already resolved by the caller — see this module's doc
    /// comment) and serve it. `project_root` is the workspace `tm-codeintel` walks; it may differ
    /// from `state_dir` (D-003: global-scope state lives outside the workspace).
    ///
    /// This opens a second [`tm_core::Store`] with its own id counters, restored from the
    /// database at open. Like any two `tm` processes writing one project at once (`tm ticket
    /// new` next to a running `tm serve`), a `ticket_dispatch` here can race another process for
    /// the next ticket id. `tm mcp` avoids that by sharing its store through
    /// [`McpServer::with_store`].
    pub fn new(project_root: PathBuf, state_dir: PathBuf) -> Result<Self> {
        let store = Arc::new(tm_core::Store::open_at(&state_dir)?);
        Ok(Self::with_store(store, project_root, state_dir))
    }

    /// Serve a project whose [`tm_core::Store`] the caller already has open, so the server and
    /// anything else in the process (an in-process scheduler) share one store and one id source.
    pub fn with_store(
        store: Arc<tm_core::Store>,
        project_root: PathBuf,
        state_dir: PathBuf,
    ) -> Self {
        McpServer {
            store,
            project_root,
            state_dir,
            client_name: Mutex::new(None),
        }
    }

    fn remember_client(&self, params: Option<&Value>) {
        let name = params
            .and_then(|p| p.get("clientInfo"))
            .and_then(|c| c.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Ok(mut slot) = self.client_name.lock() {
            *slot = name;
        }
    }

    fn dispatch_actor(&self) -> ParticipantId {
        let name = self.client_name.lock().ok().and_then(|slot| slot.clone());
        dispatch_actor(name.as_deref())
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
                    TmError::parse(format!(
                        "unknown ticket state `{state_str}`: expected one of draft, blocked, ready, leased, running, submitted, verifying, auditing, rework, replan, recovery, escalated, closed, cancelled"
                    ))
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

    /// Create a work ticket with `tm ticket new`'s defaults (`Authority::worker()`, an
    /// unlimited budget, and `tm-core`'s default executor, retry and verification policies),
    /// then activate it so a worker can take it.
    fn ticket_dispatch(&self, args: &Value) -> Result<Value> {
        let objective = get_str(args, "objective")?.trim();
        if objective.is_empty() {
            return Err(TmError::parse("`objective` can't be empty"));
        }
        let actor = self.dispatch_actor();
        let events = self.store.create_ticket(
            tm_core::TicketKind::Work,
            objective.to_string(),
            None,
            None,
            Authority::worker(),
            Vec::new(),
            tm_core::ExecutorRequirements::default(),
            Vec::new(),
            Vec::new(),
            tm_core::VerificationPolicy::default(),
            Budget::unlimited(),
            tm_core::RetryPolicy::default(),
            0,
            actor.clone(),
        )?;
        let id = events
            .iter()
            .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.clone()))
            .ok_or_else(|| TmError::invariant("create_ticket did not emit ticket.created"))?;
        self.store.activate(&id, actor)?;
        let view = self.store.view()?;
        let ticket = view
            .tickets
            .get(&id)
            .ok_or_else(|| TmError::not_found("ticket", &id))?;
        Ok(json!({ "id": id.to_string(), "state": ticket.state }))
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
            "ticket_list" => self.ticket_list(args),
            "ticket_show" => self.ticket_show(args),
            "ticket_dispatch" => self.ticket_dispatch(args),
            "search_exact" => self.search_exact(args),
            "search_hybrid" => self.search_hybrid(args),
            "symbol_def" => self.symbol_def(args),
            "symbol_outline" => self.symbol_outline(args),
            other => Err(TmError::invariant(format!(
                "internal error: tool `{other}` was dispatched without being validated first"
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
            "initialize" => {
                self.remember_client(req.params.as_ref());
                JsonRpcResponse::ok(
                    req.id,
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "tm-mcp", "version": env!("CARGO_PKG_VERSION")},
                    }),
                )
            }
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
    /// stdio spec). `tm-mcp-server` keeps this default; `tm mcp` uses
    /// [`McpServer::run_stdio_with`] and [`Framing::LineDelimited`], which is what Claude Code
    /// and every other real MCP host speak.
    pub async fn run_stdio(&self) -> Result<()> {
        self.run_stdio_with(Framing::ContentLength).await
    }

    /// Serve over this process's real stdin/stdout with `framing`, until stdin closes.
    pub async fn run_stdio_with(&self, framing: Framing) -> Result<()> {
        self.serve(tokio::io::stdin(), tokio::io::stdout(), framing)
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
            Some(json!({"name": "not_a_real_tool", "arguments": {}})),
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
            Some(json!({"name": "ticket_show", "arguments": {"id": "T-999"}})),
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
            Some(json!({"name": "ticket_list", "arguments": {}})),
        ));
        let result = resp
            .result
            .expect("listing tickets on an empty project succeeds");
        assert_eq!(result["isError"], false);
    }

    fn call(server: &McpServer, name: &str, arguments: Value) -> Value {
        let resp = server.handle_request(JsonRpcRequest::new(
            crate::protocol::RequestId::Number(7),
            "tools/call",
            Some(json!({"name": name, "arguments": arguments})),
        ));
        resp.result.expect("a known tool answers with a result")
    }

    fn payload(result: &Value) -> Value {
        let text = result["content"][0]["text"]
            .as_str()
            .expect("tool results carry one text block");
        serde_json::from_str(text).expect("tool result text is JSON")
    }

    #[test]
    fn every_tool_name_is_a_valid_host_tool_name() {
        for name in TOOL_NAMES {
            assert!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{name} would be rejected by Claude Code / the Anthropic and OpenAI APIs"
            );
        }
    }

    #[test]
    fn no_human_only_transition_is_offered() {
        for name in TOOL_NAMES {
            for human_only in ["accept", "reject", "retry"] {
                assert!(!name.contains(human_only), "{name} is human-only");
            }
        }
    }

    #[test]
    fn ticket_dispatch_creates_and_activates_a_worker_ticket() {
        let (server, _dir) = open_test_server();
        server.handle_request(JsonRpcRequest::new(
            crate::protocol::RequestId::Number(1),
            "initialize",
            Some(json!({"clientInfo": {"name": "claude-code", "version": "2.0"}})),
        ));
        let result = call(
            &server,
            "ticket_dispatch",
            json!({"objective": "Add a README"}),
        );
        assert_eq!(result["isError"], false, "{result}");
        let out = payload(&result);
        assert_eq!(out["state"], "ready");
        let id = TicketId::new(out["id"].as_str().expect("id is a string")).expect("valid id");

        let view = server.store.view().expect("view");
        let ticket = view.tickets.get(&id).expect("the ticket exists");
        assert_eq!(ticket.objective, "Add a README");
        assert_eq!(ticket.kind, tm_core::TicketKind::Work);
        assert_eq!(ticket.authority, Authority::worker());
        assert_eq!(ticket.budget, Budget::unlimited());
        assert_eq!(ticket.executor, tm_core::ExecutorRequirements::default());
        assert_eq!(ticket.retry, tm_core::RetryPolicy::default());
        assert_eq!(ticket.verification, tm_core::VerificationPolicy::default());
        assert_eq!(ticket.state, tm_core::TicketState::Ready);
    }

    #[test]
    fn ticket_dispatch_without_an_objective_is_a_tool_error() {
        let (server, _dir) = open_test_server();
        let result = call(&server, "ticket_dispatch", json!({"objective": "   "}));
        assert_eq!(result["isError"], true);
        let result = call(&server, "ticket_dispatch", json!({}));
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn dispatch_actor_names_the_host_and_always_parses() {
        assert_eq!(
            dispatch_actor(Some("claude-code")).to_string(),
            "agent:mcp/claude-code"
        );
        assert_eq!(
            dispatch_actor(Some("My Host/1")).to_string(),
            "agent:mcp/My-Host-1"
        );
        assert_eq!(dispatch_actor(None).to_string(), DEFAULT_DISPATCH_ACTOR);
        assert_eq!(
            dispatch_actor(Some(" /// ")).to_string(),
            DEFAULT_DISPATCH_ACTOR
        );
        assert!(dispatch_actor(Some("x")).is_agent());
    }
}
