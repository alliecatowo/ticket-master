//! [`McpServer`]: exposes one already-identified Ticketmaster project as a real MCP server over
//! stdio — `ticket_list`/`ticket_show` (from [`tm_core::Store::view`]), `search_*`/`symbol_*`/
//! `history_*` (from `tm_codeintel::CodeIntel`, the same facade `tm-cli`'s own `search`/`symbol`/
//! `history` subcommands call — see `crates/tm-cli/src/search.rs`), and one write:
//! `ticket_dispatch`, which creates a work ticket with `tm ticket new`'s defaults and activates
//! it, so an MCP host (Claude Code, via `tm mcp`) can hand work to tm's workers.
//! `rename_preview` is deliberately left out here, since it's write-shaped (an edit a host would
//! need to apply), unlike every other read-only `symbol_*`/`search_*`/`history_*` tool.
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

/// The latest MCP protocol version supported by this server.
const PROTOCOL_VERSION: &str = "2025-06-18";

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", PROTOCOL_VERSION];

fn negotiated_protocol_version(requested: Option<&str>) -> &'static str {
    match requested {
        Some(version) if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) => {
            SUPPORTED_PROTOCOL_VERSIONS
                .iter()
                .copied()
                .find(|supported| *supported == version)
                .unwrap_or(PROTOCOL_VERSION)
        }
        _ => PROTOCOL_VERSION,
    }
}

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
            "description": "Literal substring search over the project's tracked files. Returns at most `limit` hits (default 20, max 200); when truncated, narrow the query or pass path_glob.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200},
                    "path_glob": {"type": "string", "description": "Only search files whose path matches this glob, e.g. \"crates/tm-cli/**\"."}
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
            "name": "search_regex",
            "description": "Regex search over the project's tracked files. Returns at most `limit` hits (default 20, max 200); when truncated, narrow the query or pass path_glob.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "A regular expression."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200},
                    "path_glob": {"type": "string", "description": "Only search files whose path matches this glob, e.g. \"crates/tm-cli/**\"."}
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "search_semantic",
            "description": "Semantic (embedding) search over the project's code index for conceptually similar content, not just literal matches.",
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
            "name": "symbol_references",
            "description": "Occurrences that reference the symbol with this id. Get an id from symbol_def first.",
            "inputSchema": {
                "type": "object",
                "properties": { "symbol_id": {"type": "integer", "minimum": 0} },
                "required": ["symbol_id"]
            }
        }),
        json!({
            "name": "symbol_callers",
            "description": "Symbols that call the symbol with this id. Get an id from symbol_def first.",
            "inputSchema": {
                "type": "object",
                "properties": { "symbol_id": {"type": "integer", "minimum": 0} },
                "required": ["symbol_id"]
            }
        }),
        json!({
            "name": "symbol_callees",
            "description": "Symbols called by the symbol with this id. Get an id from symbol_def first.",
            "inputSchema": {
                "type": "object",
                "properties": { "symbol_id": {"type": "integer", "minimum": 0} },
                "required": ["symbol_id"]
            }
        }),
        json!({
            "name": "history_why",
            "description": "Git commits that last touched a line range, most recent first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "line_start": {"type": "integer", "minimum": 1},
                    "line_end": {"type": "integer", "minimum": 1}
                },
                "required": ["path", "line_start", "line_end"]
            }
        }),
        json!({
            "name": "history_search",
            "description": "Search git commit messages and diffs for a query.",
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
            "name": "history_deleted",
            "description": "Find implementations matching a query that were deleted and never reintroduced.",
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
    "search_regex",
    "search_semantic",
    "symbol_def",
    "symbol_outline",
    "symbol_references",
    "symbol_callers",
    "symbol_callees",
    "history_why",
    "history_search",
    "history_deleted",
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

/// Build a `search_exact`/`search_regex` options struct from MCP call args: `limit` (default
/// [`DEFAULT_LIMIT`], clamped to `tm_codeintel::MAX_RESULT_LIMIT`) and `path_glob`.
fn exact_search_options(args: &Value) -> tm_codeintel::SearchOptions {
    tm_codeintel::SearchOptions {
        limit: get_limit(args).min(tm_codeintel::MAX_RESULT_LIMIT),
        path_glob: args
            .get("path_glob")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// Render a `search_exact`/`search_regex` result the way this server hands it back over MCP:
/// capped hits, plus a hint when truncated so the caller knows to narrow the query or pass
/// `path_glob` rather than assume it saw everything.
fn exact_search_result_json(result: &tm_codeintel::ExactSearchResult) -> Value {
    let hits: Vec<Value> = result
        .hits
        .iter()
        .map(|h| {
            json!({
                "path": h.path,
                "line": h.line,
                "col": h.col,
                "line_text": h.line_text,
            })
        })
        .collect();
    let mut value = json!({
        "hits": hits,
        "truncated": result.truncated,
        "total_seen": result.total_seen,
    });
    if result.truncated {
        value["hint"] = json!("narrow the query or pass path_glob");
    }
    value
}

fn get_str<'a>(args: &'a Value, field: &str) -> Result<&'a str> {
    args.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| TmError::parse(format!("`{field}` is required and must be a string")))
}

fn get_u64(args: &Value, field: &str) -> Result<u64> {
    args.get(field).and_then(Value::as_u64).ok_or_else(|| {
        TmError::parse(format!(
            "`{field}` is required and must be a non-negative integer"
        ))
    })
}

fn get_u32(args: &Value, field: &str) -> Result<u32> {
    let n = get_u64(args, field)?;
    u32::try_from(n)
        .map_err(|_| TmError::parse(format!("`{field}` is too large to be a line number")))
}

/// True if `root` or any ancestor has a `.git` entry (a directory for a normal repo, or a file
/// for a linked worktree — `Path::exists` covers both). No `git2` dependency needed for this: a
/// plain filesystem walk is enough to decide whether `update_incremental`'s git-history ingest
/// has any chance of finding a `HEAD` to walk, matching `Project::code_intel()`'s
/// `git2::Repository::open(&self.root).is_ok()` gate without pulling `git2` in as a normal
/// dependency of this crate.
fn is_inside_git_work_tree(root: &std::path::Path) -> bool {
    root.ancestors().any(|dir| dir.join(".git").exists())
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

    /// Opens the code index, kept fresh the same way `Project::code_intel()` does
    /// (`crates/tm-cli/src/project.rs`): every open also runs a best-effort
    /// `update_incremental` pass, so a tool call sees a file edited since the index was last
    /// written rather than silently stale data. Same policy as `Project::code_intel()`
    /// (`nav-fix-project-codeintel-freshness`): skip the refresh entirely when `project_root`
    /// isn't inside a real git work tree, so a global-scope project in an arbitrary directory
    /// (e.g. `$HOME`) never gets walked; otherwise degrade, don't fail — a refresh error (e.g. a
    /// git repo with zero commits, so `HEAD` doesn't resolve) just logs `tracing::warn!` and
    /// returns the already-opened index, since that's still more useful to a caller than an
    /// error here would be. This crate doesn't depend on `git2` for anything else, so the check
    /// is a plain "does some ancestor have a `.git` entry" walk rather than an actual
    /// `git2::Repository::open`.
    fn code_intel(&self) -> Result<tm_codeintel::CodeIntel> {
        let ci = tm_codeintel::CodeIntel::open_at_auto(
            &self.state_dir,
            &self.project_root,
            if cfg!(test) { Some("hash") } else { None },
        )?;
        if is_inside_git_work_tree(&self.project_root) {
            if let Err(e) = ci.update_incremental(&tm_types::SystemClock) {
                tracing::warn!(
                    error = %e,
                    root = %self.project_root.display(),
                    "could not refresh the code index; continuing with what's already indexed"
                );
            }
        }
        Ok(ci)
    }

    /// Format a SymbolKind as a lowercase string.
    fn symbol_kind_str(kind: tm_codeintel::symbols::SymbolKind) -> &'static str {
        match kind {
            tm_codeintel::symbols::SymbolKind::Function => "function",
            tm_codeintel::symbols::SymbolKind::Struct => "struct",
            tm_codeintel::symbols::SymbolKind::Enum => "enum",
            tm_codeintel::symbols::SymbolKind::Interface => "interface",
            tm_codeintel::symbols::SymbolKind::Impl => "impl",
            tm_codeintel::symbols::SymbolKind::Module => "module",
            tm_codeintel::symbols::SymbolKind::Variable => "variable",
            tm_codeintel::symbols::SymbolKind::TypeAlias => "type_alias",
        }
    }

    /// Strip error prefixes for user-friendly display in MCP responses.
    fn clean_error_message(msg: &str) -> String {
        // Strip "parse: ", "storage: ", "io: ", "provider: " etc. prefixes
        if let Some(colon_pos) = msg.find(": ") {
            let potential_prefix = &msg[..colon_pos];
            // Check if this looks like an error prefix (short word + colon)
            if potential_prefix.len() <= 15 && !potential_prefix.contains(' ') {
                return msg[colon_pos + 2..].to_string();
            }
        }
        msg.to_string()
    }

    /// Map Budget to a serializable response object, converting u64::MAX to null.
    fn serialize_budget(budget: &Budget) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tokens".to_string(),
            if budget.tokens == u64::MAX {
                Value::Null
            } else {
                Value::Number(budget.tokens.into())
            },
        );
        obj.insert(
            "dollars_micros".to_string(),
            if budget.dollars_micros == u64::MAX {
                Value::Null
            } else {
                Value::Number(budget.dollars_micros.into())
            },
        );
        obj.insert(
            "wall_seconds".to_string(),
            if budget.wall_seconds == u64::MAX {
                Value::Null
            } else {
                Value::Number(budget.wall_seconds.into())
            },
        );
        Value::Object(obj)
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
        // Serialize with custom Budget formatting (u64::MAX -> null)
        let mut value = serde_json::to_value(ticket).map_err(TmError::from)?;
        if let Some(obj) = value.as_object_mut() {
            if let Some(budget_val) = obj.get("budget") {
                if let Ok(budget) = serde_json::from_value::<Budget>(budget_val.clone()) {
                    obj.insert("budget".to_string(), Self::serialize_budget(&budget));
                }
            }
        }
        Ok(value)
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
        // Return full ticket response with budget serialized properly
        let response = json!({
            "id": id.to_string(),
            "state": ticket.state,
            "budget": Self::serialize_budget(&ticket.budget),
        });
        Ok(response)
    }

    fn search_exact(&self, args: &Value) -> Result<Value> {
        let query = get_str(args, "query")?;
        let options = exact_search_options(args);
        let code_intel = self.code_intel()?;
        let result = code_intel.search_exact_with(query, &options)?;
        Ok(exact_search_result_json(&result))
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
            // `id` is only stable for the lifetime of the `SymbolIndex` that produced it
            // (`tm_codeintel::symbols::Symbol::id`'s own doc comment), which `code_intel()`'s
            // fresh `symbol_index()` parse re-derives deterministically from the same file set
            // each call — good enough to round-trip through symbol_references/symbol_callers/
            // symbol_callees within one server session, as tool_definitions() for those tells
            // the host to do.
            Some(sym) => Ok(json!({
                "id": sym.id,
                "name": sym.name,
                "kind": Self::symbol_kind_str(sym.kind),
                "path": sym.path,
                "line_start": sym.range.line_start,
                "line_end": sym.range.line_end,
            })),
            None => Err(TmError::not_found("symbol", name)),
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

    fn search_regex(&self, args: &Value) -> Result<Value> {
        let pattern = get_str(args, "query")?;
        let options = exact_search_options(args);
        let code_intel = self.code_intel()?;
        let result = code_intel.search_regex_with(pattern, &options)?;
        Ok(exact_search_result_json(&result))
    }

    fn search_semantic(&self, args: &Value) -> Result<Value> {
        let query = get_str(args, "query")?;
        let limit = get_limit(args);
        let code_intel = self.code_intel()?;
        let options = tm_codeintel::semantic::SemanticSearchOptions::default();
        let chunks = code_intel.search_semantic(query, options)?;
        let hits: Vec<Value> = chunks
            .iter()
            .take(limit)
            .map(|c| {
                json!({
                    "path": c.path,
                    "line_start": c.line_start,
                    "line_end": c.line_end,
                    "score": c.score,
                    "text": c.text,
                })
            })
            .collect();
        Ok(json!({ "hits": hits }))
    }

    fn symbol_references(&self, args: &Value) -> Result<Value> {
        let symbol_id = get_u64(args, "symbol_id")?;
        let code_intel = self.code_intel()?;
        let symbol_idx = code_intel.symbol_index()?;
        let references: Vec<Value> = symbol_idx
            .references(symbol_id)
            .iter()
            .map(|r| json!({ "path": r.path, "line": r.range.line_start }))
            .collect();
        Ok(json!({ "references": references }))
    }

    fn symbol_callers(&self, args: &Value) -> Result<Value> {
        let symbol_id = get_u64(args, "symbol_id")?;
        let code_intel = self.code_intel()?;
        let symbol_idx = code_intel.symbol_index()?;
        let callers: Vec<Value> = symbol_idx
            .callers(symbol_id)
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "name": s.name,
                    "kind": Self::symbol_kind_str(s.kind),
                    "path": s.path,
                    "line_start": s.range.line_start,
                    "line_end": s.range.line_end,
                })
            })
            .collect();
        Ok(json!({ "callers": callers }))
    }

    fn symbol_callees(&self, args: &Value) -> Result<Value> {
        let symbol_id = get_u64(args, "symbol_id")?;
        let code_intel = self.code_intel()?;
        let symbol_idx = code_intel.symbol_index()?;
        let callees: Vec<Value> = symbol_idx
            .callees(symbol_id)
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "name": s.name,
                    "kind": Self::symbol_kind_str(s.kind),
                    "path": s.path,
                    "line_start": s.range.line_start,
                    "line_end": s.range.line_end,
                })
            })
            .collect();
        Ok(json!({ "callees": callees }))
    }

    fn history_why(&self, args: &Value) -> Result<Value> {
        let path = get_str(args, "path")?;
        let line_start = get_u32(args, "line_start")?;
        let line_end = get_u32(args, "line_end")?;
        let code_intel = self.code_intel()?;
        let answer = code_intel.history_why(path, line_start, line_end)?;
        let commits: Vec<Value> = answer
            .commits
            .iter()
            .map(|c| {
                json!({
                    "sha": c.sha,
                    "author": c.author,
                    "authored_at": c.authored_at,
                    "message": c.message,
                })
            })
            .collect();
        Ok(json!({
            "path": answer.path,
            "line_start": answer.line_start,
            "line_end": answer.line_end,
            "commits": commits,
        }))
    }

    fn history_search(&self, args: &Value) -> Result<Value> {
        let query = get_str(args, "query")?;
        let limit = get_limit(args);
        let code_intel = self.code_intel()?;
        let hits: Vec<Value> = code_intel
            .history_search(query)?
            .iter()
            .take(limit)
            .map(|h| {
                json!({
                    "sha": h.commit.sha,
                    "author": h.commit.author,
                    "authored_at": h.commit.authored_at,
                    "message": h.commit.message,
                    "path": h.path,
                    "snippet": h.snippet,
                })
            })
            .collect();
        Ok(json!({ "hits": hits }))
    }

    fn history_deleted(&self, args: &Value) -> Result<Value> {
        let query = get_str(args, "query")?;
        let limit = get_limit(args);
        let code_intel = self.code_intel()?;
        let results: Vec<Value> = code_intel
            .history_deleted(query)?
            .iter()
            .take(limit)
            .map(|d| {
                json!({
                    "path": d.path,
                    "sha": d.commit.sha,
                    "author": d.commit.author,
                    "authored_at": d.commit.authored_at,
                    "message": d.commit.message,
                    "removed_text": d.removed_text,
                })
            })
            .collect();
        Ok(json!({ "results": results }))
    }

    fn execute_tool(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "ticket_list" => self.ticket_list(args),
            "ticket_show" => self.ticket_show(args),
            "ticket_dispatch" => self.ticket_dispatch(args),
            "search_exact" => self.search_exact(args),
            "search_hybrid" => self.search_hybrid(args),
            "search_regex" => self.search_regex(args),
            "search_semantic" => self.search_semantic(args),
            "symbol_def" => self.symbol_def(args),
            "symbol_outline" => self.symbol_outline(args),
            "symbol_references" => self.symbol_references(args),
            "symbol_callers" => self.symbol_callers(args),
            "symbol_callees" => self.symbol_callees(args),
            "history_why" => self.history_why(args),
            "history_search" => self.history_search(args),
            "history_deleted" => self.history_deleted(args),
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
                "content": [{"type": "text", "text": Self::clean_error_message(&e.to_string())}],
                "isError": true,
            }),
        })
    }

    /// Handle one JSON-RPC request, producing exactly the response to send back.
    pub fn handle_request(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        match req.method.as_str() {
            "initialize" => {
                self.remember_client(req.params.as_ref());
                let protocol_version = req
                    .params
                    .as_ref()
                    .and_then(|params| params.get("protocolVersion"))
                    .and_then(Value::as_str);
                JsonRpcResponse::ok(
                    req.id,
                    json!({
                        "protocolVersion": negotiated_protocol_version(protocol_version),
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
        let mut handled_any_message = false;
        loop {
            match transport.recv().await? {
                None => {
                    // Clean EOF: only OK if we handled at least one message. Otherwise, the peer
                    // closed before sending anything, which is an error for a command-line tool
                    // (e.g. `echo '' | tm mcp`).
                    if handled_any_message {
                        return Ok(());
                    } else {
                        return Err(TmError::parse(
                            "peer closed stdin before sending any messages",
                        ));
                    }
                }
                Some(Message::Request(req)) => {
                    handled_any_message = true;
                    let response = self.handle_request(req);
                    transport.send(&Message::Response(response)).await?;
                }
                // A client notification (e.g. `notifications/initialized`) needs no reply; a
                // stray `Response` (this process is never itself a JSON-RPC client on this
                // channel) is likewise not an error, just ignored.
                Some(Message::Notification(_)) | Some(Message::Response(_)) => {
                    handled_any_message = true;
                    continue;
                }
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

    /// A non-git project root (`open_test_server`'s tempdir has no `.git` anywhere above it)
    /// must skip `update_incremental` entirely rather than fail the tool call — the same
    /// degrade-not-fail policy `Project::code_intel()` follows (`nav-fix-project-codeintel-
    /// freshness`).
    #[test]
    fn a_codeintel_backed_tool_still_succeeds_in_a_non_git_project_root() {
        let (server, _dir) = open_test_server();
        let result = call(&server, "search_exact", json!({"query": "anything"}));
        assert_eq!(result["isError"], false, "{result}");
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
    fn handle_request_initialize_negotiates_supported_protocol_versions() {
        let (server, _dir) = open_test_server();
        for (requested, expected) in [
            ("2024-11-05", "2024-11-05"),
            ("2025-03-26", "2025-03-26"),
            ("2025-06-18", "2025-06-18"),
            ("unknown", "2025-06-18"),
        ] {
            let resp = server.handle_request(JsonRpcRequest::new(
                crate::protocol::RequestId::Number(1),
                "initialize",
                Some(json!({"protocolVersion": requested})),
            ));
            let result = resp.result.expect("initialize succeeds");
            assert_eq!(result["protocolVersion"], expected);
        }
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

    #[tokio::test]
    async fn eof_before_any_message_errors() {
        let (server, _dir) = open_test_server();
        // `tokio::io::empty()` returns `Ok(0)` (EOF) on the very first `read`, unlike a
        // `tokio::io::duplex` pair fed to itself (reader and writer from the *same* pair): that
        // would only see EOF once the writer half is dropped, which never happens while both
        // halves are held alive inside the same transport — `recv()` then blocks forever waiting
        // for bytes that are never written, hanging this test instead of exercising the EOF path.
        let reader = tokio::io::empty();
        let (_client_side, writer) = tokio::io::duplex(1024);
        let mut transport =
            crate::transport::FramedTransport::new(reader, writer, Framing::LineDelimited);

        // Calling recv immediately on a closed stream returns Ok(None), signaling EOF.
        // We expect serve_transport to turn this into an error when no message was handled.
        let result = server.serve_transport(&mut transport).await;
        assert!(result.is_err(), "EOF before any message should error");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("peer closed stdin before sending any messages"),
            "error should explain that peer closed before sending messages"
        );
    }

    #[test]
    fn ticket_show_serializes_unlimited_budget_as_null() {
        let (server, _dir) = open_test_server();
        let result = call(&server, "ticket_dispatch", json!({"objective": "test"}));
        assert_eq!(result["isError"], false);
        let ticket_id = payload(&result)["id"].as_str().expect("id").to_string();

        let result = call(&server, "ticket_show", json!({"id": ticket_id}));
        assert_eq!(result["isError"], false);
        let out = payload(&result);

        // Budget should have unlimited components serialized as null
        assert_eq!(out["budget"]["tokens"], Value::Null);
        assert_eq!(out["budget"]["dollars_micros"], Value::Null);
        assert_eq!(out["budget"]["wall_seconds"], Value::Null);
    }

    #[test]
    fn ticket_dispatch_includes_budget_as_null() {
        let (server, _dir) = open_test_server();
        let result = call(&server, "ticket_dispatch", json!({"objective": "test"}));
        assert_eq!(result["isError"], false);
        let out = payload(&result);

        // Budget should be in response and have unlimited components as null
        assert_eq!(out["budget"]["tokens"], Value::Null);
        assert_eq!(out["budget"]["dollars_micros"], Value::Null);
        assert_eq!(out["budget"]["wall_seconds"], Value::Null);
    }

    #[test]
    fn symbol_def_unknown_symbol_is_an_error() {
        let (server, _dir) = open_test_server();
        let result = call(
            &server,
            "symbol_def",
            json!({"name": "nonexistent_symbol_xyz"}),
        );
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().expect("error text");
        // Should not contain internal error prefixes or type names
        assert!(!msg.contains("parse:"));
        assert!(!msg.contains("SymbolKind"));
    }

    #[test]
    fn symbol_kind_str_formats_correctly() {
        use tm_codeintel::symbols::SymbolKind;
        assert_eq!(McpServer::symbol_kind_str(SymbolKind::Function), "function");
        assert_eq!(McpServer::symbol_kind_str(SymbolKind::Struct), "struct");
        assert_eq!(McpServer::symbol_kind_str(SymbolKind::Enum), "enum");
        assert_eq!(
            McpServer::symbol_kind_str(SymbolKind::Interface),
            "interface"
        );
        assert_eq!(McpServer::symbol_kind_str(SymbolKind::Impl), "impl");
        assert_eq!(McpServer::symbol_kind_str(SymbolKind::Module), "module");
        assert_eq!(McpServer::symbol_kind_str(SymbolKind::Variable), "variable");
        assert_eq!(
            McpServer::symbol_kind_str(SymbolKind::TypeAlias),
            "type_alias"
        );
    }

    #[test]
    fn clean_error_message_strips_prefixes() {
        assert_eq!(
            McpServer::clean_error_message("parse: ticket ID must look like T-<n>"),
            "ticket ID must look like T-<n>"
        );
        assert_eq!(
            McpServer::clean_error_message("storage: connection failed"),
            "connection failed"
        );
        assert_eq!(
            McpServer::clean_error_message("io: file not found"),
            "file not found"
        );
        // Message without a prefix should be unchanged
        assert_eq!(
            McpServer::clean_error_message("some error message"),
            "some error message"
        );
    }

    #[test]
    fn ticket_show_on_bad_id_has_clean_error_message() {
        let (server, _dir) = open_test_server();
        let result = call(&server, "ticket_show", json!({"id": "INVALID"}));
        assert_eq!(result["isError"], true);
        let msg = result["content"][0]["text"].as_str().expect("error text");
        // Should not contain "parse: " prefix or type names
        assert!(!msg.contains("parse:"));
        assert!(!msg.contains("TicketId"));
        assert!(msg.contains("ticket ID"));
    }

    /// A small git-backed fixture: `lib.rs` defining `old_fn`, committed, then rewritten to
    /// drop `old_fn` and add `caller`, which calls `helper`, committed again. Exercises
    /// search_regex/search_semantic/symbol_*/history_* against real content and real commits,
    /// the same way `tm-codeintel`'s own `crates/tm-codeintel/src/api.rs` tests build fixtures.
    ///
    /// The workspace (walked/indexed) and the state dir (where `index.db` lives) are two
    /// separate tempdirs, like `open_at_separates_index_dir_from_workspace_and_writes_only_
    /// index_db_there` in `tm-codeintel/src/api.rs` — unlike `open_test_server` above, these
    /// tests actually run `update_incremental` (via `McpServer::code_intel()`), and indexing
    /// `index.db`/`-wal`/`-shm` as part of the workspace it walks would be a self-reference bug
    /// (`nav-fix-codeintel-self-reference`), not a realistic fixture.
    fn open_git_test_server() -> (McpServer, tempfile::TempDir, tempfile::TempDir) {
        let workspace =
            tempfile::tempdir().expect("tempdir creation should not fail in a test sandbox");
        let state_dir =
            tempfile::tempdir().expect("tempdir creation should not fail in a test sandbox");
        let repo = git2::Repository::init(workspace.path()).expect("git init");
        let sig = git2::Signature::new("Test", "test@example.com", &git2::Time::new(0, 0))
            .expect("signature");

        let lib_rs = workspace.path().join("lib.rs");
        std::fs::write(&lib_rs, "fn old_fn() -> i32 {\n    1\n}\n").expect("write lib.rs");
        commit_all(&repo, &sig, "add old_fn");

        std::fs::write(
            &lib_rs,
            "fn helper() -> i32 {\n    42\n}\n\nfn caller() -> i32 {\n    helper()\n}\n",
        )
        .expect("rewrite lib.rs");
        commit_all(&repo, &sig, "remove old_fn, add caller and helper");

        let server = McpServer::new(
            workspace.path().to_path_buf(),
            state_dir.path().to_path_buf(),
        )
        .expect("opening a fresh git fixture project should not fail");
        (server, workspace, state_dir)
    }

    fn commit_all(repo: &git2::Repository, sig: &git2::Signature<'_>, message: &str) {
        let mut index = repo.index().expect("repo index");
        index
            .add_all(["."], git2::IndexAddOption::DEFAULT, None)
            .expect("stage all");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
        repo.commit(Some("HEAD"), sig, sig, message, &tree, &parents)
            .expect("commit");
    }

    #[test]
    fn search_regex_finds_a_pattern_in_a_tracked_file() {
        let (server, _workspace, _state_dir) = open_git_test_server();
        let result = call(&server, "search_regex", json!({"query": "fn helper"}));
        assert_eq!(result["isError"], false, "{result}");
        let out = payload(&result);
        let hits = out["hits"].as_array().expect("hits array");
        assert!(
            hits.iter().any(|h| h["path"] == "lib.rs"),
            "expected a match in lib.rs: {out}"
        );
    }

    #[test]
    fn search_exact_honors_limit_and_reports_truncation() {
        let (server, workspace, _state_dir) = open_git_test_server();
        for i in 0..30 {
            std::fs::write(
                workspace.path().join(format!("f{i}.txt")),
                "mcp_cap_marker\n",
            )
            .expect("write fixture file");
        }
        let result = call(
            &server,
            "search_exact",
            json!({"query": "mcp_cap_marker", "limit": 5}),
        );
        assert_eq!(result["isError"], false, "{result}");
        let out = payload(&result);
        assert_eq!(out["hits"].as_array().expect("hits array").len(), 5);
        assert_eq!(out["truncated"], json!(true));
        assert_eq!(out["total_seen"], json!(30));
        assert!(out["hint"].as_str().unwrap().contains("path_glob"));
    }

    #[test]
    fn search_exact_path_glob_filters_hits() {
        let (server, workspace, _state_dir) = open_git_test_server();
        std::fs::create_dir_all(workspace.path().join("nested")).expect("mkdir");
        std::fs::write(workspace.path().join("nested/marker.txt"), "glob_marker\n")
            .expect("write nested fixture");
        std::fs::write(workspace.path().join("root_marker.txt"), "glob_marker\n")
            .expect("write root fixture");

        let result = call(
            &server,
            "search_exact",
            json!({"query": "glob_marker", "path_glob": "nested/**"}),
        );
        assert_eq!(result["isError"], false, "{result}");
        let out = payload(&result);
        let hits = out["hits"].as_array().expect("hits array");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["path"], "nested/marker.txt");
    }

    #[test]
    fn search_semantic_returns_hits_for_indexed_content() {
        let (server, _workspace, _state_dir) = open_git_test_server();
        let result = call(&server, "search_semantic", json!({"query": "helper"}));
        assert_eq!(result["isError"], false, "{result}");
        let out = payload(&result);
        let hits = out["hits"].as_array().expect("hits array");
        assert!(!hits.is_empty(), "expected at least one hit: {out}");
    }

    #[test]
    fn symbol_def_then_symbol_callers_and_callees_round_trip_a_real_symbol_id() {
        let (server, _workspace, _state_dir) = open_git_test_server();

        let helper_def = payload(&call(
            &server,
            "symbol_def",
            json!({"name": "helper", "from": "lib.rs"}),
        ));
        let helper_id = helper_def["id"].as_u64().expect("helper has an id");
        assert_eq!(helper_def["name"], "helper");
        let helper_def_line = helper_def["line_start"].as_u64().expect("line_start");

        let caller_def = payload(&call(
            &server,
            "symbol_def",
            json!({"name": "caller", "from": "lib.rs"}),
        ));
        let caller_id = caller_def["id"].as_u64().expect("caller has an id");
        assert_ne!(
            caller_id, helper_id,
            "caller and helper must resolve to distinct symbol ids"
        );

        // `helper` is called exactly once, from inside `caller` — nav-fix-codeintel-self-
        // reference already landed, so a definition's own name token must not show up as a
        // reference to itself, and there is no other call site to find.
        let refs = payload(&call(
            &server,
            "symbol_references",
            json!({"symbol_id": helper_id}),
        ));
        let references = refs["references"].as_array().expect("references array");
        assert_eq!(
            references.len(),
            1,
            "expected exactly one reference to helper, the call site inside caller: {refs}"
        );
        assert_eq!(references[0]["path"], "lib.rs");
        assert_ne!(
            references[0]["line"].as_u64(),
            Some(helper_def_line),
            "the reference must be the call site, not helper's own definition line: {refs}"
        );

        let callers = payload(&call(
            &server,
            "symbol_callers",
            json!({"symbol_id": helper_id}),
        ));
        let caller_names: Vec<&str> = callers["callers"]
            .as_array()
            .expect("callers array")
            .iter()
            .map(|c| c["name"].as_str().expect("caller has a name"))
            .collect();
        assert_eq!(
            caller_names,
            vec!["caller"],
            "helper's only caller must be caller(): {callers}"
        );

        let callees = payload(&call(
            &server,
            "symbol_callees",
            json!({"symbol_id": caller_id}),
        ));
        let callee_names: Vec<&str> = callees["callees"]
            .as_array()
            .expect("callees array")
            .iter()
            .map(|c| c["name"].as_str().expect("callee has a name"))
            .collect();
        assert_eq!(
            callee_names,
            vec!["helper"],
            "caller's only callee must be helper(): {callees}"
        );
    }

    #[test]
    fn symbol_references_on_an_unknown_symbol_id_is_an_empty_list_not_an_error() {
        let (server, _workspace, _state_dir) = open_git_test_server();
        let result = call(&server, "symbol_references", json!({"symbol_id": 999_999}));
        assert_eq!(result["isError"], false, "{result}");
        let out = payload(&result);
        assert_eq!(out["references"].as_array().expect("array").len(), 0);
    }

    #[test]
    fn history_search_finds_the_commit_that_added_caller() {
        let (server, _workspace, _state_dir) = open_git_test_server();
        let result = call(&server, "history_search", json!({"query": "caller"}));
        assert_eq!(result["isError"], false, "{result}");
        let out = payload(&result);
        let hits = out["hits"].as_array().expect("hits array");
        assert!(!hits.is_empty(), "expected the caller commit: {out}");
    }

    #[test]
    fn history_deleted_finds_old_fn_removed_in_the_second_commit() {
        let (server, _workspace, _state_dir) = open_git_test_server();
        let result = call(&server, "history_deleted", json!({"query": "old_fn"}));
        assert_eq!(result["isError"], false, "{result}");
        let out = payload(&result);
        let results = out["results"].as_array().expect("results array");
        assert!(
            results.iter().any(|r| r["path"] == "lib.rs"),
            "expected old_fn's removal in lib.rs: {out}"
        );
    }

    #[test]
    fn history_why_reports_a_commit_for_a_current_line() {
        let (server, _workspace, _state_dir) = open_git_test_server();
        let result = call(
            &server,
            "history_why",
            json!({"path": "lib.rs", "line_start": 1, "line_end": 1}),
        );
        assert_eq!(result["isError"], false, "{result}");
        let out = payload(&result);
        let commits = out["commits"].as_array().expect("commits array");
        assert!(!commits.is_empty(), "expected at least one commit: {out}");
    }
}
