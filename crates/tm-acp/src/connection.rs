//! A multiplexed JSON-RPC 2.0 connection over an arbitrary `AsyncRead`/`AsyncWrite` pair.
//!
//! Both ACP client and ACP server halves of this crate are, at the wire level, peers of an
//! identical shape: each can *originate* calls/notifications and must *answer* inbound
//! requests/notifications from the other side, interleaved on the same stream. In particular, a
//! peer waiting on the response to one outbound call (e.g. `session/prompt`) must still be able
//! to answer an inbound request that arrives before that response does (e.g. the agent's
//! `session/request_permission` mid-turn, or a `session/update` notification). A naive
//! "write request, then read exactly one line" loop deadlocks the first time that happens — this
//! module exists specifically to avoid that bug: one background task owns the read side and
//! dispatches every line by shape, while [`Outbound::call`] correlates its own request by id
//! through a pending-response map rather than by "the next line read".

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

use crate::jsonrpc::{
    self, read_line, write_message, IncomingMessage, RequestId, RpcError, RpcNotification,
    RpcRequest, RpcResponse,
};

/// Answers inbound JSON-RPC traffic this connection's peer sends. Implemented once per role
/// (`crate::client::PermissionCallback` for the ACP client's handling of
/// `session/request_permission`/`session/update`; `crate::server::AcpServer` for the agent
/// side's handling of `initialize`/`session/new`/`session/prompt`).
#[async_trait::async_trait]
pub trait RequestHandler: Send + Sync {
    /// Answer an inbound request (has an `id`; the caller is waiting for exactly one response).
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, RpcError>;

    /// Handle an inbound notification (no `id`; nothing is waiting for a reply).
    async fn handle_notification(&self, method: &str, params: Value);
}

/// Why an [`Outbound::call`] did not produce a result.
#[derive(Debug, thiserror::Error)]
pub enum ConnError {
    /// Writing the request (or a response/notification) failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The peer answered with a JSON-RPC error object.
    #[error("remote error: {0}")]
    Remote(#[from] RpcError),
    /// No response arrived within the configured timeout — the caller's job to decide what to
    /// do next (this crate never blocks a caller forever, so a wedged external agent cannot
    /// hold a ticket lease open indefinitely).
    #[error("timed out after {0:?} waiting for a response")]
    Timeout(Duration),
    /// The reader loop exited (peer closed the stream, or a fatal I/O error) while this call was
    /// still pending.
    #[error("the connection closed before a response arrived")]
    ConnectionClosed,
}

/// One outbound call awaiting its response: resolved by the reader task, awaited by
/// [`Outbound::call`].
type PendingSender = oneshot::Sender<Result<Value, RpcError>>;

/// The write side plus outstanding-call bookkeeping, shared (via internal `Arc`s) between
/// whoever originates calls and the reader task that resolves them. Cheap to [`Clone`].
pub struct Outbound<W> {
    writer: Arc<Mutex<W>>,
    next_id: Arc<AtomicI64>,
    pending: Arc<Mutex<HashMap<i64, PendingSender>>>,
}

impl<W> Clone for Outbound<W> {
    fn clone(&self) -> Self {
        Outbound {
            writer: self.writer.clone(),
            next_id: self.next_id.clone(),
            pending: self.pending.clone(),
        }
    }
}

impl<W> Outbound<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    /// Build a fresh outbound handle over `writer`, with no calls in flight yet. Exposed
    /// (alongside [`Connection::from_parts`]) so a [`RequestHandler`] that needs to originate
    /// notifications of its own — `crate::server::AcpServer` sending `session/update` from
    /// inside its own `session/prompt` handling — can be handed a clone of this *before* the
    /// reader task that would dispatch requests to it is spawned, closing the race a
    /// construct-then-mutate-in-place approach would otherwise have.
    pub fn new(writer: W) -> Self {
        Outbound {
            writer: Arc::new(Mutex::new(writer)),
            next_id: Arc::new(AtomicI64::new(1)),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Send `method`/`params` as a request and wait up to `timeout` for the matching response.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ConnError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = RpcRequest::new(RequestId::Number(id), method, params);
        if let Err(e) = write_message(&mut *self.writer.lock().await, &req).await {
            self.pending.lock().await.remove(&id);
            return Err(ConnError::Io(e));
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(rpc_err))) => Err(ConnError::Remote(rpc_err)),
            Ok(Err(_dropped)) => Err(ConnError::ConnectionClosed),
            Err(_elapsed) => {
                self.pending.lock().await.remove(&id);
                Err(ConnError::Timeout(timeout))
            }
        }
    }

    /// Send `method`/`params` as a fire-and-forget notification.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), ConnError> {
        let note = RpcNotification::new(method, params);
        write_message(&mut *self.writer.lock().await, &note)
            .await
            .map_err(ConnError::Io)
    }

    /// Write a success response to an inbound request `id`. Best-effort: a write failure here
    /// only means the peer never sees the answer, which is equivalent (from its perspective) to
    /// the connection having dropped, so it is logged rather than propagated further.
    async fn respond_ok(&self, id: RequestId, result: Value) {
        let resp = RpcResponse::success(id, result);
        if let Err(e) = write_message(&mut *self.writer.lock().await, &resp).await {
            tracing::warn!(error = %e, "failed to write a success response");
        }
    }

    /// Write a failure response to an inbound request `id`.
    async fn respond_err(&self, id: RequestId, err: RpcError) {
        let resp = RpcResponse::failure(id, err);
        if let Err(e) = write_message(&mut *self.writer.lock().await, &resp).await {
            tracing::warn!(error = %e, "failed to write an error response");
        }
    }

    /// Resolve a pending outbound call by the `id` a peer's response echoed back. A response
    /// whose id was never registered (already timed out, already resolved, or the peer echoed
    /// something we never sent) is logged and dropped rather than panicking — a misbehaving or
    /// merely slow peer must never crash this side of the connection.
    async fn resolve(&self, id: RequestId, outcome: Result<Value, RpcError>) {
        let RequestId::Number(n) = id else {
            tracing::warn!(
                ?id,
                "response id was not numeric; this side never mints string ids"
            );
            return;
        };
        match self.pending.lock().await.remove(&n) {
            Some(tx) => {
                let _ = tx.send(outcome);
            }
            None => {
                tracing::warn!(id = n, "response for an id with no pending call (already timed out or duplicate reply)");
            }
        }
    }

    /// Fail every still-pending call with [`ConnError::ConnectionClosed`] — called once the
    /// reader loop observes EOF or a fatal read error, so a call already in flight does not hang
    /// forever waiting for a response that can now never arrive.
    async fn fail_all_pending(&self) {
        // Simply dropping every pending sender (rather than sending an explicit error value)
        // is enough: `Outbound::call`'s `rx.await` sees the sender drop as a `RecvError`, which
        // it already maps to `ConnError::ConnectionClosed` — the correct outcome here, and no
        // `RpcError` needs to be constructed to produce it.
        self.pending.lock().await.clear();
    }
}

/// One end of a multiplexed JSON-RPC connection: an [`Outbound`] handle plus the background
/// reader task that drives it. Dropping this aborts the reader task.
pub struct Connection<W> {
    outbound: Outbound<W>,
    reader_task: JoinHandle<()>,
}

impl<W> Connection<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    /// Start a connection: `reader` is owned entirely by the background dispatch task; `writer`
    /// is shared (behind a mutex) between that task's own responses and anything this
    /// connection's owner calls through [`Connection::call`]/[`Connection::notify`]. `handler`
    /// answers whatever the peer sends that isn't a response to something we asked.
    pub fn spawn<R, H>(reader: R, writer: W, handler: Arc<H>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        H: RequestHandler + 'static,
    {
        Self::from_parts(Outbound::new(writer), reader, handler)
    }

    /// Lower-level constructor: drive `reader` against an already-built [`Outbound`] (see
    /// [`Outbound::new`]'s doc comment for why a caller would want to build the outbound handle
    /// before the reader task that will dispatch requests through `handler` even starts).
    pub fn from_parts<R, H>(outbound: Outbound<W>, reader: R, handler: Arc<H>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        H: RequestHandler + 'static,
    {
        let reader_task = tokio::spawn(reader_loop(reader, outbound.clone(), handler));
        Connection {
            outbound,
            reader_task,
        }
    }

    /// See [`Outbound::call`].
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ConnError> {
        self.outbound.call(method, params, timeout).await
    }

    /// See [`Outbound::notify`].
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), ConnError> {
        self.outbound.notify(method, params).await
    }

    /// A cloneable handle for sending notifications/calls from outside code that doesn't own
    /// this `Connection` directly (e.g. a [`RequestHandler`] that needs to emit a
    /// `session/update` notification while it is still handling a `session/prompt` request —
    /// see `crate::server::AcpServer`, which is constructed with an `Outbound` before the
    /// `Connection` that will drive it even exists, precisely to allow this).
    pub fn outbound(&self) -> Outbound<W> {
        self.outbound.clone()
    }
}

impl<W> Drop for Connection<W> {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

/// The background task body: read ndjson lines from `reader` until EOF/error, classify each,
/// and either dispatch it to `handler` (requests/notifications, each on its own spawned task so
/// a slow handler never blocks reading the next line) or resolve a pending call (responses).
async fn reader_loop<R, W, H>(reader: R, outbound: Outbound<W>, handler: Arc<H>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    H: RequestHandler + 'static,
{
    let mut buffered = BufReader::new(reader);
    loop {
        let line = match read_line(&mut buffered).await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "jsonrpc reader loop: I/O error, closing connection");
                break;
            }
        };

        match jsonrpc::parse_line(&line) {
            Ok(IncomingMessage::Request(req)) => {
                let handler = handler.clone();
                let outbound = outbound.clone();
                tokio::spawn(async move {
                    match handler.handle_request(&req.method, req.params).await {
                        Ok(result) => outbound.respond_ok(req.id, result).await,
                        Err(err) => outbound.respond_err(req.id, err).await,
                    }
                });
            }
            Ok(IncomingMessage::Notification(note)) => {
                let handler = handler.clone();
                tokio::spawn(async move {
                    handler.handle_notification(&note.method, note.params).await;
                });
            }
            Ok(IncomingMessage::Response(resp)) => {
                let outcome = match resp.error {
                    Some(err) => Err(err),
                    None => Ok(resp.result.unwrap_or(Value::Null)),
                };
                outbound.resolve(resp.id, outcome).await;
            }
            Err(e) => {
                // A malformed line from the peer is not our own bug and must not take the whole
                // connection down; log and keep reading.
                tracing::warn!(error = %e, "jsonrpc reader loop: ignoring unparseable line");
            }
        }
    }
    outbound.fail_all_pending().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::duplex;

    /// A handler that answers `"echo"` with its own params and records every notification it
    /// sees, so tests can assert on ordering/content without needing a real peer implementation.
    struct EchoHandler {
        notifications_seen: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl RequestHandler for EchoHandler {
        async fn handle_request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
            match method {
                "echo" => Ok(params),
                "slow_echo" => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok(params)
                }
                other => Err(RpcError::method_not_found(other)),
            }
        }

        async fn handle_notification(&self, _method: &str, _params: Value) {
            self.notifications_seen.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Wires up two `Connection`s over an in-memory duplex pipe, each running `EchoHandler`, so
    /// a test can drive one side and observe the other's real (spawned-task, multiplexed)
    /// dispatch behavior without any subprocess.
    #[allow(clippy::type_complexity)]
    fn wire_pair() -> (
        Connection<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
        Connection<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let (a_io, b_io) = duplex(64 * 1024);
        let (a_read, a_write) = tokio::io::split(a_io);
        let (b_read, b_write) = tokio::io::split(b_io);

        let a_notes = Arc::new(AtomicUsize::new(0));
        let b_notes = Arc::new(AtomicUsize::new(0));

        let a_handler = Arc::new(EchoHandler {
            notifications_seen: a_notes.clone(),
        });
        let b_handler = Arc::new(EchoHandler {
            notifications_seen: b_notes.clone(),
        });

        let a = Connection::spawn(a_read, a_write, a_handler);
        let b = Connection::spawn(b_read, b_write, b_handler);
        (a, b, a_notes, b_notes)
    }

    #[tokio::test]
    async fn call_round_trips_through_a_real_duplex_pipe() {
        let (a, b, _a_notes, _b_notes) = wire_pair();
        let result = a
            .call("echo", serde_json::json!({"x": 1}), Duration::from_secs(1))
            .await
            .expect("call succeeds");
        assert_eq!(result, serde_json::json!({"x": 1}));
        // And the reverse direction works too — both sides are full peers.
        let result = b
            .call("echo", serde_json::json!({"y": 2}), Duration::from_secs(1))
            .await
            .expect("call succeeds");
        assert_eq!(result, serde_json::json!({"y": 2}));
    }

    #[tokio::test]
    async fn unknown_method_returns_a_remote_error_not_a_panic() {
        let (a, _b, ..) = wire_pair();
        let err = a
            .call("no_such_method", Value::Null, Duration::from_secs(1))
            .await
            .expect_err("unknown method fails");
        assert!(matches!(err, ConnError::Remote(_)));
    }

    #[tokio::test]
    async fn notifications_are_delivered_without_blocking_a_concurrent_call() {
        // `b` itself is never called on directly — it just needs to stay alive (not dropped, or
        // its reader task would be aborted and `a`'s notification would never be dispatched).
        let (a, _b, _a_notes, b_notes) = wire_pair();

        // `b`'s notification handler runs via a spawned task independent of any in-flight call
        // on the same connection, so firing one from `a` and then immediately making a slow call
        // must not have the notification wait behind the call (this is the multiplexing property
        // this whole module exists for).
        a.notify("ping", Value::Null).await.expect("notify");
        let result = a
            .call(
                "slow_echo",
                serde_json::json!("still works"),
                Duration::from_secs(1),
            )
            .await
            .expect("call succeeds even while a notification is in flight");
        assert_eq!(result, serde_json::json!("still works"));

        // Give the notification's spawned handler task a moment to run, then check it landed.
        for _ in 0..50 {
            if b_notes.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(b_notes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_inbound_request_is_answered_while_this_side_has_its_own_call_pending() {
        // This is the deadlock this module's doc comment warns about: `a` makes a slow call to
        // `b`, and *while that call is still outstanding*, `b` makes its own call back to `a`.
        // A "write then read one line" implementation would never see `b`'s request because it
        // is blocked reading only the response to its own call; the real multiplexed reader
        // must dispatch it concurrently.
        let (a, b, ..) = wire_pair();

        let a_clone_call = tokio::spawn(async move {
            a.call(
                "slow_echo",
                serde_json::json!("a's call"),
                Duration::from_secs(2),
            )
            .await
        });

        // Give `a`'s slow call a moment to be in flight, then have `b` call back to `a` on the
        // same connection.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let b_result = b
            .call(
                "echo",
                serde_json::json!("b's call"),
                Duration::from_secs(1),
            )
            .await
            .expect("b's call must be answered even though a's call to b is still pending");
        assert_eq!(b_result, serde_json::json!("b's call"));

        let a_result = a_clone_call
            .await
            .expect("task join")
            .expect("a's call eventually completes");
        assert_eq!(a_result, serde_json::json!("a's call"));
    }

    #[tokio::test]
    async fn call_times_out_when_no_response_arrives() {
        /// A handler that never answers `"hang"`, simulating a wedged peer.
        struct HangHandler;
        #[async_trait::async_trait]
        impl RequestHandler for HangHandler {
            async fn handle_request(
                &self,
                _method: &str,
                _params: Value,
            ) -> Result<Value, RpcError> {
                std::future::pending::<()>().await;
                unreachable!()
            }
            async fn handle_notification(&self, _method: &str, _params: Value) {}
        }

        let (a_io, b_io) = duplex(64 * 1024);
        let (a_read, a_write) = tokio::io::split(a_io);
        let (b_read, b_write) = tokio::io::split(b_io);
        let a = Connection::spawn(a_read, a_write, Arc::new(HangHandler));
        let _b = Connection::spawn(b_read, b_write, Arc::new(HangHandler));

        let err = a
            .call("hang", Value::Null, Duration::from_millis(50))
            .await
            .expect_err("times out");
        assert!(matches!(err, ConnError::Timeout(_)));
    }

    #[tokio::test]
    async fn pending_calls_fail_when_the_peer_disconnects() {
        let (a_io, b_io) = duplex(64 * 1024);
        let (a_read, a_write) = tokio::io::split(a_io);
        let (_b_read, b_write) = tokio::io::split(b_io);
        // `b`'s reader half is dropped immediately (simulating a crashed peer that will never
        // respond), while `a` still has a writer able to send a request.
        drop(_b_read);
        drop(b_write);

        let a = Connection::spawn(
            a_read,
            a_write,
            Arc::new(EchoHandler {
                notifications_seen: Arc::new(AtomicUsize::new(0)),
            }),
        );
        let err = a
            .call("echo", Value::Null, Duration::from_secs(2))
            .await
            .expect_err("fails once the peer is gone");
        assert!(matches!(
            err,
            ConnError::ConnectionClosed | ConnError::Io(_)
        ));
    }
}
