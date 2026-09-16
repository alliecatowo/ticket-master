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
        // IMPL: Store::open_with(&config.project_root, clock.clone(), ids.clone())?, and
        // EventLog::open_with_clock(&config.project_root.join(".tm").join("project.db"),
        // clock.clone())?, both wrapped in Arc::new. presence/approvals/broadcaster start empty
        // (PresenceTable::new(), ApprovalRegistry::new(), EventHub::new()); providers/harness
        // start None (callers that want them wired set AppState's fields after open()).
        todo!("open Store and a read-only EventLog on the same db path, empty presence/approvals")
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
        // IMPL: tokio::spawn(async move { loop { read_from + publish + sleep } }); a poll
        // error (I/O hiccup) should be logged via `tracing::warn!` and retried next tick, never
        // panic — this task must never crash the server.
        todo!("spawn a tokio task polling `events` and publishing new events to `broadcaster`")
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
    // IMPL: exhaustive match on TmError's variants (see crate docs for the full list); no
    // catch-all `_` arm, so a new TmError variant is a compile error here, not a silent 500.
    todo!("map each TmError variant to the status code documented above")
}

/// The machine-readable error code embedded in [`ErrorBody::error`] for a given [`TmError`].
pub fn error_code(err: &TmError) -> &'static str {
    // IMPL: exhaustive match mirroring `status_for`, e.g. NotFound -> "not_found",
    // AuthorityDenied -> "authority_denied", Conflict -> "conflict", InvalidTransition ->
    // "invalid_transition", LeaseExpired -> "lease_expired", BudgetExhausted ->
    // "budget_exhausted", Storage/Io/Provider/Parse -> "storage_error", Invariant ->
    // "invariant_violation".
    todo!("map each TmError variant to a stable machine-readable code")
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        // IMPL: build (StatusCode, Json<ErrorBody>) per variant: Domain(e) uses
        // status_for(&e)/error_code(&e)/e.to_string(); Unauthorized -> 401 "unauthorized";
        // BadRequest(msg) -> 400 "bad_request"/msg; ApprovalFailed(msg) -> 500
        // "approval_failed"/msg. Then call .into_response() on the tuple.
        todo!("render this ServerError as a (StatusCode, Json<ErrorBody>) response")
    }
}
