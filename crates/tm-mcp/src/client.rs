//! [`McpClient`]: the connection to one external MCP server — the real `initialize` handshake,
//! `tools/list`, and `tools/call`, over whatever [`crate::transport::Transport`] the caller
//! chose.
//!
//! One [`McpClient`] talks to one server. [`crate::capability::McpClientCapability`] wraps one
//! of these as a [`tm_types::CapabilityProvider`], per the audit's "one CapabilityProvider per
//! server" shape.

use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{json, Value};
use tm_types::{Result, TmError};

use crate::protocol::{JsonRpcNotification, JsonRpcRequest, Message, RequestId};
use crate::transport::Transport;

/// This crate's own name/version, sent as `clientInfo` during `initialize`.
const CLIENT_NAME: &str = "tm-mcp";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The MCP protocol version this client speaks. MCP versions its wire schema by date; this is
/// the most recent one whose `tools/list`/`tools/call` shape this client was written against.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// One tool a remote server advertised via `tools/list`, before it is wrapped as a
/// [`tm_types::ToolSchema`] by [`crate::capability::McpClientCapability`].
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteTool {
    /// The tool's wire name, exactly as the server sent it.
    pub name: String,
    /// Human-readable description, possibly empty if the server omitted one.
    pub description: String,
    /// The tool's JSON Schema input shape, exactly as the server sent it.
    pub input_schema: Value,
}

/// A live connection to one external MCP server.
pub struct McpClient {
    transport: Box<dyn Transport>,
    next_id: AtomicI64,
}

impl McpClient {
    /// Perform the real MCP handshake over `transport` (`initialize` request, then a
    /// `notifications/initialized` notification per the spec's required sequence) and fetch the
    /// server's tool list. Returns the connected client plus the tools it advertised, ready to
    /// be wrapped by [`crate::capability::McpClientCapability::new`].
    pub async fn connect(transport: Box<dyn Transport>) -> Result<(Self, Vec<RemoteTool>)> {
        let mut client = McpClient {
            transport,
            next_id: AtomicI64::new(1),
        };
        client.initialize().await?;
        let tools = client.list_tools().await?;
        Ok((client, tools))
    }

    fn next_id(&self) -> RequestId {
        RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst))
    }

    /// Send a request, then read messages until the matching [`crate::protocol::JsonRpcResponse`]
    /// arrives. A response bearing a *different* id, or an inbound request/notification the
    /// server sent unprompted (a progress or logging notification is legal MCP traffic at any
    /// time), is not an error — it is simply not what this call is waiting for, so it is skipped
    /// rather than desynchronizing the read loop.
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id();
        let req = Message::Request(JsonRpcRequest::new(id.clone(), method, params));
        self.transport.send(&req).await?;
        loop {
            match self.transport.recv().await? {
                None => {
                    return Err(TmError::Io(format!(
                        "MCP server closed the connection while awaiting a reply to `{method}`"
                    )))
                }
                Some(Message::Response(resp)) if resp.id == id => {
                    if let Some(err) = resp.error {
                        return Err(TmError::Provider(format!(
                            "MCP server error on `{method}` ({}): {}",
                            err.code, err.message
                        )));
                    }
                    return Ok(resp.result.unwrap_or(Value::Null));
                }
                Some(_stray) => continue,
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        let notif = Message::Notification(JsonRpcNotification::new(method, params));
        self.transport.send(&notif).await
    }

    async fn initialize(&mut self) -> Result<()> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": CLIENT_NAME, "version": CLIENT_VERSION },
        });
        self.request("initialize", Some(params)).await?;
        self.notify("notifications/initialized", None).await?;
        Ok(())
    }

    async fn list_tools(&mut self) -> Result<Vec<RemoteTool>> {
        let result = self.request("tools/list", Some(json!({}))).await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        tools
            .into_iter()
            .map(|t| {
                let name = t
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| TmError::parse("tools/list entry is missing `name`"))?
                    .to_string();
                let description = t
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let input_schema = t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"}));
                Ok(RemoteTool {
                    name,
                    description,
                    input_schema,
                })
            })
            .collect()
    }

    /// Call one remote tool by name, returning its raw `result` value (MCP's `tools/call` result
    /// shape: `{"content": [...], "isError": bool}` — the caller is handed this whole object
    /// rather than an unwrapped payload, since interpreting `content`/`isError` is a
    /// tool-specific concern [`crate::capability::McpClientCapability::invoke`] leaves to its
    /// caller, matching how `tm_types::CapabilityProvider::invoke` already returns a bare
    /// `serde_json::Value` for the builtin capability).
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let params = json!({ "name": name, "arguments": arguments });
        self.request("tools/call", Some(params)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Framing, JsonRpcError, JsonRpcResponse};
    use crate::transport::FramedTransport;

    /// Script a minimal fake MCP server on one half of an in-memory duplex pipe: real framing,
    /// real JSON-RPC shapes, no network and no subprocess. Handles exactly the sequence a real
    /// server would see from [`McpClient::connect`] plus one `tools/call`.
    async fn spawn_fake_server(
        server_io: tokio::io::DuplexStream,
        tool_name: &'static str,
    ) -> tokio::task::JoinHandle<()> {
        let (read, write) = tokio::io::split(server_io);
        let mut server = FramedTransport::new(read, write, Framing::LineDelimited);
        tokio::spawn(async move {
            // initialize
            let Ok(Some(Message::Request(req))) = server.recv().await else {
                return;
            };
            assert_eq!(req.method, "initialize");
            let _ = server
                .send(&Message::Response(JsonRpcResponse::ok(
                    req.id,
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fake", "version": "0.0.0"},
                    }),
                )))
                .await;

            // notifications/initialized
            let Ok(Some(Message::Notification(n))) = server.recv().await else {
                return;
            };
            assert_eq!(n.method, "notifications/initialized");

            // tools/list
            let Ok(Some(Message::Request(req))) = server.recv().await else {
                return;
            };
            assert_eq!(req.method, "tools/list");
            let _ = server
                .send(&Message::Response(JsonRpcResponse::ok(
                    req.id,
                    json!({
                        "tools": [
                            {
                                "name": tool_name,
                                "description": "echoes its input back",
                                "inputSchema": {"type": "object"},
                            }
                        ]
                    }),
                )))
                .await;

            // tools/call
            if let Ok(Some(Message::Request(req))) = server.recv().await {
                assert_eq!(req.method, "tools/call");
                let args = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let _ = server
                    .send(&Message::Response(JsonRpcResponse::ok(
                        req.id,
                        json!({"content": [{"type": "text", "text": args.to_string()}]}),
                    )))
                    .await;
            }
        })
    }

    #[tokio::test]
    async fn connect_performs_the_real_handshake_and_returns_the_advertised_tool() {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let _server = spawn_fake_server(server_io, "echo").await;

        let (client_read, client_write) = tokio::io::split(client_io);
        let transport = FramedTransport::new(client_read, client_write, Framing::LineDelimited);

        let (_client, tools) = McpClient::connect(Box::new(transport))
            .await
            .expect("handshake against a well-behaved fake server should not fail");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description, "echoes its input back");
    }

    #[tokio::test]
    async fn call_tool_round_trips_real_arguments_through_the_fake_server() {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let _server = spawn_fake_server(server_io, "echo").await;

        let (client_read, client_write) = tokio::io::split(client_io);
        let transport = FramedTransport::new(client_read, client_write, Framing::LineDelimited);

        let (mut client, _tools) = McpClient::connect(Box::new(transport))
            .await
            .expect("handshake against a well-behaved fake server should not fail");

        let result = client
            .call_tool("echo", json!({"hello": "world"}))
            .await
            .expect("calling an advertised tool should not fail");

        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .expect("fake server always replies with a text content block");
        assert!(text.contains("world"));
    }

    #[tokio::test]
    async fn request_surfaces_a_json_rpc_error_from_the_server() {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (server_read, server_write) = tokio::io::split(server_io);
        let mut server = FramedTransport::new(server_read, server_write, Framing::LineDelimited);
        tokio::spawn(async move {
            if let Ok(Some(Message::Request(req))) = server.recv().await {
                let _ = server
                    .send(&Message::Response(JsonRpcResponse::err(
                        req.id,
                        JsonRpcError::new(-32601, "method not found"),
                    )))
                    .await;
            }
        });

        let (client_read, client_write) = tokio::io::split(client_io);
        let transport = FramedTransport::new(client_read, client_write, Framing::LineDelimited);
        // Constructed directly (bypassing `connect`'s handshake) to exercise `request`'s error
        // path in isolation; accessible because `tests` is a descendant module of `client`.
        let mut client = McpClient {
            transport: Box::new(transport),
            next_id: AtomicI64::new(1),
        };

        let outcome = client.request("bogus/method", None).await;
        let message = outcome
            .expect_err("the fake server always replies with a JSON-RPC error object")
            .to_string();
        assert!(message.contains("method not found"));
    }
}
