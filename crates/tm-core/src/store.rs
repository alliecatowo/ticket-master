//! `Store`: the facade every other crate builds on.
//!
//! Owns the project's `EventLog` (via `tm_events`), an injected [`Clock`] and [`IdSource`], and
//! the command API: every method here validates the requested change against `machine.rs` and
//! `invariants.rs`, then appends the resulting event(s) and materializes them (`materialize.rs`)
//! in exactly one `tm_events::Tx`, and returns the events it emitted. This is the only module
//! that opens a transaction — every other module in this crate is pure and takes/returns plain
//! values.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tm_events::{Event, EventLog};
use tm_types::{
    ArtifactId, Authority, Clock, DecisionId, IdSource, LeaseId, MilestoneId, ParticipantId,
    Spend, TicketId, Timestamp,
};

use crate::artifact::{ArtifactKind, Evidence, EvidenceKind};
use crate::graph::GraphView;
use crate::invariants::Violation;
use crate::lease::Lease;
use crate::milestone::Milestone;
use crate::ticket::{
    ContextRef, ExecutorRequirements, ResourceClaim, RetryPolicy, Ticket, TicketKind, Trigger,
    VerificationPolicy,
};
use crate::view::{ProjectView, SchedulerView};

/// Fields a caller may change via [`Store::update_ticket`]; anything not set here is left alone.
/// Kept as an explicit struct (rather than a raw JSON patch) so the public API stays typed; the
/// JSON `fields` object on the wire (`ticket.updated`'s payload) is an implementation detail of
/// how this gets recorded, not something callers construct by hand.
#[derive(Debug, Clone, Default)]
pub struct TicketUpdate {
    /// New objective text, if changing.
    pub objective: Option<String>,
    /// New priority, if changing.
    pub priority: Option<i32>,
    /// New authority, if changing (must remain contained by the parent's, checked by
    /// `invariants.rs` after the write).
    pub authority: Option<Authority>,
    /// New resource claims, if replacing wholesale.
    pub resources: Option<Vec<ResourceClaim>>,
    /// New executor requirements, if changing.
    pub executor: Option<ExecutorRequirements>,
    /// New context refs, if replacing wholesale.
    pub context_refs: Option<Vec<ContextRef>>,
    /// New verification policy, if changing.
    pub verification: Option<VerificationPolicy>,
    /// New retry policy, if changing.
    pub retry: Option<RetryPolicy>,
}

/// The facade over one project's state: an `EventLog` plus the injected `Clock`/`IdSource` every
/// command needs to stamp determinism-sensitive fields.
pub struct Store {
    root: PathBuf,
    log: Arc<EventLog>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
}

impl Store {
    /// Open (creating if absent) the store at `project_root/.tm/project.db`, using the real wall
    /// clock and a real random-hex-backed id source. Prefer [`Store::open_with`] in tests.
    pub fn open(project_root: &Path) -> tm_types::Result<Store> {
        Store::open_with(project_root, Arc::new(tm_types::SystemClock), Arc::new(tm_types::CounterIds::new()))
    }

    /// Open the store at `project_root/.tm/project.db` with an injected clock and id source, so
    /// every command's timestamps and allocated ids are deterministic and replayable in tests.
    // IMPL: `EventLog::open_with_clock(project_root.join(".tm/project.db"), clock.clone())`; on a
    // freshly created database (no `tm_core_schema_version` rows) also run
    // `schema::migrate` against a connection to the same file for the materialized-view tables
    // (the event log's own schema is `tm_events::schema`'s concern, not this crate's — `tm-core`
    // only owns the *view* tables). Error case: propagate storage/migration failures via `?`.
    pub fn open_with(project_root: &Path, clock: Arc<dyn Clock>, ids: Arc<dyn IdSource>) -> tm_types::Result<Store> {
        todo!("EventLog::open_with_clock at project_root/.tm/project.db; run schema::migrate for the view tables; construct Store")
    }

    /// The project root this store was opened against.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Build the full [`ProjectView`] by reading every materialized table.
    // IMPL: open a read connection (or reuse `self.log`'s facilities indirectly — `tm-core` does
    // not have direct file access to `tm_events`'s write connection, so this opens its own
    // connection to the same sqlite file via `rusqlite::Connection::open` in read-only mode,
    // mirroring `tm_events::schema::open_read_connection`'s approach) and `SELECT` every table
    // `schema.rs` defines, deserializing JSON columns (authority, executor, etc.) back into their
    // typed shapes. Error case: a decode failure on any row is `TmError::storage` (corruption).
    pub fn view(&self) -> tm_types::Result<ProjectView> {
        todo!("SELECT every materialized table into a ProjectView, decoding JSON columns")
    }

    /// Build the narrower [`SchedulerView`] the scheduler needs.
    pub fn scheduler_view(&self) -> tm_types::Result<SchedulerView> {
        Ok(SchedulerView::from(&self.view()?))
    }

    /// Drop every materialized view table and replay the full event log from `seq 0` to rebuild
    /// it, asserting (by construction, since both paths share `materialize::apply`) that the
    /// result is byte-identical to what the live path would have produced.
    // IMPL: open a write connection to the view database, `schema::drop_views(&mut conn)`,
    // `schema::migrate(&mut conn)`, then stream every event via `self.log.read_from(1, BATCH)` in
    // batches (avoid loading a huge log into memory at once) and `materialize::replay` each batch
    // inside one `tm_events::Tx` per batch (or one `Tx` for the whole rebuild if log size permits
    // — either is correct, batching only matters for memory). Error case: any failure aborts the
    // rebuild via the batch's `Tx` rollback; a partially-rebuilt view must never be left in place,
    // so the drop+migrate should itself be inside the same outer transaction as the first batch,
    // or guarded by writing to a scratch file and swapping — pick the transactional approach
    // since sqlite supports nested-via-savepoint semantics `Tx` doesn't expose, so in practice
    // this likely wants its own dedicated connection/transaction rather than reusing `tx.rs`'s
    // event-log-coupled `Tx` type; document the chosen approach at the call site.
    pub fn rebuild(&self) -> tm_types::Result<()> {
        todo!("drop_views + migrate + replay every event from seq 0, transactionally, so a failed rebuild never leaves a partial view")
    }

    /// Restore `self`'s `IdSource` counters from the materialized `counters` table (e.g. after a
    /// process restart), so freshly allocated ids continue the same monotonic sequence.
    // IMPL: read `counters` rows via `self.view()?.counters`, call `self.ids.observe(kind, value)`
    // for each (the `tm_types::IdSource::observe` method folds in a floor without going
    // backwards). Error case: propagates `self.view()`'s errors.
    pub fn restore_counters(&self) -> tm_types::Result<()> {
        todo!("view().counters -> self.ids.observe(kind, value) for each entry")
    }

    /// Create a new ticket. Validates nothing state-machine-related (a fresh ticket starts in
    /// `TicketState::Draft`); appends `ticket.created` (+ `ticket.child_added` if `parent` is
    /// set) and materializes it in one transaction.
    // IMPL: allocate a `TicketId` via `self.ids.next(kind_for(kind))`, build the initial `Ticket`
    // (`state = Draft`, `created = updated = self.clock.now()`), draft
    // `TicketCreatedPayload{ticket, title: objective.clone(), parent}` (+ a
    // `TicketChildAddedPayload` if `parent.is_some()`), `self.log.begin()?`, `append_in` each
    // draft, `materialize::apply` each resulting event via `tx`, then check
    // `invariants::check_invariants` against a view that includes the pending change (rebuilding
    // a full view mid-transaction is expensive; a lighter-weight approach re-checks only the
    // affected rules — document whichever this crate's implementer picks) before `tx.commit()`;
    // roll back and return `TmError::invariant` on any violation. Error cases: id allocation
    // never fails; event append/materialize failures surface via `?`; invariant violations map to
    // `TmError::invariant`.
    pub fn create_ticket(
        &self,
        kind: TicketKind,
        objective: String,
        parent: Option<TicketId>,
        authority: Authority,
        executor: ExecutorRequirements,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        todo!("allocate TicketId, build initial Draft Ticket, append+materialize ticket.created (+ child_added), check invariants, commit")
    }

    /// Apply a typed patch to an existing ticket. Emits `ticket.updated`.
    pub fn update_ticket(&self, ticket: &TicketId, patch: TicketUpdate, actor: ParticipantId) -> tm_types::Result<Vec<Event>> {
        todo!("draft ticket.updated with a JSON fields object built from the non-None patch fields; append+materialize+commit")
    }

    /// Add a dependency edge. Emits `ticket.dependency_added`; re-evaluates whether `ticket`
    /// should move `Blocked -> Ready` if the new edge happens to already be satisfied is NOT
    /// automatic here — that is the scheduler's `DependenciesSatisfied` trigger, computed
    /// separately (see `activate`/`transition`).
    pub fn add_dependency(&self, ticket: &TicketId, depends_on: TicketId, kind: crate::ticket::DependencyKind, actor: ParticipantId) -> tm_types::Result<Vec<Event>> {
        todo!("validate depends_on exists and the resulting graph passes graph::check_cycles; append+materialize ticket.dependency_added; commit")
    }

    /// Activate a `Draft` ticket: `machine::transition(Draft, Activate)` to `Blocked`, then, if
    /// `graph.dependencies_satisfied` is already true, immediately follow with
    /// `DependenciesSatisfied` to land on `Ready` — both hops recorded as separate
    /// `ticket.state_changed` events in the same transaction (`SPEC.md` §4.3 rule 1).
    pub fn activate(&self, ticket: &TicketId, actor: ParticipantId) -> tm_types::Result<Vec<Event>> {
        todo!("machine::transition(Draft, Activate); if dependencies_satisfied also transition(Blocked, DependenciesSatisfied); append+materialize+commit")
    }

    /// Drive an arbitrary legal transition. Most callers use the more specific methods below;
    /// this is the escape hatch for triggers that don't have their own named method (e.g.
    /// `DependencyReopened`, driven by the scheduler).
    pub fn transition(&self, ticket: &TicketId, trigger: Trigger, actor: ParticipantId) -> tm_types::Result<Vec<Event>> {
        todo!("look up current state, machine::transition(state, trigger), append+materialize ticket.state_changed, check invariants, commit")
    }

    /// Submit evidence for a `Running` ticket, moving it to `Submitted` then immediately
    /// `Verifying` (`SPEC.md` §4.3 rules 5-6). A submission with no evidence is rejected before
    /// any event is drafted — a worker cannot mark itself verified.
    pub fn submit(&self, ticket: &TicketId, evidence: Vec<Evidence>, actor: ParticipantId) -> tm_types::Result<Vec<Event>> {
        todo!("require evidence non-empty; transition Running->Submitted->Verifying; append ticket.submitted + evidence rows + state_changed events; commit")
    }

    /// Record a `Verification` ticket's result against the ticket it verified.
    /// `VerificationPassed -> Auditing`, `VerificationFailed -> Recovery` (rule 6).
    pub fn verify(&self, ticket: &TicketId, verifier: TicketId, passed: bool, reason: Option<String>, actor: ParticipantId) -> tm_types::Result<Vec<Event>> {
        todo!("append ticket.verified or ticket.verification_failed + matching state_changed; commit")
    }

    /// Record an `Audit` ticket's result. Enforces "a worker never certifies itself": `auditor`
    /// must differ from the executor that produced the submission (the lease holder recorded on
    /// the most recent `ticket.leased` event for `ticket`), else `TmError::invariant`.
    /// `AuditPassed -> Closed`, `AuditRejectedMinor -> Rework`, `AuditRejectedStructural ->
    /// Replan` (rule 7).
    pub fn audit(&self, ticket: &TicketId, auditor: TicketId, outcome: AuditOutcome, actor: ParticipantId) -> tm_types::Result<Vec<Event>> {
        todo!("verify auditor != executor for ticket's latest submission; append ticket.audited/audit_rejected + state_changed; commit")
    }

    /// Explicitly close a ticket outside the audit flow (e.g. an `Investigation`/`Harness` ticket
    /// under `VerificationPolicy::None`). Enforces invariant 5 (evidence unless permitted).
    pub fn close(&self, ticket: &TicketId, reason: Option<String>, actor: ParticipantId) -> tm_types::Result<Vec<Event>> {
        todo!("validate invariant 5 (evidence present, or VerificationPolicy::None + permits_unverified_close); transition to Closed; append ticket.closed; commit")
    }

    /// Cancel a ticket from any non-terminal state, requiring `authority.tickets.cancel` on
    /// `actor`'s authority (rule 12).
    pub fn cancel(&self, ticket: &TicketId, reason: Option<String>, actor: ParticipantId, authority: &Authority) -> tm_types::Result<Vec<Event>> {
        todo!("require authority.tickets.cancel; machine::transition(state, Cancel); append ticket.cancelled + state_changed; commit")
    }

    /// Reopen a `Closed` ticket, requiring `authority.project.reopen_milestone` when the
    /// ticket's milestone is closed (rule 11).
    pub fn reopen(&self, ticket: &TicketId, reason: Option<String>, actor: ParticipantId, authority: &Authority) -> tm_types::Result<Vec<Event>> {
        todo!("if ticket's milestone is Closed, require authority.project.reopen_milestone; transition Closed->Blocked; append ticket.reopened + state_changed; commit")
    }

    /// Record a failure against a ticket (e.g. from `lease::expire_due`'s sweep, or an explicit
    /// executor-reported failure), applying `RetryPolicy` to decide `Ready` vs `Escalated`.
    pub fn record_failure(&self, ticket: &TicketId, class: crate::ticket::FailureClass, reason: String, actor: ParticipantId) -> tm_types::Result<Vec<Event>> {
        todo!("append ticket.failed; consult RetryPolicy (attempts vs max_attempts, class in non_retryable) to pick RetryPermitted/RetryExhausted trigger; transition; commit")
    }

    /// Acquire a lease on a `Ready` ticket. Delegates the pure decision to `lease::acquire`.
    pub fn acquire_lease(&self, ticket: &TicketId, holder: ParticipantId, requested: Authority, resources: Vec<ResourceClaim>, ttl_seconds: u32) -> tm_types::Result<(Lease, Vec<Event>)> {
        todo!("build inputs from current view, call lease::acquire, on success append ticket.leased + resource.claimed events, transition Ready->Leased, commit")
    }

    /// Heartbeat an existing lease. Delegates to `lease::heartbeat`.
    pub fn heartbeat(&self, lease: &LeaseId) -> tm_types::Result<Event> {
        todo!("look up Lease, call lease::heartbeat(now), append ticket.heartbeat, commit")
    }

    /// Release a lease voluntarily. Delegates to `lease::release`.
    pub fn release(&self, lease: &LeaseId, actor: ParticipantId) -> tm_types::Result<Vec<Event>> {
        todo!("look up Lease, call lease::release, append ticket.lease_released + authority.reverted, transition Leased|Running->Ready, commit")
    }

    /// Sweep for expired leases and apply every reversion. Delegates to `lease::expire_due`.
    pub fn expire_leases(&self) -> tm_types::Result<Vec<Event>> {
        todo!("call lease::expire_due(live_leases, self.clock.now()); for each action append ticket.lease_expired + authority.reverted + record_failure(LeaseTimeout); commit")
    }

    /// Record a new decision.
    pub fn record_decision(&self, subject: String, decision: String, reason: String, evidence: Vec<ArtifactId>, affected_tickets: Vec<TicketId>, affected_docs: Vec<String>, author: ParticipantId) -> tm_types::Result<(DecisionId, Vec<Event>)> {
        todo!("allocate DecisionId, append decision.created, commit")
    }

    /// Supersede an existing decision with a new one. The superseded record's own fields are
    /// never mutated in the log; only `superseded_by` changes in the materialized view.
    pub fn supersede(&self, superseded: &DecisionId, subject: String, decision: String, reason: String, evidence: Vec<ArtifactId>, affected_tickets: Vec<TicketId>, affected_docs: Vec<String>, author: ParticipantId) -> tm_types::Result<(DecisionId, Vec<Event>)> {
        todo!("record_decision for the new one, then append decision.superseded pointing superseded -> new id, commit")
    }

    /// Create a new milestone.
    pub fn create_milestone(&self, title: String, tickets: Vec<TicketId>) -> tm_types::Result<(MilestoneId, Vec<Event>)> {
        todo!("allocate MilestoneId, append milestone.created, commit")
    }

    /// Close a milestone. Delegates to `milestone::close`.
    pub fn close_milestone(&self, milestone: &MilestoneId, by: ParticipantId) -> tm_types::Result<Vec<Event>> {
        todo!("look up Milestone + ticket states, call milestone::close, append milestone.closed, commit")
    }

    /// Reopen a milestone. Delegates to `milestone::reopen`, cascading `ticket.reopened` to
    /// affected descendants via `graph.rs`.
    pub fn reopen_milestone(&self, milestone: &MilestoneId, authority: &Authority) -> tm_types::Result<Vec<Event>> {
        todo!("call milestone::reopen, walk graph::descendants for each member ticket, append milestone.reopened + ticket.reopened per affected descendant, commit")
    }

    /// Store a new artifact's bytes, spilling to disk above the inline threshold.
    pub fn store_artifact(&self, kind: ArtifactKind, media_type: String, bytes: Vec<u8>, ticket: Option<TicketId>, meta: serde_json::Value) -> tm_types::Result<(ArtifactId, Vec<Event>)> {
        todo!("allocate ArtifactId, classify_storage, write to disk if OnDisk, append artifact.created, commit")
    }

    /// Attach an evidence record to a ticket, pointing at an already-stored artifact.
    pub fn attach_evidence(&self, ticket: &TicketId, kind: EvidenceKind, artifact: ArtifactId, produced_by: ParticipantId, summary: String) -> tm_types::Result<Vec<Event>> {
        todo!("build Evidence, persist via evidence table write inside the same tx as any caller-supplied event (usually called from within submit/audit); commit if standalone")
    }

    /// Record provider usage, debiting every ancestor budget scope atomically.
    pub fn record_usage(&self, ticket: Option<TicketId>, session: Option<tm_types::SessionId>, amount: Spend) -> tm_types::Result<Vec<Event>> {
        todo!("resolve the ancestor BudgetScope chain from view(), call budget::record_usage, append usage.recorded (+ ticket.budget_exhausted on ExhaustedScope), commit")
    }
}

/// The result an `Audit` ticket reports against the submission it reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    /// The submission is accepted; the audited ticket may close.
    Passed,
    /// A minor issue; the audited ticket returns to `Rework`.
    RejectedMinor,
    /// A structural issue; the audited ticket returns to `Replan`.
    RejectedStructural,
}
