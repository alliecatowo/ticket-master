//! `Store`: the facade every other crate calls into.
//!
//! Owns the open `.tm/project.db` (via [`tm_events::EventLog`]), the injected [`Clock`]/
//! [`IdSource`], and every command that validates a proposed change with [`crate::machine`] and
//! [`crate::invariants`], appends the resulting events and materializes them via
//! [`crate::materialize::apply`] in one [`tm_events::log::Tx`], and returns the events it
//! emitted. This is the only public entry point the rest of the workspace is meant to use —
//! `tm-scheduler`, `tm-context`, etc. read through [`Store::view`]/[`Store::scheduler_view`] and
//! write through the command methods here, never by touching SQLite or `materialize::apply`
//! directly.
//!
//! # Event payload catalogue limitations
//!
//! `tm-events`' payload catalogue is closed and deliberately lean (no raw bytes, no free-form
//! blobs beyond a handful of `String`/`Value` fields). A few commands below therefore adopt a
//! documented convention rather than inventing a new event kind:
//!
//! * [`Store::create_ticket`] emits `ticket.created` (carrying `title`/`parent`, all
//!   [`tm_events::payload::TicketCreatedPayload`] allows) followed by one `ticket.updated`
//!   carrying every other field as a JSON object, since the constructor needs more than the
//!   creation payload can hold.
//! * [`Store::record_decision`]/[`Store::supersede`] fold `subject`/`decision`/`reason`/
//!   `evidence`/`affected_tickets`/`affected_paths` into `decision.created`'s `summary` field as
//!   a JSON blob (`summary`'s declared type is a free `String`, so this is within contract).
//! * [`Store::attach_evidence`] re-emits `artifact.created` with `ticket` set, per that event's
//!   role as the evidence catalogue's only linkage kind.
//! * [`Store::store_artifact`]/[`Store::attach_evidence`] write the `artifacts`/`evidence`
//!   tables' richer columns directly inside the same [`tm_events::log::Tx`] used for the event
//!   append, rather than through [`crate::materialize::apply`] — the one documented exception to
//!   "materialize::apply is the only writer", justified because the payload catalogue has no
//!   room to carry raw bytes/kind/meta through the log (mirroring how large artifact bytes
//!   already live outside the log, spilled to disk).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use tm_events::payload::{
    ArtifactCreatedPayload, AuthorityRevertedPayload, CommandCompletedPayload,
    CommandStartedPayload, DecisionCreatedPayload, DecisionSupersededPayload,
    DocInvalidatedPayload, DocReconciledPayload, DocRegisteredPayload, EffectCompletedPayload,
    EffectFailedPayload, EffectJournaledPayload, GoalClaimedCompletePayload, GoalReorientedPayload,
    GoalSetPayload, GoalStepAddedPayload, GoalStepCompletedPayload, HarnessPromotedPayload,
    MilestoneClosedPayload, MilestoneCreatedPayload, MilestoneReopenedPayload, MirrorLinkedPayload,
    MirrorPulledPayload, MirrorPushedPayload, SessionEndedPayload, SessionJoinedPayload,
    SessionStartedPayload, TicketAuditRejectedPayload, TicketAuditedPayload,
    TicketBudgetExhaustedPayload, TicketBudgetHandoffPayload, TicketCancelledPayload,
    TicketChildAddedPayload, TicketClosedPayload, TicketCreatedPayload,
    TicketDependencyAddedPayload, TicketEscalatedPayload, TicketFailedPayload,
    TicketHeartbeatPayload, TicketLeaseExpiredPayload, TicketLeaseReleasedPayload,
    TicketLeasedPayload, TicketReopenedPayload, TicketRetryScheduledPayload,
    TicketStateChangedPayload, TicketSubmittedPayload, TicketUpdatedPayload,
    TicketVerificationFailedPayload, TicketVerifiedPayload, UsageRecordedPayload,
};
use tm_events::{Event, EventDraft, EventLog, Payload, Tx};
use tm_types::{
    ArtifactId, Authority, Budget, Clock, CounterIds, DecisionId, Id, IdKind, IdSource, LeaseId,
    MilestoneId, ParticipantId, Predicate, SessionId, Spend, SystemClock, TicketId, Timestamp,
    TmError,
};

use crate::artifact::{Artifact, ArtifactKind, ArtifactStorage, Evidence, EvidenceKind};
use crate::budget::{BudgetLedger, BudgetScope, ScopedBudget};
use crate::decision::Decision;
use crate::effect::{Effect, EffectGuard, EffectKey, EffectStatus};
use crate::goal::{GoalState, GoalStep};
use crate::graph::{DependencyEdge, DependencyGraph};
use crate::lease::{Lease, LeaseStore, LeaseView};
use crate::machine;
use crate::milestone::{Milestone, MilestoneStore};
use crate::ticket::{
    ContextRef, DependencyKind, ExecutorRequirements, FailureClass, ResourceClaim, RetryPolicy,
    Ticket, TicketKind, TicketState, Trigger, VerificationPolicy,
};
use crate::view::{ParticipantSummary, ProjectView, SchedulerView};

/// The project-state facade. One `Store` per open project.
pub struct Store {
    log: EventLog,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
    /// The project's root directory, needed for artifact on-disk placement
    /// ([`crate::artifact::plan_storage`]).
    root: PathBuf,
}

/// One open [`tm_events::log::Tx`], scoped to a single [`Store::transaction`] call.
///
/// Exposes exactly the "append one already-typed draft, materializing it via
/// [`crate::materialize::apply`] in place" operation every typed command method on [`Store`] (and
/// [`Store::append`]) is built from, plus [`StoreTx::raw`] for a caller that needs the one
/// documented exception to "materialize::apply is the only writer" (see this module's top-level
/// note on [`Store::store_artifact`]/[`Store::attach_evidence`]) while still sharing one
/// transaction with other heterogeneous writes — the case [`Store::transaction`] exists for.
///
/// Do not call back out to any `Store` method from inside the [`Store::transaction`] closure that
/// hands you one of these — see that method's `# Warning`.
pub struct StoreTx<'a> {
    log: &'a EventLog,
    tx: Tx<'a>,
}

impl<'a> StoreTx<'a> {
    /// Append `draft`, materializing it via [`crate::materialize::apply`] before returning the
    /// resulting [`Event`] (durable once the enclosing [`Store::transaction`] call commits, not
    /// before).
    pub fn append(&self, draft: EventDraft) -> tm_types::Result<Event> {
        let event = self.log.append_in(&self.tx, draft)?;
        crate::materialize::apply(&self.tx, &event)?;
        Ok(event)
    }

    /// Append every draft in `drafts`, in order, via [`StoreTx::append`].
    pub fn append_all(&self, drafts: Vec<EventDraft>) -> tm_types::Result<Vec<Event>> {
        drafts.into_iter().map(|draft| self.append(draft)).collect()
    }

    /// The raw SQLite connection backing this transaction.
    pub fn raw(&self) -> &Connection {
        self.tx.raw()
    }
}

/// Which direction a mirror sync ran, for [`Store::update_mirror_link`] — `mirror.pushed` and
/// `mirror.pulled` carry the same two fields (`remote`, `reference`), differing only in kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorSyncDirection {
    /// Ticketmaster's state was pushed to the external tracker (`mirror.pushed`).
    Push,
    /// The external tracker's changes were pulled in (`mirror.pulled`).
    Pull,
}

/// One row of the `docs` table, joined with its `doc_provenance` ticket links -- the durable
/// record [`Store::register_doc`]/[`Store::invalidate_doc`]/[`Store::reconcile_doc`] produce.
///
/// This is deliberately *not* `tm_docs::registry::DocRecord`: that type additionally carries
/// `mode`/`state`/`derived_from`, none of which any catalogued event names (see `doc.registered`'s
/// materializer arm), so this crate has nothing to populate them from. A caller that needs a full
/// `DocRecord` (e.g. `tm-cli`'s `docs` verbs) joins this row against the on-disk
/// `docs/.tmdocs.toml`/front-matter declaration itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRow {
    /// The doc's id (== its registered path; see `doc.registered`'s materializer arm).
    pub id: String,
    /// `docs.title` (defaults to `id` -- see the module note on `doc.registered`).
    pub title: String,
    /// `docs.author` -- the actor that most recently registered/invalidated/reconciled this doc.
    pub author: ParticipantId,
    /// `docs.ts` -- when this doc was last touched.
    pub ts: Timestamp,
    /// Tickets recorded in `doc_provenance` as an originating source for this doc.
    pub provenance_tickets: Vec<TicketId>,
}

/// One row of the `mirror_links` table: the thin, single-adapter-per-ticket summary
/// [`Store::link_mirror`]/[`Store::update_mirror_link`] persist.
///
/// Unlike `tm_mirror::sync::MirrorLink`, this carries no `content_hash`/`degradations`/
/// `last_pulled_at` -- `crate::schema`'s `mirror_links` table has no columns for them -- so a
/// caller cannot recover push-idempotency-across-restarts or an incremental pull `since` cursor
/// from this row alone. `mirror_links.ticket` is also the table's primary key, so a ticket
/// mirrored to more than one adapter only has the most recently synced adapter's link visible
/// here; this is a pre-existing `crate::schema` limitation, not something a caller can work
/// around from this read API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorLinkRow {
    /// The linked ticket.
    pub ticket: TicketId,
    /// The external system's id for the mirrored issue; empty until the first push or pull.
    pub remote_id: String,
    /// The adapter instance name (matches `mirror.toml`'s table key / `Tracker::name()`).
    pub remote_system: String,
    /// When this link was last pushed or pulled.
    pub last_synced: Timestamp,
}

/// One row of the `harness_epochs` table: a promoted harness config snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessEpochRow {
    /// Monotonically increasing epoch number; never `0` (see [`Store::harness_epochs`]).
    pub epoch: u64,
    /// The opaque harness-config payload [`Store::promote_epoch`]'s caller supplied as
    /// `candidate`.
    pub harness_config: String,
    /// When this epoch was promoted.
    pub ts: Timestamp,
}

/// One row of the `workflows` table: a versioned `tm-workflow` `WorkflowDef` TOML source,
/// keyed by its blake3 content hash. See [`Store::register_workflow_def`]/[`Store::workflow_defs`]
/// and `crate::schema`'s module doc for why this table is a lookup cache rather than
/// replay-restored state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowDefRow {
    /// The blake3 hex digest of `source`, this row's primary key.
    pub content_hash: String,
    /// The workflow's declared name (`WorkflowDef::name`).
    pub name: String,
    /// This name's version number, one past the highest version already registered for `name`
    /// when this content hash was first seen; stable for the life of the row.
    pub version: u32,
    /// The exact TOML source this hash was computed over.
    pub source: String,
    /// When this content hash was first registered.
    pub registered_at: Timestamp,
}

fn storage_err(e: rusqlite::Error) -> TmError {
    TmError::storage(e.to_string())
}

fn to_json_text<T: serde::Serialize>(value: &T) -> tm_types::Result<String> {
    serde_json::to_string(value).map_err(TmError::from)
}

fn from_json_text<T: serde::de::DeserializeOwned>(text: &str) -> tm_types::Result<T> {
    serde_json::from_str(text).map_err(|e| TmError::storage(format!("corrupt json {text:?}: {e}")))
}

fn parse_ts(text: &str) -> tm_types::Result<Timestamp> {
    Timestamp::parse_rfc3339(text).map_err(|e| TmError::storage(e.to_string()))
}

fn state_str(state: TicketState) -> String {
    match serde_json::to_value(state) {
        Ok(serde_json::Value::String(s)) => s,
        _ => format!("{state:?}"),
    }
}

fn state_changed_draft(
    ticket: &TicketId,
    from: TicketState,
    to: TicketState,
    actor: ParticipantId,
) -> EventDraft {
    EventDraft::new(
        actor,
        Id::from(ticket.clone()),
        Payload::from(TicketStateChangedPayload {
            ticket: ticket.clone(),
            from: state_str(from),
            to: state_str(to),
        }),
    )
}

/// Shared core of a budget handoff (`SPEC.md` §31.3, `docs/audit-2026-09-18-fable.md` B-10):
/// `None` if `ticket_id` is not currently found in `view`, or is not `Leased`/`Running` (nothing
/// structurally eligible to hand off — see [`crate::machine::transition`]'s
/// `Trigger::BudgetHandoff` arms); otherwise the full draft set — `ticket.budget_handoff`, a
/// lease-release plus authority-revert per live lease held on the ticket, and the
/// `ticket.state_changed` draft for the `Trigger::BudgetHandoff` transition itself. Used
/// identically whether the handoff was discovered reactively ([`Store::record_usage`], after a
/// spend crossed the ceiling) or proactively ([`Store::budget_handoff`], before an effect the
/// caller estimated it could not afford ever started) — SPEC.md §31.3 describes one mechanism,
/// not two, regardless of which side noticed first.
fn budget_handoff_drafts(
    view: &ProjectView,
    ticket_id: &TicketId,
    actor: &ParticipantId,
    dimension: &str,
) -> Option<Vec<EventDraft>> {
    let t = view.tickets.get(ticket_id)?;
    let to = machine::transition(t.state, Trigger::BudgetHandoff).ok()?;

    let mut drafts = vec![EventDraft::new(
        actor.clone(),
        Id::from(ticket_id.clone()),
        Payload::from(TicketBudgetHandoffPayload {
            ticket: ticket_id.clone(),
            dimension: dimension.to_string(),
        }),
    )];

    // The handed-off worker is finished with this attempt's lease, so it ends here and the
    // authority it held reverts — the same reasoning `record_failure` documents for a genuine
    // failure, except nothing here is a failure (SPEC.md §31.3: "not evidence of anything going
    // wrong"). Leaving the lease live would block the next worker on the double-lease invariant
    // (SPEC.md §4.5, §8).
    for lease in view.leases.values().filter(|l| l.ticket == *ticket_id) {
        drafts.push(EventDraft::new(
            actor.clone(),
            Id::from(ticket_id.clone()),
            Payload::from(TicketLeaseReleasedPayload {
                ticket: ticket_id.clone(),
                lease: lease.id.clone(),
            }),
        ));
        drafts.push(EventDraft::new(
            actor.clone(),
            Id::from(ticket_id.clone()),
            Payload::from(AuthorityRevertedPayload {
                subject: lease.holder.clone(),
                ticket: Some(ticket_id.clone()),
                to_seq: 0,
            }),
        ));
    }

    drafts.push(state_changed_draft(ticket_id, t.state, to, actor.clone()));
    Some(drafts)
}

/// Fold the fields `decision.created`'s payload can't carry directly into `summary`, per the
/// module-level note on the closed payload catalogue.
fn decision_summary_json(
    subject: &str,
    decision: &str,
    reason: &str,
    evidence: &[ArtifactId],
    affected_tickets: &[TicketId],
    affected_paths: &[String],
) -> String {
    serde_json::json!({
        "subject": subject,
        "decision": decision,
        "reason": reason,
        "evidence": evidence,
        "affected_tickets": affected_tickets,
        "affected_paths": affected_paths,
    })
    .to_string()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> tm_types::Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(TmError::storage(
            "odd-length hex in artifact storage descriptor",
        ));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| TmError::storage(e.to_string())))
        .collect()
}

fn encode_artifact_storage(storage: &ArtifactStorage) -> String {
    match storage {
        ArtifactStorage::Inline(bytes) => format!("inline:{}", hex_encode(bytes)),
        ArtifactStorage::OnDisk(path) => format!("disk:{}", path.display()),
    }
}

fn decode_artifact_storage(text: &str) -> tm_types::Result<ArtifactStorage> {
    if let Some(hex) = text.strip_prefix("inline:") {
        Ok(ArtifactStorage::Inline(hex_decode(hex)?))
    } else if let Some(path) = text.strip_prefix("disk:") {
        Ok(ArtifactStorage::OnDisk(PathBuf::from(path)))
    } else {
        Err(TmError::storage(format!(
            "corrupt artifact storage descriptor: {text:?}"
        )))
    }
}

impl Store {
    /// Open (creating if absent) the project at `project_root`, using the real wall clock and a
    /// fresh [`tm_types::CounterIds`] restored from persisted counters. Prefer
    /// [`Store::open_with`] in tests for deterministic clock/id injection.
    pub fn open(project_root: &Path) -> tm_types::Result<Self> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let db_path = project_root.join(".tm").join("project.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let counters = {
            let mut conn = Connection::open(&db_path).map_err(storage_err)?;
            crate::schema::migrate(&mut conn, clock.as_ref())?;
            Self::read_counters(&conn)?
        };
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::with_counters(counters, 0));
        Store::open_with(project_root, clock, ids)
    }

    /// Open with an injected clock and id source, the constructor tests and deterministic
    /// callers use.
    pub fn open_with(
        project_root: &Path,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
    ) -> tm_types::Result<Self> {
        let db_path = project_root.join(".tm").join("project.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = EventLog::open_with_clock(&db_path, clock.clone())?;
        {
            let mut conn = Connection::open(&db_path).map_err(storage_err)?;
            crate::schema::migrate(&mut conn, clock.as_ref())?;
        }
        Ok(Store {
            log,
            clock,
            ids,
            root: project_root.to_path_buf(),
        })
    }

    fn read_counters(conn: &Connection) -> tm_types::Result<BTreeMap<String, u64>> {
        let mut stmt = match conn.prepare("SELECT counter_name, value FROM counters") {
            Ok(stmt) => stmt,
            Err(_) => return Ok(BTreeMap::new()),
        };
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(storage_err)?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (name, value) = row.map_err(storage_err)?;
            out.insert(name, value as u64);
        }
        Ok(out)
    }

    /// Run a command that only appends events and materializes them; `build` sees a snapshot
    /// [`ProjectView`] taken inside the transaction and returns the drafts to append. Commits
    /// only if [`crate::invariants::check_invariants`] finds no violation in the post-write
    /// state, otherwise rolls back and surfaces a `TmError::invariant`.
    fn run_command<F>(&self, build: F) -> tm_types::Result<Vec<Event>>
    where
        F: FnOnce(&ProjectView) -> tm_types::Result<Vec<EventDraft>>,
    {
        self.run_command_with_extra(build, |_tx, _events| Ok(()))
    }

    /// Like [`Store::run_command`], plus an `extra` hook run after every draft has been appended
    /// and materialized (and before the post-write invariant check), for the rare command that
    /// must write outside [`crate::materialize::apply`] (see the module-level note).
    fn run_command_with_extra<F, G>(&self, build: F, extra: G) -> tm_types::Result<Vec<Event>>
    where
        F: FnOnce(&ProjectView) -> tm_types::Result<Vec<EventDraft>>,
        G: FnOnce(&Tx<'_>, &[Event]) -> tm_types::Result<()>,
    {
        let tx = self.log.begin()?;
        let view = Self::read_view(tx.raw())?;
        let drafts = build(&view)?;
        let mut events = Vec::with_capacity(drafts.len());
        for draft in drafts {
            let event = self.log.append_in(&tx, draft)?;
            crate::materialize::apply(&tx, &event)?;
            events.push(event);
        }
        extra(&tx, &events)?;
        let post_view = Self::read_view(tx.raw())?;
        let violations = crate::invariants::check_invariants(&post_view);
        if !violations.is_empty() {
            let detail = violations
                .iter()
                .map(|v| format!("{}: {}", v.invariant, v.detail))
                .collect::<Vec<_>>()
                .join("; ");
            tx.rollback()?;
            return Err(TmError::invariant(format!(
                "{} invariant violation(s): {detail}",
                violations.len()
            )));
        }
        tx.commit()?;
        Ok(events)
    }

    /// Create a new ticket in [`crate::ticket::TicketState::Draft`].
    ///
    /// # Errors
    /// `TmError::invariant` if `parent` is set but the parent ticket doesn't exist, or if the
    /// requested `authority` is not contained by the parent's authority (child authority ⊆
    /// parent authority, `SPEC.md` §4.3 invariant).
    #[allow(clippy::too_many_arguments)]
    pub fn create_ticket(
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
    ) -> tm_types::Result<Vec<Event>> {
        self.run_command(move |view| {
            if let Some(parent_id) = &parent {
                let parent_ticket = view
                    .tickets
                    .get(parent_id)
                    .ok_or_else(|| TmError::not_found("ticket", parent_id))?;
                if !parent_ticket.authority.contains(&authority) {
                    return Err(TmError::invariant(format!(
                        "requested authority for a child of {parent_id} is not contained by the parent's authority"
                    )));
                }
            }
            let id = TicketId::new(self.ids.next(IdKind::Ticket).as_str())?;
            let now = self.clock.now();
            let mut drafts = vec![EventDraft::new(
                actor.clone(),
                Id::from(id.clone()),
                Payload::from(TicketCreatedPayload {
                    ticket: id.clone(),
                    title: objective.clone(),
                    parent: parent.clone(),
                }),
            )];
            if let Some(parent_id) = &parent {
                drafts.push(EventDraft::new(
                    actor.clone(),
                    Id::from(parent_id.clone()),
                    Payload::from(TicketChildAddedPayload {
                        parent: parent_id.clone(),
                        child: id.clone(),
                    }),
                ));
            }
            let fields = serde_json::json!({
                "kind": kind,
                "milestone": milestone,
                "authority": authority,
                "resources": resources,
                "executor": executor,
                "context_refs": context_refs,
                "success": success,
                "verification": verification,
                "budget": budget,
                "retry": retry,
                "priority": priority,
                "created": now,
                "updated": now,
            });
            drafts.push(EventDraft::new(
                actor.clone(),
                Id::from(id.clone()),
                Payload::from(TicketUpdatedPayload { ticket: id.clone(), fields }),
            ));
            Ok(drafts)
        })
    }

    /// Update mutable ticket fields (objective, success predicates, priority, ...); structural
    /// fields (`state`, `parent`, `children`, `dependencies`) go through their own methods.
    ///
    /// # Errors
    /// `TmError::not_found` if `ticket` doesn't exist.
    pub fn update_ticket(
        &self,
        ticket: &TicketId,
        fields: serde_json::Value,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket = ticket.clone();
        self.run_command(move |view| {
            if !view.tickets.contains_key(&ticket) {
                return Err(TmError::not_found("ticket", &ticket));
            }
            Ok(vec![EventDraft::new(
                actor,
                Id::from(ticket.clone()),
                Payload::from(TicketUpdatedPayload { ticket, fields }),
            )])
        })
    }

    /// Add a dependency edge `ticket -> depends_on` of `kind`.
    ///
    /// # Errors
    /// `TmError::invariant` if adding this edge would create an illegal cycle per
    /// [`crate::graph::DependencyGraph::find_illegal_cycles`] (checked against the post-add
    /// graph before committing).
    pub fn add_dependency(
        &self,
        ticket: &TicketId,
        depends_on: &TicketId,
        kind: DependencyKind,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket = ticket.clone();
        let depends_on = depends_on.clone();
        self.run_command(move |view| {
            if !view.tickets.contains_key(&ticket) {
                return Err(TmError::not_found("ticket", &ticket));
            }
            if !view.tickets.contains_key(&depends_on) {
                return Err(TmError::not_found("ticket", &depends_on));
            }
            let nodes: Vec<TicketId> = view.tickets.keys().cloned().collect();
            let mut edges: Vec<DependencyEdge> = view.graph.edges().to_vec();
            edges.push(DependencyEdge {
                from: ticket.clone(),
                to: depends_on.clone(),
                kind,
            });
            let children: Vec<(TicketId, TicketId)> = view
                .tickets
                .values()
                .flat_map(|t| t.children.iter().map(move |c| (t.id.clone(), c.clone())))
                .collect();
            let graph = DependencyGraph::build(nodes, edges, children);
            let has_budget = |tid: &TicketId| {
                view.tickets
                    .get(tid)
                    .map(|t| t.cycle.is_some())
                    .unwrap_or(false)
            };
            let violations = graph.find_illegal_cycles(has_budget);
            if !violations.is_empty() {
                return Err(TmError::invariant(format!(
                    "adding {ticket} -> {depends_on} would create an illegal cycle"
                )));
            }
            Ok(vec![EventDraft::new(
                actor,
                Id::from(ticket.clone()),
                Payload::from(TicketDependencyAddedPayload { ticket, depends_on }),
            )])
        })
    }

    /// Drive `ticket` through [`Trigger::Activate`]: `Draft -> Blocked`, continuing to `Ready`
    /// if it has no unsatisfied hard dependencies (`SPEC.md` §4.3 rule 1).
    pub fn activate(
        &self,
        ticket: &TicketId,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket = ticket.clone();
        self.run_command(move |view| {
            let t = view
                .tickets
                .get(&ticket)
                .ok_or_else(|| TmError::not_found("ticket", &ticket))?;
            let blocked = machine::transition(t.state, Trigger::Activate)
                .map_err(|e| TmError::InvalidTransition(e.to_string()))?;
            let mut drafts = vec![state_changed_draft(
                &ticket,
                t.state,
                blocked,
                actor.clone(),
            )];
            let closed: std::collections::BTreeSet<TicketId> = view
                .tickets
                .values()
                .filter(|t| t.state == TicketState::Closed)
                .map(|t| t.id.clone())
                .collect();
            if view.graph.dependencies_satisfied(&ticket, &closed) {
                let ready = machine::transition(blocked, Trigger::DependenciesSatisfied).expect(
                    "Blocked always accepts DependenciesSatisfied per the fixed transition table",
                );
                drafts.push(state_changed_draft(&ticket, blocked, ready, actor.clone()));
            }
            Ok(drafts)
        })
    }

    /// Apply an arbitrary [`Trigger`] to `ticket`, the general-purpose entry point for triggers
    /// not covered by a more specific method (`DependenciesSatisfied`/`Unsatisfied`,
    /// `LeaseExpired`/`Released` are normally driven by [`Store::expire_leases`]/
    /// [`Store::release`] instead of called directly).
    ///
    /// # Errors
    /// `TmError::InvalidTransition` if [`crate::machine::transition`] rejects `(current, trigger)`.
    pub fn transition(
        &self,
        ticket: &TicketId,
        trigger: Trigger,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket = ticket.clone();
        self.run_command(move |view| {
            let t = view
                .tickets
                .get(&ticket)
                .ok_or_else(|| TmError::not_found("ticket", &ticket))?;
            let to = machine::transition(t.state, trigger)
                .map_err(|e| TmError::InvalidTransition(e.to_string()))?;
            Ok(vec![state_changed_draft(&ticket, t.state, to, actor)])
        })
    }

    /// Submit work on `ticket`, carrying `evidence` (`SPEC.md` §4.3 rule 5: a submission must
    /// carry evidence; a worker cannot mark itself verified).
    ///
    /// # Errors
    /// `TmError::invariant` if `evidence` is empty.
    pub fn submit(
        &self,
        ticket: &TicketId,
        summary: String,
        evidence: Vec<ArtifactId>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        if evidence.is_empty() {
            return Err(TmError::invariant(
                "submission requires at least one evidence artifact",
            ));
        }
        let ticket = ticket.clone();
        self.run_command(move |view| {
            let t = view
                .tickets
                .get(&ticket)
                .ok_or_else(|| TmError::not_found("ticket", &ticket))?;
            let to = machine::transition(t.state, Trigger::Submit)
                .map_err(|e| TmError::InvalidTransition(e.to_string()))?;
            let mut drafts = vec![state_changed_draft(&ticket, t.state, to, actor.clone())];
            drafts.push(EventDraft::new(
                actor.clone(),
                Id::from(ticket.clone()),
                Payload::from(TicketSubmittedPayload {
                    ticket: ticket.clone(),
                    summary: summary.clone(),
                }),
            ));
            for artifact in &evidence {
                if !view.artifacts.contains_key(artifact) {
                    return Err(TmError::not_found("artifact", artifact));
                }
                drafts.push(evidence_draft(
                    &ticket,
                    EvidenceKind::Review,
                    artifact,
                    "submission evidence".to_string(),
                    actor.clone(),
                ));
            }
            Ok(drafts)
        })
    }

    /// Record a verification outcome for `ticket`, produced by `verifier` (a `Verification`-kind
    /// ticket, per `SPEC.md` §4.2's `TicketKind`).
    pub fn verify(
        &self,
        ticket: &TicketId,
        verifier: &TicketId,
        passed: bool,
        reason: Option<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket = ticket.clone();
        let verifier = verifier.clone();
        self.run_command(move |view| {
            let t = view
                .tickets
                .get(&ticket)
                .ok_or_else(|| TmError::not_found("ticket", &ticket))?;
            let trigger = if passed {
                Trigger::VerificationPassed
            } else {
                Trigger::VerificationFailed
            };
            let to = machine::transition(t.state, trigger)
                .map_err(|e| TmError::InvalidTransition(e.to_string()))?;
            let mut drafts = vec![state_changed_draft(&ticket, t.state, to, actor.clone())];
            if passed {
                drafts.push(EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketVerifiedPayload {
                        ticket: ticket.clone(),
                        verifier: verifier.clone(),
                    }),
                ));
            } else {
                drafts.push(EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketVerificationFailedPayload {
                        ticket: ticket.clone(),
                        verifier: verifier.clone(),
                        reason: reason.clone().unwrap_or_default(),
                    }),
                ));
            }
            Ok(drafts)
        })
    }

    /// Record an audit outcome for `ticket`, produced by `auditor` (an `Audit`-kind ticket).
    ///
    /// # Errors
    /// `TmError::invariant` if `auditor`'s lease holder equals the executor that produced the
    /// submission being audited (`SPEC.md` §4.3: a worker never certifies itself).
    pub fn audit(
        &self,
        ticket: &TicketId,
        auditor: &TicketId,
        outcome: AuditOutcome,
        reason: Option<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket = ticket.clone();
        let auditor = auditor.clone();
        self.run_command(move |view| {
            let t = view
                .tickets
                .get(&ticket)
                .ok_or_else(|| TmError::not_found("ticket", &ticket))?;
            let executor = view
                .leases
                .values()
                .filter(|l| l.ticket == ticket)
                .max_by_key(|l| l.epoch)
                .map(|l| l.holder.clone());
            if executor.as_ref() == Some(&actor) {
                return Err(TmError::invariant(
                    "the auditor must differ from the executor that produced the submission",
                ));
            }
            let trigger = match outcome {
                AuditOutcome::Passed => Trigger::AuditPassed,
                AuditOutcome::RejectedMinor => Trigger::AuditRejectedMinor,
                AuditOutcome::RejectedStructural => Trigger::AuditRejectedStructural,
            };
            let to = machine::transition(t.state, trigger)
                .map_err(|e| TmError::InvalidTransition(e.to_string()))?;
            let mut drafts = vec![state_changed_draft(&ticket, t.state, to, actor.clone())];
            drafts.push(if matches!(outcome, AuditOutcome::Passed) {
                EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketAuditedPayload {
                        ticket: ticket.clone(),
                        auditor: auditor.clone(),
                    }),
                )
            } else {
                EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketAuditRejectedPayload {
                        ticket: ticket.clone(),
                        auditor: auditor.clone(),
                        reason: reason.clone().unwrap_or_default(),
                    }),
                )
            });
            Ok(drafts)
        })
    }

    /// Close `ticket` directly (bypassing the audit trail), gated on
    /// `authority.tickets.close`. Used for administrative closes (e.g. `Investigation`/`Harness`
    /// tickets whose `VerificationPolicy::None` needs no audit path).
    ///
    /// Authority is resolved from the ticket's own `authority` field (there is not yet a
    /// separate per-participant authority store); this is a documented simplification.
    pub fn close(
        &self,
        ticket: &TicketId,
        reason: Option<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket = ticket.clone();
        self.run_command(move |view| {
            let t = view
                .tickets
                .get(&ticket)
                .ok_or_else(|| TmError::not_found("ticket", &ticket))?;
            if !t.authority.tickets.close {
                return Err(TmError::AuthorityDenied(format!(
                    "{ticket} lacks tickets.close authority"
                )));
            }
            let to = machine::transition(t.state, Trigger::AuditPassed)
                .map_err(|e| TmError::InvalidTransition(e.to_string()))?;
            Ok(vec![
                state_changed_draft(&ticket, t.state, to, actor.clone()),
                EventDraft::new(
                    actor,
                    Id::from(ticket.clone()),
                    Payload::from(TicketClosedPayload {
                        ticket: ticket.clone(),
                        reason: reason.clone(),
                    }),
                ),
            ])
        })
    }

    /// Cancel `ticket` from any non-terminal state, gated on `authority.tickets.cancel`
    /// (`SPEC.md` §4.3 rule 12).
    pub fn cancel(
        &self,
        ticket: &TicketId,
        reason: Option<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket = ticket.clone();
        self.run_command(move |view| {
            let t = view
                .tickets
                .get(&ticket)
                .ok_or_else(|| TmError::not_found("ticket", &ticket))?;
            if !t.authority.tickets.cancel {
                return Err(TmError::AuthorityDenied(format!(
                    "{ticket} lacks tickets.cancel authority"
                )));
            }
            let to = machine::transition(t.state, Trigger::Cancel)
                .map_err(|e| TmError::InvalidTransition(e.to_string()))?;
            Ok(vec![
                state_changed_draft(&ticket, t.state, to, actor.clone()),
                EventDraft::new(
                    actor,
                    Id::from(ticket.clone()),
                    Payload::from(TicketCancelledPayload {
                        ticket: ticket.clone(),
                        reason: reason.clone(),
                    }),
                ),
            ])
        })
    }

    /// Reopen a `Closed` ticket (`SPEC.md` §4.3 rule 11), gated on
    /// `authority.project.reopen_milestone` when the ticket's milestone is closed.
    pub fn reopen(
        &self,
        ticket: &TicketId,
        reason: Option<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket = ticket.clone();
        self.run_command(move |view| {
            let t = view.tickets.get(&ticket).ok_or_else(|| TmError::not_found("ticket", &ticket))?;
            if let Some(mid) = &t.milestone {
                if let Some(m) = view.milestones.get(mid) {
                    if m.state == crate::milestone::MilestoneState::Closed && !t.authority.project.reopen_milestone {
                        return Err(TmError::AuthorityDenied(format!(
                            "{ticket}'s milestone {mid} is closed; project.reopen_milestone authority required"
                        )));
                    }
                }
            }
            let to = machine::transition(t.state, Trigger::Reopen).map_err(|e| TmError::InvalidTransition(e.to_string()))?;
            Ok(vec![
                state_changed_draft(&ticket, t.state, to, actor.clone()),
                EventDraft::new(
                    actor,
                    Id::from(ticket.clone()),
                    Payload::from(TicketReopenedPayload { ticket: ticket.clone(), reason: reason.clone() }),
                ),
            ])
        })
    }

    /// Record a failure on `ticket` and let `Recovery`'s `RetryPolicy` decide the next state
    /// (`SPEC.md` §4.3 rule 8 / §4.7).
    pub fn record_failure(
        &self,
        ticket: &TicketId,
        class: FailureClass,
        detail: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket = ticket.clone();
        self.run_command(move |view| {
            let t = view
                .tickets
                .get(&ticket)
                .ok_or_else(|| TmError::not_found("ticket", &ticket))?;
            // `attempts` was incremented when the lease was acquired, so it already names the
            // attempt that just failed. A ticket that failed without ever being leased still
            // counts as one attempt.
            let attempt = t.attempts.max(1);
            let can_retry =
                class.is_retryable() && attempt < t.retry.max_attempts && !t.budget.is_exhausted();
            let trigger = if can_retry {
                Trigger::RetryScheduled
            } else {
                Trigger::RetryExhausted
            };
            let mut drafts = vec![EventDraft::new(
                actor.clone(),
                Id::from(ticket.clone()),
                Payload::from(TicketFailedPayload {
                    ticket: ticket.clone(),
                    reason: detail.clone(),
                }),
            )];
            // The failed executor is finished with this attempt, so its lease ends here and the
            // authority it held reverts. Leaving the lease live would block every later attempt
            // on the double-lease invariant (SPEC.md §4.5, §8).
            for lease in view.leases.values().filter(|l| l.ticket == ticket) {
                drafts.push(EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketLeaseReleasedPayload {
                        ticket: ticket.clone(),
                        lease: lease.id.clone(),
                    }),
                ));
                drafts.push(EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(AuthorityRevertedPayload {
                        subject: lease.holder.clone(),
                        ticket: Some(ticket.clone()),
                        to_seq: 0,
                    }),
                ));
            }
            // Failure is two transitions, not one: the ticket enters Recovery, and only from
            // there does the retry policy decide between another attempt and escalation. A
            // ticket already in Recovery (a failed verification, say) skips the first step.
            let mut from = t.state;
            if from != TicketState::Recovery {
                let into_recovery = machine::transition(from, Trigger::Failed)
                    .map_err(|e| TmError::InvalidTransition(e.to_string()))?;
                drafts.push(state_changed_draft(
                    &ticket,
                    from,
                    into_recovery,
                    actor.clone(),
                ));
                from = into_recovery;
            }
            let to = machine::transition(from, trigger)
                .map_err(|e| TmError::InvalidTransition(e.to_string()))?;
            drafts.push(state_changed_draft(&ticket, from, to, actor.clone()));
            if can_retry {
                let not_before = self
                    .clock
                    .now()
                    .plus_seconds(t.retry.delay_for_attempt(attempt) as i64);
                drafts.push(EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketRetryScheduledPayload {
                        ticket: ticket.clone(),
                        attempt,
                        not_before,
                    }),
                ));
            } else {
                drafts.push(EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketEscalatedPayload {
                        ticket: ticket.clone(),
                        reason: detail.clone(),
                    }),
                ));
            }
            Ok(drafts)
        })
    }

    /// Acquire a lease on `ticket` for `holder` (`SPEC.md` §4.5). See
    /// [`crate::lease::LeaseStore::acquire`] for the pure precondition logic this wraps.
    pub fn acquire_lease(
        &self,
        ticket: &TicketId,
        holder: ParticipantId,
        authority: Authority,
        resources: Vec<ResourceClaim>,
        ttl_seconds: u32,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket = ticket.clone();
        self.run_command(move |view| {
            let t = view
                .tickets
                .get(&ticket)
                .ok_or_else(|| TmError::not_found("ticket", &ticket))?;
            struct ViewAdapter<'a>(&'a BTreeMap<LeaseId, Lease>);
            impl<'a> LeaseView for ViewAdapter<'a> {
                fn live_leases(&self) -> Vec<&Lease> {
                    self.0.values().collect()
                }
            }
            let adapter = ViewAdapter(&view.leases);
            let lease_id = LeaseId::new(self.ids.next(IdKind::Lease).as_str())?;
            let now = self.clock.now();
            let epoch = view
                .leases
                .values()
                .filter(|l| l.ticket == ticket)
                .map(|l| l.epoch + 1)
                .max()
                .unwrap_or(0);
            let lease = LeaseStore::acquire(
                t.state,
                &t.authority,
                &adapter,
                lease_id,
                ticket.clone(),
                holder.clone(),
                authority,
                resources,
                now,
                ttl_seconds,
                epoch,
            )
            .map_err(|e| TmError::conflict(e.to_string()))?;
            let to = machine::transition(t.state, Trigger::LeaseAcquired)
                .map_err(|e| TmError::InvalidTransition(e.to_string()))?;
            Ok(vec![
                EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketLeasedPayload {
                        ticket: ticket.clone(),
                        lease: lease.id.clone(),
                        holder: holder.clone(),
                        expires_at: lease.heartbeat.plus_seconds(ttl_seconds as i64),
                    }),
                ),
                // The attempt is spent the moment the work is handed out, not when it is
                // reported failed (SPEC.md 4.3 rule 4). Counting it here is what stops a worker
                // that crashes without ever reporting from looping on the ticket for free: the
                // lease expires, the ticket returns to Ready, and the attempt is already gone.
                EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketUpdatedPayload {
                        ticket: ticket.clone(),
                        fields: serde_json::json!({ "attempts": t.attempts + 1 }),
                    }),
                ),
                state_changed_draft(&ticket, t.state, to, actor),
            ])
        })
    }

    /// Refresh a lease's heartbeat.
    ///
    /// # Errors
    /// `TmError::LeaseExpired` if the lease had already expired.
    pub fn heartbeat(&self, lease: &LeaseId, actor: ParticipantId) -> tm_types::Result<Vec<Event>> {
        let lease = lease.clone();
        self.run_command(move |view| {
            let l = view
                .leases
                .get(&lease)
                .ok_or_else(|| TmError::not_found("lease", &lease))?;
            let now = self.clock.now();
            if l.is_expired(now) {
                return Err(TmError::LeaseExpired(lease.to_string()));
            }
            Ok(vec![EventDraft::new(
                actor,
                Id::from(l.ticket.clone()),
                Payload::from(TicketHeartbeatPayload {
                    ticket: l.ticket.clone(),
                    lease: lease.clone(),
                    expires_at: now.plus_seconds(l.ttl_seconds as i64),
                }),
            )])
        })
    }

    /// Release a lease early (worker finished or gave up before expiry).
    pub fn release(&self, lease: &LeaseId, actor: ParticipantId) -> tm_types::Result<Vec<Event>> {
        let lease = lease.clone();
        self.run_command(move |view| {
            let l = view
                .leases
                .get(&lease)
                .ok_or_else(|| TmError::not_found("lease", &lease))?;
            let t = view
                .tickets
                .get(&l.ticket)
                .ok_or_else(|| TmError::not_found("ticket", &l.ticket))?;
            let to = machine::transition(t.state, Trigger::LeaseReleased)
                .map_err(|e| TmError::InvalidTransition(e.to_string()))?;
            Ok(vec![
                EventDraft::new(
                    actor.clone(),
                    Id::from(l.ticket.clone()),
                    Payload::from(TicketLeaseReleasedPayload {
                        ticket: l.ticket.clone(),
                        lease: lease.clone(),
                    }),
                ),
                state_changed_draft(&l.ticket, t.state, to, actor),
            ])
        })
    }

    /// Sweep for expired leases as of the injected clock's current time, reverting each per
    /// [`crate::lease::LeaseStore::expire_due`].
    pub fn expire_leases(&self) -> tm_types::Result<Vec<Event>> {
        self.run_command(move |view| {
            let now = self.clock.now();
            struct ViewAdapter<'a>(&'a BTreeMap<LeaseId, Lease>);
            impl<'a> LeaseView for ViewAdapter<'a> {
                fn live_leases(&self) -> Vec<&Lease> {
                    self.0.values().collect()
                }
            }
            let adapter = ViewAdapter(&view.leases);
            let actions = LeaseStore::expire_due(&adapter, now);
            let system = ParticipantId::system();
            let mut drafts = Vec::new();
            for action in &actions {
                drafts.push(EventDraft::new(
                    system.clone(),
                    Id::from(action.ticket.clone()),
                    Payload::from(TicketLeaseExpiredPayload {
                        ticket: action.ticket.clone(),
                        lease: action.lease.clone(),
                    }),
                ));
                if let Some(lease) = view.leases.get(&action.lease) {
                    drafts.push(EventDraft::new(
                        system.clone(),
                        Id::from(action.ticket.clone()),
                        Payload::from(AuthorityRevertedPayload {
                            subject: lease.holder.clone(),
                            ticket: Some(action.ticket.clone()),
                            to_seq: 0,
                        }),
                    ));
                }
                if let Some(t) = view.tickets.get(&action.ticket) {
                    if let Ok(to) = machine::transition(t.state, Trigger::LeaseExpired) {
                        drafts.push(state_changed_draft(
                            &action.ticket,
                            t.state,
                            to,
                            system.clone(),
                        ));
                    }
                }
            }
            Ok(drafts)
        })
    }

    /// Record a new decision.
    #[allow(clippy::too_many_arguments)]
    pub fn record_decision(
        &self,
        subject: String,
        decision: String,
        reason: String,
        evidence: Vec<ArtifactId>,
        affected_tickets: Vec<TicketId>,
        affected_paths: Vec<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.run_command(move |_view| {
            let id = DecisionId::new(self.ids.next(IdKind::Decision).as_str())?;
            let summary = decision_summary_json(
                &subject,
                &decision,
                &reason,
                &evidence,
                &affected_tickets,
                &affected_paths,
            );
            Ok(vec![EventDraft::new(
                actor,
                Id::from(id.clone()),
                Payload::from(DecisionCreatedPayload {
                    decision: id,
                    ticket: affected_tickets.first().cloned(),
                    summary,
                }),
            )])
        })
    }

    /// Supersede an existing decision with a new one.
    ///
    /// # Errors
    /// `TmError::not_found` if `supersedes` doesn't exist. `TmError::conflict` if it is already
    /// superseded.
    #[allow(clippy::too_many_arguments)]
    pub fn supersede(
        &self,
        supersedes: &DecisionId,
        subject: String,
        decision: String,
        reason: String,
        evidence: Vec<ArtifactId>,
        affected_tickets: Vec<TicketId>,
        affected_paths: Vec<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let supersedes = supersedes.clone();
        self.run_command(move |view| {
            let old = view
                .decisions
                .get(&supersedes)
                .ok_or_else(|| TmError::not_found("decision", &supersedes))?;
            if !old.is_active() {
                return Err(TmError::conflict(format!(
                    "decision {supersedes} is already superseded"
                )));
            }
            let new_id = DecisionId::new(self.ids.next(IdKind::Decision).as_str())?;
            let summary = decision_summary_json(
                &subject,
                &decision,
                &reason,
                &evidence,
                &affected_tickets,
                &affected_paths,
            );
            Ok(vec![
                EventDraft::new(
                    actor.clone(),
                    Id::from(new_id.clone()),
                    Payload::from(DecisionCreatedPayload {
                        decision: new_id.clone(),
                        ticket: affected_tickets.first().cloned(),
                        summary,
                    }),
                ),
                EventDraft::new(
                    actor,
                    Id::from(supersedes.clone()),
                    Payload::from(DecisionSupersededPayload {
                        decision: supersedes,
                        superseded_by: new_id,
                    }),
                ),
            ])
        })
    }

    /// Create a new milestone.
    pub fn create_milestone(
        &self,
        title: String,
        tickets: Vec<TicketId>,
        assumptions: Vec<DecisionId>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.run_command(move |view| {
            for t in &tickets {
                if !view.tickets.contains_key(t) {
                    return Err(TmError::not_found("ticket", t));
                }
            }
            let id = MilestoneId::new(self.ids.next(IdKind::Milestone).as_str())?;
            let mut drafts = vec![EventDraft::new(
                actor.clone(),
                Id::from(id.clone()),
                Payload::from(MilestoneCreatedPayload {
                    milestone: id.clone(),
                    title,
                }),
            )];
            for t in &tickets {
                let fields = serde_json::json!({ "milestone": id });
                drafts.push(EventDraft::new(
                    actor.clone(),
                    Id::from(t.clone()),
                    Payload::from(TicketUpdatedPayload {
                        ticket: t.clone(),
                        fields,
                    }),
                ));
            }
            // `assumptions` (decisions this milestone rests on) has no dedicated linkage event
            // in the closed catalogue; accepted here but not yet independently persisted.
            let _ = assumptions;
            Ok(drafts)
        })
    }

    /// Close a milestone. See [`crate::milestone::MilestoneStore::close`].
    pub fn close_milestone(
        &self,
        milestone: &MilestoneId,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let milestone_id = milestone.clone();
        self.run_command(move |view| {
            let m = view
                .milestones
                .get(&milestone_id)
                .ok_or_else(|| TmError::not_found("milestone", &milestone_id))?;
            for t in &m.tickets {
                if let Some(tt) = view.tickets.get(t) {
                    if !tt.authority.project.close_milestone {
                        return Err(TmError::AuthorityDenied(format!(
                            "{t} lacks project.close_milestone authority"
                        )));
                    }
                }
            }
            let member_states: BTreeMap<TicketId, TicketState> = m
                .tickets
                .iter()
                .filter_map(|t| view.tickets.get(t).map(|tt| (t.clone(), tt.state)))
                .collect();
            MilestoneStore::close(m, &member_states, actor.clone())
                .map_err(|e| TmError::conflict(e.to_string()))?;
            Ok(vec![EventDraft::new(
                actor,
                Id::from(milestone_id.clone()),
                Payload::from(MilestoneClosedPayload {
                    milestone: milestone_id,
                }),
            )])
        })
    }

    /// Reopen a milestone. See [`crate::milestone::MilestoneStore::reopen`].
    pub fn reopen_milestone(
        &self,
        milestone: &MilestoneId,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let milestone_id = milestone.clone();
        self.run_command(move |view| {
            let m = view
                .milestones
                .get(&milestone_id)
                .ok_or_else(|| TmError::not_found("milestone", &milestone_id))?;
            for t in &m.tickets {
                if let Some(tt) = view.tickets.get(t) {
                    if !tt.authority.project.reopen_milestone {
                        return Err(TmError::AuthorityDenied(format!(
                            "{t} lacks project.reopen_milestone authority"
                        )));
                    }
                }
            }
            let (_, affected) = MilestoneStore::reopen(m, &view.graph);
            let mut drafts = vec![EventDraft::new(
                actor.clone(),
                Id::from(milestone_id.clone()),
                Payload::from(MilestoneReopenedPayload {
                    milestone: milestone_id.clone(),
                }),
            )];
            for t in &affected {
                if let Some(tt) = view.tickets.get(t) {
                    if let Ok(to) = machine::transition(tt.state, Trigger::Reopen) {
                        drafts.push(EventDraft::new(
                            actor.clone(),
                            Id::from(t.clone()),
                            Payload::from(TicketReopenedPayload {
                                ticket: t.clone(),
                                reason: Some(format!(
                                    "cascaded from milestone {milestone_id} reopening"
                                )),
                            }),
                        ));
                        drafts.push(state_changed_draft(t, tt.state, to, actor.clone()));
                    }
                }
            }
            Ok(drafts)
        })
    }

    /// Store a new artifact's bytes, choosing inline vs. on-disk placement per
    /// [`crate::artifact::plan_storage`].
    pub fn store_artifact(
        &self,
        kind: ArtifactKind,
        media_type: String,
        bytes: Vec<u8>,
        meta: serde_json::Value,
        ticket: Option<TicketId>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let id = ArtifactId::new(self.ids.next(IdKind::Artifact).as_str())?;
        let (hash, storage) = crate::artifact::plan_storage(&self.root, &bytes);
        if let ArtifactStorage::OnDisk(path) = &storage {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, &bytes)?;
        }
        let storage_text = encode_artifact_storage(&storage);
        let kind_text = to_json_text(&kind)?;
        let meta_text = serde_json::to_string(&meta).map_err(TmError::from)?;
        let bytes_len = bytes.len() as i64;
        let insert_id = id.clone();
        let media_type_for_sql = media_type.clone();
        let storage_text_for_sql = storage_text.clone();

        self.run_command_with_extra(
            move |_view| {
                Ok(vec![EventDraft::new(
                    actor,
                    Id::from(id.clone()),
                    Payload::from(ArtifactCreatedPayload {
                        artifact: id.clone(),
                        ticket: ticket.clone(),
                        path: storage_text.clone(),
                        media_type: media_type.clone(),
                    }),
                )])
            },
            move |tx, _events| {
                tx.raw()
                    .execute(
                        "INSERT INTO artifacts (id, kind, media_type, bytes_len, hash, storage, meta)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, media_type = excluded.media_type,
                            bytes_len = excluded.bytes_len, hash = excluded.hash, storage = excluded.storage,
                            meta = excluded.meta",
                        rusqlite::params![
                            insert_id.as_str(),
                            kind_text,
                            media_type_for_sql,
                            bytes_len,
                            hash,
                            storage_text_for_sql,
                            meta_text,
                        ],
                    )
                    .map_err(storage_err)?;
                Ok(())
            },
        )
    }

    /// Attach an evidence record linking `ticket` to `artifact`.
    pub fn attach_evidence(
        &self,
        ticket: &TicketId,
        kind: EvidenceKind,
        artifact: &ArtifactId,
        summary: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket = ticket.clone();
        let artifact = artifact.clone();
        self.run_command(move |view| {
            if !view.tickets.contains_key(&ticket) {
                return Err(TmError::not_found("ticket", &ticket));
            }
            if !view.artifacts.contains_key(&artifact) {
                return Err(TmError::not_found("artifact", &artifact));
            }
            Ok(vec![evidence_draft(
                &ticket,
                kind,
                &artifact,
                summary.clone(),
                actor.clone(),
            )])
        })
    }

    /// Record usage against `ticket` (and its ancestor scopes), debiting via
    /// [`crate::budget::BudgetLedger::record_usage`].
    ///
    /// # Errors
    /// `TmError::BudgetExhausted` naming the exhausted scope. On exhaustion, when `ticket` is
    /// currently `Leased` or `Running` (mid-run resource exhaustion, not a hard failure), this is
    /// a **handoff, not death** (`SPEC.md` §31.3, `docs/audit-2026-09-18-fable.md` B-10): the
    /// ticket's lease is released and it transitions `-> Ready` via [`Trigger::BudgetHandoff`],
    /// never `Recovery` — no retry attempt is consumed, and only `ticket.budget_handoff` (not
    /// `ticket.budget_exhausted`) is emitted at the ticket level, since a handoff must not read
    /// as blocked work to a status digest. When `ticket` is in some other live state (e.g.
    /// `Verifying`, where `Trigger::BudgetHandoff` has no legal target — see
    /// [`crate::machine::transition`]), this narrower case is not handoff-eligible and keeps the
    /// prior behavior of emitting `ticket.budget_exhausted` and routing toward `Recovery` like
    /// any other verification-time failure.
    pub fn record_usage(
        &self,
        ticket: Option<&TicketId>,
        session: Option<&SessionId>,
        amount: Spend,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket_id = ticket.cloned();
        let session_id = session.cloned();
        let mut exhausted: Option<BudgetScope> = None;
        let result = self.run_command(|view| {
            let mut chain = Vec::new();
            if let Some(tid) = &ticket_id {
                let t = view
                    .tickets
                    .get(tid)
                    .ok_or_else(|| TmError::not_found("ticket", tid))?;
                chain.push(ScopedBudget {
                    scope: BudgetScope::Ticket(tid.clone()),
                    budget: t.budget,
                });
            }
            let project_budget = view
                .budgets
                .iter()
                .find(|b| b.scope == BudgetScope::Project)
                .map(|b| b.budget)
                .unwrap_or_else(Budget::unlimited);
            chain.push(ScopedBudget {
                scope: BudgetScope::Project,
                budget: project_budget,
            });

            let mut drafts = vec![EventDraft::new(
                actor.clone(),
                ticket_id.clone().map(Id::from).unwrap_or_else(Id::none),
                Payload::from(UsageRecordedPayload {
                    ticket: ticket_id.clone(),
                    session: session_id.clone(),
                    tokens: amount.tokens,
                    dollars_micros: amount.dollars_micros,
                    wall_seconds: amount.wall_seconds,
                }),
            )];

            if let Err(e) = BudgetLedger::record_usage(chain, amount) {
                exhausted = Some(e.scope.clone());
                if let Some(tid) = &ticket_id {
                    if view.tickets.contains_key(tid) {
                        let dimension = format!("{:?}", e.scope);
                        match budget_handoff_drafts(view, tid, &actor, &dimension) {
                            // Handoff-eligible: emit only `ticket.budget_handoff`, never
                            // `ticket.budget_exhausted` — the latter is read by
                            // `tm-cli`'s status digest as "blocked" work
                            // (`crates/tm-cli/src/project.rs`), and a handoff is deliberately
                            // not that (`SPEC.md` §31.3: "not evidence of anything going
                            // wrong"). The ceiling itself is still on record via this call's
                            // own `usage.recorded` draft above and its `Err(TmError::
                            // BudgetExhausted)` return value.
                            Some(handoff_drafts) => drafts.extend(handoff_drafts),
                            // Not handoff-eligible (ticket isn't `Leased`/`Running` right now,
                            // e.g. `Verifying`): this is a genuine failure-shaped exhaustion, so
                            // it keeps the prior behavior — `ticket.budget_exhausted` plus
                            // routing toward `Recovery` like any other verification-time
                            // failure, rather than silently dropping the state transition.
                            None => {
                                drafts.push(EventDraft::new(
                                    actor.clone(),
                                    Id::from(tid.clone()),
                                    Payload::from(TicketBudgetExhaustedPayload {
                                        ticket: tid.clone(),
                                        dimension: dimension.clone(),
                                        limit: 0,
                                        spent: 0,
                                    }),
                                ));
                                if let Some(t) = view.tickets.get(tid) {
                                    if let Ok(to) =
                                        machine::transition(t.state, Trigger::VerificationFailed)
                                    {
                                        drafts.push(state_changed_draft(
                                            tid,
                                            t.state,
                                            to,
                                            actor.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(drafts)
        });

        match (result, exhausted) {
            (Ok(_events), Some(scope)) => Err(TmError::BudgetExhausted(format!("{scope:?}"))),
            (Ok(events), None) => Ok(events),
            (Err(e), _) => Err(e),
        }
    }

    /// Hand `ticket` off cleanly before an effect it cannot afford ever starts (`SPEC.md` §31.2
    /// "refuse to start what it cannot finish", §31.3 "handoff, not death"): releases any live
    /// lease(s) held on it and transitions it `Leased|Running -> Ready` via
    /// [`Trigger::BudgetHandoff`] — never `Recovery`, never consuming a retry attempt
    /// (`docs/audit-2026-09-18-fable.md` B-10). `dimension` names why (e.g. `"Tokens"`), carried
    /// on the emitted `ticket.budget_handoff` event only — it drives no branching here.
    ///
    /// Goal state (`SPEC.md` §29) is left untouched by construction: the `goals` table is keyed
    /// by ticket, not by lease or attempt, so a fresh worker that later re-leases this same
    /// ticket via [`Store::goal_state`] reads it back exactly as it was left.
    ///
    /// A no-op (`Ok(vec![])`) if `ticket` does not exist or is not currently `Leased`/`Running` —
    /// e.g. a race where [`Store::record_usage`]'s own reactive handoff, or a plain lease expiry,
    /// already moved it back to `Ready` by the time this call lands. Callers (e.g.
    /// `tm_agent::agent_loop::AgentLoop`'s proactive `can_afford` check) call this defensively
    /// and should never fail a run just because it lost a race with the store's own bookkeeping.
    pub fn budget_handoff(
        &self,
        ticket: &TicketId,
        dimension: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let ticket_id = ticket.clone();
        self.run_command(move |view| {
            Ok(budget_handoff_drafts(view, &ticket_id, &actor, &dimension).unwrap_or_default())
        })
    }

    /// Read the current materialized `effects` row for `key`, if any (`SPEC.md` §21.5).
    pub fn effect_status(&self, key: &EffectKey) -> tm_types::Result<Option<Effect>> {
        let conn = tm_events::schema::open_read_connection(self.log.path())?;
        let row = conn.query_row(
            "SELECT key, ticket, attempt, kind, status, receipt_artifact, started, completed
             FROM effects WHERE key = ?1",
            rusqlite::params![key.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            },
        );
        match row {
            Ok((key_s, ticket_s, attempt, kind, status, receipt, started, completed)) => {
                let ticket: TicketId = ticket_s
                    .parse()
                    .map_err(|e| TmError::storage(format!("corrupt effects.ticket: {e}")))?;
                let status = EffectStatus::parse(&status)?;
                let started = Timestamp::parse_rfc3339(&started)
                    .map_err(|e| TmError::storage(format!("corrupt effects.started: {e}")))?;
                let completed = completed
                    .map(|c| Timestamp::parse_rfc3339(&c))
                    .transpose()
                    .map_err(|e| TmError::storage(format!("corrupt effects.completed: {e}")))?;
                Ok(Some(Effect {
                    key: EffectKey::from_hex(key_s),
                    ticket,
                    attempt: attempt as u32,
                    kind,
                    status,
                    receipt_artifact: receipt,
                    started,
                    completed,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }

    /// Journal-first idempotent-effect guard (`SPEC.md` §21.5): look up `key`'s current
    /// `effects` row before doing anything else.
    ///
    /// * No row exists: writes a fresh `effect.journaled` event/row and returns a guard with
    ///   neither [`EffectGuard::already_completed`] nor [`EffectGuard::resumed`] set — the caller
    ///   should perform the effect and call [`EffectGuard::complete`].
    /// * A `completed` row exists: writes nothing (the whole point — a completed effect is never
    ///   re-journaled) and returns a guard with [`EffectGuard::already_completed`] set, carrying
    ///   the prior receipt. The caller must not repeat the effect.
    /// * A `journaled`/`failed` row exists (an earlier attempt began this exact effect and never
    ///   reached `completed`): writes nothing and returns a guard with only
    ///   [`EffectGuard::resumed`] set. The caller should try its effect kind's `confirm()`-shaped
    ///   recovery probe before deciding whether to re-run.
    pub fn begin_effect(
        &self,
        key: EffectKey,
        ticket: TicketId,
        attempt: u32,
        kind: impl Into<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<EffectGuard> {
        let kind = kind.into();
        if let Some(existing) = self.effect_status(&key)? {
            return Ok(match existing.status {
                EffectStatus::Completed => EffectGuard::new(
                    key,
                    ticket,
                    attempt,
                    kind,
                    actor,
                    true,
                    true,
                    existing.receipt_artifact,
                ),
                EffectStatus::Journaled | EffectStatus::Failed => {
                    EffectGuard::new(key, ticket, attempt, kind, actor, false, true, None)
                }
            });
        }
        self.append(vec![EventDraft::new(
            actor.clone(),
            Id::from(ticket.clone()),
            Payload::from(EffectJournaledPayload {
                key: key.as_str().to_string(),
                ticket: ticket.clone(),
                attempt,
                kind: kind.clone(),
            }),
        )])?;
        Ok(EffectGuard::new(
            key, ticket, attempt, kind, actor, false, false, None,
        ))
    }

    /// Mark `key`'s effect completed. Called only from [`EffectGuard::complete`], which already
    /// guards against calling this on an already-completed effect.
    pub(crate) fn complete_effect(
        &self,
        key: &EffectKey,
        ticket: &TicketId,
        receipt_artifact: Option<&str>,
        actor: ParticipantId,
    ) -> tm_types::Result<()> {
        self.append(vec![EventDraft::new(
            actor,
            Id::from(ticket.clone()),
            Payload::from(EffectCompletedPayload {
                key: key.as_str().to_string(),
                ticket: ticket.clone(),
                receipt_artifact: receipt_artifact.map(str::to_string),
            }),
        )])?;
        Ok(())
    }

    /// Mark `key`'s effect failed. Called only from [`EffectGuard::fail`].
    pub(crate) fn fail_effect(
        &self,
        key: &EffectKey,
        ticket: &TicketId,
        reason: &str,
        actor: ParticipantId,
    ) -> tm_types::Result<()> {
        self.append(vec![EventDraft::new(
            actor,
            Id::from(ticket.clone()),
            Payload::from(EffectFailedPayload {
                key: key.as_str().to_string(),
                ticket: ticket.clone(),
                reason: reason.to_string(),
            }),
        )])?;
        Ok(())
    }

    /// The full project view, assembled from materialized state.
    pub fn view(&self) -> tm_types::Result<ProjectView> {
        let conn = tm_events::schema::open_read_connection(self.log.path())?;
        Self::read_view(&conn)
    }

    /// The narrow view `tm-scheduler` needs.
    pub fn scheduler_view(&self) -> tm_types::Result<SchedulerView> {
        let view = self.view()?;
        Ok(SchedulerView::from(&view))
    }

    /// Drop every materialized table and replay the entire event log from `seq` 0 through
    /// [`crate::materialize::replay`], reproducing byte-identical materialized state by
    /// construction (both this path and the live per-event path call the same `apply`).
    pub fn rebuild(&self) -> tm_types::Result<()> {
        let tx = self.log.begin()?;
        crate::schema::drop_views(tx.raw())?;
        crate::schema::create_views(tx.raw())?;
        let mut seq = 1u64;
        loop {
            let batch = self.log.read_from(seq, 1024)?;
            if batch.is_empty() {
                break;
            }
            crate::materialize::replay(&tx, &batch)?;
            seq += batch.len() as u64;
        }
        // `IdSource` carries no `Any` bound, so a generically-injected `self.ids` cannot be
        // downcast back to `CounterIds` here to restore its high-water marks in place; a caller
        // that needs that guarantee after `rebuild` should reopen via `Store::open`/
        // `Store::open_with` using `Store::counters()` read post-rebuild, as `Store::open` does
        // on a fresh open.
        tx.commit()?;
        Ok(())
    }

    /// Run [`crate::invariants::check_invariants`] against the current view.
    pub fn check_invariants(&self) -> tm_types::Result<Vec<crate::invariants::Violation>> {
        Ok(crate::invariants::check_invariants(&self.view()?))
    }

    /// Run `f` against one shared [`StoreTx`], committing only if `f` succeeds and
    /// [`crate::invariants::check_invariants`] finds no violation in the resulting state
    /// (otherwise rolling back and surfacing a `TmError::invariant`, mirroring
    /// [`Store::run_command_with_extra`]'s own commit discipline).
    ///
    /// This is the entry point for a caller that needs to commit several heterogeneous writes as
    /// one atomic unit — e.g. `tm-genesis`'s `commit_graph`, which today calls `create_ticket`/
    /// `create_milestone`/`add_dependency` as separate transactions purely because `Store` had no
    /// shared-transaction primitive to call instead (see that function's own module note); a
    /// caller like it can now do `store.transaction(|tx| { tx.append(draft_a)?; tx.append(draft_b)?; ... })`
    /// and get "commit together or reject wholesale" for real, without this crate needing to know
    /// anything about genesis' domain.
    ///
    /// # Warning
    /// `f` must not call any other `Store` method (including [`Store::append`]/
    /// [`Store::run_command`] and every typed command/helper built on them) — `tm_events::EventLog`
    /// serializes writers with one non-reentrant lock, held for the whole transaction by the
    /// [`tm_events::log::Tx`] this call already opened, so a nested `Store` call deadlocks rather
    /// than erroring. Build every write inside `f` as an [`tm_events::EventDraft`] and append it
    /// through [`StoreTx::append`]/[`StoreTx::append_all`] (or write raw SQL via [`StoreTx::raw`],
    /// per this module's top-level note) instead of calling back out to `Store`.
    ///
    /// # Errors
    /// Whatever `f` returns, or `TmError::invariant` if the post-write state violates an
    /// invariant.
    pub fn transaction<F, T>(&self, f: F) -> tm_types::Result<T>
    where
        F: FnOnce(&StoreTx<'_>) -> tm_types::Result<T>,
    {
        let tx = self.log.begin()?;
        let store_tx = StoreTx { log: &self.log, tx };
        let result = f(&store_tx)?;
        let post_view = Self::read_view(store_tx.tx.raw())?;
        let violations = crate::invariants::check_invariants(&post_view);
        if !violations.is_empty() {
            let detail = violations
                .iter()
                .map(|v| format!("{}: {}", v.invariant, v.detail))
                .collect::<Vec<_>>()
                .join("; ");
            store_tx.tx.rollback()?;
            return Err(TmError::invariant(format!(
                "{} invariant violation(s): {detail}",
                violations.len()
            )));
        }
        store_tx.tx.commit()?;
        Ok(result)
    }

    /// Append every draft in `drafts`, materializing each via [`crate::materialize::apply`] in
    /// the same transaction as the append (built on [`Store::transaction`]). Every typed
    /// convenience method below that doesn't need [`Store::run_command`]'s view-snapshot-driven
    /// validation (`start_session`, `register_doc`, `record_command`, ...) is built on this;
    /// it is also the escape hatch for a caller that already holds a typed `EventDraft` — e.g.
    /// `tm-context::command::run`'s `command.started`/`command.completed` drafts, previously
    /// dropped on the floor because `Store` exposed no generic append entry point at all (see
    /// `tm-agent::tools::run_shell_like`'s own comment on that gap).
    ///
    /// # Errors
    /// Whatever the underlying append/materialize calls return, or `TmError::invariant` if the
    /// post-write state violates an invariant.
    pub fn append(&self, drafts: Vec<EventDraft>) -> tm_types::Result<Vec<Event>> {
        self.transaction(|tx| tx.append_all(drafts))
    }

    /// Start a new session for `participant`, allocating a fresh [`SessionId`]. The session id
    /// is recoverable from the returned event's payload (`events[0].payload.as_session_started()`),
    /// the same convention [`Store::record_decision`]'s callers already use to pull a fresh id
    /// back out of a generic `Vec<Event>` return.
    pub fn start_session(
        &self,
        participant: ParticipantId,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let session = SessionId::new(self.ids.next(IdKind::Session).as_str())?;
        self.append(vec![EventDraft::new(
            actor,
            Id::from(session.clone()),
            Payload::from(SessionStartedPayload {
                session,
                participant,
            }),
        )])
    }

    /// Record `participant` joining an already-started `session`.
    pub fn join_session(
        &self,
        session: &SessionId,
        participant: ParticipantId,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.append(vec![EventDraft::new(
            actor,
            Id::from(session.clone()),
            Payload::from(SessionJoinedPayload {
                session: session.clone(),
                participant,
            }),
        )])
    }

    /// End `session`.
    pub fn end_session(
        &self,
        session: &SessionId,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.append(vec![EventDraft::new(
            actor,
            Id::from(session.clone()),
            Payload::from(SessionEndedPayload {
                session: session.clone(),
            }),
        )])
    }

    /// Register a doc at `path`, optionally attributing its registration to `ticket`. See
    /// [`crate::materialize::apply`]'s `doc.registered` arm for the `path`-as-`docs.id` mapping
    /// decision this relies on.
    pub fn register_doc(
        &self,
        path: String,
        ticket: Option<TicketId>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.append(vec![EventDraft::new(
            actor,
            Id::new(path.clone()),
            Payload::from(DocRegisteredPayload { path, ticket }),
        )])
    }

    /// Mark the doc at `path` invalidated, for `reason` (a free-form description; see
    /// [`crate::materialize::apply`]'s `doc.invalidated` arm for why `reason` itself isn't
    /// persisted into `docs`).
    pub fn invalidate_doc(
        &self,
        path: String,
        reason: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.append(vec![EventDraft::new(
            actor,
            Id::new(path.clone()),
            Payload::from(DocInvalidatedPayload { path, reason }),
        )])
    }

    /// Mark the doc at `path` reconciled (its content once again matches its declared basis).
    pub fn reconcile_doc(
        &self,
        path: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.append(vec![EventDraft::new(
            actor,
            Id::new(path.clone()),
            Payload::from(DocReconciledPayload { path }),
        )])
    }

    /// Promote `candidate` to a new harness epoch. `harness_epochs.epoch` is allocated inside
    /// [`crate::materialize::apply`]'s `harness.promoted` arm, not here, since it must be derived
    /// deterministically from replay order rather than from any id source this method could call.
    pub fn promote_epoch(
        &self,
        candidate: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.append(vec![EventDraft::new(
            actor,
            Id::none(),
            Payload::from(HarnessPromotedPayload { candidate }),
        )])
    }

    /// Record a new mirror link for `ticket` onto `remote` (an adapter/tracker name, matching
    /// `tm-mirror::sync::SyncEngine`'s own `tracker.name()` convention).
    pub fn link_mirror(
        &self,
        ticket: &TicketId,
        remote: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.append(vec![EventDraft::new(
            actor,
            Id::from(ticket.clone()),
            Payload::from(MirrorLinkedPayload { remote }),
        )])
    }

    /// Record a push or pull that synced `ticket` against `remote`'s `reference` (the external
    /// id) — see [`MirrorSyncDirection`].
    pub fn update_mirror_link(
        &self,
        ticket: &TicketId,
        remote: String,
        reference: String,
        direction: MirrorSyncDirection,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let payload = match direction {
            MirrorSyncDirection::Push => Payload::from(MirrorPushedPayload { remote, reference }),
            MirrorSyncDirection::Pull => Payload::from(MirrorPulledPayload { remote, reference }),
        };
        self.append(vec![EventDraft::new(
            actor,
            Id::from(ticket.clone()),
            payload,
        )])
    }

    /// Record a command run's `command.started`/`command.completed` pair together, mirroring
    /// `tm-context::command::run`'s own event-building shape (see that function's doc comment) —
    /// the typed entry point that closes the gap `tm-agent::tools::run_shell_like` currently
    /// works around by dropping the drafts `command::run` already builds.
    #[allow(clippy::too_many_arguments)]
    pub fn record_command(
        &self,
        command: String,
        ticket: Option<TicketId>,
        session: Option<SessionId>,
        exit_code: i32,
        duration_ms: u64,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let subject = ticket.clone().map(Id::from).unwrap_or_else(Id::none);
        self.append(vec![
            EventDraft::new(
                actor.clone(),
                subject.clone(),
                Payload::from(CommandStartedPayload {
                    command: command.clone(),
                    ticket: ticket.clone(),
                    session: session.clone(),
                }),
            ),
            EventDraft::new(
                actor,
                subject,
                Payload::from(CommandCompletedPayload {
                    command,
                    ticket,
                    session,
                    exit_code,
                    duration_ms,
                }),
            ),
        ])
    }

    /// Set (or replace) `ticket`'s durable goal (`SPEC.md` §29, `docs/audit-2026-09-18-fable.md`
    /// B-09) — the worker's live decomposition of how it is getting to the ticket's own
    /// `objective`, distinct from that objective itself. Replacing an existing goal resets its
    /// step list and `claimed_complete` flag (see the `goal.set` materializer arm), so this is
    /// also how a loop starts a fresh decomposition after abandoning a stale one.
    pub fn set_goal(
        &self,
        ticket: &TicketId,
        text: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.append(vec![EventDraft::new(
            actor,
            Id::from(ticket.clone()),
            Payload::from(GoalSetPayload {
                ticket: ticket.clone(),
                text,
            }),
        )])
    }

    /// Add one step to `ticket`'s current goal decomposition, identified by `step_id` (stable
    /// across a later [`Store::complete_goal_step`] call for the same step — the caller mints
    /// this, typically from the read-back [`GoalState::steps`] length; see
    /// `tm-agent::agent_loop`'s own step-id convention for the concrete scheme it uses).
    pub fn add_goal_step(
        &self,
        ticket: &TicketId,
        step_id: String,
        text: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.append(vec![EventDraft::new(
            actor,
            Id::from(ticket.clone()),
            Payload::from(GoalStepAddedPayload {
                ticket: ticket.clone(),
                step_id,
                text,
            }),
        )])
    }

    /// Mark one step of `ticket`'s goal decomposition done.
    pub fn complete_goal_step(
        &self,
        ticket: &TicketId,
        step_id: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.append(vec![EventDraft::new(
            actor,
            Id::from(ticket.clone()),
            Payload::from(GoalStepCompletedPayload {
                ticket: ticket.clone(),
                step_id,
            }),
        )])
    }

    /// Record that a loop re-read `ticket`'s goal state against observed state at the start of
    /// `at_step` (`SPEC.md` §29's "re-orientation is explicit") — a durable trace of when
    /// re-orientation happened, not itself a change to the goal's text or steps.
    pub fn reorient_goal(
        &self,
        ticket: &TicketId,
        at_step: u32,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.append(vec![EventDraft::new(
            actor,
            Id::from(ticket.clone()),
            Payload::from(GoalReorientedPayload {
                ticket: ticket.clone(),
                at_step,
            }),
        )])
    }

    /// Record that a loop believes `ticket`'s goal is met, carrying `summary`. A claim only:
    /// `SPEC.md` §16's verification ladder (never this method, never the caller) decides whether
    /// it was — see `tm-agent::agent_loop::AgentLoop`'s own doc comment on why this is always
    /// followed by [`Store::submit`], never [`Store::verify`].
    pub fn claim_goal_complete(
        &self,
        ticket: &TicketId,
        summary: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        self.append(vec![EventDraft::new(
            actor,
            Id::from(ticket.clone()),
            Payload::from(GoalClaimedCompletePayload {
                ticket: ticket.clone(),
                summary,
            }),
        )])
    }

    /// Read `ticket`'s current materialized goal state, or `None` if no `goal.set` has ever been
    /// recorded for it. The read path a loop's re-orientation uses (`SPEC.md` §29): re-reading
    /// this against observed state at the start of every step, rather than trusting only its own
    /// in-memory conversation history, is what keeps a long run on target across turns and
    /// resumptions.
    pub fn goal_state(&self, ticket: &TicketId) -> tm_types::Result<Option<GoalState>> {
        let conn = tm_events::schema::open_read_connection(self.log.path())?;
        let row = conn.query_row(
            "SELECT text, steps, claimed_complete, last_reoriented_step, set_at, updated_at
             FROM goals WHERE ticket = ?1",
            rusqlite::params![ticket.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        );
        match row {
            Ok((text, steps_json, claimed_complete, last_reoriented_step, set_at, updated_at)) => {
                let steps: Vec<GoalStep> = serde_json::from_str(&steps_json)
                    .map_err(|e| TmError::storage(format!("corrupt goals.steps: {e}")))?;
                let set_at = Timestamp::parse_rfc3339(&set_at)
                    .map_err(|e| TmError::storage(format!("corrupt goals.set_at: {e}")))?;
                let updated_at = Timestamp::parse_rfc3339(&updated_at)
                    .map_err(|e| TmError::storage(format!("corrupt goals.updated_at: {e}")))?;
                Ok(Some(GoalState {
                    ticket: ticket.clone(),
                    text,
                    steps,
                    claimed_complete: claimed_complete != 0,
                    last_reoriented_step: last_reoriented_step as u32,
                    set_at,
                    updated_at,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }

    /// Total number of events recorded against `subject` so far (a raw `COUNT(*)` over
    /// `tm-events`' own `events` table, keyed by its `subject` column — see
    /// [`tm_events::log::EventLog::read_subject`], which this deliberately does not call: reading
    /// every row back just to `.len()` them would work but do needless deserialization work for
    /// what a caller like `tm-agent::agent_loop::AgentLoop`'s `max_events_per_ticket` backstop
    /// checks on every single step of a run). Not scoped to any one [`tm_events::EventKind`] —
    /// the backstop this feeds is a dumb, global ceiling on *any* accumulation against one
    /// ticket, per `SPEC.md` §21.5's "dumb global backstop" the same audit item (B-09) cites.
    pub fn event_count_for(&self, subject: &Id) -> tm_types::Result<u64> {
        let conn = tm_events::schema::open_read_connection(self.log.path())?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE subject = ?1",
                rusqlite::params![subject.as_str()],
                |row| row.get(0),
            )
            .map_err(storage_err)?;
        Ok(count as u64)
    }

    /// Current high-water mark of every id counter, for persistence/diagnostics.
    pub fn counters(&self) -> tm_types::Result<BTreeMap<String, u64>> {
        Ok(self.view()?.counters)
    }

    /// Every registered doc, joined with its `doc_provenance` originating tickets. See
    /// [`DocRow`]'s own doc comment for why this is a distinct, thinner shape than
    /// `tm_docs::registry::DocRecord`.
    pub fn docs(&self) -> tm_types::Result<Vec<DocRow>> {
        let conn = tm_events::schema::open_read_connection(self.log.path())?;
        let mut stmt = conn
            .prepare("SELECT id, title, author, ts FROM docs ORDER BY id")
            .map_err(storage_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(storage_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage_err)?;

        let mut docs = Vec::with_capacity(rows.len());
        for (id, title, author, ts) in rows {
            let mut prov_stmt = conn
                .prepare("SELECT source FROM doc_provenance WHERE doc_id = ?1 ORDER BY source")
                .map_err(storage_err)?;
            let provenance_tickets = prov_stmt
                .query_map(rusqlite::params![&id], |r| r.get::<_, String>(0))
                .map_err(storage_err)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(storage_err)?
                .into_iter()
                .filter_map(|s| TicketId::new(s).ok())
                .collect();
            docs.push(DocRow {
                id,
                title,
                author: author.parse::<ParticipantId>()?,
                ts: parse_ts(&ts)?,
                provenance_tickets,
            });
        }
        Ok(docs)
    }

    /// Every persisted mirror link, one row per ticket (`mirror_links.ticket` is the table's
    /// primary key -- see [`MirrorLinkRow`]'s doc comment for what that means for a ticket
    /// mirrored to more than one adapter).
    pub fn mirror_links(&self) -> tm_types::Result<Vec<MirrorLinkRow>> {
        let conn = tm_events::schema::open_read_connection(self.log.path())?;
        let mut stmt = conn
            .prepare(
                "SELECT ticket, remote_id, remote_system, last_synced FROM mirror_links ORDER BY ticket",
            )
            .map_err(storage_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(storage_err)?;
        let mut links = Vec::new();
        for row in rows {
            let (ticket, remote_id, remote_system, last_synced) = row.map_err(storage_err)?;
            links.push(MirrorLinkRow {
                ticket: TicketId::new(ticket)?,
                remote_id,
                remote_system,
                last_synced: parse_ts(&last_synced)?,
            });
        }
        Ok(links)
    }

    /// Every promoted harness epoch, oldest first. The genesis epoch (number `0`) is never
    /// persisted here -- see `harness.promoted`'s materializer arm -- so an empty result means
    /// "nothing has ever been promoted", not "no epochs exist".
    pub fn harness_epochs(&self) -> tm_types::Result<Vec<HarnessEpochRow>> {
        let conn = tm_events::schema::open_read_connection(self.log.path())?;
        let mut stmt = conn
            .prepare("SELECT epoch, harness_config, ts FROM harness_epochs ORDER BY epoch")
            .map_err(storage_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(storage_err)?;
        let mut epochs = Vec::new();
        for row in rows {
            let (epoch, harness_config, ts) = row.map_err(storage_err)?;
            epochs.push(HarnessEpochRow {
                epoch: epoch as u64,
                harness_config,
                ts: parse_ts(&ts)?,
            });
        }
        Ok(epochs)
    }

    /// Register a `tm-workflow` `WorkflowDef`'s TOML `source` under `content_hash` (its blake3
    /// hex digest, computed by the caller -- `tm-core` has no notion of `WorkflowDef`), returning
    /// the version number assigned to it.
    ///
    /// Idempotent: registering a `content_hash` already present returns the version it was
    /// first assigned, `source` unchanged (a hash collision on differing source is not possible
    /// short of a blake3 break, so this never needs to detect or reject one). Otherwise assigns
    /// one past the highest version already registered under `name`, so two different
    /// definitions sharing a `name` (an edited `.toml` on disk, re-registered) are distinguishable
    /// and orderable without this table needing its own id counter.
    ///
    /// Written directly to `workflows`' raw columns via [`Store::transaction`], the same
    /// exception [`Store::store_artifact`]'s module note documents -- see `crate::schema`'s
    /// module doc for why this table is not part of replay-derived state.
    pub fn register_workflow_def(
        &self,
        name: String,
        content_hash: String,
        source: String,
    ) -> tm_types::Result<u32> {
        let now = self.clock.now().to_rfc3339();
        self.transaction(move |tx| {
            let existing: Option<i64> = tx
                .raw()
                .query_row(
                    "SELECT version FROM workflows WHERE content_hash = ?1",
                    rusqlite::params![content_hash],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage_err)?;
            if let Some(version) = existing {
                return Ok(version as u32);
            }
            let next_version: i64 = tx
                .raw()
                .query_row(
                    "SELECT COALESCE(MAX(version), 0) + 1 FROM workflows WHERE name = ?1",
                    rusqlite::params![name],
                    |row| row.get(0),
                )
                .map_err(storage_err)?;
            tx.raw()
                .execute(
                    "INSERT INTO workflows (content_hash, name, version, source, registered_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![content_hash, name, next_version, source, now],
                )
                .map_err(storage_err)?;
            Ok(next_version as u32)
        })
    }

    /// Every registered workflow definition version, newest first, optionally filtered to one
    /// `name`.
    pub fn workflow_defs(&self, name: Option<&str>) -> tm_types::Result<Vec<WorkflowDefRow>> {
        let conn = tm_events::schema::open_read_connection(self.log.path())?;
        let mut stmt = match name {
            Some(_) => conn
                .prepare(
                    "SELECT content_hash, name, version, source, registered_at FROM workflows
                     WHERE name = ?1 ORDER BY version DESC",
                )
                .map_err(storage_err)?,
            None => conn
                .prepare(
                    "SELECT content_hash, name, version, source, registered_at FROM workflows
                     ORDER BY name, version DESC",
                )
                .map_err(storage_err)?,
        };
        let map_row = |row: &rusqlite::Row<'_>| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        };
        let rows = match name {
            Some(n) => stmt
                .query_map(rusqlite::params![n], map_row)
                .map_err(storage_err)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(storage_err)?,
            None => stmt
                .query_map([], map_row)
                .map_err(storage_err)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(storage_err)?,
        };
        let mut out = Vec::with_capacity(rows.len());
        for (content_hash, name, version, source, registered_at) in rows {
            out.push(WorkflowDefRow {
                content_hash,
                name,
                version: version as u32,
                source,
                registered_at: parse_ts(&registered_at)?,
            });
        }
        Ok(out)
    }

    /// Assemble a [`ProjectView`] from every row visible on `conn`, the single read path shared
    /// by [`Store::view`] (a fresh read connection) and the invariant checks
    /// [`Store::run_command_with_extra`] runs mid-transaction (`tx.raw()`).
    fn read_view(conn: &Connection) -> tm_types::Result<ProjectView> {
        let mut view = ProjectView::empty();

        {
            let mut stmt = conn
                .prepare(
                    "SELECT id, kind, objective, state, parent, milestone, authority, resources, executor,
                            context_refs, success, verification, budget, retry, cycle, attempts, failures,
                            priority, created, updated FROM tickets",
                )
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, String>(11)?,
                        row.get::<_, String>(12)?,
                        row.get::<_, String>(13)?,
                        row.get::<_, Option<String>>(14)?,
                        row.get::<_, i64>(15)?,
                        row.get::<_, String>(16)?,
                        row.get::<_, i64>(17)?,
                        row.get::<_, String>(18)?,
                        row.get::<_, String>(19)?,
                    ))
                })
                .map_err(storage_err)?;
            for row in rows {
                let (
                    id,
                    kind,
                    objective,
                    state,
                    parent,
                    milestone,
                    authority,
                    resources,
                    executor,
                    context_refs,
                    success,
                    verification,
                    budget,
                    retry,
                    cycle,
                    attempts,
                    failures,
                    priority,
                    created,
                    updated,
                ) = row.map_err(storage_err)?;
                let ticket_id = TicketId::new(id)?;
                let ticket = Ticket {
                    id: ticket_id.clone(),
                    kind: from_json_text(&kind)?,
                    objective,
                    state: from_json_text(&state)?,
                    parent: parent.map(TicketId::new).transpose()?,
                    children: Vec::new(),
                    dependencies: Vec::new(),
                    milestone: milestone.map(MilestoneId::new).transpose()?,
                    authority: from_json_text(&authority)?,
                    resources: from_json_text(&resources)?,
                    executor: from_json_text(&executor)?,
                    context_refs: from_json_text(&context_refs)?,
                    success: from_json_text(&success)?,
                    verification: from_json_text(&verification)?,
                    budget: from_json_text(&budget)?,
                    retry: from_json_text(&retry)?,
                    cycle: cycle.map(|c| from_json_text(&c)).transpose()?,
                    attempts: attempts as u32,
                    failures: from_json_text(&failures)?,
                    priority: priority as i32,
                    created: parse_ts(&created)?,
                    updated: parse_ts(&updated)?,
                };
                view.tickets.insert(ticket_id, ticket);
            }
        }

        let mut children_pairs = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT parent, child FROM ticket_children")
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(storage_err)?;
            for row in rows {
                let (parent, child) = row.map_err(storage_err)?;
                let parent_id = TicketId::new(parent)?;
                let child_id = TicketId::new(child)?;
                if let Some(t) = view.tickets.get_mut(&parent_id) {
                    t.children.push(child_id.clone());
                }
                children_pairs.push((parent_id, child_id));
            }
        }

        let mut edges = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT ticket, depends_on, kind FROM ticket_deps")
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(storage_err)?;
            for row in rows {
                let (ticket, depends_on, kind) = row.map_err(storage_err)?;
                let from = TicketId::new(ticket)?;
                let to = TicketId::new(depends_on)?;
                let dep_kind: DependencyKind = from_json_text(&kind)?;
                if let Some(t) = view.tickets.get_mut(&from) {
                    t.dependencies.push(to.clone());
                }
                edges.push(DependencyEdge {
                    from,
                    to,
                    kind: dep_kind,
                });
            }
        }

        let nodes: Vec<TicketId> = view.tickets.keys().cloned().collect();
        view.graph = DependencyGraph::build(nodes, edges, children_pairs);

        {
            let mut stmt = conn
                .prepare("SELECT id, ticket, holder, authority, resources, acquired, heartbeat, ttl_seconds, epoch FROM leases")
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                    ))
                })
                .map_err(storage_err)?;
            for row in rows {
                let (
                    id,
                    ticket,
                    holder,
                    authority,
                    resources,
                    acquired,
                    heartbeat,
                    ttl_seconds,
                    epoch,
                ) = row.map_err(storage_err)?;
                let lease_id = LeaseId::new(id)?;
                let lease = Lease {
                    id: lease_id.clone(),
                    ticket: TicketId::new(ticket)?,
                    holder: holder.parse::<ParticipantId>()?,
                    authority: from_json_text(&authority)?,
                    resources: from_json_text(&resources)?,
                    acquired: parse_ts(&acquired)?,
                    heartbeat: parse_ts(&heartbeat)?,
                    ttl_seconds: ttl_seconds as u32,
                    epoch: epoch as u64,
                };
                view.leases.insert(lease_id, lease);
            }
        }

        {
            let mut stmt = conn
                .prepare("SELECT id, subject, decision, reason, evidence, affected_tickets, affected_paths, author, ts, supersedes, superseded_by FROM decisions")
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                    ))
                })
                .map_err(storage_err)?;
            for row in rows {
                let (
                    id,
                    subject,
                    decision,
                    reason,
                    evidence,
                    affected_tickets,
                    affected_paths,
                    author,
                    ts,
                    supersedes,
                    superseded_by,
                ) = row.map_err(storage_err)?;
                let decision_id = DecisionId::new(id)?;
                let d = Decision {
                    id: decision_id.clone(),
                    subject,
                    decision,
                    reason,
                    evidence: from_json_text(&evidence)?,
                    affected_tickets: from_json_text(&affected_tickets)?,
                    affected_paths: from_json_text(&affected_paths)?,
                    author: author.parse::<ParticipantId>()?,
                    ts: parse_ts(&ts)?,
                    supersedes: supersedes.map(DecisionId::new).transpose()?,
                    superseded_by: superseded_by.map(DecisionId::new).transpose()?,
                };
                view.decisions.insert(decision_id, d);
            }
        }

        {
            let mut stmt = conn
                .prepare("SELECT id, title, tickets, state, closed_by, assumptions FROM milestones")
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
                .map_err(storage_err)?;
            for row in rows {
                let (id, title, tickets, state, closed_by, assumptions) =
                    row.map_err(storage_err)?;
                let milestone_id = MilestoneId::new(id)?;
                // Membership is authoritative from each ticket's own `milestone` pointer; the
                // milestone row's `tickets` column is merged in for members not yet visible from
                // that direction (e.g. this replay hasn't reached them yet).
                let mut members: Vec<TicketId> = view
                    .tickets
                    .values()
                    .filter(|t| t.milestone.as_ref() == Some(&milestone_id))
                    .map(|t| t.id.clone())
                    .collect();
                let stored: Vec<TicketId> = from_json_text(&tickets).unwrap_or_default();
                for t in stored {
                    if !members.contains(&t) {
                        members.push(t);
                    }
                }
                let m = Milestone {
                    id: milestone_id.clone(),
                    title,
                    tickets: members,
                    state: from_json_text(&state)?,
                    closed_by: closed_by.map(|s| s.parse::<ParticipantId>()).transpose()?,
                    assumptions: from_json_text(&assumptions)?,
                };
                view.milestones.insert(milestone_id, m);
            }
        }

        {
            let mut stmt = conn
                .prepare(
                    "SELECT id, kind, media_type, bytes_len, hash, storage, meta FROM artifacts",
                )
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                })
                .map_err(storage_err)?;
            for row in rows {
                let (id, kind, media_type, bytes_len, hash, storage, meta) =
                    row.map_err(storage_err)?;
                let artifact_id = ArtifactId::new(id)?;
                let a = Artifact {
                    id: artifact_id.clone(),
                    kind: from_json_text(&kind)?,
                    media_type,
                    bytes_len: bytes_len as u64,
                    hash,
                    storage: decode_artifact_storage(&storage)?,
                    meta: serde_json::from_str(&meta)
                        .map_err(|e| TmError::storage(e.to_string()))?,
                };
                view.artifacts.insert(artifact_id, a);
            }
        }

        {
            let mut stmt = conn
                .prepare("SELECT ticket, kind, artifact, produced_by, ts, summary FROM evidence")
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
                .map_err(storage_err)?;
            for row in rows {
                let (ticket, kind, artifact, produced_by, ts, summary) =
                    row.map_err(storage_err)?;
                view.evidence.push(Evidence {
                    ticket: TicketId::new(ticket)?,
                    kind: from_json_text(&kind)?,
                    artifact: ArtifactId::new(artifact)?,
                    produced_by: produced_by.parse::<ParticipantId>()?,
                    ts: parse_ts(&ts)?,
                    summary,
                });
            }
        }

        {
            let mut stmt = conn
                .prepare("SELECT scope, scope_id, limits FROM budgets")
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(storage_err)?;
            for row in rows {
                let (scope, scope_id, limits) = row.map_err(storage_err)?;
                let scope = match scope.as_str() {
                    "project" => BudgetScope::Project,
                    "milestone" => {
                        BudgetScope::Milestone(MilestoneId::new(scope_id.unwrap_or_default())?)
                    }
                    "ticket" => BudgetScope::Ticket(TicketId::new(scope_id.unwrap_or_default())?),
                    "lease" => BudgetScope::Lease(LeaseId::new(scope_id.unwrap_or_default())?),
                    other => {
                        return Err(TmError::storage(format!("unknown budget scope: {other}")))
                    }
                };
                view.budgets.push(ScopedBudget {
                    scope,
                    budget: from_json_text(&limits)?,
                });
            }
        }

        {
            let mut stmt = conn
                .prepare("SELECT id, status FROM participants")
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(storage_err)?;
            for row in rows {
                let (id, status) = row.map_err(storage_err)?;
                let pid = id.parse::<ParticipantId>()?;
                view.participants
                    .insert(pid.clone(), ParticipantSummary { id: pid, status });
            }
        }

        {
            let mut stmt = conn
                .prepare("SELECT counter_name, value FROM counters")
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(storage_err)?;
            for row in rows {
                let (name, value) = row.map_err(storage_err)?;
                view.counters.insert(name, value as u64);
            }
        }

        Ok(view)
    }
}

/// `attach_evidence`'s and `submit`'s shared draft builder: no dedicated `EventKind` exists for
/// evidence beyond `artifact.created`'s linkage (see the module-level note), so this both
/// records the fact via a `ticket.updated`-shaped event (for the append-only log's audit trail)
fn evidence_draft(
    ticket: &TicketId,
    kind: EvidenceKind,
    artifact: &ArtifactId,
    summary: String,
    actor: ParticipantId,
) -> EventDraft {
    let fields = serde_json::json!({
        "evidence_attached": {
            "kind": kind,
            "artifact": artifact,
            "summary": summary,
        }
    });
    EventDraft::new(
        actor,
        Id::from(ticket.clone()),
        Payload::from(TicketUpdatedPayload {
            ticket: ticket.clone(),
            fields,
        }),
    )
}

/// The outcome an auditor records for [`Store::audit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    /// `Auditing -> Closed`.
    Passed,
    /// `Auditing -> Rework`: a bounded fix by the same lineage.
    RejectedMinor,
    /// `Auditing -> Replan`: children must be regenerated.
    RejectedStructural,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tm_types::{CounterIds, FixedClock, PatternSet, Role, Tolerance};

    fn open_store() -> (TempDir, Store) {
        let dir = TempDir::new().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Store::open_with(dir.path(), clock, ids).expect("open store");
        (dir, store)
    }

    fn executor() -> ExecutorRequirements {
        ExecutorRequirements {
            role: Role::CoderFast,
            human_required: false,
            min_capability: Tolerance::Any,
        }
    }

    fn retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 1,
            backoff_multiplier: 2.0,
            max_delay_seconds: 60,
        }
    }

    fn actor() -> ParticipantId {
        ParticipantId::system()
    }

    fn create_root_ticket(store: &Store) -> TicketId {
        let events = store
            .create_ticket(
                TicketKind::Work,
                "do the thing".into(),
                None,
                None,
                Authority::root(),
                vec![],
                executor(),
                vec![],
                vec![],
                VerificationPolicy::None,
                Budget::unlimited(),
                retry(),
                0,
                actor(),
            )
            .expect("create_ticket should succeed");
        TicketId::new(events[0].subject.as_str()).expect("subject is a ticket id")
    }

    #[test]
    fn open_with_starts_with_an_empty_view() {
        let (_dir, store) = open_store();
        let view = store.view().expect("view");
        assert!(view.tickets.is_empty());
    }

    #[test]
    fn create_ticket_produces_a_draft_ticket_in_the_view() {
        let (_dir, store) = open_store();
        let ticket_id = create_root_ticket(&store);
        let view = store.view().expect("view");
        let ticket = view.tickets.get(&ticket_id).expect("ticket materialized");
        assert_eq!(ticket.state, TicketState::Draft);
        assert_eq!(ticket.objective, "do the thing");
    }

    #[test]
    fn create_ticket_rejects_a_missing_parent() {
        let (_dir, store) = open_store();
        let bogus_parent = TicketId::new("T-999").unwrap();
        let err = store
            .create_ticket(
                TicketKind::Work,
                "orphan".into(),
                Some(bogus_parent),
                None,
                Authority::none(),
                vec![],
                executor(),
                vec![],
                vec![],
                VerificationPolicy::None,
                Budget::unlimited(),
                retry(),
                0,
                actor(),
            )
            .unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }

    #[test]
    fn create_ticket_rejects_child_authority_exceeding_parent_authority() {
        let (_dir, store) = open_store();
        let parent = store
            .create_ticket(
                TicketKind::Work,
                "parent".into(),
                None,
                None,
                Authority::none(),
                vec![],
                executor(),
                vec![],
                vec![],
                VerificationPolicy::None,
                Budget::unlimited(),
                retry(),
                0,
                actor(),
            )
            .unwrap();
        let parent_id = TicketId::new(parent[0].subject.as_str()).unwrap();
        let err = store
            .create_ticket(
                TicketKind::Work,
                "child".into(),
                Some(parent_id),
                None,
                Authority::root(),
                vec![],
                executor(),
                vec![],
                vec![],
                VerificationPolicy::None,
                Budget::unlimited(),
                retry(),
                0,
                actor(),
            )
            .unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }

    #[test]
    fn transition_rejects_an_illegal_pair() {
        let (_dir, store) = open_store();
        let ticket_id = create_root_ticket(&store);
        let err = store
            .transition(&ticket_id, Trigger::Reopen, actor())
            .unwrap_err();
        assert!(matches!(err, TmError::InvalidTransition(_)));
    }

    #[test]
    fn activate_moves_a_dependency_free_ticket_all_the_way_to_ready() {
        let (_dir, store) = open_store();
        let ticket_id = create_root_ticket(&store);
        let events = store.activate(&ticket_id, actor()).expect("activate");
        assert_eq!(events.len(), 2);
        let view = store.view().expect("view");
        assert_eq!(
            view.tickets.get(&ticket_id).unwrap().state,
            TicketState::Ready
        );
    }

    #[test]
    fn add_dependency_rejects_a_hard_cycle() {
        let (_dir, store) = open_store();
        let a = create_root_ticket(&store);
        let b_events = store
            .create_ticket(
                TicketKind::Work,
                "b".into(),
                None,
                None,
                Authority::none(),
                vec![],
                executor(),
                vec![],
                vec![],
                VerificationPolicy::None,
                Budget::unlimited(),
                retry(),
                0,
                actor(),
            )
            .unwrap();
        let b = TicketId::new(b_events[0].subject.as_str()).unwrap();
        store
            .add_dependency(&a, &b, DependencyKind::Hard, actor())
            .expect("a depends on b");
        let err = store
            .add_dependency(&b, &a, DependencyKind::Hard, actor())
            .unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }

    #[test]
    fn submit_rejects_empty_evidence() {
        let (_dir, store) = open_store();
        let ticket_id = create_root_ticket(&store);
        let err = store
            .submit(&ticket_id, "done".into(), vec![], actor())
            .unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }

    #[test]
    fn acquire_lease_requires_the_ticket_to_be_ready() {
        let (_dir, store) = open_store();
        let ticket_id = create_root_ticket(&store);
        let err = store
            .acquire_lease(&ticket_id, actor(), Authority::none(), vec![], 60, actor())
            .unwrap_err();
        assert!(matches!(err, TmError::Conflict(_)));
    }

    #[test]
    fn acquire_lease_succeeds_once_ready_and_transitions_the_ticket_to_leased() {
        let (_dir, store) = open_store();
        let ticket_id = create_root_ticket(&store);
        store.activate(&ticket_id, actor()).expect("activate");
        let claim = ResourceClaim {
            paths: PatternSet::parse(["src/**"]).unwrap(),
            mode: crate::ticket::ResourceMode::Exclusive,
        };
        store
            .acquire_lease(
                &ticket_id,
                actor(),
                Authority::none(),
                vec![claim],
                60,
                actor(),
            )
            .expect("acquire lease");
        let view = store.view().expect("view");
        assert_eq!(
            view.tickets.get(&ticket_id).unwrap().state,
            TicketState::Leased
        );
        assert_eq!(view.leases.len(), 1);
    }

    /// SPEC.md §17 invariant 5: a dead worker cannot block the project.
    ///
    /// A worker that crashes never calls `record_failure`, so if the attempt were only counted
    /// on a reported failure, the lease would expire, the ticket would return to `Ready`, and
    /// the same crash could repeat forever. Counting the attempt when the lease is handed out
    /// is what bounds that loop.
    #[test]
    fn invariant_5_a_crashed_worker_spends_an_attempt() {
        let dir = TempDir::new().unwrap();
        let clock = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Store::open_with(dir.path(), clock.clone(), ids).unwrap();
        let ticket_id = create_root_ticket(&store);
        store.activate(&ticket_id, actor()).unwrap();

        let attempts = |s: &Store| s.view().unwrap().tickets.get(&ticket_id).unwrap().attempts;
        assert_eq!(attempts(&store), 0);

        for expected in 1..=3u32 {
            store
                .acquire_lease(&ticket_id, actor(), Authority::none(), vec![], 10, actor())
                .expect("acquire lease");
            assert_eq!(
                attempts(&store),
                expected,
                "handing out the work spends the attempt immediately"
            );

            // The worker dies: no heartbeat, no failure report, just silence past the TTL.
            clock.advance_seconds(11);
            store.expire_leases().expect("expire");
            assert_eq!(
                store.view().unwrap().tickets.get(&ticket_id).unwrap().state,
                TicketState::Ready,
                "the ticket returns to the ready set rather than staying stuck"
            );
            assert_eq!(
                attempts(&store),
                expected,
                "expiry must not double-count what leasing already charged"
            );
        }
    }

    /// The retry ceiling has to be reachable, or `max_attempts` is decorative.
    #[test]
    fn invariant_5_repeated_failure_escalates_instead_of_looping() {
        let (_dir, store) = open_store();
        let ticket_id = create_root_ticket(&store);
        store.activate(&ticket_id, actor()).unwrap();

        let mut escalated_after = None;
        for round in 1..=6u32 {
            store
                .acquire_lease(&ticket_id, actor(), Authority::none(), vec![], 60, actor())
                .expect("acquire lease");
            store
                .record_failure(
                    &ticket_id,
                    crate::ticket::FailureClass::ExecutorCrash,
                    "worker died".to_string(),
                    actor(),
                )
                .expect("record failure");
            let state = store.view().unwrap().tickets.get(&ticket_id).unwrap().state;
            if state == TicketState::Escalated {
                escalated_after = Some(round);
                break;
            }
            // Recovery hands the ticket back to Ready so the next attempt can be leased.
            if state == TicketState::Recovery {
                store
                    .transition(&ticket_id, Trigger::RetryScheduled, actor())
                    .expect("recovery back to ready");
            }
        }
        assert!(
            escalated_after.is_some(),
            "a ticket that keeps failing must escalate, not retry forever"
        );
    }

    // ---- Store::record_usage / Store::budget_handoff (SPEC.md §31, audit B-10) --------------

    fn tight_budget_ticket(store: &Store) -> TicketId {
        let events = store
            .create_ticket(
                TicketKind::Work,
                "do the thing".into(),
                None,
                None,
                Authority::root(),
                vec![],
                executor(),
                vec![],
                vec![],
                VerificationPolicy::None,
                Budget::new(100, u64::MAX, u64::MAX),
                retry(),
                0,
                actor(),
            )
            .expect("create_ticket should succeed");
        TicketId::new(events[0].subject.as_str()).expect("subject is a ticket id")
    }

    /// The deliberate regression test the audit calls for (`docs/audit-2026-09-18-fable.md`
    /// B-10, `SPEC.md` §31.3): a ticket whose usage crosses its own budget ceiling while
    /// `Running` must hand off cleanly — return to `Ready` with its lease released and its
    /// attempt count untouched, recording `ticket.budget_handoff` — never `Recovery`, and never
    /// `ticket.failed`/`ticket.retry_scheduled`/`ticket.escalated` alongside it.
    #[test]
    fn budget_handoff_does_not_consume_a_retry_or_enter_recovery() {
        let (_dir, store) = open_store();
        let ticket_id = tight_budget_ticket(&store);

        store.activate(&ticket_id, actor()).expect("activate");
        store
            .acquire_lease(&ticket_id, actor(), Authority::none(), vec![], 60, actor())
            .expect("acquire lease");
        store
            .transition(&ticket_id, Trigger::WorkStarted, actor())
            .expect("work started");

        let attempts_before = store
            .view()
            .unwrap()
            .tickets
            .get(&ticket_id)
            .unwrap()
            .attempts;
        assert_eq!(attempts_before, 1, "leasing already spent the one attempt");

        let err = store
            .record_usage(Some(&ticket_id), None, Spend::tokens(150), actor())
            .expect_err("spending past the ticket's own budget must be reported");
        assert!(matches!(err, TmError::BudgetExhausted(_)));

        let ticket = store
            .view()
            .unwrap()
            .tickets
            .get(&ticket_id)
            .cloned()
            .expect("ticket still exists");
        assert_eq!(
            ticket.state,
            TicketState::Ready,
            "a budget handoff returns the ticket to Ready"
        );
        assert_ne!(
            ticket.state,
            TicketState::Recovery,
            "a budget handoff must never route through Recovery"
        );
        assert_eq!(
            ticket.attempts, attempts_before,
            "a budget handoff must not consume a retry attempt"
        );
        assert!(
            store
                .view()
                .unwrap()
                .leases
                .values()
                .all(|l| l.ticket != ticket_id),
            "the lease must be released, not left dangling"
        );

        let recorded = store
            .log
            .read_subject(&Id::from(ticket_id.clone()))
            .expect("read_subject");
        assert!(
            recorded
                .iter()
                .any(|e| e.kind == tm_events::EventKind::TicketBudgetHandoff),
            "a ticket.budget_handoff event must be recorded"
        );
        assert!(
            !recorded.iter().any(|e| matches!(
                e.kind,
                tm_events::EventKind::TicketFailed
                    | tm_events::EventKind::TicketRetryScheduled
                    | tm_events::EventKind::TicketEscalated
                    | tm_events::EventKind::TicketBudgetExhausted
            )),
            "a budget handoff must never look like a failure/retry/escalation/exhaustion in the \
             log -- ticket.budget_exhausted specifically is read by tm-cli's status digest as \
             blocked work, which a clean handoff must not be"
        );
    }

    /// `SPEC.md` §31.3/§29, `docs/audit-2026-09-18-fable.md` B-10's "done looks like": a budget
    /// handoff leaves goal state exactly as it was, so the next worker that re-leases this ticket
    /// reads it back unchanged rather than restarting from nothing.
    #[test]
    fn budget_handoff_preserves_goal_state_for_the_next_worker() {
        let (_dir, store) = open_store();
        let ticket_id = tight_budget_ticket(&store);

        store.activate(&ticket_id, actor()).expect("activate");
        store
            .acquire_lease(&ticket_id, actor(), Authority::none(), vec![], 60, actor())
            .expect("acquire lease");
        store
            .transition(&ticket_id, Trigger::WorkStarted, actor())
            .expect("work started");
        store
            .set_goal(&ticket_id, "make the tests pass".to_string(), actor())
            .expect("set_goal");
        store
            .add_goal_step(
                &ticket_id,
                "step-1".to_string(),
                "write the fix".to_string(),
                actor(),
            )
            .expect("add_goal_step");

        let before = store
            .goal_state(&ticket_id)
            .expect("goal_state")
            .expect("goal exists");

        store
            .record_usage(Some(&ticket_id), None, Spend::tokens(150), actor())
            .expect_err("overspend must be reported");

        let after = store
            .goal_state(&ticket_id)
            .expect("goal_state")
            .expect("goal still exists");
        assert_eq!(
            before, after,
            "a budget handoff must leave goal state exactly as it was"
        );
    }

    /// `Store::budget_handoff` itself (`docs/audit-2026-09-18-fable.md` B-10's proactive path,
    /// used by `tm_agent::agent_loop::AgentLoop::can_afford`): callable directly, before any
    /// spend, and idempotent when the ticket has already left `Leased`/`Running`.
    #[test]
    fn budget_handoff_directly_hands_off_a_running_ticket_and_is_idempotent_after() {
        let (_dir, store) = open_store();
        let ticket_id = create_root_ticket(&store);
        store.activate(&ticket_id, actor()).expect("activate");
        store
            .acquire_lease(&ticket_id, actor(), Authority::none(), vec![], 60, actor())
            .expect("acquire lease");
        store
            .transition(&ticket_id, Trigger::WorkStarted, actor())
            .expect("work started");

        let events = store
            .budget_handoff(&ticket_id, "Tokens".to_string(), actor())
            .expect("budget_handoff");
        assert!(!events.is_empty(), "a live handoff must produce events");
        assert_eq!(
            store.view().unwrap().tickets.get(&ticket_id).unwrap().state,
            TicketState::Ready
        );

        // Calling it again on a ticket that is no longer Leased/Running is a defensive no-op,
        // not an error — a caller (e.g. a proactive `can_afford` check) may lose a race with the
        // store's own bookkeeping and must not fail the run just because of that.
        let again = store
            .budget_handoff(&ticket_id, "Tokens".to_string(), actor())
            .expect("budget_handoff is idempotent");
        assert!(again.is_empty());
    }

    /// The narrower, non-handoff-eligible case `budget_handoff_does_not_consume_a_retry_or_
    /// enter_recovery` deliberately does not cover: a ticket whose usage is recorded while
    /// `Verifying` (not `Leased`/`Running`) has no legal `Trigger::BudgetHandoff` target, so this
    /// is a genuine failure-shaped exhaustion — `ticket.budget_exhausted` is still recorded and
    /// the ticket still routes toward `Recovery`, exactly as before this track's changes.
    #[test]
    fn budget_exhaustion_outside_leased_running_still_records_budget_exhausted_and_recovery() {
        let (_dir, store) = open_store();
        let ticket_id = tight_budget_ticket(&store);

        store.activate(&ticket_id, actor()).expect("activate");
        store
            .acquire_lease(&ticket_id, actor(), Authority::none(), vec![], 60, actor())
            .expect("acquire lease");
        store
            .transition(&ticket_id, Trigger::WorkStarted, actor())
            .expect("work started");
        let artifact_events = store
            .store_artifact(
                ArtifactKind::File,
                "text/plain".into(),
                b"evidence".to_vec(),
                serde_json::json!({}),
                Some(ticket_id.clone()),
                actor(),
            )
            .expect("store_artifact");
        let artifact_id =
            ArtifactId::new(artifact_events[0].subject.as_str()).expect("artifact id");
        store
            .submit(&ticket_id, "done".to_string(), vec![artifact_id], actor())
            .expect("submit");
        store
            .transition(&ticket_id, Trigger::VerificationStarted, actor())
            .expect("verification started");
        assert_eq!(
            store.view().unwrap().tickets.get(&ticket_id).unwrap().state,
            TicketState::Verifying
        );

        let err = store
            .record_usage(Some(&ticket_id), None, Spend::tokens(150), actor())
            .expect_err("overspend must be reported");
        assert!(matches!(err, TmError::BudgetExhausted(_)));

        let ticket = store
            .view()
            .unwrap()
            .tickets
            .get(&ticket_id)
            .unwrap()
            .clone();
        assert_eq!(ticket.state, TicketState::Recovery);

        let recorded = store
            .log
            .read_subject(&Id::from(ticket_id.clone()))
            .expect("read_subject");
        assert!(
            recorded
                .iter()
                .any(|e| e.kind == tm_events::EventKind::TicketBudgetExhausted),
            "the genuine-failure path must still record ticket.budget_exhausted"
        );
        assert!(
            !recorded
                .iter()
                .any(|e| e.kind == tm_events::EventKind::TicketBudgetHandoff),
            "a non-handoff-eligible exhaustion must not also claim to be a handoff"
        );
    }

    #[test]
    fn heartbeat_rejects_an_already_expired_lease() {
        let dir = TempDir::new().unwrap();
        let clock = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Store::open_with(dir.path(), clock.clone(), ids).unwrap();
        let ticket_id = create_root_ticket(&store);
        store.activate(&ticket_id, actor()).unwrap();
        let events = store
            .acquire_lease(&ticket_id, actor(), Authority::none(), vec![], 10, actor())
            .unwrap();
        let lease_id =
            LeaseId::new(events[0].payload.as_ticket_leased().unwrap().lease.as_str()).unwrap();
        clock.advance_seconds(20);
        let err = store.heartbeat(&lease_id, actor()).unwrap_err();
        assert!(matches!(err, TmError::LeaseExpired(_)));
    }

    #[test]
    fn record_decision_then_supersede_marks_the_old_decision_inactive() {
        let (_dir, store) = open_store();
        let events = store
            .record_decision(
                "naming".into(),
                "use snake_case".into(),
                "consistency".into(),
                vec![],
                vec![],
                vec![],
                actor(),
            )
            .expect("record decision");
        let decision_id = DecisionId::new(events[0].subject.as_str()).unwrap();
        store
            .supersede(
                &decision_id,
                "naming".into(),
                "use camelCase".into(),
                "changed mind".into(),
                vec![],
                vec![],
                vec![],
                actor(),
            )
            .expect("supersede");
        let view = store.view().expect("view");
        assert!(!view.decisions.get(&decision_id).unwrap().is_active());
    }

    #[test]
    fn supersede_rejects_an_already_superseded_decision() {
        let (_dir, store) = open_store();
        let events = store
            .record_decision(
                "naming".into(),
                "use snake_case".into(),
                "consistency".into(),
                vec![],
                vec![],
                vec![],
                actor(),
            )
            .unwrap();
        let decision_id = DecisionId::new(events[0].subject.as_str()).unwrap();
        store
            .supersede(
                &decision_id,
                "naming".into(),
                "use camelCase".into(),
                "changed mind".into(),
                vec![],
                vec![],
                vec![],
                actor(),
            )
            .unwrap();
        let err = store
            .supersede(
                &decision_id,
                "naming".into(),
                "use kebab-case".into(),
                "changed again".into(),
                vec![],
                vec![],
                vec![],
                actor(),
            )
            .unwrap_err();
        assert!(matches!(err, TmError::Conflict(_)));
    }

    #[test]
    fn store_artifact_round_trips_inline_bytes_through_the_view() {
        let (_dir, store) = open_store();
        let events = store
            .store_artifact(
                ArtifactKind::File,
                "text/plain".into(),
                b"hello".to_vec(),
                serde_json::json!({}),
                None,
                actor(),
            )
            .expect("store_artifact");
        let artifact_id = ArtifactId::new(events[0].subject.as_str()).unwrap();
        let view = store.view().expect("view");
        let artifact = view
            .artifacts
            .get(&artifact_id)
            .expect("artifact materialized");
        assert_eq!(artifact.bytes_len, 5);
        assert!(matches!(artifact.storage, ArtifactStorage::Inline(_)));
    }

    #[test]
    fn attach_evidence_rejects_an_unknown_artifact() {
        let (_dir, store) = open_store();
        let ticket_id = create_root_ticket(&store);
        let bogus = ArtifactId::new("ART-000000000000").unwrap();
        let err = store
            .attach_evidence(
                &ticket_id,
                EvidenceKind::Review,
                &bogus,
                "n/a".into(),
                actor(),
            )
            .unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }

    #[test]
    fn cancel_is_gated_on_tickets_cancel_authority() {
        let (_dir, store) = open_store();
        let events = store
            .create_ticket(
                TicketKind::Work,
                "no cancel authority".into(),
                None,
                None,
                Authority::none(),
                vec![],
                executor(),
                vec![],
                vec![],
                VerificationPolicy::None,
                Budget::unlimited(),
                retry(),
                0,
                actor(),
            )
            .unwrap();
        let ticket_id = TicketId::new(events[0].subject.as_str()).unwrap();
        let err = store.cancel(&ticket_id, None, actor()).unwrap_err();
        assert!(matches!(err, TmError::AuthorityDenied(_)));
    }

    #[test]
    fn cancel_succeeds_with_root_authority() {
        let (_dir, store) = open_store();
        let ticket_id = create_root_ticket(&store);
        let events = store
            .cancel(&ticket_id, Some("no longer needed".into()), actor())
            .expect("cancel");
        assert!(!events.is_empty());
        let view = store.view().expect("view");
        assert_eq!(
            view.tickets.get(&ticket_id).unwrap().state,
            TicketState::Cancelled
        );
    }

    #[test]
    fn rebuild_reproduces_the_same_ticket_state() {
        let (_dir, store) = open_store();
        let ticket_id = create_root_ticket(&store);
        store.activate(&ticket_id, actor()).unwrap();
        store.rebuild().expect("rebuild");
        let view = store.view().expect("view after rebuild");
        assert_eq!(
            view.tickets.get(&ticket_id).unwrap().state,
            TicketState::Ready
        );
    }

    #[test]
    fn counters_reflect_allocated_ticket_ids() {
        let (_dir, store) = open_store();
        create_root_ticket(&store);
        let counters = store.counters().expect("counters");
        assert_eq!(counters.get(IdKind::Ticket.counter()), Some(&1));
    }

    #[test]
    fn transaction_rolls_back_when_the_post_write_state_violates_an_invariant() {
        let (_dir, store) = open_store();
        let ready = create_root_ticket(&store);
        store.activate(&ready, actor()).unwrap();
        let blocked_on = create_root_ticket(&store); // stays Draft: never activated

        let err = store
            .transaction(|tx| {
                tx.raw()
                    .execute(
                        "INSERT INTO ticket_deps (ticket, depends_on, kind)
                         VALUES (?1, ?2, '\"hard\"')",
                        rusqlite::params![ready.as_str(), blocked_on.as_str()],
                    )
                    .unwrap();
                Ok(())
            })
            .unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));

        let view = store.view().expect("view");
        assert!(
            view.graph.edges().is_empty(),
            "the dependency edge must not survive a rolled-back transaction"
        );
    }

    /// Row counts for a couple of the B-05 tables, read off a raw connection (no public `Store`
    /// read path exists for them yet — out of B-05's scope).
    fn session_and_doc_row_counts(db_path: &std::path::Path) -> (i64, i64) {
        let conn = Connection::open(db_path).expect("raw connection");
        let sessions = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .expect("count sessions");
        let docs = conn
            .query_row("SELECT COUNT(*) FROM docs", [], |r| r.get(0))
            .expect("count docs");
        (sessions, docs)
    }

    #[test]
    fn transaction_commits_heterogeneous_writes_together() {
        let (dir, store) = open_store();
        let alice = ParticipantId::new("human:alice").unwrap();

        store
            .transaction(|tx| {
                let session = SessionId::new("S-1").unwrap();
                tx.append(EventDraft::new(
                    actor(),
                    Id::from(session.clone()),
                    Payload::from(SessionStartedPayload {
                        session,
                        participant: alice.clone(),
                    }),
                ))?;
                tx.append(EventDraft::new(
                    actor(),
                    Id::new("docs/a.md"),
                    Payload::from(DocRegisteredPayload {
                        path: "docs/a.md".into(),
                        ticket: None,
                    }),
                ))?;
                Ok(())
            })
            .expect("transaction");

        let db_path = dir.path().join(".tm").join("project.db");
        assert_eq!(session_and_doc_row_counts(&db_path), (1, 1));
    }

    #[test]
    fn transaction_rolls_back_heterogeneous_writes_together_when_the_closure_errs() {
        let (dir, store) = open_store();
        let alice = ParticipantId::new("human:alice").unwrap();

        let err = store
            .transaction(|tx| {
                let session = SessionId::new("S-1").unwrap();
                tx.append(EventDraft::new(
                    actor(),
                    Id::from(session.clone()),
                    Payload::from(SessionStartedPayload {
                        session,
                        participant: alice.clone(),
                    }),
                ))?;
                tx.append(EventDraft::new(
                    actor(),
                    Id::new("docs/a.md"),
                    Payload::from(DocRegisteredPayload {
                        path: "docs/a.md".into(),
                        ticket: None,
                    }),
                ))?;
                Err::<(), TmError>(TmError::invariant("synthetic failure after both writes"))
            })
            .unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));

        let db_path = dir.path().join(".tm").join("project.db");
        assert_eq!(
            session_and_doc_row_counts(&db_path),
            (0, 0),
            "neither write should survive since the closure itself returned Err"
        );
    }

    #[test]
    fn append_materializes_and_commits_an_arbitrary_typed_draft() {
        let (_dir, store) = open_store();
        let events = store
            .append(vec![EventDraft::new(
                actor(),
                Id::none(),
                Payload::from(tm_events::payload::ProjectAttachedPayload {
                    name: "demo".into(),
                    root: "/tmp/demo".into(),
                }),
            )])
            .expect("append");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, tm_events::EventKind::ProjectAttached);
    }

    #[test]
    fn start_session_then_join_and_end_round_trip_through_rebuild() {
        let (dir, store) = open_store();
        let alice = ParticipantId::new("human:alice").unwrap();
        let bob = ParticipantId::new("agent:mock/bob").unwrap();

        let started = store
            .start_session(alice.clone(), actor())
            .expect("start_session");
        let session = started[0]
            .payload
            .as_session_started()
            .expect("session.started payload")
            .session
            .clone();

        store
            .join_session(&session, bob.clone(), actor())
            .expect("join_session");
        store.end_session(&session, actor()).expect("end_session");

        // No public read path exists for `sessions`/`participants` yet (out of B-05's scope), so
        // this reaches for a raw connection the same way `tm-e2e`'s replay fixture does.
        let db_path = dir.path().join(".tm").join("project.db");
        let session_row = |conn: &Connection| -> (String, String) {
            conn.query_row(
                "SELECT id, participant FROM sessions WHERE id = ?1",
                rusqlite::params![session.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("sessions row")
        };
        let bob_status = |conn: &Connection| -> String {
            conn.query_row(
                "SELECT status FROM participants WHERE id = ?1",
                rusqlite::params![bob.as_str()],
                |r| r.get(0),
            )
            .expect("participants row")
        };

        let before_conn = Connection::open(&db_path).expect("raw connection before rebuild");
        let before_session = session_row(&before_conn);
        assert_eq!(before_session.0, session.as_str());
        assert_eq!(before_session.1, alice.as_str());
        assert_eq!(bob_status(&before_conn), "active");

        store.rebuild().expect("rebuild");

        let after_conn = Connection::open(&db_path).expect("raw connection after rebuild");
        assert_eq!(
            session_row(&after_conn),
            before_session,
            "the sessions row must survive rebuild byte-identical"
        );
        assert_eq!(bob_status(&after_conn), "active");
    }

    #[test]
    fn register_doc_then_invalidate_and_reconcile_do_not_error() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);
        store
            .register_doc("docs/architecture.md".into(), Some(ticket), actor())
            .expect("register_doc");
        store
            .invalidate_doc(
                "docs/architecture.md".into(),
                "source moved".into(),
                actor(),
            )
            .expect("invalidate_doc");
        store
            .reconcile_doc("docs/architecture.md".into(), actor())
            .expect("reconcile_doc");
    }

    #[test]
    fn promote_epoch_appends_a_harness_promoted_event() {
        let (_dir, store) = open_store();
        let events = store
            .promote_epoch("config-a".into(), actor())
            .expect("promote_epoch");
        assert_eq!(
            events[0].payload.as_harness_promoted().unwrap().candidate,
            "config-a"
        );
    }

    #[test]
    fn link_mirror_then_update_mirror_link_do_not_error() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);
        store
            .link_mirror(&ticket, "github".into(), actor())
            .expect("link_mirror");
        store
            .update_mirror_link(
                &ticket,
                "github".into(),
                "owner/repo#1".into(),
                MirrorSyncDirection::Push,
                actor(),
            )
            .expect("update_mirror_link push");
        store
            .update_mirror_link(
                &ticket,
                "github".into(),
                "owner/repo#1".into(),
                MirrorSyncDirection::Pull,
                actor(),
            )
            .expect("update_mirror_link pull");
    }

    #[test]
    fn record_command_appends_started_and_completed_events() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);
        let events = store
            .record_command("cargo test".into(), Some(ticket), None, 0, 1234, actor())
            .expect("record_command");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, tm_events::EventKind::CommandStarted);
        assert_eq!(events[1].kind, tm_events::EventKind::CommandCompleted);
    }

    // ---- Store::begin_effect / EffectGuard (SPEC.md §21.5, audit B-11) --------------------

    #[test]
    fn begin_effect_on_an_unseen_key_journals_and_returns_a_fresh_guard() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);
        let key = EffectKey::compute(&ticket, 0, "git.push", "abc");

        let guard = store
            .begin_effect(key.clone(), ticket.clone(), 0, "git.push", actor())
            .expect("begin_effect");

        assert!(!guard.already_completed());
        assert!(!guard.resumed());
        assert_eq!(guard.key(), &key);

        let row = store
            .effect_status(&key)
            .expect("effect_status")
            .expect("row exists");
        assert_eq!(row.status, EffectStatus::Journaled);
        assert_eq!(row.ticket, ticket);
        assert_eq!(row.attempt, 0);
        assert_eq!(row.kind, "git.push");
        assert!(row.receipt_artifact.is_none());
        assert!(row.completed.is_none());
    }

    #[test]
    fn complete_marks_the_row_completed_with_the_receipt() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);
        let key = EffectKey::compute(&ticket, 0, "git.push", "abc");

        let guard = store
            .begin_effect(key.clone(), ticket.clone(), 0, "git.push", actor())
            .expect("begin_effect");
        guard
            .complete(&store, Some("refs/heads/main@deadbeef"))
            .expect("complete");

        let row = store
            .effect_status(&key)
            .expect("effect_status")
            .expect("row exists");
        assert_eq!(row.status, EffectStatus::Completed);
        assert_eq!(
            row.receipt_artifact.as_deref(),
            Some("refs/heads/main@deadbeef")
        );
        assert!(row.completed.is_some());
    }

    #[test]
    fn begin_effect_on_a_completed_key_does_not_repeat_and_returns_the_prior_receipt() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);
        let key = EffectKey::compute(&ticket, 0, "git.push", "abc");

        let first = store
            .begin_effect(key.clone(), ticket.clone(), 0, "git.push", actor())
            .expect("begin_effect first");
        first.complete(&store, Some("receipt-1")).expect("complete");

        // A second `begin_effect` for the *exact same key* (same ticket/attempt/kind/args) must
        // not journal a new attempt -- this is the actual idempotency guarantee B-11 asks for.
        let second = store
            .begin_effect(key.clone(), ticket.clone(), 0, "git.push", actor())
            .expect("begin_effect second");
        assert!(second.already_completed());
        assert!(second.resumed());
        assert_eq!(second.prior_receipt(), Some("receipt-1"));

        // Calling `complete` again on the second guard must be a harmless no-op, not overwrite
        // the original receipt.
        second
            .complete(&store, Some("receipt-2-should-be-ignored"))
            .expect("complete is a no-op on an already-completed guard");
        let row = store
            .effect_status(&key)
            .expect("effect_status")
            .expect("row exists");
        assert_eq!(row.receipt_artifact.as_deref(), Some("receipt-1"));
    }

    #[test]
    fn begin_effect_on_a_journaled_but_uncompleted_key_reports_resumed_without_re_journaling() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);
        let key = EffectKey::compute(&ticket, 0, "git.push", "abc");

        // Simulates a crash: the effect was journaled but the process died before `complete` was
        // ever called.
        let first = store
            .begin_effect(key.clone(), ticket.clone(), 0, "git.push", actor())
            .expect("begin_effect first");
        drop(first); // never completed

        let resumed_guard = store
            .begin_effect(key.clone(), ticket.clone(), 0, "git.push", actor())
            .expect("begin_effect on resume");
        assert!(!resumed_guard.already_completed());
        assert!(resumed_guard.resumed());
        assert_eq!(resumed_guard.prior_receipt(), None);

        // Still only one `effects` row for this key -- re-beginning does not fork the journal.
        let view_conn =
            tm_events::schema::open_read_connection(store.log.path()).expect("read connection");
        let count: i64 = view_conn
            .query_row(
                "SELECT COUNT(*) FROM effects WHERE key = ?1",
                rusqlite::params![key.as_str()],
                |r| r.get(0),
            )
            .expect("count effects rows");
        assert_eq!(count, 1);
    }

    #[test]
    fn different_ticket_attempt_kind_or_args_produce_independent_effects() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);

        let key_a = EffectKey::compute(&ticket, 0, "git.push", "abc");
        let key_b = EffectKey::compute(&ticket, 1, "git.push", "abc"); // different attempt
        assert_ne!(key_a, key_b);

        store
            .begin_effect(key_a.clone(), ticket.clone(), 0, "git.push", actor())
            .expect("begin_effect a")
            .complete(&store, Some("receipt-a"))
            .expect("complete a");

        // A different attempt of the *same* ticket/kind/args is a wholly independent effect: it
        // must not see attempt 0's completion.
        let guard_b = store
            .begin_effect(key_b.clone(), ticket.clone(), 1, "git.push", actor())
            .expect("begin_effect b");
        assert!(!guard_b.already_completed());
        assert!(!guard_b.resumed());
    }

    #[test]
    fn fail_marks_the_row_failed_and_a_later_begin_effect_reports_resumed() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);
        let key = EffectKey::compute(&ticket, 0, "mirror.push:github", "hash-1");

        let guard = store
            .begin_effect(
                key.clone(),
                ticket.clone(),
                0,
                "mirror.push:github",
                actor(),
            )
            .expect("begin_effect");
        guard.fail(&store, "github returned 503").expect("fail");

        let row = store
            .effect_status(&key)
            .expect("effect_status")
            .expect("row exists");
        assert_eq!(row.status, EffectStatus::Failed);

        // Failed is not terminal like Completed: a later attempt at the identical effect is
        // allowed to retry.
        let retry_guard = store
            .begin_effect(
                key.clone(),
                ticket.clone(),
                0,
                "mirror.push:github",
                actor(),
            )
            .expect("begin_effect retry");
        assert!(!retry_guard.already_completed());
        assert!(retry_guard.resumed());
    }

    #[test]
    fn effect_status_survives_rebuild() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);
        let key = EffectKey::compute(&ticket, 0, "git.push", "abc");

        store
            .begin_effect(key.clone(), ticket.clone(), 0, "git.push", actor())
            .expect("begin_effect")
            .complete(&store, Some("receipt-1"))
            .expect("complete");

        store.rebuild().expect("rebuild");

        let row = store
            .effect_status(&key)
            .expect("effect_status after rebuild")
            .expect("row survives rebuild");
        assert_eq!(row.status, EffectStatus::Completed);
        assert_eq!(row.receipt_artifact.as_deref(), Some("receipt-1"));
    }

    #[test]
    fn effect_status_returns_none_for_an_unknown_key() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);
        let key = EffectKey::compute(&ticket, 0, "git.push", "never-begun");
        assert!(store.effect_status(&key).expect("effect_status").is_none());
    }

    // ---- goal loop (SPEC.md §29, audit B-09) ----

    #[test]
    fn goal_state_is_none_before_any_goal_set() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);
        assert!(store.goal_state(&ticket).expect("goal_state").is_none());
    }

    #[test]
    fn set_goal_then_add_and_complete_steps_round_trips_through_goal_state() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);

        store
            .set_goal(&ticket, "ship the feature".into(), actor())
            .expect("set_goal");
        store
            .add_goal_step(&ticket, "step-1".into(), "read the code".into(), actor())
            .expect("add_goal_step");
        store
            .add_goal_step(&ticket, "step-2".into(), "write the patch".into(), actor())
            .expect("add_goal_step");
        store
            .complete_goal_step(&ticket, "step-1".into(), actor())
            .expect("complete_goal_step");

        let state = store
            .goal_state(&ticket)
            .expect("goal_state")
            .expect("goal exists");
        assert_eq!(state.text, "ship the feature");
        assert_eq!(state.steps.len(), 2);
        assert!(state.steps[0].done);
        assert!(!state.steps[1].done);
        assert!(!state.claimed_complete);
    }

    #[test]
    fn reorient_goal_and_claim_complete_update_goal_state() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);

        store
            .set_goal(&ticket, "ship the feature".into(), actor())
            .expect("set_goal");
        store
            .reorient_goal(&ticket, 2, actor())
            .expect("reorient_goal");
        store
            .claim_goal_complete(&ticket, "shipped it".into(), actor())
            .expect("claim_goal_complete");

        let state = store
            .goal_state(&ticket)
            .expect("goal_state")
            .expect("goal exists");
        assert_eq!(state.last_reoriented_step, 2);
        assert!(state.claimed_complete);
    }

    #[test]
    fn goal_state_survives_rebuild() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);

        store
            .set_goal(&ticket, "ship the feature".into(), actor())
            .expect("set_goal");
        store
            .add_goal_step(&ticket, "step-1".into(), "read the code".into(), actor())
            .expect("add_goal_step");

        store.rebuild().expect("rebuild");

        let state = store
            .goal_state(&ticket)
            .expect("goal_state after rebuild")
            .expect("goal survives rebuild");
        assert_eq!(state.text, "ship the feature");
        assert_eq!(state.steps.len(), 1);
    }

    #[test]
    fn event_count_for_counts_every_event_recorded_against_a_subject() {
        let (_dir, store) = open_store();
        let ticket = create_root_ticket(&store);
        let subject = Id::from(ticket.clone());

        let before = store.event_count_for(&subject).expect("event_count_for");
        store
            .set_goal(&ticket, "ship the feature".into(), actor())
            .expect("set_goal");
        store
            .add_goal_step(&ticket, "step-1".into(), "read the code".into(), actor())
            .expect("add_goal_step");
        let after = store.event_count_for(&subject).expect("event_count_for");

        assert_eq!(after, before + 2);
    }

    #[test]
    fn event_count_for_an_unknown_subject_is_zero() {
        let (_dir, store) = open_store();
        let subject = Id::new("T-999999");
        assert_eq!(store.event_count_for(&subject).expect("event_count_for"), 0);
    }
}
