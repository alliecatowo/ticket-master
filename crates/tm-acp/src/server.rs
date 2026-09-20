//! The ACP **server** half: exposes a Ticketmaster project as an ACP agent, so Zed (or any
//! other ACP-speaking client) can connect over stdio and hold a conversation against real
//! project state — `initialize`, `session/new`, `session/prompt`, with the reply delivered as a
//! real `session/update` `agent_message_chunk` notification (not just folded into the
//! `session/prompt` response, which the real protocol carries no message content in — only a
//! `stopReason`).
//!
//! Scope, stated plainly: [`ProjectAgentBackend`] answers a prompt with a live summary of the
//! project's tickets (proving this is wired to a real `tm_core::Store`, not a static string),
//! not a full coding-agent turn — this server does not itself call an LLM or make tool calls of
//! its own. That is a deliberate, honest MVP boundary: the [`AgentBackend`] trait is the seam a
//! richer backend (one that actually drives `tm-agent`'s own loop per prompt) would plug into
//! later without touching the protocol-handling code in this module.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, OnceCell};

use tm_core::Store;
use tm_types::{IdKind, IdSource};

use crate::connection::{Connection, Outbound, RequestHandler};
use crate::jsonrpc::RpcError;
use crate::protocol::{
    self, methods, AgentCapabilities, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionNotification, SessionUpdate, StopReason,
};

/// What [`AcpServer`] delegates the two session-lifecycle methods to, so the protocol-handling
/// code in this module (framing, method dispatch, sending the `session/update` notification) is
/// reusable against a backend richer than [`ProjectAgentBackend`]'s "summarize project state"
/// MVP later, without change.
#[async_trait::async_trait]
pub trait AgentBackend: Send + Sync {
    /// Create a new session rooted at `cwd`, returning its id.
    async fn new_session(&self, cwd: &str) -> Result<String, RpcError>;

    /// Answer one prompt turn for `session_id`, returning the reply text to stream back as an
    /// `agent_message_chunk`.
    async fn prompt(&self, session_id: &str, prompt: &[ContentBlock]) -> Result<String, RpcError>;
}

/// The real (not mocked) backend: answers a prompt with a live summary of `store`'s current
/// tickets. Session ids are minted from an injected [`IdSource`] rather than `rand`/a random
/// UUID — this crate's own hygiene rules (see `crates/xtask/src/hygiene.rs`) forbid ambient
/// non-determinism outside the clock substrate, and there is no reason an ACP session id needs
/// to be anything other than deterministic given the same `IdSource` sequence.
pub struct ProjectAgentBackend {
    store: Arc<Store>,
    ids: Arc<dyn IdSource>,
    /// Every session this backend has created, keyed by its minted id, valued by the `cwd` the
    /// client requested. Not consulted for anything beyond "does this session id exist" today
    /// (`prompt` answers identically regardless of `cwd`), but real state a richer backend would
    /// build on rather than a placeholder.
    sessions: Mutex<BTreeMap<String, String>>,
}

impl ProjectAgentBackend {
    /// Build a backend answering from `store`'s live state, minting session ids from `ids`.
    pub fn new(store: Arc<Store>, ids: Arc<dyn IdSource>) -> Self {
        ProjectAgentBackend {
            store,
            ids,
            sessions: Mutex::new(BTreeMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl AgentBackend for ProjectAgentBackend {
    async fn new_session(&self, cwd: &str) -> Result<String, RpcError> {
        let session_id = format!("acp-{}", self.ids.next(IdKind::Session).as_str());
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), cwd.to_string());
        Ok(session_id)
    }

    async fn prompt(&self, session_id: &str, _prompt: &[ContentBlock]) -> Result<String, RpcError> {
        if !self.sessions.lock().await.contains_key(session_id) {
            return Err(RpcError::invalid_params(format!(
                "unknown session id {session_id}"
            )));
        }
        let view = self
            .store
            .view()
            .map_err(|e| RpcError::internal(format!("failed to read project state: {e}")))?;
        let mut lines = vec![format!(
            "Ticketmaster project status: {} ticket(s).",
            view.tickets.len()
        )];
        for (id, ticket) in view.tickets.iter().take(20) {
            lines.push(format!("- {id}: {} [{:?}]", ticket.objective, ticket.state));
        }
        Ok(lines.join("\n"))
    }
}

/// The writer type [`AcpServer`] boxes its connections' outbound half as, matching
/// `crate::client::AcpClient`'s own boxing so both halves of this crate present the same
/// "accepts any `AsyncRead`/`AsyncWrite` pair, real subprocess or an in-memory pipe" shape.
type BoxedWriter = Box<dyn AsyncWrite + Unpin + Send>;

/// Exposes `backend` as an ACP agent: answers `initialize`, `session/new` and `session/prompt`
/// over whatever `AsyncRead`/`AsyncWrite` pair [`AcpServer::attach`] is given (real process
/// stdio, or an in-memory pipe in a test).
///
/// One `AcpServer` is meant to serve exactly one connection: [`AcpServer::attach`] stores the
/// resulting [`Outbound`] handle (needed to send `session/update` notifications from inside
/// `session/prompt` handling) in a [`OnceCell`], so a second `attach` call on the same instance
/// would silently misdirect its notifications to the *first* connection. Construct a fresh
/// `AcpServer` per connection — the natural shape anyway, since a real ACP agent subprocess has
/// exactly one stdin/stdout pair for its entire lifetime.
pub struct AcpServer<B> {
    backend: Arc<B>,
    outbound: OnceCell<Outbound<BoxedWriter>>,
}

impl<B: AgentBackend + 'static> AcpServer<B> {
    /// Build a server around `backend`, not yet attached to any connection.
    pub fn new(backend: Arc<B>) -> Arc<Self> {
        Arc::new(AcpServer {
            backend,
            outbound: OnceCell::new(),
        })
    }

    /// Start serving one ACP connection over `reader`/`writer`. Returns the live [`Connection`]
    /// — drop it (or let it fall out of scope) to stop serving; see [`Connection`]'s own doc
    /// comment on why dropping (not just letting a `JoinHandle` go out of scope) is what
    /// actually stops the reader task.
    pub fn attach(
        self: &Arc<Self>,
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Connection<BoxedWriter> {
        let boxed: BoxedWriter = Box::new(writer);
        let outbound = Outbound::new(boxed);
        // Stored before the reader task (which is what would dispatch a `session/prompt`
        // request to `handle_request`, the only place that reads this) is spawned by
        // `Connection::from_parts` below — no request can race ahead of this being set.
        let _ = self.outbound.set(outbound.clone());
        Connection::from_parts(outbound, reader, self.clone())
    }
}

#[async_trait::async_trait]
impl<B: AgentBackend + 'static> RequestHandler for AcpServer<B> {
    async fn handle_request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            methods::INITIALIZE => {
                let req: InitializeRequest =
                    serde_json::from_value(params).map_err(RpcError::invalid_params)?;
                // Per the spec's negotiation rule: echo the client's version back if we support
                // it; otherwise respond with the latest version we do support. This crate
                // supports exactly one version today, so those two cases collapse to "equal or
                // not", but are written to keep working unmodified once `PROTOCOL_VERSION` moves.
                let version = if req.protocol_version == protocol::PROTOCOL_VERSION {
                    req.protocol_version
                } else {
                    protocol::PROTOCOL_VERSION
                };
                let resp = InitializeResponse {
                    protocol_version: version,
                    agent_capabilities: AgentCapabilities::default(),
                    auth_methods: Vec::new(),
                    agent_info: Some(Implementation {
                        name: "tm-acp".to_string(),
                        title: Some("Ticketmaster".to_string()),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                    }),
                };
                serde_json::to_value(resp).map_err(|e| RpcError::internal(e.to_string()))
            }
            methods::SESSION_NEW => {
                let req: NewSessionRequest =
                    serde_json::from_value(params).map_err(RpcError::invalid_params)?;
                let session_id = self.backend.new_session(&req.cwd).await?;
                let resp = NewSessionResponse {
                    session_id,
                    modes: None,
                    config_options: None,
                };
                serde_json::to_value(resp).map_err(|e| RpcError::internal(e.to_string()))
            }
            methods::SESSION_PROMPT => {
                let req: PromptRequest =
                    serde_json::from_value(params).map_err(RpcError::invalid_params)?;
                let reply = self.backend.prompt(&req.session_id, &req.prompt).await?;

                match self.outbound.get() {
                    Some(outbound) => {
                        let note = SessionNotification {
                            session_id: req.session_id.clone(),
                            update: SessionUpdate::AgentMessageChunk(ContentChunk {
                                content: ContentBlock::text(reply),
                                message_id: None,
                            }),
                        };
                        let payload = serde_json::to_value(&note)
                            .map_err(|e| RpcError::internal(e.to_string()))?;
                        if let Err(e) = outbound.notify(methods::SESSION_UPDATE, payload).await {
                            tracing::warn!(error = %e, "failed to send session/update notification");
                        }
                    }
                    None => {
                        // Can only happen if `handle_request` runs before `AcpServer::attach`
                        // finished storing the outbound handle it just built — impossible per
                        // `attach`'s own ordering (see its doc comment), kept as a defensive,
                        // logged fallback rather than a panic.
                        tracing::warn!(
                            "session/prompt handled with no attached outbound handle; no \
                             session/update was sent"
                        );
                    }
                }

                let resp = PromptResponse {
                    stop_reason: StopReason::EndTurn,
                };
                serde_json::to_value(resp).map_err(|e| RpcError::internal(e.to_string()))
            }
            other => Err(RpcError::method_not_found(other)),
        }
    }

    async fn handle_notification(&self, method: &str, _params: Value) {
        tracing::debug!(method, "unhandled ACP notification (server side)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::CounterIds;

    fn open_store() -> (tempfile::TempDir, Arc<Store>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Arc::new(Store::open_with(dir.path(), clock, ids).expect("open store"));
        (dir, store)
    }

    #[tokio::test]
    async fn project_agent_backend_rejects_a_prompt_for_an_unknown_session() {
        let (_dir, store) = open_store();
        let backend = ProjectAgentBackend::new(store, Arc::new(CounterIds::new()));
        let err = backend
            .prompt("no-such-session", &[ContentBlock::text("hi")])
            .await
            .unwrap_err();
        assert_eq!(err.code, crate::jsonrpc::error_codes::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn project_agent_backend_answers_from_live_store_state() {
        let (_dir, store) = open_store();
        let events = store
            .create_ticket(
                tm_core::ticket::TicketKind::Work,
                "wire the ACP server".to_string(),
                None,
                None,
                tm_types::Authority::root(),
                vec![],
                tm_core::ticket::ExecutorRequirements {
                    role: tm_types::Role::CoderFast,
                    human_required: false,
                    min_capability: tm_types::Tolerance::Any,
                },
                vec![],
                vec![],
                tm_core::ticket::VerificationPolicy::None,
                tm_types::Budget::unlimited(),
                tm_core::ticket::RetryPolicy {
                    max_attempts: 3,
                    base_delay_seconds: 1,
                    backoff_multiplier: 2.0,
                    max_delay_seconds: 60,
                },
                0,
                tm_types::ParticipantId::system(),
            )
            .expect("create_ticket");
        let ticket_id = tm_types::TicketId::new(events[0].subject.as_str())
            .expect("event subject is a ticket id");

        let backend = ProjectAgentBackend::new(store, Arc::new(CounterIds::new()));
        let session_id = backend.new_session("/repo").await.expect("new_session");
        let reply = backend
            .prompt(&session_id, &[ContentBlock::text("status?")])
            .await
            .expect("prompt");
        assert!(reply.contains("wire the ACP server"));
        assert!(reply.contains(ticket_id.as_str()));
    }
}
