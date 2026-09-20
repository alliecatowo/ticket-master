+++
[doc]
id = "wiki/architecture/tm-mirror"
mode = "generated"
derived_from = ["crates/tm-mirror/src/**"]
+++

# Architecture: tm-mirror

## Module tree

- `crates/tm-mirror/src/config.rs`
- `crates/tm-mirror/src/github.rs`
- `crates/tm-mirror/src/gitlab.rs`
- `crates/tm-mirror/src/jira.rs`
- `crates/tm-mirror/src/lib.rs`
- `crates/tm-mirror/src/linear.rs`
- `crates/tm-mirror/src/projection.rs`
- `crates/tm-mirror/src/sync.rs`
- `crates/tm-mirror/src/tracker.rs`

## Public symbols

### `crates/tm-mirror/src/config.rs`

- `pub enum AdapterKind`
- `pub struct ProjectionOverrides`
- `pub struct CredentialEnv`
- `impl CredentialEnv`
  - `pub fn resolve(&self) -> Result<BTreeMap<String, String>>`
- `pub struct AdapterConfig`
- `pub struct MirrorConfig`
- `impl MirrorConfig`
  - `pub fn parse(source: &str) -> Result<Self>`
  - `pub fn validate(&self) -> Result<()>`
  - `pub fn enabled_adapters(&self) -> Vec<&AdapterConfig>`

### `crates/tm-mirror/src/github.rs`

- `pub const GITHUB_TOKEN_ENV_VAR: &str = "GITHUB_TOKEN";`
- `pub const DEFAULT_BASE_URL: &str = "https://api.github.com";`
- `pub struct GitHubTracker`
- `impl GitHubTracker`
  - `pub fn from_env(
        name: impl Into<String>,
        owner: impl Into<String>,
        repo: impl Into<String>,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Result<Self>`
  - `pub fn with_config(
        name: impl Into<String>,
        owner: impl Into<String>,
        repo: impl Into<String>,
        token: String,
        base_url: String,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Result<Self>`
  - `pub fn set_max_retries(&mut self, max_retries: u32)`

### `crates/tm-mirror/src/gitlab.rs`

- `pub const GITLAB_TOKEN_ENV_VAR: &str = "GITLAB_TOKEN";`
- `pub struct GitLabTracker`
- `impl GitLabTracker`
  - `pub fn from_env(name: impl Into<String>, project_id: impl Into<String>) -> Result<Self>`
  - `pub fn with_config(
        name: impl Into<String>,
        project_id: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self>`
- `pub struct GitLabIssue`
- `pub struct GitLabUser`
- `pub struct GitLabMilestone`
- `pub struct GitLabNote`

### `crates/tm-mirror/src/jira.rs`

- `pub const JIRA_EMAIL_ENV_VAR: &str = "JIRA_EMAIL";`
- `pub const JIRA_API_TOKEN_ENV_VAR: &str = "JIRA_API_TOKEN";`
- `pub const DEFAULT_BASE_URL: &str = "https://api.atlassian.net/rest/api/3";`
- `pub struct JiraTracker`
- `impl JiraTracker`
  - `pub fn from_env(name: impl Into<String>, project_key: impl Into<String>) -> Result<Self>`
  - `pub fn with_config(
        name: impl Into<String>,
        project_key: impl Into<String>,
        email: impl Into<String>,
        api_token: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self>`

### `crates/tm-mirror/src/lib.rs`

- `pub mod config;`
- `pub mod github;`
- `pub mod gitlab;`
- `pub mod jira;`
- `pub mod linear;`
- `pub mod projection;`
- `pub mod sync;`
- `pub mod tracker;`

### `crates/tm-mirror/src/linear.rs`

- `pub const LINEAR_API_KEY_ENV_VAR: &str = "LINEAR_API_KEY";`
- `pub const DEFAULT_BASE_URL: &str = "https://api.linear.app/graphql";`
- `pub struct LinearTracker`
- `impl LinearTracker`
  - `pub fn from_env(name: impl Into<String>, team_id: impl Into<String>) -> Result<Self>`
  - `pub fn with_config(
        name: impl Into<String>,
        team_id: impl Into<String>,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self>`

### `crates/tm-mirror/src/projection.rs`

- `pub enum Degradation`
- `pub struct ChecklistItem`
- `pub struct Projection`
- `pub struct ProjectionPolicy`
- `impl ProjectionPolicy`
  - `pub fn default_policy() -> Self`
  - `pub fn should_mirror(&self, ticket: &Ticket, mirror_override: Option<bool>) -> bool`
  - `pub fn map_state(
        &self,
        state: TicketState,
        caps: &TrackerCapabilities,
    ) -> (String, Option<Degradation>)`
  - `pub fn checklist_rollup(&self, descendants: &[Ticket]) -> Vec<ChecklistItem>`
  - `pub fn project(
        &self,
        ticket: &Ticket,
        descendants: &[Ticket],
        caps: &TrackerCapabilities,
    ) -> Projection`

### `crates/tm-mirror/src/sync.rs`

- `pub struct MirrorLink`
- `pub enum FieldOwner`
- `pub enum ConflictField`
- `impl ConflictField`
  - `pub fn owner(self) -> FieldOwner`
- `pub enum InboundAction`
- `pub struct SyncEngine`
- `impl SyncEngine`
  - `pub fn new(clock: Arc<dyn Clock>, ids: Arc<dyn IdSource>, actor: ParticipantId) -> Self`
  - `pub fn projection_hash(projection: &Projection) -> String`
  - `pub async fn push(
        &self,
        tracker: &dyn Tracker,
        projection: &Projection,
        existing: Option<&MirrorLink>,
    ) -> Result<(MirrorLink, Option<EventDraft>)>`
  - `pub async fn pull(
        &self,
        tracker: &dyn Tracker,
        link: &MirrorLink,
    ) -> Result<(MirrorLink, Vec<EventDraft>)>`
  - `pub fn translate(
        &self,
        known_ticket: Option<TicketId>,
        change: &ExternalChange,
    ) -> Option<InboundAction>`
  - `pub fn to_event_drafts(&self, action: &InboundAction) -> Vec<EventDraft>`

### `crates/tm-mirror/src/tracker.rs`

- `pub struct TrackerCapabilities`
- `pub struct ExternalRef`
- `pub enum ExternalChangeKind`
- `pub struct ExternalChange`
- `pub trait Tracker: Send + Sync`
- `pub struct NullTracker`
- `impl NullTracker`
  - `pub fn new(name: impl Into<String>) -> Self`
- `pub struct RecordingTracker`
- `impl RecordingTracker`
  - `pub fn new(name: impl Into<String>, capabilities: TrackerCapabilities) -> Self`
  - `pub fn script_pull(&self, changes: Vec<ExternalChange>)`
  - `pub fn pushed(&self) -> Vec<Projection>`
  - `pub fn pull_calls(&self) -> Vec<Timestamp>`
