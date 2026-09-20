//! Integration test: drives the real `tm_acp::client::AcpClient` against a hand-rolled fake
//! external ACP agent (raw JSON-RPC over one half of an in-memory pipe — no real subprocess)
//! that sends *two* `session/request_permission` requests **while the client's own
//! `session/prompt` call is still outstanding**, before finally answering that prompt.
//!
//! This is the scenario `tm_acp::connection`'s module doc comment calls out by name: a naive
//! "write the request, then read exactly one line for its response" client would deadlock the
//! moment the fake agent tries to call back, because it would be blocked reading only the
//! `session/prompt` response and would never see the permission request at all. This test proves
//! the real multiplexed reader dispatches the inbound permission requests concurrently, and that
//! each one is answered by consulting `Authority::permits` (via `tm_acp::permission`) rather than
//! being auto-approved or auto-denied — one in-scope edit (approved) and one out-of-scope edit
//! (denied), so both polarities are exercised in the same turn.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use tm_acp::client::AcpClient;
use tm_acp::jsonrpc::{read_line, write_message};
use tm_acp::protocol::{ContentBlock, StopReason};
use tm_types::{Authority, PatternSet, RepoAuthority};

#[tokio::test]
async fn client_answers_mid_turn_permission_requests_via_authority_permits_without_deadlocking() {
    let (agent_io, client_io) = tokio::io::duplex(64 * 1024);
    let (agent_read, agent_write) = tokio::io::split(agent_io);
    let (client_read, client_write) = tokio::io::split(client_io);

    let cwd = PathBuf::from("/repo");
    let authority = Authority {
        repository: RepoAuthority {
            read: PatternSet::all(),
            write: PatternSet::parse(["src/**"]).unwrap(),
        },
        ..Authority::none()
    };

    let client = AcpClient::connect(
        client_read,
        client_write,
        authority,
        cwd,
        Duration::from_secs(5),
    );

    let agent_task = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(agent_read);
        let mut writer = agent_write;

        // initialize
        let req = next_request(&mut reader).await;
        assert_eq!(req["method"], "initialize");
        respond(&mut writer, &req, json!({"protocolVersion": 1})).await;

        // session/new
        let req = next_request(&mut reader).await;
        assert_eq!(req["method"], "session/new");
        respond(&mut writer, &req, json!({"sessionId": "s-1"})).await;

        // session/prompt: the client is now blocked awaiting this response. Before answering
        // it, call back twice for permission — this is the multiplexing this test exists to
        // prove.
        let prompt_req = next_request(&mut reader).await;
        assert_eq!(prompt_req["method"], "session/prompt");

        // 1) an in-scope edit: Authority permits `src/**` writes, so `allow` must be selected.
        let outcome = request_permission(
            &mut reader,
            &mut writer,
            9001,
            json!({
                "sessionId": "s-1",
                "toolCall": {
                    "toolCallId": "call-1",
                    "kind": "edit",
                    "locations": [{"path": "/repo/src/lib.rs"}]
                },
                "options": [
                    {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"}
                ]
            }),
        )
        .await;
        assert_eq!(outcome["outcome"], "selected");
        assert_eq!(outcome["optionId"], "allow");

        // 2) an out-of-scope edit: Authority denies anything outside `src/**`, so `reject` must
        // be selected instead — proving this isn't just "always pick the first option".
        let outcome = request_permission(
            &mut reader,
            &mut writer,
            9002,
            json!({
                "sessionId": "s-1",
                "toolCall": {
                    "toolCallId": "call-2",
                    "kind": "edit",
                    "locations": [{"path": "/repo/outside/lib.rs"}]
                },
                "options": [
                    {"optionId": "allow2", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "reject2", "name": "Reject", "kind": "reject_once"}
                ]
            }),
        )
        .await;
        assert_eq!(outcome["outcome"], "selected");
        assert_eq!(outcome["optionId"], "reject2");

        // Only now stream the reply and answer the original `session/prompt` call.
        write_message(
            &mut writer,
            &json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s-1",
                    "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "done editing"}}
                }
            }),
        )
        .await
        .expect("write session/update");
        respond(&mut writer, &prompt_req, json!({"stopReason": "end_turn"})).await;
    });

    let init = client.initialize().await.expect("initialize");
    assert_eq!(init.protocol_version, 1);

    let session = client.new_session().await.expect("session/new");
    assert_eq!(session.session_id, "s-1");

    // This call would hang forever against a non-multiplexed client, since the fake agent above
    // will not answer it until it has made (and received answers to) its own two nested calls.
    let prompt_resp = tokio::time::timeout(
        Duration::from_secs(5),
        client.prompt(
            &session.session_id,
            vec![ContentBlock::text("please edit the widget")],
        ),
    )
    .await
    .expect("session/prompt must not deadlock behind the agent's own nested calls")
    .expect("session/prompt succeeds");
    assert_eq!(prompt_resp.stop_reason, StopReason::EndTurn);

    let transcript = client.drain_transcript().await;
    assert_eq!(transcript, vec!["done editing".to_string()]);

    agent_task.await.expect("fake agent task panicked");
}

/// Read one line from the fake agent's side and parse it as a JSON-RPC request.
async fn next_request(reader: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> Value {
    let line = tokio::time::timeout(Duration::from_secs(5), read_line(reader))
        .await
        .expect("no timeout")
        .expect("read ok")
        .expect("a line, not EOF");
    serde_json::from_str(&line).expect("valid json")
}

/// Write a success response to `req` carrying `result`.
async fn respond(writer: &mut (impl tokio::io::AsyncWrite + Unpin), req: &Value, result: Value) {
    write_message(
        writer,
        &json!({"jsonrpc": "2.0", "id": req["id"], "result": result}),
    )
    .await
    .expect("write response");
}

/// Send a `session/request_permission` request with `params` (id `id`) and return the
/// `outcome` object from the client's response.
async fn request_permission(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    id: i64,
    params: Value,
) -> Value {
    write_message(
        writer,
        &json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": params}),
    )
    .await
    .expect("write session/request_permission");
    let line = tokio::time::timeout(Duration::from_secs(5), read_line(reader))
        .await
        .expect("no timeout")
        .expect("read ok")
        .expect("a line, not EOF");
    let value: Value = serde_json::from_str(&line).expect("valid json");
    assert_eq!(value["id"], id);
    value["result"]["outcome"].clone()
}
