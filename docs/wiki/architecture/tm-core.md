+++
[doc]
id = "wiki/architecture/tm-core"
mode = "generated"
derived_from = ["crates/tm-core/src/**"]
+++

# Architecture: tm-core

## Module tree

- `crates/tm-core/src/artifact.rs`
- `crates/tm-core/src/budget.rs`
- `crates/tm-core/src/decision.rs`
- `crates/tm-core/src/effect.rs`
- `crates/tm-core/src/executor.rs`
- `crates/tm-core/src/goal.rs`
- `crates/tm-core/src/graph.rs`
- `crates/tm-core/src/invariants.rs`
- `crates/tm-core/src/lease.rs`
- `crates/tm-core/src/lib.rs`
- `crates/tm-core/src/machine.rs`
- `crates/tm-core/src/materialize.rs`
- `crates/tm-core/src/milestone.rs`
- `crates/tm-core/src/schema.rs`
- `crates/tm-core/src/store.rs`
- `crates/tm-core/src/ticket.rs`
- `crates/tm-core/src/view.rs`

## Public symbols

### `crates/tm-core/src/artifact.rs`

- `pub const INLINE_LIMIT_BYTES: usize = 64 * 1024;`
- `pub enum ArtifactKind`
- `pub enum ArtifactStorage`
- `pub struct Artifact`
- `pub enum EvidenceKind`
- `pub struct Evidence`
- `pub fn hash_bytes(bytes: &[u8]) -> String`
- `pub fn plan_storage(state_dir: &Path, bytes: &[u8]) -> (String, ArtifactStorage)`

### `crates/tm-core/src/budget.rs`

- `pub enum BudgetScope`
- `pub struct ExhaustedScope`
- `pub struct ScopedBudget`
- `pub struct BudgetLedger;`
- `impl BudgetLedger`
  - `pub fn record_usage(
        chain: Vec<ScopedBudget>,
        amount: Spend,
    ) -> Result<Vec<ScopedBudget>, ExhaustedScope>`

### `crates/tm-core/src/decision.rs`

- `pub struct Decision`
- `impl Decision`
  - `pub fn is_active(&self) -> bool`
  - `pub fn create(
        id: DecisionId,
        subject: String,
        decision: String,
        reason: String,
        evidence: Vec<ArtifactId>,
        affected_tickets: Vec<TicketId>,
        affected_paths: Vec<String>,
        author: ParticipantId,
        ts: Timestamp,
    ) -> Self`
  - `pub fn superseding(
        &self,
        new_id: DecisionId,
        subject: String,
        decision: String,
        reason: String,
        evidence: Vec<ArtifactId>,
        affected_tickets: Vec<TicketId>,
        affected_paths: Vec<String>,
        author: ParticipantId,
        ts: Timestamp,
    ) -> Self`
- `pub trait DecisionStore`

### `crates/tm-core/src/effect.rs`

- `pub struct EffectKey`
- `impl EffectKey`
  - `pub fn compute(ticket: &TicketId, attempt: u32, kind: &str, canonical_args: &str) -> Self`
  - `pub fn as_str(&self) -> &str`
  - `pub fn from_hex(hex: impl Into<String>) -> Self`
- `pub enum EffectStatus`
- `impl EffectStatus`
  - `pub(crate) fn as_str(self) -> &'static str`
  - `pub(crate) fn parse(s: &str) -> tm_types::Result<Self>`
- `pub struct Effect`
- `pub struct EffectGuard`
- `impl EffectGuard`
  - `pub(crate) fn new(
        key: EffectKey,
        ticket: TicketId,
        attempt: u32,
        kind: String,
        actor: ParticipantId,
        already_completed: bool,
        resumed: bool,
        prior_receipt: Option<String>,
    ) -> Self`
  - `pub fn key(&self) -> &EffectKey`
  - `pub fn ticket(&self) -> &TicketId`
  - `pub fn already_completed(&self) -> bool`
  - `pub fn resumed(&self) -> bool`
  - `pub fn prior_receipt(&self) -> Option<&str>`
  - `pub fn complete(
        &self,
        store: &crate::store::Store,
        receipt_artifact: Option<&str>,
    ) -> tm_types::Result<()>`
  - `pub fn fail(&self, store: &crate::store::Store, reason: &str) -> tm_types::Result<()>`
  - `pub fn kind(&self) -> &str`
  - `pub fn attempt(&self) -> u32`

### `crates/tm-core/src/executor.rs`

- `pub enum CostClass`
- `pub struct ExecutorCapabilities`
- `pub struct ExecutionHandle`
- `impl ExecutionHandle`
  - `pub fn new(id: impl Into<String>) -> Self`
  - `pub fn as_str(&self) -> &str`
- `pub struct ExecutorTask`
- `pub struct ExecutorFailure`
- `pub struct ExecutorOutcome`
- `impl ExecutorOutcome`
  - `pub fn is_success(&self) -> bool`
- `pub trait Executor: Send + Sync`
- `pub struct FsScope`
- `pub struct NetPolicy`
- `pub struct Sandbox`
- `pub fn sandbox_for(authority: &Authority) -> Sandbox`
- `pub enum ReturnScopeViolation`
- `pub fn validate_return_scope(
    diff: &str,
    write_scope: &PatternSet,
) -> Result<Vec<String>, ReturnScopeViolation>`

### `crates/tm-core/src/goal.rs`

- `pub struct GoalStep`
- `pub struct GoalState`

### `crates/tm-core/src/graph.rs`

- `pub struct DependencyEdge`
- `pub struct DependencyGraph`
- `pub struct CycleViolation`
- `impl DependencyGraph`
  - `pub fn build(
        nodes: impl IntoIterator<Item = TicketId>,
        edges: impl IntoIterator<Item = DependencyEdge>,
        children: impl IntoIterator<Item = (TicketId, TicketId)>,
    ) -> Self`
  - `pub fn nodes(&self) -> impl Iterator<Item = &TicketId>`
  - `pub fn edges(&self) -> &[DependencyEdge]`
  - `pub fn parent_of(&self, ticket: &TicketId) -> Option<&TicketId>`
  - `pub fn children_of(&self, ticket: &TicketId) -> &[TicketId]`
  - `pub fn ancestors(&self, ticket: &TicketId) -> BTreeSet<TicketId>`
  - `pub fn descendants(&self, ticket: &TicketId) -> BTreeSet<TicketId>`
  - `pub fn dependencies_satisfied(&self, ticket: &TicketId, closed: &BTreeSet<TicketId>) -> bool`
  - `pub fn find_illegal_cycles(
        &self,
        has_cycle_budget: impl Fn(&TicketId) -> bool,
    ) -> Vec<CycleViolation>`
  - `pub fn topological_order(&self) -> Option<Vec<TicketId>>`
  - `pub fn critical_path_length(&self, ticket: &TicketId) -> u32`

### `crates/tm-core/src/invariants.rs`

- `pub struct Violation`
- `pub mod names`
  - `pub const READY_DEPENDENCIES_SATISFIED: &str = "4.3-ready-dependencies-satisfied";`
  - `pub const NO_DOUBLE_LEASE: &str = "4.3-no-double-lease";`
  - `pub const NO_CONFLICTING_CLAIMS: &str = "4.3-no-conflicting-claims";`
  - `pub const CHILD_AUTHORITY_CONTAINED: &str = "4.3-child-authority-contained";`
  - `pub const CLOSED_HAS_EVIDENCE: &str = "4.3-closed-has-evidence";`
  - `pub const AUDITOR_NOT_EXECUTOR: &str = "4.3-auditor-not-executor";`
  - `pub const ACYCLIC_EXCEPT_LOOP: &str = "4.3-acyclic-except-loop";`
- `pub fn check_invariants(view: &ProjectView) -> Vec<Violation>`
- `pub fn check_ready_dependencies_satisfied(view: &ProjectView) -> Vec<Violation>`
- `pub fn check_no_double_lease(view: &ProjectView) -> Vec<Violation>`
- `pub fn check_no_conflicting_claims(view: &ProjectView) -> Vec<Violation>`
- `pub fn check_child_authority_contained(view: &ProjectView) -> Vec<Violation>`
- `pub fn check_closed_has_evidence(view: &ProjectView) -> Vec<Violation>`
- `pub fn check_auditor_not_executor(view: &ProjectView) -> Vec<Violation>`
- `pub fn check_acyclic_except_loop(view: &ProjectView) -> Vec<Violation>`

### `crates/tm-core/src/lease.rs`

- `pub struct Lease`
- `impl Lease`
  - `pub fn is_expired(&self, now: Timestamp) -> bool`
- `pub enum AcquireError`
- `pub enum HeartbeatError`
- `pub struct ReversionAction`
- `pub trait LeaseView`
- `pub struct LeaseStore;`
- `impl LeaseStore`
  - `pub fn acquire(
        ticket_state: TicketState,
        ticket_authority: &Authority,
        view: &dyn LeaseView,
        id: tm_types::LeaseId,
        ticket: TicketId,
        holder: ParticipantId,
        authority: Authority,
        resources: Vec<ResourceClaim>,
        now: Timestamp,
        ttl_seconds: u32,
        epoch: u64,
    ) -> Result<Lease, AcquireError>`
  - `pub fn heartbeat(lease: &mut Lease, now: Timestamp) -> Result<(), HeartbeatError>`
  - `pub fn expire_due(view: &dyn LeaseView, now: Timestamp) -> Vec<ReversionAction>`
- `pub fn claims_conflict(a: &ResourceClaim, b: &ResourceClaim) -> bool`

### `crates/tm-core/src/lib.rs`

- `pub mod artifact;`
- `pub mod budget;`
- `pub mod decision;`
- `pub mod effect;`
- `pub mod executor;`
- `pub mod goal;`
- `pub mod graph;`
- `pub mod invariants;`
- `pub mod lease;`
- `pub mod machine;`
- `pub mod materialize;`
- `pub mod milestone;`
- `pub mod schema;`
- `pub mod store;`
- `pub mod ticket;`
- `pub mod view;`

### `crates/tm-core/src/machine.rs`

- `pub struct InvalidTransition`
- `pub fn transition(from: TicketState, trigger: Trigger) -> Result<TicketState, InvalidTransition>`
- `pub fn is_terminal(state: TicketState) -> bool`
- `pub fn is_live(state: TicketState) -> bool`
- `pub fn can_lease(state: TicketState) -> bool`
- `pub struct TransitionTable`
- `impl TransitionTable`
  - `pub fn build() -> Self`
  - `pub fn entries(&self) -> &[(TicketState, Trigger, TicketState)]`

### `crates/tm-core/src/materialize.rs`

- `pub fn apply(tx: &Tx<'_>, event: &Event) -> tm_types::Result<()>`
- `pub fn replay(tx: &Tx<'_>, events: &[Event]) -> tm_types::Result<()>`

### `crates/tm-core/src/milestone.rs`

- `pub enum MilestoneState`
- `pub struct Milestone`
- `pub enum CloseError`
- `pub struct MilestoneStore;`
- `impl MilestoneStore`
  - `pub fn close(
        milestone: &Milestone,
        member_states: &std::collections::BTreeMap<TicketId, TicketState>,
        closed_by: ParticipantId,
    ) -> Result<Milestone, CloseError>`
  - `pub fn reopen(milestone: &Milestone, graph: &DependencyGraph) -> (Milestone, Vec<TicketId>)`
  - `pub fn membership_of<'a>(all: &'a [Milestone], ticket: &TicketId) -> Vec<&'a Milestone>`

### `crates/tm-core/src/schema.rs`

- `pub const SCHEMA_VERSION: i64 = 3;`
- `pub struct TableDef`
- `pub const TABLES: &[TableDef] = &[
    TableDef {
        name: "counters",
        create_sql: "
            CREATE TABLE IF NOT EXISTS counters (
                counter_name TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            )
        ",
    },
    TableDef {
        name: "participants",
        create_sql: "
            CREATE TABLE IF NOT EXISTS participants (
                id TEXT PRIMARY KEY,
                status TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "sessions",
        create_sql: "
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                participant TEXT NOT NULL,
                started TEXT NOT NULL,
                last_seen TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "artifacts",
        create_sql: "
            CREATE TABLE IF NOT EXISTS artifacts (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                media_type TEXT NOT NULL,
                bytes_len INTEGER NOT NULL,
                hash TEXT NOT NULL,
                storage TEXT NOT NULL,
                meta TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "decisions",
        create_sql: "
            CREATE TABLE IF NOT EXISTS decisions (
                id TEXT PRIMARY KEY,
                subject TEXT NOT NULL,
                decision TEXT NOT NULL,
                reason TEXT NOT NULL,
                evidence TEXT NOT NULL,
                affected_tickets TEXT NOT NULL,
                affected_paths TEXT NOT NULL,
                author TEXT NOT NULL,
                ts TEXT NOT NULL,
                supersedes TEXT,
                superseded_by TEXT
            )
        ",
    },
    TableDef {
        name: "milestones",
        create_sql: "
            CREATE TABLE IF NOT EXISTS milestones (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                tickets TEXT NOT NULL,
                state TEXT NOT NULL,
                closed_by TEXT,
                assumptions TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "tickets",
        create_sql: "
            CREATE TABLE IF NOT EXISTS tickets (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                objective TEXT NOT NULL,
                state TEXT NOT NULL,
                parent TEXT,
                milestone TEXT,
                authority TEXT NOT NULL,
                resources TEXT NOT NULL,
                executor TEXT NOT NULL,
                context_refs TEXT NOT NULL,
                success TEXT NOT NULL,
                verification TEXT NOT NULL,
                budget TEXT NOT NULL,
                retry TEXT NOT NULL,
                cycle TEXT,
                attempts INTEGER NOT NULL,
                failures TEXT NOT NULL,
                priority INTEGER NOT NULL,
                created TEXT NOT NULL,
                updated TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "ticket_deps",
        create_sql: "
            CREATE TABLE IF NOT EXISTS ticket_deps (
                ticket TEXT NOT NULL,
                depends_on TEXT NOT NULL,
                kind TEXT NOT NULL,
                PRIMARY KEY (ticket, depends_on)
            )
        ",
    },
    TableDef {
        name: "ticket_children",
        create_sql: "
            CREATE TABLE IF NOT EXISTS ticket_children (
                parent TEXT NOT NULL,
                child TEXT NOT NULL,
                PRIMARY KEY (parent, child)
            )
        ",
    },
    TableDef {
        name: "leases",
        create_sql: "
            CREATE TABLE IF NOT EXISTS leases (
                id TEXT PRIMARY KEY,
                ticket TEXT NOT NULL,
                holder TEXT NOT NULL,
                authority TEXT NOT NULL,
                resources TEXT NOT NULL,
                acquired TEXT NOT NULL,
                heartbeat TEXT NOT NULL,
                ttl_seconds INTEGER NOT NULL,
                epoch INTEGER NOT NULL
            )
        ",
    },
    TableDef {
        name: "resource_claims",
        create_sql: "
            CREATE TABLE IF NOT EXISTS resource_claims (
                lease TEXT NOT NULL,
                paths TEXT NOT NULL,
                mode TEXT NOT NULL,
                PRIMARY KEY (lease)
            )
        ",
    },
    TableDef {
        name: "evidence",
        create_sql: "
            CREATE TABLE IF NOT EXISTS evidence (
                ticket TEXT NOT NULL,
                kind TEXT NOT NULL,
                artifact TEXT NOT NULL,
                produced_by TEXT NOT NULL,
                ts TEXT NOT NULL,
                summary TEXT NOT NULL,
                PRIMARY KEY (ticket, artifact)
            )
        ",
    },
    TableDef {
        name: "budgets",
        create_sql: "
            CREATE TABLE IF NOT EXISTS budgets (
                scope TEXT NOT NULL,
                scope_id TEXT,
                limits TEXT NOT NULL,
                spent TEXT NOT NULL,
                PRIMARY KEY (scope, scope_id)
            )
        ",
    },
    TableDef {
        name: "docs",
        create_sql: "
            CREATE TABLE IF NOT EXISTS docs (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                content TEXT NOT NULL,
                author TEXT NOT NULL,
                ts TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "doc_provenance",
        create_sql: "
            CREATE TABLE IF NOT EXISTS doc_provenance (
                doc_id TEXT NOT NULL,
                source TEXT NOT NULL,
                reason TEXT NOT NULL,
                PRIMARY KEY (doc_id, source)
            )
        ",
    },
    TableDef {
        name: "provider_usage",
        create_sql: "
            CREATE TABLE IF NOT EXISTS provider_usage (
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                tokens_used INTEGER NOT NULL,
                dollars_micros INTEGER NOT NULL,
                last_updated TEXT NOT NULL,
                PRIMARY KEY (provider, model)
            )
        ",
    },
    TableDef {
        name: "harness_epochs",
        create_sql: "
            CREATE TABLE IF NOT EXISTS harness_epochs (
                epoch INTEGER PRIMARY KEY,
                harness_config TEXT NOT NULL,
                ts TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "mirror_links",
        create_sql: "
            CREATE TABLE IF NOT EXISTS mirror_links (
                ticket TEXT PRIMARY KEY,
                remote_id TEXT NOT NULL,
                remote_system TEXT NOT NULL,
                last_synced TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "effects",
        create_sql: "
            CREATE TABLE IF NOT EXISTS effects (
                key TEXT PRIMARY KEY,
                ticket TEXT NOT NULL,
                attempt INTEGER NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                receipt_artifact TEXT,
                started TEXT NOT NULL,
                completed TEXT
            )
        ",
    },
    TableDef {
        name: "goals",
        create_sql: "
            CREATE TABLE IF NOT EXISTS goals (
                ticket TEXT PRIMARY KEY,
                text TEXT NOT NULL,
                steps TEXT NOT NULL,
                claimed_complete INTEGER NOT NULL,
                last_reoriented_step INTEGER NOT NULL,
                set_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "workflows",
        create_sql: "
            CREATE TABLE IF NOT EXISTS workflows (
                content_hash TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                version INTEGER NOT NULL,
                source TEXT NOT NULL,
                registered_at TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "meta",
        create_sql: "
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )
        ",
    },
];`
- `pub const INDEXES_SQL: &str = "
CREATE INDEX IF NOT EXISTS ticket_deps_depends_on_idx ON ticket_deps (depends_on);
CREATE INDEX IF NOT EXISTS ticket_children_child_idx ON ticket_children (child);
CREATE INDEX IF NOT EXISTS leases_ticket_idx ON leases (ticket);
CREATE INDEX IF NOT EXISTS leases_holder_idx ON leases (holder);
CREATE INDEX IF NOT EXISTS decisions_subject_idx ON decisions (subject);
CREATE INDEX IF NOT EXISTS evidence_ticket_idx ON evidence (ticket);
CREATE INDEX IF NOT EXISTS sessions_participant_idx ON sessions (participant);
CREATE INDEX IF NOT EXISTS tickets_parent_idx ON tickets (parent);
CREATE INDEX IF NOT EXISTS tickets_milestone_idx ON tickets (milestone);
CREATE INDEX IF NOT EXISTS effects_ticket_idx ON effects (ticket);
CREATE INDEX IF NOT EXISTS workflows_name_idx ON workflows (name);
";`
- `pub fn migrate(conn: &mut Connection, clock: &dyn tm_types::Clock) -> tm_types::Result<()>`
- `pub fn create_views(conn: &Connection) -> tm_types::Result<()>`
- `pub fn drop_views(conn: &Connection) -> tm_types::Result<()>`

### `crates/tm-core/src/store.rs`

- `pub struct Store`
- `pub struct StoreTx<'a>`
- `impl<'a> StoreTx<'a>`
  - `pub fn append(&self, draft: EventDraft) -> tm_types::Result<Event>`
  - `pub fn append_all(&self, drafts: Vec<EventDraft>) -> tm_types::Result<Vec<Event>>`
  - `pub fn raw(&self) -> &Connection`
- `pub enum MirrorSyncDirection`
- `pub struct DocRow`
- `pub struct MirrorLinkRow`
- `pub struct HarnessEpochRow`
- `pub struct WorkflowDefRow`
- `impl Store`
  - `pub fn open(project_root: &Path) -> tm_types::Result<Self>`
  - `pub fn open_at(state_dir: &Path) -> tm_types::Result<Self>`
  - `pub fn open_with(
        project_root: &Path,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
    ) -> tm_types::Result<Self>`
  - `pub fn open_with_at(
        state_dir: &Path,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
    ) -> tm_types::Result<Self>`
  - `pub fn state_dir(&self) -> &Path`
  - `pub fn create_ticket(
        &self,
        kind: TicketKind,
        objective: String,
        parent: Option<TicketId>,
        milestone: Option<MilestoneId>,
        authority: Authority,
        resources: Vec<ResourceClaim>,
        executor: ExecutorRequirements,
        context_refs: Vec<ContextRef>,
        success: Vec<Predicate>,
        verification: VerificationPolicy,
        budget: Budget,
        retry: RetryPolicy,
        priority: i32,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn update_ticket(
        &self,
        ticket: &TicketId,
        fields: serde_json::Value,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn add_dependency(
        &self,
        ticket: &TicketId,
        depends_on: &TicketId,
        kind: DependencyKind,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn activate(
        &self,
        ticket: &TicketId,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn transition(
        &self,
        ticket: &TicketId,
        trigger: Trigger,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn submit(
        &self,
        ticket: &TicketId,
        summary: String,
        evidence: Vec<ArtifactId>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn verify(
        &self,
        ticket: &TicketId,
        verifier: &TicketId,
        passed: bool,
        reason: Option<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn audit(
        &self,
        ticket: &TicketId,
        auditor: &TicketId,
        outcome: AuditOutcome,
        reason: Option<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn close(
        &self,
        ticket: &TicketId,
        reason: Option<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn cancel(
        &self,
        ticket: &TicketId,
        reason: Option<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn reopen(
        &self,
        ticket: &TicketId,
        reason: Option<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn record_failure(
        &self,
        ticket: &TicketId,
        class: FailureClass,
        detail: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn acquire_lease(
        &self,
        ticket: &TicketId,
        holder: ParticipantId,
        authority: Authority,
        resources: Vec<ResourceClaim>,
        ttl_seconds: u32,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn heartbeat(&self, lease: &LeaseId, actor: ParticipantId) -> tm_types::Result<Vec<Event>>`
  - `pub fn release(&self, lease: &LeaseId, actor: ParticipantId) -> tm_types::Result<Vec<Event>>`
  - `pub fn expire_leases(&self) -> tm_types::Result<Vec<Event>>`
  - `pub fn record_decision(
        &self,
        subject: String,
        decision: String,
        reason: String,
        evidence: Vec<ArtifactId>,
        affected_tickets: Vec<TicketId>,
        affected_paths: Vec<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn supersede(
        &self,
        supersedes: &DecisionId,
        subject: String,
        decision: String,
        reason: String,
        evidence: Vec<ArtifactId>,
        affected_tickets: Vec<TicketId>,
        affected_paths: Vec<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn create_milestone(
        &self,
        title: String,
        tickets: Vec<TicketId>,
        assumptions: Vec<DecisionId>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn close_milestone(
        &self,
        milestone: &MilestoneId,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn reopen_milestone(
        &self,
        milestone: &MilestoneId,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn store_artifact(
        &self,
        kind: ArtifactKind,
        media_type: String,
        bytes: Vec<u8>,
        meta: serde_json::Value,
        ticket: Option<TicketId>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn attach_evidence(
        &self,
        ticket: &TicketId,
        kind: EvidenceKind,
        artifact: &ArtifactId,
        summary: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn record_usage(
        &self,
        ticket: Option<&TicketId>,
        session: Option<&SessionId>,
        amount: Spend,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn budget_handoff(
        &self,
        ticket: &TicketId,
        dimension: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn effect_status(&self, key: &EffectKey) -> tm_types::Result<Option<Effect>>`
  - `pub fn begin_effect(
        &self,
        key: EffectKey,
        ticket: TicketId,
        attempt: u32,
        kind: impl Into<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<EffectGuard>`
  - `pub(crate) fn complete_effect(
        &self,
        key: &EffectKey,
        ticket: &TicketId,
        receipt_artifact: Option<&str>,
        actor: ParticipantId,
    ) -> tm_types::Result<()>`
  - `pub(crate) fn fail_effect(
        &self,
        key: &EffectKey,
        ticket: &TicketId,
        reason: &str,
        actor: ParticipantId,
    ) -> tm_types::Result<()>`
  - `pub fn view(&self) -> tm_types::Result<ProjectView>`
  - `pub fn scheduler_view(&self) -> tm_types::Result<SchedulerView>`
  - `pub fn rebuild(&self) -> tm_types::Result<()>`
  - `pub fn check_invariants(&self) -> tm_types::Result<Vec<crate::invariants::Violation>>`
  - `pub fn transaction<F, T>(&self, f: F) -> tm_types::Result<T>
    where
        F: FnOnce(&StoreTx<'_>) -> tm_types::Result<T>,`
  - `pub fn append(&self, drafts: Vec<EventDraft>) -> tm_types::Result<Vec<Event>>`
  - `pub fn start_session(
        &self,
        participant: ParticipantId,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn join_session(
        &self,
        session: &SessionId,
        participant: ParticipantId,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn end_session(
        &self,
        session: &SessionId,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn register_doc(
        &self,
        path: String,
        ticket: Option<TicketId>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn invalidate_doc(
        &self,
        path: String,
        reason: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn reconcile_doc(
        &self,
        path: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn promote_epoch(
        &self,
        candidate: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn link_mirror(
        &self,
        ticket: &TicketId,
        remote: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn update_mirror_link(
        &self,
        ticket: &TicketId,
        remote: String,
        reference: String,
        direction: MirrorSyncDirection,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn record_command(
        &self,
        command: String,
        ticket: Option<TicketId>,
        session: Option<SessionId>,
        exit_code: i32,
        duration_ms: u64,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn set_goal(
        &self,
        ticket: &TicketId,
        text: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn add_goal_step(
        &self,
        ticket: &TicketId,
        step_id: String,
        text: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn complete_goal_step(
        &self,
        ticket: &TicketId,
        step_id: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn reorient_goal(
        &self,
        ticket: &TicketId,
        at_step: u32,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn claim_goal_complete(
        &self,
        ticket: &TicketId,
        summary: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn goal_state(&self, ticket: &TicketId) -> tm_types::Result<Option<GoalState>>`
  - `pub fn fork_ticket(
        &self,
        source: &TicketId,
        seq: u64,
        actor: ParticipantId,
    ) -> tm_types::Result<(TicketId, Vec<Event>)>`
  - `pub fn event_count_for(&self, subject: &Id) -> tm_types::Result<u64>`
  - `pub fn counters(&self) -> tm_types::Result<BTreeMap<String, u64>>`
  - `pub fn docs(&self) -> tm_types::Result<Vec<DocRow>>`
  - `pub fn mirror_links(&self) -> tm_types::Result<Vec<MirrorLinkRow>>`
  - `pub fn harness_epochs(&self) -> tm_types::Result<Vec<HarnessEpochRow>>`
  - `pub fn register_workflow_def(
        &self,
        name: String,
        content_hash: String,
        source: String,
    ) -> tm_types::Result<u32>`
  - `pub fn workflow_defs(&self, name: Option<&str>) -> tm_types::Result<Vec<WorkflowDefRow>>`
- `pub enum AuditOutcome`

### `crates/tm-core/src/ticket.rs`

- `pub enum TicketKind`
- `impl TicketKind`
  - `pub fn permits_unverified_close(self) -> bool`
- `pub enum TicketState`
- `impl TicketState`
  - `pub const ALL: &'static [TicketState] = &[
        TicketState::Draft,
        TicketState::Blocked,
        TicketState::Ready,
        TicketState::Leased,
        TicketState::Running,
        TicketState::Submitted,
        TicketState::Verifying,
        TicketState::Auditing,
        TicketState::Rework,
        TicketState::Replan,
        TicketState::Recovery,
        TicketState::Escalated,
        TicketState::Closed,
        TicketState::Cancelled,
    ];`
- `pub enum Trigger`
- `pub enum DependencyKind`
- `pub enum ResourceMode`
- `pub struct ResourceClaim`
- `pub struct ExecutorRequirements`
- `pub struct ContextRef`
- `pub enum VerificationPolicy`
- `pub struct RetryPolicy`
- `impl RetryPolicy`
  - `pub fn delay_for_attempt(&self, attempt: u32) -> u32`
- `pub struct CycleBudget`
- `impl CycleBudget`
  - `pub fn has_budget(&self) -> bool`
- `pub enum FailureClass`
- `impl FailureClass`
  - `pub fn is_retryable(self) -> bool`
- `pub struct FailureRecord`
- `pub struct Ticket`

### `crates/tm-core/src/view.rs`

- `pub struct ProjectView`
- `pub struct ParticipantSummary`
- `impl ProjectView`
  - `pub fn empty() -> Self`
- `pub struct SchedulerView`
