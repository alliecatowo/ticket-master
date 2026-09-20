+++
[doc]
id = "wiki/architecture/tm-events"
mode = "generated"
derived_from = ["crates/tm-events/src/**"]
+++

# Architecture: tm-events

## Module tree

- `crates/tm-events/src/event.rs`
- `crates/tm-events/src/kind.rs`
- `crates/tm-events/src/lib.rs`
- `crates/tm-events/src/log.rs`
- `crates/tm-events/src/payload.rs`
- `crates/tm-events/src/schema.rs`
- `crates/tm-events/src/stream.rs`

## Public symbols

### `crates/tm-events/src/event.rs`

- `pub const GENESIS_PREV_HASH: &str = "";`
- `pub struct EventDraft`
- `impl EventDraft`
  - `pub fn new(actor: ParticipantId, subject: Id, payload: Payload) -> Self`
  - `pub fn with_session(mut self, session: SessionId) -> Self`
  - `pub fn with_causation(mut self, causation: u64) -> Self`
  - `pub fn with_correlation(mut self, correlation: impl Into<String>) -> Self`
  - `pub fn kind(&self) -> EventKind`
- `pub struct Event`
- `impl Event`
  - `pub fn canonical_body(&self) -> tm_types::Result<Vec<u8>>`
  - `pub fn verifies_against(&self, prev_hash: &str) -> tm_types::Result<bool>`
- `pub fn canonical_body(
    ts: Timestamp,
    kind: EventKind,
    subject: &Id,
    actor: &ParticipantId,
    session: Option<&SessionId>,
    causation: Option<u64>,
    correlation: Option<&str>,
    payload: &Payload,
) -> tm_types::Result<Vec<u8>>`
- `pub fn chain_hash(prev_hash: &str, body: &[u8]) -> String`

### `crates/tm-events/src/kind.rs`

- `pub enum EventCategory`
- `pub enum EventKind`
- `pub const ALL: &[EventKind] = &[
    EventKind::ProjectCreated,
    EventKind::ProjectAttached,
    EventKind::TicketCreated,
    EventKind::TicketUpdated,
    EventKind::TicketStateChanged,
    EventKind::TicketDependencyAdded,
    EventKind::TicketDependencyRemoved,
    EventKind::TicketChildAdded,
    EventKind::TicketLeased,
    EventKind::TicketHeartbeat,
    EventKind::TicketLeaseExpired,
    EventKind::TicketLeaseReleased,
    EventKind::TicketDelegated,
    EventKind::TicketSubmitted,
    EventKind::TicketVerified,
    EventKind::TicketVerificationFailed,
    EventKind::TicketAudited,
    EventKind::TicketAuditRejected,
    EventKind::TicketClosed,
    EventKind::TicketCancelled,
    EventKind::TicketReopened,
    EventKind::TicketFailed,
    EventKind::TicketRetryScheduled,
    EventKind::TicketEscalated,
    EventKind::TicketBudgetExhausted,
    EventKind::TicketBudgetHandoff,
    EventKind::TicketForked,
    EventKind::DecisionCreated,
    EventKind::DecisionSuperseded,
    EventKind::AuthorityGranted,
    EventKind::AuthorityDelegated,
    EventKind::AuthorityRevoked,
    EventKind::AuthorityReverted,
    EventKind::ResourceClaimed,
    EventKind::ResourceReleased,
    EventKind::ResourceConflictDetected,
    EventKind::ArtifactCreated,
    EventKind::CommandStarted,
    EventKind::CommandCompleted,
    EventKind::MilestoneCreated,
    EventKind::MilestoneClosed,
    EventKind::MilestoneReopened,
    EventKind::SessionStarted,
    EventKind::SessionJoined,
    EventKind::SessionLeft,
    EventKind::SessionEnded,
    EventKind::PresenceUpdated,
    EventKind::CommentCreated,
    EventKind::ApprovalRequested,
    EventKind::ApprovalDecided,
    EventKind::ProviderSelected,
    EventKind::ProviderExhausted,
    EventKind::ProviderDegraded,
    EventKind::ProviderRecovered,
    EventKind::ExecutorFailed,
    EventKind::UsageRecorded,
    EventKind::DocRegistered,
    EventKind::DocGenerated,
    EventKind::DocInvalidated,
    EventKind::DocReconciled,
    EventKind::IndexUpdated,
    EventKind::HarnessChanged,
    EventKind::HarnessBenchmarked,
    EventKind::HarnessPromoted,
    EventKind::GenesisStarted,
    EventKind::GenesisStageEntered,
    EventKind::GenesisStageCompleted,
    EventKind::GenesisAssumptionRecorded,
    EventKind::GenesisMaturityEvaluated,
    EventKind::GenesisCompleted,
    EventKind::MirrorLinked,
    EventKind::MirrorPushed,
    EventKind::MirrorPulled,
    EventKind::EffectJournaled,
    EventKind::EffectCompleted,
    EventKind::EffectFailed,
    EventKind::GoalSet,
    EventKind::GoalStepAdded,
    EventKind::GoalStepCompleted,
    EventKind::GoalReoriented,
    EventKind::GoalClaimedComplete,
];`
- `impl EventKind`
  - `pub fn as_str(self) -> &'static str`
  - `pub fn category(self) -> EventCategory`
  - `pub fn is_ticket_terminal(self) -> bool`

### `crates/tm-events/src/lib.rs`

- `pub mod event;`
- `pub mod kind;`
- `pub mod log;`
- `pub mod payload;`
- `pub mod schema;`
- `pub mod stream;`

### `crates/tm-events/src/log.rs`

- `pub struct ChainReport`
- `impl ChainReport`
  - `pub fn is_valid(&self) -> bool`
- `pub struct Tx<'a>`
- `impl<'a> Tx<'a>`
  - `pub fn raw(&self) -> &Connection`
  - `pub fn raw_mut(&mut self) -> &mut Connection`
  - `pub fn commit(mut self) -> tm_types::Result<()>`
  - `pub fn rollback(mut self) -> tm_types::Result<()>`
- `pub struct EventLog`
- `impl EventLog`
  - `pub fn open(path: &Path) -> tm_types::Result<Self>`
  - `pub fn open_with_clock(path: &Path, clock: Arc<dyn Clock>) -> tm_types::Result<Self>`
  - `pub fn path(&self) -> &Path`
  - `pub fn begin(&self) -> tm_types::Result<Tx<'_>>`
  - `pub fn append_in(&self, tx: &Tx<'_>, draft: EventDraft) -> tm_types::Result<Event>`
  - `pub fn append(&self, draft: EventDraft) -> tm_types::Result<Event>`
  - `pub fn append_all(&self, drafts: Vec<EventDraft>) -> tm_types::Result<Vec<Event>>`
  - `pub fn read_from(&self, seq: u64, limit: usize) -> tm_types::Result<Vec<Event>>`
  - `pub fn read_subject(&self, subject: &Id) -> tm_types::Result<Vec<Event>>`
  - `pub fn read_range(&self, from: u64, to: u64) -> tm_types::Result<Vec<Event>>`
  - `pub fn head(&self) -> tm_types::Result<u64>`
  - `pub fn verify_chain(&self) -> tm_types::Result<ChainReport>`
  - `pub fn subscribe(&self) -> EventStream`

### `crates/tm-events/src/schema.rs`

- `pub const SCHEMA_VERSION: i64 = 1;`
- `pub const EVENTS_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS events (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    subject     TEXT    NOT NULL,
    actor       TEXT    NOT NULL,
    session     TEXT,
    causation   INTEGER,
    correlation TEXT,
    payload     TEXT    NOT NULL,
    hash        TEXT    NOT NULL
);
";`
- `pub const EVENTS_INDEXES_SQL: &str = "
CREATE INDEX IF NOT EXISTS events_subject_idx ON events (subject);
CREATE INDEX IF NOT EXISTS events_correlation_idx ON events (correlation);
CREATE INDEX IF NOT EXISTS events_causation_idx ON events (causation);
";`
- `pub const EVENTS_NO_UPDATE_TRIGGER_SQL: &str = "
CREATE TRIGGER IF NOT EXISTS events_no_update
BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'events is append-only: UPDATE is forbidden');
END;
";`
- `pub const EVENTS_NO_DELETE_TRIGGER_SQL: &str = "
CREATE TRIGGER IF NOT EXISTS events_no_delete
BEFORE DELETE ON events
BEGIN
    SELECT RAISE(ABORT, 'events is append-only: DELETE is forbidden');
END;
";`
- `pub const SCHEMA_VERSION_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS schema_version (
    version     INTEGER NOT NULL PRIMARY KEY,
    applied_at  TEXT    NOT NULL
);
";`
- `pub struct Migration`
- `pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    ddl: "", // Composed from EVENTS_TABLE_SQL + EVENTS_INDEXES_SQL + the two trigger SQL
             // constants + SCHEMA_VERSION_TABLE_SQL by the migrate function; kept empty here
             // so each DDL fragment stays independently documented above and reusable.
}];`
- `pub fn apply_pragmas(conn: &Connection) -> tm_types::Result<()>`
- `pub fn migrate(conn: &mut Connection, clock: &dyn Clock) -> tm_types::Result<()>`
- `pub fn open_write_connection(path: &Path, clock: &dyn Clock) -> tm_types::Result<Connection>`
- `pub fn open_read_connection(path: &Path) -> tm_types::Result<Connection>`

### `crates/tm-events/src/stream.rs`

- `pub struct EventHub`
- `impl EventHub`
  - `pub fn new() -> Self`
  - `pub fn subscribe(&self) -> EventStream`
  - `pub fn publish(&self, event: &Event)`
  - `pub fn subscriber_count(&self) -> usize`
- `pub struct EventStream`
- `impl EventStream`
  - `pub fn try_recv(&self) -> Option<Event>`
  - `pub fn recv(&self) -> Option<Event>`
  - `pub fn iter(&self) -> impl Iterator<Item = Event> + '_`
