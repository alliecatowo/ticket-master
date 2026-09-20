//! The ACP **client**: connects to an external ACP-speaking agent over stdio and drives
//! `initialize` -> `session/new` -> `session/prompt`, answering the agent's
//! `session/request_permission` callbacks from `Authority::permits` (`crate::permission`) rather
//! than auto-approving or auto-denying every tool call. This is the half `crate::executor`
//! wraps as the `acp` `tm_core::Executor`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use tm_types::Authority;

use crate::connection::{Connection, RequestHandler};
use crate::error::AcpError;
use crate::jsonrpc::RpcError;
use crate::permission::{self, PermissionDecision};
use crate::protocol::{
    self, methods, ContentBlock, Implementation, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, RequestPermissionRequest,
    RequestPermissionResponse, SessionNotification, SessionUpdate,
};

/// Default per-call timeout: how long `AcpClient` waits for a response to any single
/// `initialize`/`session/new`/`session/prompt` call before treating the agent as wedged (see
/// `crate::connection::Outbound::call`'s doc comment — nothing in this crate blocks forever).
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Answers the two ACP methods/notifications the *client* side of the protocol must serve:
/// `session/request_permission` (a request; answered via `Authority::permits`) and
/// `session/update` (a notification; accumulated so [`AcpClient::drain_transcript`] can hand the
/// agent's reply text back to a caller).
struct ClientHandler {
    authority: Authority,
    cwd: PathBuf,
    transcript: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl RequestHandler for ClientHandler {
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            methods::SESSION_REQUEST_PERMISSION => {
                let req: RequestPermissionRequest =
                    serde_json::from_value(params).map_err(RpcError::invalid_params)?;
                let decision = permission::evaluate(&self.authority, &req.tool_call, &self.cwd);
                let outcome = permission::choose_option(&decision, &req.options);
                tracing::info!(
                    tool_call_id = %req.tool_call.tool_call_id,
                    kind = ?req.tool_call.kind,
                    allowed = matches!(decision, PermissionDecision::Allow),
                    "answered session/request_permission via Authority::permits"
                );
                let resp = RequestPermissionResponse { outcome };
                serde_json::to_value(resp).map_err(|e| RpcError::internal(e.to_string()))
            }
            other => Err(RpcError::method_not_found(other)),
        }
    }

    async fn handle_notification(&self, method: &str, params: Value) {
        if method != methods::SESSION_UPDATE {
            tracing::debug!(method, "unhandled ACP notification");
            return;
        }
        let note: SessionNotification = match serde_json::from_value(params) {
            Ok(note) => note,
            Err(e) => {
                tracing::warn!(error = %e, "received a malformed session/update notification");
                return;
            }
        };
        if let SessionUpdate::AgentMessageChunk(chunk) = note.update {
            if let Some(text) = chunk.content.text {
                self.transcript.lock().await.push(text);
            }
        }
    }
}

/// A live connection to one external ACP agent, either a spawned subprocess (the real usage,
/// [`AcpClient::spawn`]) or an arbitrary reader/writer pair (used by this crate's own
/// multiplexing integration test — [`AcpClient::connect`] — to exercise the exact same request/
/// permission-callback logic without a real agent binary).
pub struct AcpClient {
    connection: Connection<Box<dyn AsyncWrite + Unpin + Send>>,
    handler: Arc<ClientHandler>,
    cwd: PathBuf,
    timeout: Duration,
    child: Option<Child>,
}

impl AcpClient {
    fn from_io(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
        authority: Authority,
        cwd: PathBuf,
        timeout: Duration,
        child: Option<Child>,
    ) -> Self {
        let handler = Arc::new(ClientHandler {
            authority,
            cwd: cwd.clone(),
            transcript: Mutex::new(Vec::new()),
        });
        let boxed_writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(writer);
        let connection = Connection::spawn(reader, boxed_writer, handler.clone());
        AcpClient {
            connection,
            handler,
            cwd,
            timeout,
            child,
        }
    }

    /// Spawn `command` (argv; `command[0]` is the program) as a child process rooted at `cwd`
    /// and connect to it as an ACP agent over its stdin/stdout. Every tool call it makes that
    /// needs permission is checked against `authority`.
    pub async fn spawn(
        command: &[String],
        cwd: &Path,
        authority: Authority,
        timeout: Duration,
    ) -> Result<Self, AcpError> {
        let (program, args) = command.split_first().ok_or_else(|| {
            AcpError::InvalidConfig(
                "command must have at least one element (the program)".to_string(),
            )
        })?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherit stderr rather than piping-and-discarding: an external agent's diagnostics
            // are the only debugging signal available when it misbehaves, and this crate does
            // not (yet) capture/attach them as evidence.
            .stderr(Stdio::inherit())
            // Belt-and-suspenders alongside `AcpClient::kill`'s explicit, unconditional kill on
            // every path through `AcpExecutor::execute`: `tokio::process::Child` orphans (does
            // not kill) its child on drop by default, so a panic between `spawn` and the first
            // `kill()` call, or an early `?` return before a `Child` is even wrapped in an
            // `AcpClient` (e.g. `child.stdin.take()` returning `None` just below), would
            // otherwise leak a live subprocess.
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AcpError::InvalidConfig("child stdin was not piped".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AcpError::InvalidConfig("child stdout was not piped".to_string()))?;
        Ok(Self::from_io(
            stdout,
            stdin,
            authority,
            cwd.to_path_buf(),
            timeout,
            Some(child),
        ))
    }

    /// Connect to an already-open ACP agent peer over `reader`/`writer` (e.g. one half of an
    /// in-memory duplex pipe in a test) instead of spawning a subprocess.
    pub fn connect(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
        authority: Authority,
        cwd: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self::from_io(reader, writer, authority, cwd, timeout, None)
    }

    /// `initialize`: negotiate protocol version and capabilities. Fails
    /// [`AcpError::UnsupportedProtocolVersion`] if the agent responds with a version newer than
    /// [`protocol::PROTOCOL_VERSION`] — per the spec's negotiation rule ("If the Client does not
    /// support the version specified by the Agent ... SHOULD close the connection"), rather than
    /// asserting exact equality, so this keeps working unmodified once this crate's own
    /// supported version is bumped to match a future agent.
    pub async fn initialize(&self) -> Result<InitializeResponse, AcpError> {
        let req = InitializeRequest {
            protocol_version: protocol::PROTOCOL_VERSION,
            client_capabilities: Default::default(),
            client_info: Some(Implementation {
                name: "tm-acp".to_string(),
                title: Some("Ticketmaster".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
            }),
        };
        let value = self
            .connection
            .call(
                methods::INITIALIZE,
                serde_json::to_value(&req)?,
                self.timeout,
            )
            .await?;
        let resp: InitializeResponse = serde_json::from_value(value)?;
        if resp.protocol_version > protocol::PROTOCOL_VERSION {
            return Err(AcpError::UnsupportedProtocolVersion {
                found: resp.protocol_version,
                max: protocol::PROTOCOL_VERSION,
            });
        }
        Ok(resp)
    }

    /// `session/new`, rooted at this client's own `cwd`.
    pub async fn new_session(&self) -> Result<NewSessionResponse, AcpError> {
        let req = NewSessionRequest {
            cwd: self.cwd.to_string_lossy().into_owned(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
        };
        let value = self
            .connection
            .call(
                methods::SESSION_NEW,
                serde_json::to_value(&req)?,
                self.timeout,
            )
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// `session/prompt`: send `prompt` to `session_id` and wait for the turn to end. Any
    /// `session/update` chunks the agent sends while this call is outstanding are handled
    /// concurrently by [`ClientHandler::handle_notification`] (see `crate::connection`'s doc
    /// comment on why this does not deadlock) and can be retrieved via
    /// [`AcpClient::drain_transcript`] afterward.
    pub async fn prompt(
        &self,
        session_id: &str,
        prompt: Vec<ContentBlock>,
    ) -> Result<PromptResponse, AcpError> {
        let req = PromptRequest {
            session_id: session_id.to_string(),
            prompt,
        };
        let value = self
            .connection
            .call(
                methods::SESSION_PROMPT,
                serde_json::to_value(&req)?,
                self.timeout,
            )
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Take (and clear) every piece of agent-reply text accumulated from `session/update`
    /// `agent_message_chunk` notifications since the last drain.
    pub async fn drain_transcript(&self) -> Vec<String> {
        let mut guard = self.handler.transcript.lock().await;
        std::mem::take(&mut *guard)
    }

    /// Best-effort kill of the spawned child process; a no-op for an [`AcpClient::connect`]-built
    /// client with no child of its own. Called on every path `crate::executor::AcpExecutor`
    /// finishes a run through (success, failure, or a version mismatch during handshake) so a
    /// wedged or otherwise misbehaving external agent is never left running past the call that
    /// was driving it.
    pub async fn kill(&mut self) {
        if let Some(child) = &mut self.child {
            if let Err(e) = child.kill().await {
                tracing::warn!(error = %e, "failed to kill ACP agent child process");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::error_codes;
    use serde_json::json;
    use tm_types::{PatternSet, RepoAuthority};

    fn scoped_authority() -> Authority {
        Authority {
            repository: RepoAuthority {
                read: PatternSet::all(),
                write: PatternSet::parse(["src/**"]).unwrap(),
            },
            ..Authority::none()
        }
    }

    #[tokio::test]
    async fn client_handler_answers_permission_request_with_an_offered_option() {
        let handler = ClientHandler {
            authority: scoped_authority(),
            cwd: PathBuf::from("/repo"),
            transcript: Mutex::new(Vec::new()),
        };
        let params = json!({
            "sessionId": "s-1",
            "toolCall": {
                "toolCallId": "call-1",
                "kind": "edit",
                "locations": [{"path": "/repo/src/lib.rs"}]
            },
            "options": [
                {"optionId": "a1", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "r1", "name": "Reject once", "kind": "reject_once"}
            ]
        });
        let result = handler
            .handle_request(methods::SESSION_REQUEST_PERMISSION, params)
            .await
            .expect("handled");
        let resp: RequestPermissionResponse = serde_json::from_value(result).unwrap();
        assert_eq!(
            resp.outcome,
            crate::protocol::RequestPermissionOutcome::Selected {
                option_id: "a1".to_string()
            }
        );
    }

    #[tokio::test]
    async fn client_handler_denies_outside_scope_by_selecting_a_reject_option() {
        let handler = ClientHandler {
            authority: scoped_authority(),
            cwd: PathBuf::from("/repo"),
            transcript: Mutex::new(Vec::new()),
        };
        let params = json!({
            "sessionId": "s-1",
            "toolCall": {
                "toolCallId": "call-1",
                "kind": "edit",
                "locations": [{"path": "/repo/outside/lib.rs"}]
            },
            "options": [
                {"optionId": "a1", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "r1", "name": "Reject once", "kind": "reject_once"}
            ]
        });
        let result = handler
            .handle_request(methods::SESSION_REQUEST_PERMISSION, params)
            .await
            .expect("handled");
        let resp: RequestPermissionResponse = serde_json::from_value(result).unwrap();
        assert_eq!(
            resp.outcome,
            crate::protocol::RequestPermissionOutcome::Selected {
                option_id: "r1".to_string()
            }
        );
    }

    #[tokio::test]
    async fn client_handler_rejects_an_unknown_method() {
        let handler = ClientHandler {
            authority: scoped_authority(),
            cwd: PathBuf::from("/repo"),
            transcript: Mutex::new(Vec::new()),
        };
        let err = handler
            .handle_request("session/unknown_thing", Value::Null)
            .await
            .unwrap_err();
        assert_eq!(err.code, error_codes::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn client_handler_accumulates_agent_message_chunks_from_session_update() {
        let handler = ClientHandler {
            authority: scoped_authority(),
            cwd: PathBuf::from("/repo"),
            transcript: Mutex::new(Vec::new()),
        };
        let params = json!({
            "sessionId": "s-1",
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hello"}}
        });
        handler
            .handle_notification(methods::SESSION_UPDATE, params)
            .await;
        let params2 = json!({
            "sessionId": "s-1",
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "world"}}
        });
        handler
            .handle_notification(methods::SESSION_UPDATE, params2)
            .await;
        let transcript = handler.transcript.lock().await.clone();
        assert_eq!(transcript, vec!["hello".to_string(), "world".to_string()]);
    }

    #[tokio::test]
    async fn client_handler_ignores_non_agent_message_updates_and_malformed_notifications() {
        let handler = ClientHandler {
            authority: scoped_authority(),
            cwd: PathBuf::from("/repo"),
            transcript: Mutex::new(Vec::new()),
        };
        // A `plan` update classifies as `SessionUpdate::Unknown` and contributes no text.
        let params =
            json!({"sessionId": "s-1", "update": {"sessionUpdate": "plan", "entries": []}});
        handler
            .handle_notification(methods::SESSION_UPDATE, params)
            .await;
        // A malformed payload must not panic.
        handler
            .handle_notification(methods::SESSION_UPDATE, json!("not an object"))
            .await;
        assert!(handler.transcript.lock().await.is_empty());
    }
}
