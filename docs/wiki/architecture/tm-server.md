+++
[doc]
id = "wiki/architecture/tm-server"
mode = "generated"
derived_from = ["crates/tm-server/src/**"]
+++

# Architecture: tm-server

## Module tree

- `crates/tm-server/src/approvals.rs`
- `crates/tm-server/src/auth.rs`
- `crates/tm-server/src/lib.rs`
- `crates/tm-server/src/presence.rs`
- `crates/tm-server/src/routes.rs`
- `crates/tm-server/src/sse.rs`
- `crates/tm-server/src/state.rs`
- `crates/tm-server/src/wiki.rs`

## Public symbols

### `crates/tm-server/src/approvals.rs`

- `pub type ApprovalId = String;`
- `pub struct ApprovalRequest`
- `pub enum ApprovalDecision`
- `pub enum ApprovalStatus`
- `pub struct PendingApproval`
- `pub enum ApprovalError`
- `pub struct ApprovalRegistry`
- `impl ApprovalRegistry`
  - `pub fn new() -> Self`
  - `pub fn open(&self, request: ApprovalRequest) -> ApprovalWaiter`
  - `pub fn pending(&self) -> Vec<PendingApproval>`
  - `pub fn get(&self, id: &ApprovalId) -> Option<ApprovalStatus>`
  - `pub fn decide(
        &self,
        id: &ApprovalId,
        decision: ApprovalDecision,
        decided_by: ParticipantId,
        decided_at: Timestamp,
    ) -> Result<(), ApprovalError>`
- `pub struct ApprovalWaiter`
- `impl ApprovalWaiter`
  - `pub async fn wait(self) -> Result<ApprovalDecision, ApprovalError>`
- `pub type ApprovalStore = ApprovalRegistry;`
- `pub type Approval = ApprovalRequest;`

### `crates/tm-server/src/auth.rs`

- `pub type AuthConfig = ServerConfig;`
- `pub struct BindAddress`
- `impl BindAddress`
  - `pub fn is_loopback(&self) -> bool`
  - `pub fn requires_token(&self) -> bool`
- `pub enum AuthError`
- `pub fn resolve_token(configured: Option<&str>) -> Option<String>`
- `pub fn token_matches(expected: &str, provided: &str) -> bool`
- `pub fn bearer_token(req: &Request) -> Option<&str>`
- `pub async fn authenticate(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ServerError>`

### `crates/tm-server/src/lib.rs`

- `pub mod approvals;`
- `pub mod auth;`
- `pub mod presence;`
- `pub mod routes;`
- `pub mod sse;`
- `pub mod state;`
- `pub mod wiki;`

### `crates/tm-server/src/presence.rs`

- `pub struct PresenceEntry`
- `impl PresenceEntry`
  - `pub fn is_expired(&self, now: Timestamp) -> bool`
- `pub struct PathLeaseSummary`
- `pub struct PresenceTable`
- `impl PresenceTable`
  - `pub fn new() -> Self`
  - `pub fn upsert(&self, entry: PresenceEntry)`
  - `pub fn remove(&self, participant: &ParticipantId)`
  - `pub fn snapshot(&self) -> Vec<PresenceEntry>`
  - `pub fn sweep_expired(&self, now: Timestamp) -> Vec<ParticipantId>`
- `pub fn sweep_expired_at(
    entries: &mut BTreeMap<ParticipantId, PresenceEntry>,
    now: Timestamp,
) -> Vec<ParticipantId>`
- `pub fn path_leases(view: &ProjectView) -> Vec<PathLeaseSummary>`
- `pub struct Presence`
- `pub type PresenceStore = PresenceTable;`
- `pub type SurfacedLease = PathLeaseSummary;`

### `crates/tm-server/src/routes.rs`

- `pub fn router(state: AppState) -> Router`

### `crates/tm-server/src/sse.rs`

- `pub struct EventStreamParams`
- `pub struct ResumableStream`
- `impl ResumableStream`
  - `pub fn new(from_seq: u64) -> Self`
  - `pub fn next_seq(&self) -> u64`
  - `pub fn admit(&mut self, event: &Event) -> bool`
- `pub fn to_sse_event(event: &Event) -> tm_types::Result<SseEvent>`
- `pub async fn sse_handler(
    State(state): State<AppState>,
    Query(params): Query<EventStreamParams>,
) -> Result<Sse<BoxEventStream>, ServerError>`

### `crates/tm-server/src/state.rs`

- `pub struct ServerConfig`
- `impl ServerConfig`
  - `pub fn requires_auth(&self) -> bool`
- `pub struct AppState`
- `impl AppState`
  - `pub fn open(
        config: ServerConfig,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
    ) -> tm_types::Result<Self>`
  - `pub fn spawn_broadcast_poller(self: &Arc<Self>) -> tokio::task::JoinHandle<()>`
- `pub enum ServerError`
- `pub struct ErrorBody`
- `pub fn status_for(err: &TmError) -> StatusCode`
- `pub fn error_code(err: &TmError) -> &'static str`

### `crates/tm-server/src/wiki.rs`

- `pub const WIKI_DIR: &str = "docs/wiki";`
- `pub async fn get_wiki_page(
    State(state): State<AppState>,
    AxumPath(path): AxumPath<String>,
) -> Result<Html<String>, ServerError>`
- `pub async fn list_wiki_pages(State(state): State<AppState>) -> Result<Html<String>, ServerError>`
