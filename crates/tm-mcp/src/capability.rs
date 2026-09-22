//! [`McpClientCapability`]: wraps one connected [`crate::client::McpClient`] as a
//! [`tm_types::CapabilityProvider`], so Ticketmaster's own tool-dispatch machinery
//! (`tm_agent::tools::ToolRegistry`) sees a remote MCP server's tools exactly like it sees any
//! built-in capability crate — `docs/audit-2026-09-18-fable.md` B-12's "one CapabilityProvider
//! per server".
//!
//! # Authority mapping: `Action::NetFetch` on a synthetic `mcp://<server>/<tool>` URL
//!
//! `tm_types::Action` has no MCP-shaped variant, and adding one would mean editing a type shared
//! by every other capability crate (`tm-browser`/`tm-computer`/`tm-pty`, plus whatever the
//! concurrently-developed `tm-acp` track needed from the same enum this session) for a single
//! new client — a change with a much wider blast radius than this crate's own scope. Calling a
//! remote server's tool, whether over a spawned stdio subprocess or an SSE connection, is in
//! both cases "reach something outside this process's own address space to get work done" —
//! exactly what [`tm_types::Action::NetFetch`] already exists to gate. This capability maps
//! every tool call to `Action::NetFetch { url: "mcp://<server_id>/<tool>" }`; a worker's
//! `Authority.network.allowlist` must contain the server's id (or hold `network.arbitrary`) to
//! actually invoke a tool, giving one operator-controlled name per connected server without
//! touching `tm_types::Action`/`Authority` at all. `NetworkAuthority::host_of` is a plain
//! `"://".split(1).split('/').next()` — it treats `server_id` as the URL's host, so this holds
//! for any `server_id` that itself contains neither `://` nor `/`
//! (`McpClientCapability::new`'s `server_id` parameter is a slug the operator picks when wiring
//! up a connection, the same trust level as a `tm-browser`-managed channel name).
//!
//! One asymmetry worth naming rather than discovering later: [`requires`] is
//! [`tm_types::AuthorityRequirement::Network`], which admits (for *listing* purposes) an
//! authority holding `network.docs` alone — but `NetworkAuthority::permits_url` (the *call-time*
//! check `Authority::permits` runs against the synthesized URL) requires `arbitrary` or the
//! specific `server_id` in the allowlist. A worker with only `network.docs` therefore sees this
//! provider's tools listed but is denied at call time. `tm_browser::capability` already has this
//! exact shape (coarse `Network` listing gate, precise per-URL call-time check) — see that
//! module's own doc comment — so this is following established precedent, not introducing a new
//! gap.
//!
//! # `ToolSchema::name`/`description` are `&'static str`; remote tool names are not
//!
//! [`tm_types::capability::ToolSchema`] was designed around capability crates whose tool names
//! are compile-time literals (`"fs.read"`, `"browser.click"`, ...). A connected MCP server's
//! tool names are learned at runtime from `tools/list`. Rather than widen `ToolSchema` itself
//! (shared infrastructure several other tracks depend on this session), [`McpClientCapability::new`]
//! leaks each tool's name/description once, in the constructor, via `Box::leak` — turning an
//! owned `String` into a `&'static str` at the one-time cost of that memory never being freed.
//! This is bounded and deliberate: it happens exactly once per tool a server advertises at
//! connect time (typically single digits to low dozens), never per call, never per
//! [`tm_types::CapabilityProvider::tools`] invocation (which [`ToolRegistry`] documents as
//! callable more than once — the leaked strings are computed once in the constructor and the
//! resulting [`tm_types::ToolSchema`] vec is cloned on every `tools()` call, not re-leaked).
//! A long-running process that connects and disconnects from many thousands of distinct MCP
//! servers over its lifetime would accumulate leaked memory; a single long-lived process
//! connected to a bounded, operator-configured set of servers (this crate's actual use case)
//! does not.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tm_types::{
    Action, AuthorityRequirement, CallContext, CapabilityProvider, CostClass, Result, ToolSchema,
};
use tokio::sync::Mutex as AsyncMutex;

use crate::client::{McpClient, RemoteTool};

/// One connected MCP server, exposed as a [`tm_types::CapabilityProvider`].
///
/// `client` is behind an `Arc<Mutex<_>>` rather than owned by value because
/// [`tm_types::CapabilityProvider::invoke`] takes `&self` (registries hold providers behind
/// `Arc<dyn CapabilityProvider>`, shared across concurrent dispatches) while
/// [`McpClient::call_tool`] needs `&mut McpClient` to drive its transport. The mutex also
/// correctly serializes concurrent calls against one server: MCP request/response correlation
/// here is a synchronous send-then-await-matching-id loop (see
/// `crate::client::McpClient::request`'s doc comment), so two calls racing on the same
/// transport without this lock could each read the other's response.
pub struct McpClientCapability {
    id: String,
    server_id: String,
    client: Arc<AsyncMutex<McpClient>>,
    tools: Vec<ToolSchema>,
    /// Owned copy of every advertised tool name, for a cheap, leak-free "is this actually one of
    /// ours" membership check independent of the leaked `&'static str`s inside `tools`.
    tool_names: HashMap<String, ()>,
}

impl McpClientCapability {
    /// Wrap an already-[`McpClient::connect`]ed client and the tool list that connection
    /// returned. `server_id` names this connection for [`CapabilityProvider::id`] (rendered as
    /// `"mcp:<server_id>"`) and for the synthetic `Action::NetFetch` host this provider's
    /// `to_action` maps every call to — see this module's doc comment.
    pub fn new(server_id: String, client: McpClient, remote_tools: Vec<RemoteTool>) -> Self {
        let mut tool_names = HashMap::new();
        let tools = remote_tools
            .into_iter()
            .map(|t| {
                tool_names.insert(t.name.clone(), ());
                let name: &'static str = Box::leak(t.name.into_boxed_str());
                let description: &'static str = Box::leak(t.description.into_boxed_str());
                ToolSchema {
                    name,
                    description,
                    input_schema: t.input_schema,
                    // Reaching a remote server is real work, but read-only tool families
                    // (`ticket.*`/`search.*`/`symbol.*` on the server side this client would
                    // typically connect to) are not state-mutating from *this* worker's
                    // perspective, so `Moderate` (a recomputation/subprocess-shaped cost) fits
                    // better than `Mutating`. A future server-declared cost hint (MCP has no
                    // such field today) could refine this per tool.
                    cost: CostClass::Moderate,
                    // The provider-level `requires()` below already carries the real gate; see
                    // this module's doc comment for why per-tool stays `Always`.
                    requires: AuthorityRequirement::Always,
                }
            })
            .collect();
        McpClientCapability {
            id: format!("mcp:{server_id}"),
            server_id,
            client: Arc::new(AsyncMutex::new(client)),
            tools,
            tool_names,
        }
    }

    /// Whether `tool` is one this connection actually advertised (as opposed to, say, a stale
    /// call from a cached tool surface after a reconnect changed the server's tool list).
    pub fn has_tool(&self, tool: &str) -> bool {
        self.tool_names.contains_key(tool)
    }
}

#[async_trait]
impl CapabilityProvider for McpClientCapability {
    fn id(&self) -> &str {
        &self.id
    }

    fn tools(&self) -> Vec<ToolSchema> {
        self.tools.clone()
    }

    fn to_action(&self, tool: &str, _input: &Value) -> Result<Action> {
        Ok(Action::NetFetch {
            url: format!("mcp://{}/{}", self.server_id, tool),
        })
    }

    fn requires(&self) -> AuthorityRequirement {
        AuthorityRequirement::Network
    }

    async fn invoke(&self, tool: &str, input: Value, _ctx: &CallContext<'_>) -> Result<Value> {
        let mut client = self.client.lock().await;
        client.call_tool(tool, input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Framing, JsonRpcResponse, Message};
    use crate::transport::{FramedTransport, Transport};
    use serde_json::json;
    use tm_types::{Authority, NetworkAuthority};

    /// Same fake-peer pattern as `client.rs`'s tests: a real handshake, real framing, over an
    /// in-memory duplex pipe — no network, no subprocess.
    async fn connected_capability(server_id: &str) -> McpClientCapability {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (server_read, server_write) = tokio::io::split(server_io);
        let mut server = FramedTransport::new(server_read, server_write, Framing::LineDelimited);
        tokio::spawn(async move {
            let Ok(Some(Message::Request(req))) = server.recv().await else {
                return;
            };
            let _ = server
                .send(&Message::Response(JsonRpcResponse::ok(
                    req.id,
                    json!({"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name":"fake","version":"0"}}),
                )))
                .await;
            let Ok(Some(Message::Notification(_))) = server.recv().await else {
                return;
            };
            let Ok(Some(Message::Request(req))) = server.recv().await else {
                return;
            };
            let _ = server
                .send(&Message::Response(JsonRpcResponse::ok(
                    req.id,
                    json!({"tools": [{"name": "echo", "description": "", "inputSchema": {"type": "object"}}]}),
                )))
                .await;
            if let Ok(Some(Message::Request(req))) = server.recv().await {
                let _ = server
                    .send(&Message::Response(JsonRpcResponse::ok(
                        req.id,
                        json!({"content": [{"type": "text", "text": "ok"}]}),
                    )))
                    .await;
            }
        });

        let (client_read, client_write) = tokio::io::split(client_io);
        let transport = FramedTransport::new(client_read, client_write, Framing::LineDelimited);
        let (client, tools) = McpClient::connect(Box::new(transport))
            .await
            .expect("handshake against the fake server should not fail");
        McpClientCapability::new(server_id.to_string(), client, tools)
    }

    #[tokio::test]
    async fn tools_exposes_the_remote_tool_with_a_leaked_static_name() {
        let cap = connected_capability("acme").await;
        let tools = cap.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert!(cap.has_tool("echo"));
        assert!(!cap.has_tool("nonexistent"));
    }

    #[tokio::test]
    async fn tools_is_stable_across_repeated_calls_without_re_leaking() {
        let cap = connected_capability("acme").await;
        let first = cap.tools();
        let second = cap.tools();
        assert_eq!(first.len(), second.len());
        assert_eq!(first[0].name, second[0].name);
    }

    #[tokio::test]
    async fn id_is_namespaced_by_server_id() {
        let cap = connected_capability("acme").await;
        assert_eq!(cap.id(), "mcp:acme");
    }

    #[tokio::test]
    async fn to_action_maps_to_net_fetch_with_a_synthetic_mcp_url() {
        let cap = connected_capability("acme").await;
        let action = cap
            .to_action("echo", &json!({}))
            .expect("to_action is a pure, infallible mapping for this provider");
        match action {
            Action::NetFetch { url } => assert_eq!(url, "mcp://acme/echo"),
            other => panic!("expected NetFetch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn requires_is_network_and_gates_listing_the_way_authority_permits_would_gate_calling() {
        let cap = connected_capability("acme").await;
        assert_eq!(cap.requires(), AuthorityRequirement::Network);

        let mut allowed = Authority::none();
        allowed.network = NetworkAuthority {
            docs: false,
            arbitrary: false,
            allowlist: std::collections::BTreeSet::from(["acme".to_string()]),
        };
        assert!(cap.requires().admits(&allowed));
        assert!(allowed.network.permits_url("mcp://acme/echo"));

        let mut listed_but_not_permitted = Authority::none();
        listed_but_not_permitted.network.docs = true;
        assert!(cap.requires().admits(&listed_but_not_permitted));
        assert!(!listed_but_not_permitted
            .network
            .permits_url("mcp://acme/echo"));
    }

    #[tokio::test]
    async fn invoke_round_trips_a_real_call_through_the_fake_server() {
        let cap = connected_capability("acme").await;
        let ticket = tm_types::TicketId::new("T-1").expect("valid ticket id");
        let session = tm_types::SessionId::new("S-1").expect("valid session id");
        let actor = tm_types::ParticipantId::new("agent:test/1").expect("valid participant id");
        let authority = Authority::none();
        let clock = tm_types::FixedClock::epoch();
        let ids = tm_types::TestIds::new();
        let root = std::env::temp_dir();
        let ctx = CallContext {
            authority: &authority,
            ticket: Some(&ticket),
            session: &session,
            actor: &actor,
            clock: &clock,
            ids: &ids,
            root: &root,
        };

        let result = cap
            .invoke("echo", json!({"x": 1}), &ctx)
            .await
            .expect("the fake server always answers `tools/call` successfully");
        assert_eq!(
            result
                .get("content")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str),
            Some("ok")
        );
    }
}
