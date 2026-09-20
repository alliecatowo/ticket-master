//! JSON-RPC 2.0 message shapes and wire framing, kept independent of anything ACP-specific so
//! it can be unit-tested in complete isolation from a real subprocess (per this crate's own
//! test-plan requirement).
//!
//! # Framing: newline-delimited JSON, not Content-Length-prefixed
//!
//! The Agent Client Protocol is JSON-RPC 2.0 "over stdio (stdin/stdout)": the editor spawns the
//! agent as a subprocess and the two exchange messages bidirectionally. Unlike the Language
//! Server Protocol, ACP does **not** use LSP-style `Content-Length:`-prefixed framing — it uses
//! newline-delimited JSON (ndjson): exactly one compact JSON value per line, `\n`-terminated.
//! This was confirmed against the real spec (`agentclientprotocol.com`'s protocol pages plus the
//! downloaded `schema.json` from `agentclientprotocol/agent-client-protocol`'s latest release,
//! cross-checked against public ACP implementer write-ups) rather than assumed — the schema
//! itself is silent on transport framing (it only defines message *shapes*), so the framing
//! choice here rests on the docs/implementer-consensus evidence, not the schema file. This
//! module implements ndjson accordingly; see `docs/decisions/D-004-acp-wire-framing.md` for the
//! full record of that choice, including the honest caveat about what the schema alone does and
//! does not establish.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// The only `jsonrpc` version this crate speaks or accepts.
pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC request/response id: either a number or a string, per the JSON-RPC 2.0 spec.
/// This crate always mints numeric ids for its own outbound calls (see
/// [`crate::connection::Outbound::call`]), but must still be able to parse a string id from a
/// spec-compliant peer without failing closed on a shape it merely doesn't itself produce.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// A numeric id (this crate's own convention for calls it originates).
    Number(i64),
    /// A string id, accepted from a peer that chooses this shape.
    String(String),
}

/// A JSON-RPC request: expects a response correlated by `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    /// Always [`JSONRPC_VERSION`].
    pub jsonrpc: String,
    /// Correlates this request with its eventual response.
    pub id: RequestId,
    /// The dotted method name, e.g. `"initialize"` or `"session/prompt"`.
    pub method: String,
    /// Method parameters. `Value::Null` when a method takes none.
    #[serde(default)]
    pub params: Value,
}

/// A JSON-RPC notification: fire-and-forget, no response expected or possible (no `id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcNotification {
    /// Always [`JSONRPC_VERSION`].
    pub jsonrpc: String,
    /// The dotted method name, e.g. `"session/update"`.
    pub method: String,
    /// Notification parameters.
    #[serde(default)]
    pub params: Value,
}

/// A JSON-RPC error object, carried inside [`RpcResponse::error`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message} (code {code})")]
pub struct RpcError {
    /// A JSON-RPC or application-defined error code.
    pub code: i64,
    /// A short, human-readable description.
    pub message: String,
    /// Optional structured detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Standard JSON-RPC 2.0 reserved error codes this crate actually produces.
pub mod error_codes {
    /// The method does not exist or is not available on this side of the connection.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameter(s) — e.g. they failed to deserialize into the expected shape.
    pub const INVALID_PARAMS: i64 = -32602;
    /// An internal error on the responding side (never used to describe a *caller* mistake).
    pub const INTERNAL_ERROR: i64 = -32603;
}

impl RpcError {
    /// Build a [`error_codes::METHOD_NOT_FOUND`] error naming `method`.
    pub fn method_not_found(method: &str) -> Self {
        RpcError {
            code: error_codes::METHOD_NOT_FOUND,
            message: format!("method not found: {method}"),
            data: None,
        }
    }

    /// Build an [`error_codes::INVALID_PARAMS`] error wrapping `detail`.
    pub fn invalid_params(detail: impl std::fmt::Display) -> Self {
        RpcError {
            code: error_codes::INVALID_PARAMS,
            message: format!("invalid params: {detail}"),
            data: None,
        }
    }

    /// Build an [`error_codes::INTERNAL_ERROR`] error wrapping `detail`.
    pub fn internal(detail: impl std::fmt::Display) -> Self {
        RpcError {
            code: error_codes::INTERNAL_ERROR,
            message: detail.to_string(),
            data: None,
        }
    }
}

/// A JSON-RPC response: exactly one of `result`/`error` is set, correlated to a prior request by
/// `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    /// Always [`JSONRPC_VERSION`].
    pub jsonrpc: String,
    /// The id of the request this responds to.
    pub id: RequestId,
    /// Present on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Present on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    /// Build a success response.
    pub fn success(id: RequestId, result: Value) -> Self {
        RpcResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Build a failure response.
    pub fn failure(id: RequestId, error: RpcError) -> Self {
        RpcResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

impl RpcRequest {
    /// Build a request. `id` is left to the caller (`Outbound::call` mints one).
    pub fn new(id: RequestId, method: impl Into<String>, params: Value) -> Self {
        RpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

impl RpcNotification {
    /// Build a notification.
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        RpcNotification {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

/// One line of ndjson input, classified by shape (JSON-RPC distinguishes request/notification/
/// response by which fields are present, not by an explicit discriminator).
#[derive(Debug, Clone)]
pub enum IncomingMessage {
    /// Has both `method` and `id`: expects a response.
    Request(RpcRequest),
    /// Has `method` but no `id`: fire-and-forget.
    Notification(RpcNotification),
    /// Has `id` and (`result` or `error`) but no `method`: a reply to something we sent.
    Response(RpcResponse),
}

/// Why a raw ndjson line could not be classified as a well-formed JSON-RPC 2.0 message.
#[derive(Debug, Clone, thiserror::Error)]
pub enum FramingError {
    /// The line was not valid JSON at all.
    #[error("malformed JSON: {0}")]
    InvalidJson(String),
    /// The line was valid JSON but not a JSON object.
    #[error("expected a JSON object, got: {0}")]
    NotAnObject(String),
    /// The line was an object but matched none of request/notification/response shape (no
    /// `method`, and no `id` alongside a `result`/`error`).
    #[error("neither a request, a notification, nor a response: {0}")]
    UnrecognizedShape(String),
    /// A `method`+`id` or `id`+`result`/`error` combination was present but did not deserialize
    /// into the expected concrete type.
    #[error("recognized shape but failed to deserialize: {0}")]
    Malformed(String),
}

/// Classify and parse one raw line (already stripped of its trailing newline) into an
/// [`IncomingMessage`]. Fails closed: a line that matches no known shape is a [`FramingError`],
/// never silently coerced into some default.
pub fn parse_line(line: &str) -> Result<IncomingMessage, FramingError> {
    let value: Value =
        serde_json::from_str(line).map_err(|e| FramingError::InvalidJson(e.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| FramingError::NotAnObject(line.to_string()))?;

    if obj.contains_key("method") {
        if obj.contains_key("id") {
            let req: RpcRequest = serde_json::from_value(value.clone())
                .map_err(|e| FramingError::Malformed(e.to_string()))?;
            Ok(IncomingMessage::Request(req))
        } else {
            let note: RpcNotification = serde_json::from_value(value.clone())
                .map_err(|e| FramingError::Malformed(e.to_string()))?;
            Ok(IncomingMessage::Notification(note))
        }
    } else if obj.contains_key("id") && (obj.contains_key("result") || obj.contains_key("error")) {
        let resp: RpcResponse = serde_json::from_value(value.clone())
            .map_err(|e| FramingError::Malformed(e.to_string()))?;
        Ok(IncomingMessage::Response(resp))
    } else {
        Err(FramingError::UnrecognizedShape(line.to_string()))
    }
}

/// Serialize `value` as one compact JSON line (no embedded newline is possible from
/// `serde_json`'s compact writer) and write it, `\n`-terminated, flushing so a peer blocked on
/// reading a line sees it promptly rather than sitting in an internal buffer.
pub async fn write_message<W, T>(writer: &mut W, value: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

/// Read one ndjson line from `reader`, skipping blank lines (defensive against a peer that emits
/// stray newlines between messages). Returns `Ok(None)` at EOF.
pub async fn read_line<R>(reader: &mut R) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let mut buf = String::new();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = buf.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        return Ok(Some(trimmed.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_id_round_trips_number_and_string() {
        let n = RequestId::Number(42);
        let s = RequestId::String("abc".to_string());
        assert_eq!(
            serde_json::from_str::<RequestId>(&serde_json::to_string(&n).unwrap()).unwrap(),
            n
        );
        assert_eq!(
            serde_json::from_str::<RequestId>(&serde_json::to_string(&s).unwrap()).unwrap(),
            s
        );
    }

    #[test]
    fn parse_line_classifies_a_request() {
        let line = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": 1}
        })
        .to_string();
        match parse_line(&line).expect("parses") {
            IncomingMessage::Request(req) => {
                assert_eq!(req.id, RequestId::Number(1));
                assert_eq!(req.method, "initialize");
                assert_eq!(req.params["protocolVersion"], 1);
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_classifies_a_notification() {
        let line = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": "s-1"}
        })
        .to_string();
        match parse_line(&line).expect("parses") {
            IncomingMessage::Notification(note) => {
                assert_eq!(note.method, "session/update");
            }
            other => panic!("expected Notification, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_classifies_a_success_response() {
        let line = json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}}).to_string();
        match parse_line(&line).expect("parses") {
            IncomingMessage::Response(resp) => {
                assert_eq!(resp.id, RequestId::Number(7));
                assert_eq!(resp.result, Some(json!({"ok": true})));
                assert!(resp.error.is_none());
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_classifies_an_error_response() {
        let line = json!({
            "jsonrpc": "2.0",
            "id": "req-2",
            "error": {"code": -32601, "message": "method not found"}
        })
        .to_string();
        match parse_line(&line).expect("parses") {
            IncomingMessage::Response(resp) => {
                assert_eq!(resp.id, RequestId::String("req-2".to_string()));
                assert!(resp.result.is_none());
                let err = resp.error.expect("error present");
                assert_eq!(err.code, -32601);
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_rejects_malformed_json() {
        let err = parse_line("{not json").unwrap_err();
        assert!(matches!(err, FramingError::InvalidJson(_)));
    }

    #[test]
    fn parse_line_rejects_a_json_array() {
        let err = parse_line("[1,2,3]").unwrap_err();
        assert!(matches!(err, FramingError::NotAnObject(_)));
    }

    #[test]
    fn parse_line_rejects_an_unrecognized_object_shape() {
        // Neither a method, nor an id+result/error pairing.
        let line = json!({"jsonrpc": "2.0", "foo": "bar"}).to_string();
        let err = parse_line(&line).unwrap_err();
        assert!(matches!(err, FramingError::UnrecognizedShape(_)));
    }

    #[test]
    fn parse_line_rejects_a_response_with_id_but_no_result_or_error() {
        // Has an id, but neither result nor error: not a valid response, and no method either.
        let line = json!({"jsonrpc": "2.0", "id": 1}).to_string();
        let err = parse_line(&line).unwrap_err();
        assert!(matches!(err, FramingError::UnrecognizedShape(_)));
    }

    #[tokio::test]
    async fn write_message_then_read_line_round_trips() {
        let mut buf: Vec<u8> = Vec::new();
        let req = RpcRequest::new(
            RequestId::Number(1),
            "initialize",
            json!({"protocolVersion": 1}),
        );
        write_message(&mut buf, &req).await.expect("write");

        // Exactly one newline-terminated line was written, and it parses back to an equivalent
        // request.
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 1);
        let mut reader = tokio::io::BufReader::new(buf.as_slice());
        let line = read_line(&mut reader)
            .await
            .expect("read")
            .expect("some line");
        match parse_line(&line).expect("parses") {
            IncomingMessage::Request(parsed) => {
                assert_eq!(parsed.method, "initialize");
                assert_eq!(parsed.id, RequestId::Number(1));
            }
            other => panic!("expected Request, got {other:?}"),
        }

        // EOF after the one line.
        assert_eq!(read_line(&mut reader).await.expect("read"), None);
    }

    #[tokio::test]
    async fn read_line_skips_blank_lines_between_messages() {
        let raw = b"\n\n{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{}}\n\n";
        let mut reader = tokio::io::BufReader::new(&raw[..]);
        let line = read_line(&mut reader)
            .await
            .expect("read")
            .expect("some line");
        assert!(line.contains("session/update"));
        assert_eq!(read_line(&mut reader).await.expect("read"), None);
    }

    #[tokio::test]
    async fn read_line_reads_multiple_ndjson_messages_in_order() {
        let raw = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"a\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"b\"}\n";
        let mut reader = tokio::io::BufReader::new(&raw[..]);
        let first = read_line(&mut reader).await.expect("read").expect("line 1");
        let second = read_line(&mut reader).await.expect("read").expect("line 2");
        assert!(first.contains("\"method\":\"a\""));
        assert!(second.contains("\"method\":\"b\""));
        assert_eq!(read_line(&mut reader).await.expect("read"), None);
    }
}
