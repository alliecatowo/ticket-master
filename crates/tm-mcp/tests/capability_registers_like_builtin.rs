//! Proves the sentence `docs/audit-2026-09-18-fable.md` B-12 states as the target shape: a
//! connected MCP server's tools, wrapped as [`tm_mcp::capability::McpClientCapability`], are
//! visible to `tm_agent::tools::ToolRegistry` — Ticketmaster's real tool-dispatch machinery —
//! exactly like any built-in capability crate's tools are, gated by the same
//! [`tm_types::AuthorityRequirement`] mechanism `tm-browser`/`tm-computer`/`tm-pty` already use.
//!
//! `tm-agent` is a dev-dependency only (see `Cargo.toml`): this crate's own runtime code never
//! depends on it, only this proof does.

use std::sync::Arc;

use serde_json::json;
use tempfile::tempdir;

use tm_agent::tools::ToolRegistry;
use tm_core::Store;
use tm_types::{Authority, CapabilityProvider, NetworkAuthority};

use tm_mcp::capability::McpClientCapability;
use tm_mcp::client::McpClient;
use tm_mcp::protocol::{Framing, JsonRpcResponse, Message};
use tm_mcp::transport::{FramedTransport, Transport};

/// Script a minimal fake MCP server (real framing, real JSON-RPC shapes) advertising one tool
/// named `mcp_echo`, over one half of an in-memory duplex pipe.
async fn spawn_fake_server(server_io: tokio::io::DuplexStream) {
    let (read, write) = tokio::io::split(server_io);
    let mut server = FramedTransport::new(read, write, Framing::LineDelimited);
    tokio::spawn(async move {
        let Ok(Some(Message::Request(req))) = server.recv().await else {
            return;
        };
        let _ = server
            .send(&Message::Response(JsonRpcResponse::ok(
                req.id,
                json!({"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake", "version": "0"}}),
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
                json!({"tools": [{"name": "mcp_echo", "description": "echoes input", "inputSchema": {"type": "object"}}]}),
            )))
            .await;
    });
}

async fn connect_capability(server_id: &str) -> McpClientCapability {
    let (client_io, server_io) = tokio::io::duplex(8192);
    spawn_fake_server(server_io).await;
    let (client_read, client_write) = tokio::io::split(client_io);
    let transport = FramedTransport::new(client_read, client_write, Framing::LineDelimited);
    let (client, tools) = McpClient::connect(Box::new(transport))
        .await
        .expect("handshake against the fake server should not fail");
    McpClientCapability::new(server_id.to_string(), client, tools)
}

#[tokio::test]
async fn registry_admits_the_mcp_tool_only_for_authority_that_permits_this_server() {
    let dir = tempdir().expect("tempdir creation should not fail in a test sandbox");
    let store = Arc::new(
        Store::open_at(dir.path()).expect("opening a fresh tempdir project should not fail"),
    );

    let capability = connect_capability("acme").await;
    let provider: Arc<dyn CapabilityProvider> = Arc::new(capability);

    // `ToolRegistry::new` (not `::with_capabilities`, which also wires the builtin capability
    // and needs `CommandCache`/`CommandExecutor` mocks this proof does not need) takes exactly
    // `Vec<Arc<dyn CapabilityProvider>>` — the MCP client capability is registered with no
    // special-casing at all, the claim this test exists to check.
    let registry = ToolRegistry::new(vec![provider], store);

    let mut permitted = Authority::none();
    permitted.network = NetworkAuthority {
        docs: false,
        arbitrary: false,
        allowlist: std::collections::BTreeSet::from(["acme".to_string()]),
    };
    let admitted_names: Vec<String> = registry
        .tool_defs_for(&permitted)
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(
        admitted_names.contains(&"mcp_echo".to_string()),
        "an authority allowlisting this server's id should see its tool, got {admitted_names:?}"
    );

    let denied = Authority::none();
    let denied_names: Vec<String> = registry
        .tool_defs_for(&denied)
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(
        !denied_names.contains(&"mcp_echo".to_string()),
        "an authority with no network grant at all should not see the tool, got {denied_names:?}"
    );
}
