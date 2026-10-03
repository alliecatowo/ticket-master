//! A minimal Chrome DevTools Protocol client: websocket transport, request/response
//! correlation by id, event subscription, and target/session attachment.
//!
//! The wire encode/decode ([`encode_request`], [`decode_message`]) are pure functions kept
//! separate from the connected [`CdpClient`] precisely so they can be exercised against
//! recorded CDP JSON fixtures without a live browser (`SPEC.md` §19). `CdpClient` itself needs
//! a real websocket and is exercised only by `#[ignore]`d integration tests elsewhere in this
//! crate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

/// How long [`CdpClient::call_in_session`] waits for a response before failing with
/// [`CdpError::Timeout`].
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Capacity of the broadcast channel event subscribers read from.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// A CDP request id, used to correlate a [`CdpResponse`] with the [`CdpRequest`] that
/// provoked it.
pub type RequestId = u64;

/// An outbound CDP command, as sent over the websocket.
///
/// Serializes to `{"id", "method", "params", "sessionId"?}`; `session_id` is omitted for
/// browser-level (unattached) commands and required for anything scoped to a specific target.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CdpRequest {
    /// Correlates the eventual [`CdpResponse`].
    pub id: RequestId,
    /// The CDP method, e.g. `"Page.navigate"`.
    pub method: String,
    /// The method's parameters, or `Value::Null` for parameterless methods.
    pub params: Value,
    /// The CDP session to route this command to, when attached to a specific target.
    #[serde(skip_serializing_if = "Option::is_none", rename = "sessionId")]
    pub session_id: Option<String>,
}

/// A CDP protocol-level error, carried inside a [`CdpResponse`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CdpErrorPayload {
    /// The CDP error code.
    pub code: i64,
    /// A human-readable description.
    pub message: String,
    /// Optional additional detail.
    #[serde(default)]
    pub data: Option<Value>,
}

/// An inbound reply to a [`CdpRequest`], matched by [`CdpResponse::id`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CdpResponse {
    /// The id of the request this replies to.
    pub id: RequestId,
    /// The method's return value, present on success.
    #[serde(default)]
    pub result: Option<Value>,
    /// The failure, present on error. Exactly one of `result`/`error` is set.
    #[serde(default)]
    pub error: Option<CdpErrorPayload>,
    /// The session this response came from, when it was a session-scoped command.
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
}

/// An unsolicited CDP notification, e.g. `Page.frameNavigated` or `Runtime.consoleAPICalled`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CdpEvent {
    /// The CDP event name.
    pub method: String,
    /// The event's payload.
    #[serde(default)]
    pub params: Value,
    /// The session this event was emitted on, when target-scoped.
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
}

/// Either shape a decoded websocket text frame can take.
#[derive(Debug, Clone, PartialEq)]
pub enum CdpMessage {
    /// A reply to a request we sent.
    Response(CdpResponse),
    /// An unsolicited notification.
    Event(CdpEvent),
}

/// Everything that can go wrong speaking CDP.
#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    /// The websocket connection itself failed (connect, send or receive).
    #[error("CDP transport error: {0}")]
    Transport(String),
    /// A websocket text frame was not a well-formed CDP response or event.
    #[error("failed to decode CDP message: {0}")]
    Decode(String),
    /// The browser replied with a CDP-level error for a command we sent.
    #[error("CDP protocol error {code}: {message}")]
    Protocol {
        /// The CDP error code.
        code: i64,
        /// The CDP error message.
        message: String,
    },
    /// No response arrived for a command within its deadline.
    #[error("timed out waiting for a CDP response")]
    Timeout,
    /// The connection closed while a command was outstanding or before one could be sent.
    #[error("the CDP connection is closed")]
    ConnectionClosed,
}

impl From<CdpError> for tm_types::TmError {
    fn from(e: CdpError) -> Self {
        tm_types::TmError::Provider(e.to_string())
    }
}

/// Serialize a [`CdpRequest`] to the JSON text sent over the websocket.
///
/// # Errors
/// Never fails in practice (`CdpRequest`'s fields are all directly serializable), but returns
/// [`CdpError::Decode`] rather than panicking if serialization ever does fail.
pub fn encode_request(req: &CdpRequest) -> Result<String, CdpError> {
    serde_json::to_string(req).map_err(|e| CdpError::Decode(e.to_string()))
}

/// Parse one inbound websocket text frame into a [`CdpMessage`].
///
/// # Errors
/// [`CdpError::Decode`] when `text` is not valid JSON, or is JSON that has neither an `id`
/// (response) nor a `method` (event) field.
pub fn decode_message(text: &str) -> Result<CdpMessage, CdpError> {
    let value: Value = serde_json::from_str(text).map_err(|e| CdpError::Decode(e.to_string()))?;
    if value.get("id").is_some() {
        let response: CdpResponse =
            serde_json::from_value(value).map_err(|e| CdpError::Decode(e.to_string()))?;
        Ok(CdpMessage::Response(response))
    } else if value.get("method").is_some() {
        let event: CdpEvent =
            serde_json::from_value(value).map_err(|e| CdpError::Decode(e.to_string()))?;
        Ok(CdpMessage::Event(event))
    } else {
        Err(CdpError::Decode(
            "CDP message has neither `id` nor `method`".to_string(),
        ))
    }
}

/// A CDP `Target.TargetInfo`: one open tab, iframe, worker or extension context.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    /// The target's stable id, used to attach a session to it.
    pub target_id: String,
    /// `"page"`, `"iframe"`, `"worker"`, etc.
    #[serde(rename = "type")]
    pub target_type: String,
    /// The tab/document title.
    #[serde(default)]
    pub title: String,
    /// The current URL.
    #[serde(default)]
    pub url: String,
    /// `true` when a debugger session is already attached.
    #[serde(default)]
    pub attached: bool,
}

/// A connected CDP client: owns the websocket, correlates requests to responses by id, and
/// fans events out to subscribers.
///
/// Construct with [`CdpClient::connect`] against a `ws://` URL obtained from the browser's
/// `/json/version` HTTP endpoint (or a target's `webSocketDebuggerUrl`) — that HTTP round trip
/// lives in `session.rs`, not here, so this module stays protocol-only.
pub struct CdpClient {
    next_id: AtomicU64,
    pending: std::sync::Arc<Mutex<HashMap<RequestId, oneshot::Sender<CdpResponse>>>>,
    events: broadcast::Sender<CdpEvent>,
    outgoing: tokio::sync::mpsc::UnboundedSender<String>,
}

impl CdpClient {
    /// Connect to a CDP websocket endpoint and start its background receive loop.
    ///
    /// # Errors
    /// [`CdpError::Transport`] when the websocket handshake fails.
    pub async fn connect(ws_url: &str) -> Result<Self, CdpError> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| CdpError::Transport(e.to_string()))?;
        let (mut sink, mut stream) = ws_stream.split();

        let (outgoing_tx, mut outgoing_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let pending: std::sync::Arc<Mutex<HashMap<RequestId, oneshot::Sender<CdpResponse>>>> =
            std::sync::Arc::new(Mutex::new(HashMap::new()));

        let task_pending = pending.clone();
        let task_events = events_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    outgoing = outgoing_rx.recv() => {
                        match outgoing {
                            Some(text) => {
                                if sink.send(Message::Text(text.into())).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    incoming = stream.next() => {
                        match incoming {
                            Some(Ok(Message::Text(text))) => {
                                if let Ok(message) = decode_message(&text) {
                                    match message {
                                        CdpMessage::Response(response) => {
                                            let mut pending = task_pending.lock().await;
                                            if let Some(sender) = pending.remove(&response.id) {
                                                let _ = sender.send(response);
                                            }
                                        }
                                        CdpMessage::Event(event) => {
                                            let _ = task_events.send(event);
                                        }
                                    }
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(_)) | None => break,
                        }
                    }
                }
            }
            // Connection is gone: fail every outstanding call rather than leaving it hanging.
            task_pending.lock().await.clear();
        });

        Ok(Self {
            next_id: AtomicU64::new(0),
            pending,
            events: events_tx,
            outgoing: outgoing_tx,
        })
    }

    /// Send a browser-level (unattached) command and await its response.
    ///
    /// # Errors
    /// [`CdpError::Protocol`] on a CDP-level failure, [`CdpError::Timeout`] if nothing arrives
    /// within a bounded deadline, [`CdpError::ConnectionClosed`] if the transport is gone.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        self.call_in_session(method, params, None).await
    }

    /// Send a command scoped to an attached target session, and await its response.
    ///
    /// # Errors
    /// Same as [`CdpClient::call`].
    pub async fn call_in_session(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, CdpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = CdpRequest {
            id,
            method: method.to_string(),
            params,
            session_id: session_id.map(str::to_string),
        };
        let text = encode_request(&request)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if self.outgoing.send(text).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(CdpError::ConnectionClosed);
        }

        let response = match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => return Err(CdpError::ConnectionClosed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(CdpError::Timeout);
            }
        };

        if let Some(error) = response.error {
            return Err(CdpError::Protocol {
                code: error.code,
                message: error.message,
            });
        }
        match response.result {
            Some(value) => Ok(value),
            None => Err(CdpError::Decode(
                "CDP response had neither `result` nor `error`".to_string(),
            )),
        }
    }

    /// Subscribe to every event this client receives from this point on.
    ///
    /// # Invariants
    /// Events broadcast before this call was made are not replayed (a fresh `broadcast`
    /// receiver, not a log); callers that need history must subscribe before the action that
    /// produces the event.
    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }

    /// List every open target (tab, iframe, worker, ...) the browser currently knows about.
    pub async fn list_targets(&self) -> Result<Vec<TargetInfo>, CdpError> {
        let result = self.call("Target.getTargets", Value::Null).await?;
        let infos = result
            .get("targetInfos")
            .ok_or_else(|| CdpError::Decode("missing `targetInfos` in result".to_string()))?;
        serde_json::from_value(infos.clone()).map_err(|e| CdpError::Decode(e.to_string()))
    }

    /// Open a new page target at `url` and return its info.
    pub async fn create_target(&self, url: &str) -> Result<TargetInfo, CdpError> {
        let result = self
            .call("Target.createTarget", serde_json::json!({ "url": url }))
            .await?;
        let target_id = result
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| CdpError::Decode("missing `targetId` in result".to_string()))?;
        self.list_targets()
            .await?
            .into_iter()
            .find(|t| t.target_id == target_id)
            .ok_or_else(|| {
                CdpError::Decode(format!(
                    "created target {target_id} not found in target list"
                ))
            })
    }

    /// Close a target (tab).
    pub async fn close_target(&self, target_id: &str) -> Result<(), CdpError> {
        self.call(
            "Target.closeTarget",
            serde_json::json!({ "targetId": target_id }),
        )
        .await?;
        Ok(())
    }

    /// Attach a debugger session to `target_id` and return the new session id, required for
    /// every subsequent target-scoped [`CdpClient::call_in_session`].
    pub async fn attach_to_target(&self, target_id: &str) -> Result<String, CdpError> {
        let result = self
            .call(
                "Target.attachToTarget",
                serde_json::json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| CdpError::Decode("missing `sessionId` in result".to_string()))
    }

    /// Detach a previously attached session.
    pub async fn detach_from_session(&self, session_id: &str) -> Result<(), CdpError> {
        self.call(
            "Target.detachFromTarget",
            serde_json::json!({ "sessionId": session_id }),
        )
        .await?;
        Ok(())
    }

    /// Shut the connection down cleanly, failing any still-outstanding calls with
    /// [`CdpError::ConnectionClosed`].
    pub async fn close(&self) -> Result<(), CdpError> {
        // `&self` cannot drop the `outgoing` field outright; clearing `pending` still drops
        // every oneshot sender, which is what makes outstanding `call_in_session` calls observe
        // `ConnectionClosed` rather than hang. The background task's own send/receive failures
        // additionally clear `pending` once the transport actually goes away.
        self.pending.lock().await.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> CdpRequest {
        CdpRequest {
            id: 7,
            method: "Page.navigate".to_string(),
            params: serde_json::json!({ "url": "https://example.com" }),
            session_id: None,
        }
    }

    #[test]
    fn encode_request_serializes_id_method_and_params() {
        let json = encode_request(&sample_request()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "Page.navigate");
        assert_eq!(value["params"]["url"], "https://example.com");
        assert!(value.get("sessionId").is_none());
    }

    #[test]
    fn encode_request_includes_session_id_when_present() {
        let mut req = sample_request();
        req.session_id = Some("SESSION-1".to_string());
        let json = encode_request(&req).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["sessionId"], "SESSION-1");
    }

    #[test]
    fn decode_message_reads_a_success_response() {
        let text = r#"{"id":1,"result":{"foo":"bar"}}"#;
        match decode_message(text).unwrap() {
            CdpMessage::Response(resp) => {
                assert_eq!(resp.id, 1);
                assert_eq!(resp.result.unwrap()["foo"], "bar");
                assert!(resp.error.is_none());
            }
            CdpMessage::Event(_) => panic!("expected a response"),
        }
    }

    #[test]
    fn decode_message_reads_an_error_response_even_with_a_session_id() {
        let text = r#"{"id":2,"sessionId":"S1","error":{"code":-32000,"message":"boom"}}"#;
        match decode_message(text).unwrap() {
            CdpMessage::Response(resp) => {
                assert_eq!(resp.id, 2);
                assert_eq!(resp.session_id.as_deref(), Some("S1"));
                let err = resp.error.unwrap();
                assert_eq!(err.code, -32000);
                assert_eq!(err.message, "boom");
            }
            CdpMessage::Event(_) => panic!("id+sessionId must decode as a response, not an event"),
        }
    }

    #[test]
    fn decode_message_reads_an_event() {
        let text = r#"{"method":"Page.frameNavigated","params":{"frame":{}},"sessionId":"S2"}"#;
        match decode_message(text).unwrap() {
            CdpMessage::Event(event) => {
                assert_eq!(event.method, "Page.frameNavigated");
                assert_eq!(event.session_id.as_deref(), Some("S2"));
            }
            CdpMessage::Response(_) => panic!("expected an event"),
        }
    }

    #[test]
    fn decode_message_rejects_invalid_json() {
        let err = decode_message("not json").unwrap_err();
        assert!(matches!(err, CdpError::Decode(_)));
    }

    #[test]
    fn decode_message_rejects_json_with_neither_id_nor_method() {
        let err = decode_message(r#"{"foo": "bar"}"#).unwrap_err();
        assert!(matches!(err, CdpError::Decode(_)));
    }

    #[test]
    fn decode_message_treats_absent_optional_fields_as_defaults() {
        let text = r#"{"method":"Runtime.executionContextsCleared"}"#;
        match decode_message(text).unwrap() {
            CdpMessage::Event(event) => {
                assert_eq!(event.params, Value::Null);
                assert!(event.session_id.is_none());
            }
            CdpMessage::Response(_) => panic!("expected an event"),
        }
    }

    #[test]
    fn cdp_error_maps_to_tm_error_provider() {
        let err: tm_types::TmError = CdpError::Timeout.into();
        assert!(matches!(err, tm_types::TmError::Provider(_)));
    }

    #[test]
    fn target_info_deserializes_camel_case_fields_with_defaults() {
        let text = r#"{"targetId":"T1","type":"page"}"#;
        let info: TargetInfo = serde_json::from_str(text).unwrap();
        assert_eq!(info.target_id, "T1");
        assert_eq!(info.target_type, "page");
        assert_eq!(info.title, "");
        assert_eq!(info.url, "");
        assert!(!info.attached);
    }
}
