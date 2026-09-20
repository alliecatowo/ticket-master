//! Integration test: spins up the real `tm_acp::server::AcpServer` half over an in-memory pipe
//! and drives it with a minimal, hand-rolled raw-JSON-RPC client (not `tm_acp::client::AcpClient`
//! — deliberately, so this proves wire-level compatibility rather than merely that this crate's
//! two halves agree with each other) — no real subprocess, no real external ACP agent binary.
//!
//! Proves the real round trip the task asks for: `initialize` -> `session/new` ->
//! `session/prompt`, against a *real* ticket seeded in a real (tempdir-backed) `tm_core::Store`,
//! with the reply delivered as a genuine `session/update` `agent_message_chunk` notification
//! interleaved with (arriving before) the `session/prompt` response itself — exactly how a real
//! ACP client must expect the wire to behave, since `session/prompt`'s own response carries only
//! a `stopReason`, never message content.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::tempdir;

use tm_acp::jsonrpc::{read_line, write_message};
use tm_acp::server::{AcpServer, ProjectAgentBackend};
use tm_core::{ExecutorRequirements, RetryPolicy, Store, TicketKind, VerificationPolicy};
use tm_types::{Authority, Budget, CounterIds, FixedClock, ParticipantId, Role, Tolerance};

#[tokio::test]
async fn initialize_session_new_and_session_prompt_round_trip_against_a_real_ticket() {
    let dir = tempdir().expect("tempdir");
    let clock: Arc<dyn tm_types::Clock> = Arc::new(FixedClock::epoch());
    let ids: Arc<dyn tm_types::IdSource> = Arc::new(CounterIds::new());
    let store = Arc::new(Store::open_with(dir.path(), clock, ids.clone()).expect("open store"));

    store
        .create_ticket(
            TicketKind::Work,
            "implement the ACP server round trip".to_string(),
            None,
            None,
            Authority::root(),
            vec![],
            ExecutorRequirements {
                role: Role::CoderFast,
                human_required: false,
                min_capability: Tolerance::Any,
            },
            vec![],
            vec![],
            VerificationPolicy::None,
            Budget::unlimited(),
            RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 1,
                backoff_multiplier: 2.0,
                max_delay_seconds: 60,
            },
            0,
            ParticipantId::system(),
        )
        .expect("create_ticket");

    let backend = Arc::new(ProjectAgentBackend::new(store, ids));
    let server = AcpServer::new(backend);

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, client_write) = tokio::io::split(client_io);

    // Keep `_connection` alive for the whole test — dropping it aborts the server's reader task.
    let _connection = server.attach(server_read, server_write);

    let mut reader = tokio::io::BufReader::new(client_read);
    let mut writer = client_write;

    // --- initialize ---
    write_message(
        &mut writer,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}),
    )
    .await
    .expect("write initialize");
    let line = tokio::time::timeout(Duration::from_secs(5), read_line(&mut reader))
        .await
        .expect("no timeout")
        .expect("read ok")
        .expect("a line, not EOF");
    let resp: Value = serde_json::from_str(&line).expect("valid json");
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["protocolVersion"], 1);
    assert_eq!(resp["result"]["agentInfo"]["name"], "tm-acp");

    // --- session/new ---
    let cwd = dir.path().to_string_lossy().into_owned();
    write_message(
        &mut writer,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": cwd, "mcpServers": []}}),
    )
    .await
    .expect("write session/new");
    let line = tokio::time::timeout(Duration::from_secs(5), read_line(&mut reader))
        .await
        .expect("no timeout")
        .expect("read ok")
        .expect("a line, not EOF");
    let resp: Value = serde_json::from_str(&line).expect("valid json");
    assert_eq!(resp["id"], 2);
    let session_id = resp["result"]["sessionId"]
        .as_str()
        .expect("sessionId is a string")
        .to_string();
    assert!(!session_id.is_empty());

    // --- session/prompt ---
    write_message(
        &mut writer,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "what's the project status?"}]}
        }),
    )
    .await
    .expect("write session/prompt");

    // The real protocol interleaves a `session/update` notification (no `id`) carrying the
    // reply text with the eventual `session/prompt` response (`id: 3`, `stopReason` only, no
    // message content) — read until both have been observed, order-agnostic, since a
    // spec-compliant client must not assume a fixed order either.
    let mut saw_update = false;
    let mut saw_response = false;
    for _ in 0..2 {
        let line = tokio::time::timeout(Duration::from_secs(5), read_line(&mut reader))
            .await
            .expect("no timeout")
            .expect("read ok")
            .expect("a line, not EOF");
        let value: Value = serde_json::from_str(&line).expect("valid json");
        if value.get("method").and_then(Value::as_str) == Some("session/update") {
            saw_update = true;
            assert_eq!(value["params"]["sessionId"], session_id);
            assert_eq!(
                value["params"]["update"]["sessionUpdate"],
                "agent_message_chunk"
            );
            let text = value["params"]["update"]["content"]["text"]
                .as_str()
                .expect("text field");
            assert!(
                text.contains("implement the ACP server round trip"),
                "reply should be derived from the real seeded ticket, got: {text}"
            );
        } else if value.get("id") == Some(&json!(3)) {
            saw_response = true;
            assert_eq!(value["result"]["stopReason"], "end_turn");
        } else {
            panic!("unexpected message: {value}");
        }
    }
    assert!(saw_update, "expected a session/update notification");
    assert!(saw_response, "expected the session/prompt response");
}

#[tokio::test]
async fn session_prompt_for_an_unknown_session_id_is_rejected() {
    let dir = tempdir().expect("tempdir");
    let clock: Arc<dyn tm_types::Clock> = Arc::new(FixedClock::epoch());
    let ids: Arc<dyn tm_types::IdSource> = Arc::new(CounterIds::new());
    let store = Arc::new(Store::open_with(dir.path(), clock, ids.clone()).expect("open store"));
    let backend = Arc::new(ProjectAgentBackend::new(store, ids));
    let server = AcpServer::new(backend);

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, client_write) = tokio::io::split(client_io);
    let _connection = server.attach(server_read, server_write);

    let mut reader = tokio::io::BufReader::new(client_read);
    let mut writer = client_write;

    write_message(
        &mut writer,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/prompt",
            "params": {"sessionId": "never-created", "prompt": [{"type": "text", "text": "hi"}]}
        }),
    )
    .await
    .expect("write session/prompt");
    let line = tokio::time::timeout(Duration::from_secs(5), read_line(&mut reader))
        .await
        .expect("no timeout")
        .expect("read ok")
        .expect("a line, not EOF");
    let resp: Value = serde_json::from_str(&line).expect("valid json");
    assert_eq!(resp["id"], 1);
    assert!(
        resp.get("error").is_some(),
        "expected a JSON-RPC error, got {resp}"
    );
}
