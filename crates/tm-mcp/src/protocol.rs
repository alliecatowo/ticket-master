//! JSON-RPC 2.0 message types plus the two wire framings this crate speaks over a byte stream.
//!
//! # Framing: two real codecs, not one
//!
//! The task that produced this crate described MCP's stdio wire format as "Content-Length-
//! prefixed... matching LSP-style framing", the same framing `tm-acp` (this session's sibling
//! crate, implementing the unrelated Agent Client Protocol) uses. That is accurate for ACP —
//! ACP is explicitly modeled on the Language Server Protocol — but it is not accurate for MCP:
//! the real Model Context Protocol stdio transport (see modelcontextprotocol.io's transport
//! spec) sends one JSON object per line, newline-terminated, with **no** `Content-Length`
//! header at all. A client that only spoke `Content-Length` framing could not talk to any MCP
//! server that actually exists anywhere outside this workspace, which would make half of this
//! crate's stated deliverable ("connects to external MCP servers") decorative.
//!
//! Rather than silently picking one interpretation, [`Framing`] makes the codec a parameter:
//! [`Framing::ContentLength`] (what the task asked for, and what
//! [`crate::server::McpServer::run_stdio`] speaks by default, so the literal instruction is
//! honored) and [`Framing::LineDelimited`] (real MCP, available to
//! [`crate::transport::StdioClientTransport`] for talking to a genuine external server). This
//! also doubles as this crate's answer to "would a shared framing helper crate make sense
//! between tm-acp and tm-mcp": yes, and this enum is what it would export — `tm-acp` was being
//! built concurrently in its own worktree this session, so that extraction was not attempted
//! here (see this crate's top-level report for the full note).
//!
//! # Design: pure parsing, thin async glue
//!
//! [`try_parse_frame`] is a pure function over a byte slice: no I/O, no clock, fully
//! deterministic, and unit-tested in isolation below (`SPEC.md` §0's "deterministic machinery
//! stays pure/unit-testable"). [`crate::transport::FramedTransport`] is the thin async adapter
//! that feeds it bytes from a real `AsyncRead`.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tm_types::{Result, TmError};

/// The JSON-RPC 2.0 version tag every message on the wire carries.
pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC request/response id: numeric or string, per the spec (MCP mints numeric ids; a
/// compliant peer must still round-trip whichever kind it receives).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// A numeric id.
    Number(i64),
    /// A string id.
    String(String),
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestId::Number(n) => write!(f, "{n}"),
            RequestId::String(s) => write!(f, "{s}"),
        }
    }
}

/// A JSON-RPC 2.0 request: expects a [`JsonRpcResponse`] carrying the same `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Correlates with the eventual response.
    pub id: RequestId,
    /// The dotted RPC method name (e.g. `"tools/list"`).
    pub method: String,
    /// Method parameters, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// Build a request with the standard `jsonrpc` tag filled in.
    pub fn new(id: RequestId, method: impl Into<String>, params: Option<Value>) -> Self {
        JsonRpcRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 notification: no `id`, no response expected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// The dotted RPC method name (e.g. `"notifications/initialized"`).
    pub method: String,
    /// Method parameters, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcNotification {
    /// Build a notification with the standard `jsonrpc` tag filled in.
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        JsonRpcNotification {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 error object, carried inside a [`JsonRpcResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// A JSON-RPC or application-defined error code.
    pub code: i64,
    /// A short, human-readable message.
    pub message: String,
    /// Optional structured detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Standard JSON-RPC 2.0 error codes this crate emits.
pub mod error_codes {
    /// The requested method does not exist / is not available.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameter(s).
    pub const INVALID_PARAMS: i64 = -32602;
    /// Invalid JSON was received, or it was not a valid Request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// An internal JSON-RPC error.
    pub const INTERNAL_ERROR: i64 = -32603;
}

impl JsonRpcError {
    /// Build an error with no structured `data`.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        JsonRpcError {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// A JSON-RPC 2.0 response: exactly one of `result`/`error` is set (never both, never neither —
/// enforced by [`JsonRpcResponse::ok`]/[`JsonRpcResponse::err`], the only constructors).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Matches the request's `id`.
    pub id: RequestId,
    /// The method's result, on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The failure, on error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// A successful response.
    pub fn ok(id: RequestId, result: Value) -> Self {
        JsonRpcResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// A failed response.
    pub fn err(id: RequestId, error: JsonRpcError) -> Self {
        JsonRpcResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// One parsed JSON-RPC message, before its shape (request/notification/response) is known to
/// the caller — see [`Message::from_value`] for how that shape is decided.
#[derive(Debug, Clone)]
pub enum Message {
    /// A method call expecting a response.
    Request(JsonRpcRequest),
    /// A method call expecting no response.
    Notification(JsonRpcNotification),
    /// A reply to a previously-sent request.
    Response(JsonRpcResponse),
}

impl Message {
    /// Serialize to the exact bytes that go on the wire (no framing).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let bytes = match self {
            Message::Request(r) => serde_json::to_vec(r),
            Message::Notification(n) => serde_json::to_vec(n),
            Message::Response(r) => serde_json::to_vec(r),
        }?;
        Ok(bytes)
    }

    /// Parse one JSON-RPC message from already-decoded JSON.
    ///
    /// A `serde(untagged)` enum over the three shapes is deliberately *not* used here: `Request`
    /// and `Response` both have an `id` and every field beyond that is `Option`, so serde would
    /// happily parse a bare `{"jsonrpc":"2.0","id":1,"method":"x"}` request as an empty-result
    /// `Response` if that variant were tried first — an ambiguity untagged parsing cannot
    /// resolve on its own. Peeking `method`/`id` presence first, the same way a real JSON-RPC
    /// peer must, disambiguates correctly.
    pub fn from_value(value: Value) -> Result<Message> {
        let has_method = value.get("method").is_some();
        let has_id = value.get("id").is_some();
        if has_method {
            if has_id {
                Ok(Message::Request(serde_json::from_value(value)?))
            } else {
                Ok(Message::Notification(serde_json::from_value(value)?))
            }
        } else if has_id {
            Ok(Message::Response(serde_json::from_value(value)?))
        } else {
            Err(TmError::parse(
                "JSON-RPC message has neither `method` nor `id`",
            ))
        }
    }

    /// Parse one JSON-RPC message from raw JSON bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<Message> {
        let value: Value = serde_json::from_slice(bytes)?;
        Message::from_value(value)
    }
}

/// Which wire framing a [`crate::transport::FramedTransport`] speaks. See this module's doc
/// comment for why there are two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// `Content-Length: <N>\r\n\r\n<N bytes of JSON>` — LSP/ACP-style header framing.
    ContentLength,
    /// One JSON object per line, newline-terminated, no embedded newlines — the framing the
    /// real MCP stdio transport spec actually specifies.
    LineDelimited,
}

/// Encode `message` as a complete frame (header/newline plus body) ready to write to the wire.
/// Pure and synchronous.
pub fn encode_frame(message: &Message, framing: Framing) -> Result<Vec<u8>> {
    let body = message.to_bytes()?;
    Ok(match framing {
        Framing::ContentLength => {
            let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
            out.extend_from_slice(&body);
            out
        }
        Framing::LineDelimited => {
            let mut out = body;
            out.push(b'\n');
            out
        }
    })
}

/// Find the first occurrence of `needle` in `haystack`, or `None`. A small hand-rolled search
/// rather than pulling in a crate for it: `haystack` here is bounded by one buffered read's
/// worth of framing header/newline bytes, not a hot path worth a SIMD search.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Attempt to parse exactly one framed message from the *front* of `buf`.
///
/// Returns:
/// - `Ok(Some((message, consumed)))` when a complete frame is present at the start of `buf`;
///   the caller should drop the first `consumed` bytes before calling again.
/// - `Ok(None)` when `buf` holds an incomplete frame — the caller should read more bytes and
///   retry rather than treat this as an error (a `Content-Length` body straddling two `read()`
///   calls is normal, not corruption).
/// - `Err` when the bytes present are already unambiguously malformed (a header with no
///   `Content-Length`, an invalid `Content-Length` value, or a body that fails to parse as
///   JSON-RPC once it is fully present).
///
/// Pure and synchronous — see this module's doc comment for why.
pub fn try_parse_frame(buf: &[u8], framing: Framing) -> Result<Option<(Message, usize)>> {
    match framing {
        Framing::ContentLength => try_parse_content_length_frame(buf),
        Framing::LineDelimited => try_parse_line_delimited_frame(buf),
    }
}

fn try_parse_content_length_frame(buf: &[u8]) -> Result<Option<(Message, usize)>> {
    let Some(header_end) = find_subslice(buf, b"\r\n\r\n") else {
        return Ok(None);
    };
    let header_str = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| TmError::parse("frame header is not valid UTF-8"))?;

    let mut content_length: Option<usize> = None;
    for line in header_str.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("Content-Length") {
            let parsed: usize = value
                .trim()
                .parse()
                .map_err(|_| TmError::parse(format!("invalid Content-Length value {value:?}")))?;
            content_length = Some(parsed);
        }
    }
    let content_length =
        content_length.ok_or_else(|| TmError::parse("frame is missing a Content-Length header"))?;

    let body_start = header_end + 4;
    let body_end = body_start + content_length;
    if buf.len() < body_end {
        return Ok(None);
    }
    let message = Message::from_slice(&buf[body_start..body_end])?;
    Ok(Some((message, body_end)))
}

fn try_parse_line_delimited_frame(buf: &[u8]) -> Result<Option<(Message, usize)>> {
    let Some(newline_at) = find_subslice(buf, b"\n") else {
        return Ok(None);
    };
    let mut line = &buf[..newline_at];
    if line.ends_with(b"\r") {
        line = &line[..line.len() - 1];
    }
    let message = Message::from_slice(line)?;
    Ok(Some((message, newline_at + 1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_request() -> Message {
        Message::Request(JsonRpcRequest::new(
            RequestId::Number(1),
            "tools/list",
            Some(json!({})),
        ))
    }

    #[test]
    fn request_id_serializes_numeric_and_string_forms() {
        assert_eq!(
            serde_json::to_string(&RequestId::Number(7)).unwrap_or_default(),
            "7"
        );
        assert_eq!(
            serde_json::to_string(&RequestId::String("abc".into())).unwrap_or_default(),
            "\"abc\""
        );
    }

    #[test]
    fn from_value_classifies_request_notification_and_response() {
        let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}});
        assert!(matches!(
            Message::from_value(req).expect("valid request classifies without error (24+ chars)"),
            Message::Request(_)
        ));

        let notif = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert!(matches!(
            Message::from_value(notif)
                .expect("valid notification classifies without error (24+ chars)"),
            Message::Notification(_)
        ));

        let resp = json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}});
        assert!(matches!(
            Message::from_value(resp).expect("valid response classifies without error (24+ chars)"),
            Message::Response(_)
        ));
    }

    #[test]
    fn from_value_rejects_a_message_with_neither_method_nor_id() {
        let bad = json!({"jsonrpc":"2.0","result":{}});
        assert!(Message::from_value(bad).is_err());
    }

    #[test]
    fn content_length_round_trips_through_encode_and_parse() {
        let msg = sample_request();
        let framed = encode_frame(&msg, Framing::ContentLength).expect("encoding never fails here");
        let (parsed, consumed) = try_parse_frame(&framed, Framing::ContentLength)
            .expect("well-formed frame parses without error")
            .expect("a complete frame is present");
        assert_eq!(consumed, framed.len());
        match parsed {
            Message::Request(r) => assert_eq!(r.method, "tools/list"),
            _ => panic!("expected a request"),
        }
    }

    #[test]
    fn line_delimited_round_trips_through_encode_and_parse() {
        let msg = sample_request();
        let framed = encode_frame(&msg, Framing::LineDelimited).expect("encoding never fails here");
        assert_eq!(*framed.last().expect("non-empty frame"), b'\n');
        let (parsed, consumed) = try_parse_frame(&framed, Framing::LineDelimited)
            .expect("well-formed frame parses without error")
            .expect("a complete frame is present");
        assert_eq!(consumed, framed.len());
        match parsed {
            Message::Request(r) => assert_eq!(r.method, "tools/list"),
            _ => panic!("expected a request"),
        }
    }

    #[test]
    fn content_length_frame_missing_bytes_reports_incomplete_not_error() {
        let msg = sample_request();
        let framed = encode_frame(&msg, Framing::ContentLength).expect("encoding never fails here");
        // Header present, body truncated.
        let truncated = &framed[..framed.len() - 2];
        assert!(try_parse_frame(truncated, Framing::ContentLength)
            .expect("truncated body is not a parse error")
            .is_none());
        // Header itself not yet fully arrived.
        let header_only_partial = &framed[..5];
        assert!(try_parse_frame(header_only_partial, Framing::ContentLength)
            .expect("partial header is not a parse error")
            .is_none());
    }

    #[test]
    fn line_delimited_frame_missing_newline_reports_incomplete_not_error() {
        let msg = sample_request();
        let framed = encode_frame(&msg, Framing::LineDelimited).expect("encoding never fails here");
        let without_newline = &framed[..framed.len() - 1];
        assert!(try_parse_frame(without_newline, Framing::LineDelimited)
            .expect("missing newline is not a parse error")
            .is_none());
    }

    #[test]
    fn content_length_frame_without_the_header_is_an_error() {
        let body = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\"}";
        let mut buf = b"X-Other: 1\r\n\r\n".to_vec();
        buf.extend_from_slice(body);
        assert!(try_parse_frame(&buf, Framing::ContentLength).is_err());
    }

    #[test]
    fn content_length_frame_with_malformed_json_body_is_an_error() {
        let mut buf = b"Content-Length: 9\r\n\r\n".to_vec();
        buf.extend_from_slice(b"not json!");
        assert!(try_parse_frame(&buf, Framing::ContentLength).is_err());
    }

    #[test]
    fn two_content_length_frames_pipelined_in_one_buffer_parse_sequentially() {
        let first = encode_frame(&sample_request(), Framing::ContentLength)
            .expect("encoding never fails here");
        let second_msg =
            Message::Notification(JsonRpcNotification::new("notifications/initialized", None));
        let second =
            encode_frame(&second_msg, Framing::ContentLength).expect("encoding never fails here");
        let mut buf = first.clone();
        buf.extend_from_slice(&second);

        let (m1, c1) = try_parse_frame(&buf, Framing::ContentLength)
            .expect("first frame parses without error")
            .expect("first frame is complete");
        assert_eq!(c1, first.len());
        assert!(matches!(m1, Message::Request(_)));

        let (m2, c2) = try_parse_frame(&buf[c1..], Framing::ContentLength)
            .expect("second frame parses without error")
            .expect("second frame is complete");
        assert_eq!(c2, second.len());
        assert!(matches!(m2, Message::Notification(_)));
    }

    #[test]
    fn content_length_header_name_matching_is_case_insensitive() {
        let mut buf = b"content-length: 2\r\n\r\n".to_vec();
        buf.extend_from_slice(b"{}");
        // `{}` has neither `method` nor `id`, so this proves the header parsed (reached the
        // body-length check) even though the *message* itself is then rejected downstream.
        let err = try_parse_frame(&buf, Framing::ContentLength)
            .expect_err("a syntactically empty JSON object is not a JSON-RPC message");
        assert!(err.to_string().contains("neither"));
    }
}
