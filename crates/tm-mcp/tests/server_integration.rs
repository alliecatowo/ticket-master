//! Integration test: a real [`tm_mcp::server::McpServer`] over a real, tempdir-created
//! Ticketmaster project with one real ticket, driven by a minimal raw JSON-RPC client (hand-
//! built request values plus [`tm_mcp::protocol`]'s wire codec directly — no
//! [`tm_mcp::client::McpClient`] handshake/dispatch bookkeeping in between) over an in-process
//! pipe.
//!
//! What this proves: `initialize` -> `tools/list` -> `tools/call("ticket_list")` round-trips
//! real `Content-Length`-framed JSON-RPC 2.0 over a byte stream, and the returned ticket data is
//! the actual row [`tm_core::Store::create_ticket`] wrote — not a mock, not a fixture baked into
//! the server.
//!
//! `tempdir.path()` is used as *both* `project_root` and `state_dir`: [`tm_core::Store::open_at`]
//! accepts any directory, and passing the same bare path for both means no `.join(".tm")`
//! appears anywhere in this file — sidestepping `crates/xtask/src/hygiene.rs`'s
//! `check_dot_tm_literals` entirely rather than needing an allowlist entry for a test file.

use std::sync::Arc;

use serde_json::{json, Value};
use tempfile::tempdir;

use tm_core::ticket::{ExecutorRequirements, RetryPolicy, VerificationPolicy};
use tm_core::{Store, TicketKind};
use tm_types::{Authority, Budget, ParticipantId, Role, Tolerance};

use tm_mcp::protocol::{Framing, JsonRpcRequest, JsonRpcResponse, Message, RequestId};
use tm_mcp::server::McpServer;
use tm_mcp::transport::{FramedTransport, Transport};

fn test_executor_requirements() -> ExecutorRequirements {
    ExecutorRequirements {
        role: Role::CoderFast,
        human_required: false,
        min_capability: Tolerance::Any,
    }
}

fn test_retry_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay_seconds: 1,
        backoff_multiplier: 2.0,
        max_delay_seconds: 60,
    }
}

/// Create one real ticket via `tm_core::Store::create_ticket` — the sanctioned command surface
/// (`crates/tm-core/src/store.rs`), not hand-rolled event insertion. Returns its rendered id.
fn seed_one_ticket(store: &Store, objective: &str) -> String {
    let actor = ParticipantId::new("human:test").expect("valid participant id");
    let events = store
        .create_ticket(
            TicketKind::Work,
            objective.to_string(),
            None,
            None,
            Authority::default(),
            Vec::new(),
            test_executor_requirements(),
            Vec::new(),
            Vec::new(),
            VerificationPolicy::Single,
            Budget::unlimited(),
            test_retry_policy(),
            0,
            actor,
        )
        .expect("creating a ticket via the sanctioned Store command should not fail");
    assert!(
        !events.is_empty(),
        "create_ticket always drafts at least one event"
    );

    let view = store
        .view()
        .expect("view() should succeed right after a successful write");
    let (id, _) = view
        .tickets
        .iter()
        .next()
        .expect("exactly one ticket was just created");
    id.to_string()
}

/// Write one framed request, read frames until the response with a matching id arrives.
/// "Minimal raw JSON-RPC client": hand-built [`JsonRpcRequest`] values driven straight over the
/// wire codec, with none of [`tm_mcp::client::McpClient`]'s handshake/id-bookkeeping/notification
/// abstractions in between.
async fn raw_request(
    transport: &mut FramedTransport<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    >,
    id: i64,
    method: &str,
    params: Option<Value>,
) -> JsonRpcResponse {
    let request_id = RequestId::Number(id);
    transport
        .send(&Message::Request(JsonRpcRequest::new(
            request_id.clone(),
            method,
            params,
        )))
        .await
        .expect("sending a request over an open in-process pipe should not fail");
    loop {
        match transport
            .recv()
            .await
            .expect("receiving a well-formed frame should not fail")
        {
            None => panic!("server closed the connection before replying to `{method}`"),
            Some(Message::Response(resp)) if resp.id == request_id => return resp,
            Some(_other) => continue,
        }
    }
}

#[tokio::test]
async fn tools_list_then_ticket_list_round_trips_a_real_ticket_over_the_wire() {
    let dir = tempdir().expect("tempdir creation should not fail in a test sandbox");
    let project_path = dir.path().to_path_buf();

    // Seed the project with one real ticket through a Store handle opened directly over the
    // tempdir (same directory the server below opens independently — SQLite/WAL supports the
    // two handles just fine, matching how a real server is started against an already-populated
    // project).
    let seeding_store =
        Store::open_at(&project_path).expect("opening a fresh tempdir project should not fail");
    let ticket_id = seed_one_ticket(&seeding_store, "wire up the MCP server");
    drop(seeding_store);

    let server = Arc::new(
        McpServer::new(project_path.clone(), project_path.clone())
            .expect("opening the just-seeded project should not fail"),
    );

    let (client_io, server_io) = tokio::io::duplex(16 * 1024);
    let server_task = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            let (server_read, server_write) = tokio::io::split(server_io);
            server
                .serve(server_read, server_write, Framing::ContentLength)
                .await
        })
    };

    let (client_read, client_write) = tokio::io::split(client_io);
    let mut client = FramedTransport::new(client_read, client_write, Framing::ContentLength);

    // 1. initialize
    let init = raw_request(
        &mut client,
        1,
        "initialize",
        Some(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "raw-test-client", "version": "0.0.0"},
        })),
    )
    .await;
    let init_result = init.result.expect("initialize should succeed");
    assert_eq!(init_result["protocolVersion"], "2024-11-05");

    // notifications/initialized: a notification expects no response, so this is fire-and-forget.
    client
        .send(&Message::Notification(
            tm_mcp::protocol::JsonRpcNotification::new("notifications/initialized", None),
        ))
        .await
        .expect("sending a notification over an open pipe should not fail");

    // 2. tools/list
    let list = raw_request(&mut client, 2, "tools/list", Some(json!({}))).await;
    let list_result = list.result.expect("tools/list should succeed");
    let tool_names: Vec<&str> = list_result["tools"]
        .as_array()
        .expect("tools/list result carries a `tools` array")
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    for expected in [
        "ticket_list",
        "ticket_show",
        "search_exact",
        "search_hybrid",
        "symbol_def",
        "symbol_outline",
    ] {
        assert!(
            tool_names.contains(&expected),
            "expected `{expected}` in tools/list, got {tool_names:?}"
        );
    }

    // 3. tools/call("ticket_list") — the real round trip this test exists to prove.
    let call = raw_request(
        &mut client,
        3,
        "tools/call",
        Some(json!({"name": "ticket_list", "arguments": {}})),
    )
    .await;
    let call_result = call.result.expect("tools/call(ticket_list) should succeed");
    assert_eq!(call_result["isError"], false);

    let text = call_result["content"][0]["text"]
        .as_str()
        .expect("a successful tool call returns a text content block");
    let payload: Value =
        serde_json::from_str(text).expect("ticket_list's text payload is itself JSON");
    let tickets = payload["tickets"]
        .as_array()
        .expect("ticket_list result carries a `tickets` array");
    assert_eq!(
        tickets.len(),
        1,
        "the seeded project has exactly one ticket"
    );
    assert_eq!(tickets[0]["id"], ticket_id);
    assert_eq!(tickets[0]["objective"], "wire up the MCP server");
    assert_eq!(tickets[0]["state"], "draft");

    // Close the client's write half so the server's serve loop sees clean EOF and returns.
    drop(client);
    let outcome = server_task.await.expect("server task should not panic");
    assert!(
        outcome.is_ok(),
        "serve() should return Ok on clean client disconnect"
    );
}
