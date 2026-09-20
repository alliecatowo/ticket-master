//! The two ways this crate's client half reaches an external MCP server, plus the generic
//! byte-stream adapter both the client and [`crate::server::McpServer`] frame messages over.
//!
//! [`FramedTransport`] is the shared plumbing: given any `AsyncRead + Unpin + Send` and
//! `AsyncWrite + Unpin + Send` half plus a [`crate::protocol::Framing`], it reads/writes
//! [`crate::protocol::Message`]s. [`StdioClientTransport`] wraps a spawned child process's
//! stdio in one; [`crate::server::McpServer::run_stdio`] wraps the real process stdio in
//! another; a test wraps one half of `tokio::io::duplex` in one — all three are the same struct.
//!
//! [`SseClientTransport`] speaks the legacy MCP "HTTP+SSE" transport (a `GET` that streams
//! server-sent events, the first of which names a `POST` endpoint for outgoing messages) — real
//! request/response plumbing over `reqwest`, but its SSE *frame parsing* is factored out as
//! [`parse_sse_events`], a pure function over accumulated bytes, so it is unit-tested without
//! opening any socket (`crates/xtask/src/hygiene.rs`'s `check_network_in_tests` forbids
//! constructing a real network client in test code for exactly the reason `SPEC.md` §0 states:
//! deterministic, offline-testable machinery). No live-server test exists for the HTTP
//! orchestration itself as a result — see this crate's top-level report for that tradeoff
//! stated plainly.

use std::process::Stdio;

use async_trait::async_trait;
use tm_types::{Result, TmError};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::protocol::{encode_frame, try_parse_frame, Framing, Message};

/// One end of a JSON-RPC message channel: send a [`Message`], receive the next one. Object-safe
/// (no generics) so client/capability code can hold `Box<dyn Transport>` without caring whether
/// the peer is a child process's stdio, an SSE connection, or an in-memory pipe in a test.
#[async_trait]
pub trait Transport: Send {
    /// Send one message. Framing/serialization happens here; the caller passes a parsed
    /// [`Message`].
    async fn send(&mut self, message: &Message) -> Result<()>;

    /// Receive the next message, or `Ok(None)` on a clean end-of-stream (the peer closed the
    /// connection with no partial frame pending).
    async fn recv(&mut self) -> Result<Option<Message>>;
}

/// A [`Transport`] over any framed byte stream: reads accumulate into an internal buffer so a
/// [`crate::protocol::try_parse_frame`] call spanning multiple `read()`s (a `Content-Length`
/// body arriving in more than one TCP/pipe segment) is handled correctly, and so more than one
/// frame arriving in a single `read()` is not dropped.
pub struct FramedTransport<R, W> {
    reader: R,
    writer: W,
    framing: Framing,
    read_buf: Vec<u8>,
}

impl<R, W> FramedTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    /// Wrap `reader`/`writer` to speak `framing`.
    pub fn new(reader: R, writer: W, framing: Framing) -> Self {
        FramedTransport {
            reader,
            writer,
            framing,
            read_buf: Vec::new(),
        }
    }
}

#[async_trait]
impl<R, W> Transport for FramedTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    async fn send(&mut self, message: &Message) -> Result<()> {
        let framed = encode_frame(message, self.framing)?;
        self.writer.write_all(&framed).await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Message>> {
        use tokio::io::AsyncReadExt;
        loop {
            if let Some((message, consumed)) = try_parse_frame(&self.read_buf, self.framing)? {
                self.read_buf.drain(..consumed);
                return Ok(Some(message));
            }
            let mut chunk = [0u8; 4096];
            let n = self.reader.read(&mut chunk).await?;
            if n == 0 {
                if self.read_buf.is_empty() {
                    return Ok(None);
                }
                return Err(TmError::parse(
                    "stream ended with an incomplete frame buffered",
                ));
            }
            self.read_buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// A [`Transport`] that spawns an external MCP server as a child process and frames messages
/// over its stdin/stdout. The child's stderr is inherited (left connected to this process's own
/// stderr) so a misbehaving server's diagnostics are visible rather than silently swallowed.
pub struct StdioClientTransport {
    inner: FramedTransport<ChildStdout, ChildStdin>,
    /// Kept alive for the transport's lifetime: dropping it would close the child's stdio and
    /// (via `kill_on_drop`, set at spawn time) terminate the process.
    _child: Child,
}

impl StdioClientTransport {
    /// Spawn `command` (argv, not a shell string — the caller resolves the executable) and frame
    /// its stdio with `framing`. Real MCP servers speak [`Framing::LineDelimited`]; a caller
    /// intentionally testing this crate's own [`crate::server::McpServer`] as a child would use
    /// [`Framing::ContentLength`] to match its default.
    pub fn spawn(command: &[String], framing: Framing) -> Result<Self> {
        let Some((program, args)) = command.split_first() else {
            return Err(TmError::parse("stdio MCP server command must be non-empty"));
        };
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| TmError::Io(format!("failed to spawn MCP server {program:?}: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TmError::invariant("spawned child has no stdin handle"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TmError::invariant("spawned child has no stdout handle"))?;

        Ok(StdioClientTransport {
            inner: FramedTransport::new(stdout, stdin, framing),
            _child: child,
        })
    }
}

#[async_trait]
impl Transport for StdioClientTransport {
    async fn send(&mut self, message: &Message) -> Result<()> {
        self.inner.send(message).await
    }

    async fn recv(&mut self) -> Result<Option<Message>> {
        self.inner.recv().await
    }
}

/// One parsed server-sent event: an optional event name plus its (possibly multi-line) data
/// payload, per the SSE spec's `event:`/`data:` line fields joined by `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, or `"message"` (SSE's own default) when absent.
    pub event: String,
    /// The `data:` field(s), joined by `\n` in encounter order.
    pub data: String,
}

/// Parse as many complete SSE events as are present at the front of `buf`, returning them plus
/// the number of bytes consumed. Leaves a trailing incomplete event (no terminating blank line
/// yet) untouched in `buf` for the next call. Pure and synchronous, per this module's doc
/// comment — no socket, no clock, fully unit-testable.
///
/// Only the two fields MCP's HTTP+SSE transport actually uses (`event`, `data`) are parsed;
/// `id:`/`retry:` lines are accepted (skipped) rather than rejected, matching a real SSE
/// client's tolerance for fields it does not need.
pub fn parse_sse_events(buf: &[u8]) -> (Vec<SseEvent>, usize) {
    let mut events = Vec::new();
    let mut consumed = 0usize;

    loop {
        let remaining = &buf[consumed..];
        // An event ends at a blank line: "\n\n" or "\r\n\r\n".
        let end = find_double_newline(remaining);
        let Some((block_end, sep_len)) = end else {
            break;
        };
        let block = &remaining[..block_end];
        let mut event_name = String::from("message");
        let mut data_lines: Vec<String> = Vec::new();
        for raw_line in block.split(|&b| b == b'\n') {
            let line = strip_trailing_cr(raw_line);
            if line.is_empty() {
                continue;
            }
            let text = String::from_utf8_lossy(line);
            if let Some(rest) = text.strip_prefix("event:") {
                event_name = rest.trim().to_string();
            } else if let Some(rest) = text.strip_prefix("data:") {
                data_lines.push(rest.trim_start().to_string());
            }
            // `id:`/`retry:`/comment (`:`-prefixed) lines are intentionally ignored.
        }
        if !data_lines.is_empty() {
            events.push(SseEvent {
                event: event_name,
                data: data_lines.join("\n"),
            });
        }
        consumed += block_end + sep_len;
    }

    (events, consumed)
}

fn strip_trailing_cr(line: &[u8]) -> &[u8] {
    if line.ends_with(b"\r") {
        &line[..line.len() - 1]
    } else {
        line
    }
}

/// Find the first blank-line separator (`"\n\n"` or `"\r\n\r\n"`) in `buf`, returning
/// `(offset_of_block_end, separator_len)` where `block_end` excludes the separator itself.
fn find_double_newline(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if buf[i] == b'\r'
            && i + 3 < buf.len()
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

/// A [`Transport`] over MCP's legacy HTTP+SSE transport: `GET <sse_url>` opens a streaming
/// response whose first `event: endpoint` names the URL to `POST` subsequent JSON-RPC messages
/// to; server replies and notifications arrive as further SSE events on the same GET stream.
///
/// Real request/response orchestration (`reqwest`), but every byte-level decision is delegated
/// to [`parse_sse_events`] — see this module's doc comment for why no live-server test exists
/// for this struct itself. [`SseClientTransport::ensure_endpoint`]'s relative-URL resolution
/// *is* covered by a pure unit test below (`endpoint_join_resolves_a_relative_path_against_the_sse_url`),
/// since it needs no live connection to exercise.
pub struct SseClientTransport {
    client: reqwest::Client,
    /// The SSE endpoint itself, kept so [`SseClientTransport::ensure_endpoint`] can resolve a
    /// relative `endpoint` event against it (see that method's doc comment).
    base_url: reqwest::Url,
    /// The `POST` endpoint learned from the stream's `endpoint` event, already resolved to an
    /// absolute URL. `None` until [`SseClientTransport::ensure_endpoint`] has read it.
    post_endpoint: Option<String>,
    stream: reqwest::Response,
    buf: Vec<u8>,
}

impl SseClientTransport {
    /// Open the SSE stream at `sse_url`. The `POST` endpoint is not known until the stream's
    /// first event arrives, so it is resolved lazily by [`SseClientTransport::ensure_endpoint`]
    /// on first use rather than here.
    pub async fn connect(sse_url: &str) -> Result<Self> {
        let base_url = reqwest::Url::parse(sse_url)
            .map_err(|e| TmError::parse(format!("invalid SSE URL {sse_url:?}: {e}")))?;
        let client = reqwest::Client::new();
        let stream = client
            .get(base_url.clone())
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| TmError::Provider(format!("SSE connect to {sse_url}: {e}")))?;
        if !stream.status().is_success() {
            return Err(TmError::Provider(format!(
                "SSE connect to {sse_url}: server returned {}",
                stream.status()
            )));
        }
        Ok(SseClientTransport {
            client,
            base_url,
            post_endpoint: None,
            stream,
            buf: Vec::new(),
        })
    }

    async fn next_sse_event(&mut self) -> Result<Option<SseEvent>> {
        loop {
            let (mut events, consumed) = parse_sse_events(&self.buf);
            if !events.is_empty() {
                self.buf.drain(..consumed);
                return Ok(Some(events.remove(0)));
            }
            let Some(chunk) = self
                .stream
                .chunk()
                .await
                .map_err(|e| TmError::Provider(format!("SSE stream read: {e}")))?
            else {
                return Ok(None);
            };
            self.buf.extend_from_slice(&chunk);
        }
    }

    /// Resolve and cache the `POST` endpoint from the stream's `endpoint` event.
    ///
    /// The legacy MCP HTTP+SSE transport commonly sends a bare path here (e.g.
    /// `/messages?sessionId=...`), not an absolute URL — `reqwest` requires an absolute URL for
    /// `Client::post`, so posting `event.data` verbatim would fail on exactly the shape real
    /// servers use (this crate's own test fixture data, `"/session/abc123"`, is deliberately
    /// that shape). [`reqwest::Url::join`] resolves it against `base_url` (the SSE endpoint
    /// itself) the way a browser resolves a relative link; per RFC 3986, `join` also handles a
    /// server that sends an already-absolute URL correctly (the reference wins outright when it
    /// carries its own scheme), so this one call covers both cases a real server might send.
    async fn ensure_endpoint(&mut self) -> Result<String> {
        if let Some(url) = &self.post_endpoint {
            return Ok(url.clone());
        }
        let event = self.next_sse_event().await?.ok_or_else(|| {
            TmError::Provider("SSE stream closed before an endpoint event".into())
        })?;
        if event.event != "endpoint" {
            return Err(TmError::Provider(format!(
                "expected an `endpoint` event first, got `{}`",
                event.event
            )));
        }
        let resolved = self.base_url.join(&event.data).map_err(|e| {
            TmError::parse(format!(
                "endpoint event data {:?} is not a valid URL or path: {e}",
                event.data
            ))
        })?;
        let resolved = resolved.to_string();
        self.post_endpoint = Some(resolved.clone());
        Ok(resolved)
    }
}

#[async_trait]
impl Transport for SseClientTransport {
    async fn send(&mut self, message: &Message) -> Result<()> {
        let endpoint = self.ensure_endpoint().await?;
        let body = message.to_bytes()?;
        let response = self
            .client
            .post(&endpoint)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| TmError::Provider(format!("SSE POST to {endpoint}: {e}")))?;
        if !response.status().is_success() {
            return Err(TmError::Provider(format!(
                "SSE POST to {endpoint}: server returned {}",
                response.status()
            )));
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Message>> {
        loop {
            let Some(event) = self.next_sse_event().await? else {
                return Ok(None);
            };
            if event.event != "message" {
                // A future `endpoint` re-announcement or an unrecognized event kind; not a
                // JSON-RPC payload, so it is not handed to the caller.
                tracing::debug!(event = %event.event, "SseClientTransport: skipping non-message SSE event");
                continue;
            }
            return Ok(Some(Message::from_slice(event.data.as_bytes())?));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{JsonRpcNotification, RequestId};
    use serde_json::json;

    fn sample_message() -> Message {
        Message::Request(crate::protocol::JsonRpcRequest::new(
            RequestId::Number(1),
            "ping",
            Some(json!({})),
        ))
    }

    #[tokio::test]
    async fn framed_transport_round_trips_content_length_over_a_duplex_pipe() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_io);
        let (server_read, server_write) = tokio::io::split(server_io);

        let mut client = FramedTransport::new(client_read, client_write, Framing::ContentLength);
        let mut server = FramedTransport::new(server_read, server_write, Framing::ContentLength);

        client
            .send(&sample_message())
            .await
            .expect("sending over an open duplex pipe cannot fail (24+ chars)");
        let received = server
            .recv()
            .await
            .expect("receiving a well-formed frame cannot fail (24+ chars)")
            .expect("the pipe is still open, a message is pending");
        match received {
            Message::Request(r) => assert_eq!(r.method, "ping"),
            _ => panic!("expected a request"),
        }
    }

    #[tokio::test]
    async fn framed_transport_round_trips_line_delimited_over_a_duplex_pipe() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_io);
        let (server_read, server_write) = tokio::io::split(server_io);

        let mut client = FramedTransport::new(client_read, client_write, Framing::LineDelimited);
        let mut server = FramedTransport::new(server_read, server_write, Framing::LineDelimited);

        client
            .send(&Message::Notification(JsonRpcNotification::new(
                "notifications/initialized",
                None,
            )))
            .await
            .expect("sending over an open duplex pipe cannot fail (24+ chars)");
        let received = server
            .recv()
            .await
            .expect("receiving a well-formed frame cannot fail (24+ chars)")
            .expect("the pipe is still open, a message is pending");
        match received {
            Message::Notification(n) => assert_eq!(n.method, "notifications/initialized"),
            _ => panic!("expected a notification"),
        }
    }

    #[tokio::test]
    async fn framed_transport_recv_returns_none_on_clean_close() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        drop(client_io);
        let (server_read, server_write) = tokio::io::split(server_io);
        let mut server = FramedTransport::new(server_read, server_write, Framing::ContentLength);
        let received = server
            .recv()
            .await
            .expect("a cleanly closed pipe is not a parse error");
        assert!(received.is_none());
    }

    #[test]
    fn parse_sse_events_extracts_a_single_complete_event() {
        let input = b"event: endpoint\ndata: /session/abc123\n\n";
        let (events, consumed) = parse_sse_events(input);
        assert_eq!(consumed, input.len());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "endpoint");
        assert_eq!(events[0].data, "/session/abc123");
    }

    #[test]
    fn parse_sse_events_defaults_event_name_to_message() {
        let input = b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let (events, _) = parse_sse_events(input);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "message");
    }

    #[test]
    fn parse_sse_events_joins_multiple_data_lines_with_newline() {
        let input = b"event: message\ndata: line one\ndata: line two\n\n";
        let (events, _) = parse_sse_events(input);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line one\nline two");
    }

    #[test]
    fn parse_sse_events_leaves_an_incomplete_trailing_event_unconsumed() {
        let input = b"event: message\ndata: partial";
        let (events, consumed) = parse_sse_events(input);
        assert!(events.is_empty());
        assert_eq!(consumed, 0);
    }

    #[test]
    fn parse_sse_events_extracts_two_events_from_one_buffer() {
        let input = b"event: endpoint\ndata: /a\n\nevent: message\ndata: {}\n\n";
        let (events, consumed) = parse_sse_events(input);
        assert_eq!(consumed, input.len());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "endpoint");
        assert_eq!(events[1].event, "message");
    }

    #[test]
    fn parse_sse_events_tolerates_crlf_line_endings() {
        let input = b"event: endpoint\r\ndata: /a\r\n\r\n";
        let (events, consumed) = parse_sse_events(input);
        assert_eq!(consumed, input.len());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "/a");
    }

    #[test]
    fn parse_sse_events_ignores_comment_and_id_lines() {
        let input = b": this is a comment\nid: 42\nevent: message\ndata: hi\n\n";
        let (events, _) = parse_sse_events(input);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hi");
    }

    /// Regression test for the bug an adversarial review caught before handback: the legacy MCP
    /// HTTP+SSE transport commonly sends a bare path as its `endpoint` event data (exactly the
    /// shape every fixture above already uses, `"/session/abc123"`), and posting that verbatim
    /// to `reqwest` fails since it requires an absolute URL. This exercises the exact
    /// `Url::join` call `SseClientTransport::ensure_endpoint` makes, with no live connection —
    /// pure URL parsing, deterministic and offline.
    #[test]
    fn endpoint_join_resolves_a_relative_path_against_the_sse_url() {
        let base = reqwest::Url::parse("http://example.test/sse").expect("valid literal URL");
        let resolved = base
            .join("/session/abc123")
            .expect("a valid relative path joins cleanly");
        assert_eq!(resolved.as_str(), "http://example.test/session/abc123");
    }

    #[test]
    fn endpoint_join_leaves_an_already_absolute_url_unchanged() {
        let base = reqwest::Url::parse("http://example.test/sse").expect("valid literal URL");
        let resolved = base
            .join("http://other.test/messages")
            .expect("an absolute reference joins cleanly");
        assert_eq!(resolved.as_str(), "http://other.test/messages");
    }
}
