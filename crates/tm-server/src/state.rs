//! Shared server state: the `Store` handle, the event broadcaster, presence and configuration.
//!
//! [`AppState`] is the one value axum hands to every handler (via `axum::extract::State`). It
//! owns:
//!
//! * `store` — the single [`tm_core::Store`] open on the project, the only thing that ever
//!   writes durable state.
//! * `events`/`broadcaster` — a *second*, read-only [`EventLog`] opened on the same
//!   `.tm/project.db`, plus an in-process [`EventHub`] this crate feeds by polling that log.
//!   This split exists because `Store` does not expose its internal `EventLog` or a
//!   subscribe hook: two `EventLog` handles on the same WAL-mode SQLite file see each other's
//!   committed writes just fine for reads (`read_from`/`head`), so a small polling loop
//!   (spawned by [`AppState::spawn_broadcast_poller`]) is what turns `store`'s commits into
//!   [`sse`](crate::sse) pushes without requiring any change to `tm-core`.
//! * `presence`/`approvals` — the two pieces of genuinely server-local state (`SPEC.md` §14):
//!   presence is intentionally non-durable (TTL'd, lost on restart by design), and approvals'
//!   durable half (the decision record) already lives in `tm-core` — only the "who is blocked
//!   waiting" rendezvous is local.
//! * `config` — bind address, token, TTL defaults.
//!
//! [`ServerError`] is the crate-wide HTTP error type: every handler in [`crate::routes`] returns
//! `Result<_, ServerError>`, and [`ServerError`]'s `IntoResponse` impl is what applies the
//! `TmError` → status code mapping this module owns ([`status_for`]).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use tm_core::Store;
use tm_events::{EventHub, EventLog};
use tm_types::{Clock, IdSource, TmError};

use crate::approvals::ApprovalRegistry;
use crate::presence::PresenceTable;

/// Server-wide configuration, resolved once at startup.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The project root this server serves (`.tm/project.db` lives under it).
    pub project_root: PathBuf,
    /// The address the HTTP listener binds to.
    pub bind_addr: SocketAddr,
    /// Bearer token required when `bind_addr` is not loopback. `None` is only valid alongside a
    /// loopback `bind_addr` — see [`crate::auth`].
    pub token: Option<String>,
    /// Default presence TTL, seconds, used when a `POST /sessions/:id/presence` call omits one.
    pub presence_ttl_seconds: u32,
    /// How often the broadcast poller checks the log for new events.
    pub broadcast_poll_interval: Duration,
    /// Maximum events served in one backlog page during SSE replay.
    pub sse_replay_page_size: usize,
}

impl ServerConfig {
    /// True when `bind_addr` is not a loopback address, i.e. when [`crate::auth`] must require a
    /// bearer token.
    pub fn requires_auth(&self) -> bool {
        !self.bind_addr.ip().is_loopback()
    }
}

/// Everything an axum handler needs, cloned cheaply (every field is an `Arc` or `Copy`/small).
#[derive(Clone)]
pub struct AppState {
    /// The project-state facade; the only writer of durable truth.
    pub store: Arc<Store>,
    /// Read-only handle onto the same event log, used for SSE backlog replay and by the
    /// broadcast poller. See the module docs for why this is a second `EventLog` instance.
    pub events: Arc<EventLog>,
    /// In-process fan-out fed by [`AppState::spawn_broadcast_poller`]; SSE handlers subscribe to
    /// this for the live half of the resumable stream.
    pub broadcaster: Arc<EventHub>,
    /// Participant presence, TTL'd, not durable.
    pub presence: Arc<PresenceTable>,
    /// Pending/decided approval requests and the channels blocking their requesters.
    pub approvals: Arc<ApprovalRegistry>,
    /// Resolved server configuration.
    pub config: ServerConfig,
    /// Injected clock; never read wall-clock time directly (`SPEC.md` determinism rule).
    pub clock: Arc<dyn Clock>,
    /// Injected id source, for anything this crate allocates ids for (approval ids, etc).
    pub ids: Arc<dyn IdSource>,
    /// Read-only fabric state backing `GET /providers`. `None` when this server instance was
    /// started without fabric wiring (e.g. a state-only test harness); handlers then report an
    /// empty provider list rather than erroring.
    pub providers: Option<Arc<std::sync::Mutex<tm_provider::FabricState>>>,
    /// The harness epoch registry backing `GET /harness`. `None` when unwired, same convention
    /// as `providers`.
    pub harness: Option<Arc<tm_harness::EpochRegistry>>,
}

impl AppState {
    /// Open `store` and a second read-only `EventLog` on `config.project_root`, with fresh empty
    /// presence/approval tables and a not-yet-started broadcaster.
    ///
    /// # Errors
    /// Whatever [`tm_core::Store::open_with`]/[`EventLog::open_with_clock`] return opening the
    /// project database.
    pub fn open(
        config: ServerConfig,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
    ) -> tm_types::Result<Self> {
        let store = Arc::new(Store::open_with(
            &config.project_root,
            clock.clone(),
            ids.clone(),
        )?);
        let db_path = config.project_root.join(".tm").join("project.db");
        let events = Arc::new(EventLog::open_with_clock(&db_path, clock.clone())?);
        Ok(AppState {
            store,
            events,
            broadcaster: Arc::new(EventHub::new()),
            presence: Arc::new(PresenceTable::new()),
            approvals: Arc::new(ApprovalRegistry::new()),
            config,
            clock,
            ids,
            providers: None,
            harness: None,
        })
    }

    /// Spawn the background task that turns `store`'s commits into `broadcaster` publishes.
    ///
    /// # Invariant
    /// Must never drop or duplicate an event: the poller tracks `last_seq` starting from
    /// `events.head()` at spawn time, and on each tick calls
    /// `events.read_from(last_seq + 1, page_size)`, publishing each event to `broadcaster` in
    /// `seq` order and advancing `last_seq` to the last one published, before sleeping
    /// `config.broadcast_poll_interval`. Runs until `self` (specifically `events`) is dropped;
    /// callers keep the returned `JoinHandle` only to abort it on shutdown.
    pub fn spawn_broadcast_poller(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let page_size = state.config.sse_replay_page_size;
            let poll_interval = state.config.broadcast_poll_interval;
            let mut last_seq = match state.events.head() {
                Ok(head) => head,
                Err(e) => {
                    tracing::warn!(
                        "failed to read event log head on broadcast poller startup: {}",
                        e
                    );
                    0
                }
            };

            loop {
                match state.events.read_from(last_seq + 1, page_size) {
                    Ok(events) => {
                        for event in events {
                            state.broadcaster.publish(&event);
                            last_seq = event.seq;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("broadcast poller read_from failed: {}", e);
                    }
                }

                tokio::time::sleep(poll_interval).await;
            }
        })
    }
}

/// The crate-wide HTTP error type. Every [`crate::routes`] handler resolves to
/// `Result<_, ServerError>`; axum calls [`IntoResponse::into_response`] on the `Err` case.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// A `tm-core`/`tm-types` domain error, mapped to a status code by [`status_for`].
    #[error(transparent)]
    Domain(#[from] TmError),
    /// The request lacked a valid bearer token where one was required.
    #[error("unauthorized")]
    Unauthorized,
    /// The request body/query failed validation before it ever reached `Store`.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// An approval this request depended on was cancelled or the server shut down while a
    /// request was still blocked waiting on a decision.
    #[error("approval rendezvous failed: {0}")]
    ApprovalFailed(String),
}

/// The JSON body every error response carries: `{"error": "<code>", "message": "<detail>"}`.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    /// A short machine-readable error code (e.g. `"not_found"`, `"authority_denied"`).
    pub error: &'static str,
    /// A human-readable detail message.
    pub message: String,
}

/// Pure mapping from [`TmError`] to an HTTP status code, per `SPEC.md` §14's contract: authority
/// denied → 403, not found → 404, conflict → 409, invariant → 500. Kept separate from
/// [`ServerError`]'s `IntoResponse` impl so the mapping itself is unit-testable without
/// constructing a real HTTP response.
///
/// # Mapping
/// - `AuthorityDenied` → 403 Forbidden
/// - `NotFound` → 404 Not Found
/// - `Conflict`, `InvalidTransition`, `LeaseExpired`, `BudgetExhausted` → 409 Conflict (all name
///   a legal-but-currently-blocked state transition)
/// - `Storage`, `Provider`, `Io`, `Parse`, `Invariant` → 500 Internal Server Error
pub fn status_for(err: &TmError) -> StatusCode {
    match err {
        TmError::AuthorityDenied(_) => StatusCode::FORBIDDEN,
        TmError::NotFound { .. } => StatusCode::NOT_FOUND,
        TmError::Conflict(_) => StatusCode::CONFLICT,
        TmError::InvalidTransition(_) => StatusCode::CONFLICT,
        TmError::LeaseExpired(_) => StatusCode::CONFLICT,
        TmError::BudgetExhausted(_) => StatusCode::CONFLICT,
        TmError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        TmError::Provider(_) => StatusCode::INTERNAL_SERVER_ERROR,
        TmError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        TmError::Parse(_) => StatusCode::INTERNAL_SERVER_ERROR,
        TmError::Invariant(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The machine-readable error code embedded in [`ErrorBody::error`] for a given [`TmError`].
pub fn error_code(err: &TmError) -> &'static str {
    match err {
        TmError::NotFound { .. } => "not_found",
        TmError::AuthorityDenied(_) => "authority_denied",
        TmError::Conflict(_) => "conflict",
        TmError::InvalidTransition(_) => "invalid_transition",
        TmError::LeaseExpired(_) => "lease_expired",
        TmError::BudgetExhausted(_) => "budget_exhausted",
        TmError::Storage(_) => "storage_error",
        TmError::Io(_) => "storage_error",
        TmError::Provider(_) => "storage_error",
        TmError::Parse(_) => "storage_error",
        TmError::Invariant(_) => "invariant_violation",
    }
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            ServerError::Domain(ref e) => (status_for(e), error_code(e), e.to_string()),
            ServerError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "unauthorized".to_string(),
            ),
            ServerError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg),
            ServerError::ApprovalFailed(msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "approval_failed", msg)
            }
        };

        let body = ErrorBody {
            error: code,
            message,
        };

        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;

    #[test]
    fn status_for_authority_denied_is_403() {
        let err = TmError::AuthorityDenied("test".into());
        assert_eq!(status_for(&err), StatusCode::FORBIDDEN);
    }

    #[test]
    fn status_for_not_found_is_404() {
        let err = TmError::NotFound {
            kind: "ticket",
            id: "T-1".to_string(),
        };
        assert_eq!(status_for(&err), StatusCode::NOT_FOUND);
    }

    #[test]
    fn status_for_conflict_is_409() {
        let err = TmError::Conflict("test".into());
        assert_eq!(status_for(&err), StatusCode::CONFLICT);
    }

    #[test]
    fn status_for_invalid_transition_is_409() {
        let err = TmError::InvalidTransition("test".into());
        assert_eq!(status_for(&err), StatusCode::CONFLICT);
    }

    #[test]
    fn status_for_lease_expired_is_409() {
        let err = TmError::LeaseExpired("test".into());
        assert_eq!(status_for(&err), StatusCode::CONFLICT);
    }

    #[test]
    fn status_for_budget_exhausted_is_409() {
        let err = TmError::BudgetExhausted("test".into());
        assert_eq!(status_for(&err), StatusCode::CONFLICT);
    }

    #[test]
    fn status_for_storage_is_500() {
        let err = TmError::Storage("test".into());
        assert_eq!(status_for(&err), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn status_for_provider_is_500() {
        let err = TmError::Provider("test".into());
        assert_eq!(status_for(&err), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn status_for_io_is_500() {
        let err = TmError::Io("test".into());
        assert_eq!(status_for(&err), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn status_for_parse_is_500() {
        let err = TmError::Parse("test".into());
        assert_eq!(status_for(&err), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn status_for_invariant_is_500() {
        let err = TmError::Invariant("test".into());
        assert_eq!(status_for(&err), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn error_code_for_not_found() {
        let err = TmError::NotFound {
            kind: "ticket",
            id: "T-1".to_string(),
        };
        assert_eq!(error_code(&err), "not_found");
    }

    #[test]
    fn error_code_for_authority_denied() {
        let err = TmError::AuthorityDenied("test".into());
        assert_eq!(error_code(&err), "authority_denied");
    }

    #[test]
    fn error_code_for_conflict() {
        let err = TmError::Conflict("test".into());
        assert_eq!(error_code(&err), "conflict");
    }

    #[test]
    fn error_code_for_invalid_transition() {
        let err = TmError::InvalidTransition("test".into());
        assert_eq!(error_code(&err), "invalid_transition");
    }

    #[test]
    fn error_code_for_lease_expired() {
        let err = TmError::LeaseExpired("test".into());
        assert_eq!(error_code(&err), "lease_expired");
    }

    #[test]
    fn error_code_for_budget_exhausted() {
        let err = TmError::BudgetExhausted("test".into());
        assert_eq!(error_code(&err), "budget_exhausted");
    }

    #[test]
    fn error_code_for_storage() {
        let err = TmError::Storage("test".into());
        assert_eq!(error_code(&err), "storage_error");
    }

    #[test]
    fn error_code_for_io() {
        let err = TmError::Io("test".into());
        assert_eq!(error_code(&err), "storage_error");
    }

    #[test]
    fn error_code_for_provider() {
        let err = TmError::Provider("test".into());
        assert_eq!(error_code(&err), "storage_error");
    }

    #[test]
    fn error_code_for_parse() {
        let err = TmError::Parse("test".into());
        assert_eq!(error_code(&err), "storage_error");
    }

    #[test]
    fn error_code_for_invariant() {
        let err = TmError::Invariant("test".into());
        assert_eq!(error_code(&err), "invariant_violation");
    }

    #[test]
    fn server_error_domain_renders_with_correct_status() {
        let err: ServerError = TmError::NotFound {
            kind: "ticket",
            id: "T-1".to_string(),
        }
        .into();
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn server_error_unauthorized_renders_401() {
        let err = ServerError::Unauthorized;
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn server_error_bad_request_renders_400() {
        let err = ServerError::BadRequest("invalid input".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn server_error_approval_failed_renders_500() {
        let err = ServerError::ApprovalFailed("rendezvous failed".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn server_config_requires_auth_on_non_loopback() {
        let config = ServerConfig {
            project_root: PathBuf::from("/tmp/test"),
            bind_addr: "127.0.0.1:8080".parse().unwrap(),
            token: None,
            presence_ttl_seconds: 300,
            broadcast_poll_interval: Duration::from_secs(1),
            sse_replay_page_size: 100,
        };
        assert!(!config.requires_auth());
    }

    #[test]
    fn server_config_requires_auth_on_non_loopback_ip() {
        let config = ServerConfig {
            project_root: PathBuf::from("/tmp/test"),
            bind_addr: (IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 8080).into(),
            token: Some("token".into()),
            presence_ttl_seconds: 300,
            broadcast_poll_interval: Duration::from_secs(1),
            sse_replay_page_size: 100,
        };
        assert!(config.requires_auth());
    }
}
