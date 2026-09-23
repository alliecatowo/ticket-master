//! `Store`: the facade every other crate calls into.
//!
//! Owns the open `project.db` at this project's state directory (`<root>/.tm/project.db` in repo
//! scope, `$TM_HOME/projects/<key>/project.db` in global scope -- see D-003) via
//! [`tm_events::EventLog`], the injected [`Clock`]/
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
    TicketDependencyAddedPayload, TicketEscalatedPayload, TicketFailedPayload, TicketForkedPayload,
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
    ContextRef, DependencyKind, ExecutorRequirements, FailureClass, FailureRecord, ResourceClaim,
    RetryPolicy, Ticket, TicketKind, TicketState, Trigger, VerificationPolicy,
};
use crate::view::{ParticipantSummary, ProjectView, SchedulerView};

/// The project-state facade. One `Store` per open project.
pub struct Store {
    log: EventLog,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
    /// The project's state directory (`<root>/.tm` for a repo-scoped project, or
    /// `$TM_HOME/projects/<key>/` for a global-scope one), needed for artifact on-disk
    /// placement ([`crate::artifact::plan_storage`]).
    state_dir: PathBuf,
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
    ///
    /// `draft.payload` is redacted for secret-shaped substrings (`tm_auth::redact_json`) before
    /// the hash chain is computed or anything reaches SQLite -- this is the single real choke
    /// point every event in the whole system passes through (`Store::append`/`append_all` and
    /// every typed convenience method on [`Store`] all funnel through here), so it is where
    /// `docs/audit-2026-09-18-fable.md`'s "M-04" wires the event half of its durable-persistence
    /// redaction guarantee (`Store::store_artifact` is the artifact half; `Fabric::execute` in
    /// `crates/tm-provider/src/fabric.rs` is the outbound-request half). Redacting before hashing
    /// keeps the persisted hash chain consistent with what is actually stored; redacting before
    /// [`crate::materialize::apply`] keeps the in-memory [`crate::view::ProjectView`] a caller
    /// reads back from never holding the plaintext either.
    pub fn append(&self, draft: EventDraft) -> tm_types::Result<Event> {
        let draft = redact_event_draft(draft)?;
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

/// Read `ticket`'s materialized `goals` row off `conn` (any connection carrying this crate's
/// schema — a live project's own, or the scratch schema
/// [`Store::ticket_and_goal_as_of`] replays into), or `None` if no `goal.set` has ever been
/// recorded for it there. The one read path [`Store::goal_state`] and
/// [`Store::ticket_and_goal_as_of`] share, so "current goal state" and "goal state as of a
/// bounded replay" are read identically.
fn read_goal_state(conn: &Connection, ticket: &TicketId) -> tm_types::Result<Option<GoalState>> {
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

fn state_str(state: TicketState) -> String {
    match serde_json::to_value(state) {
        Ok(serde_json::Value::String(s)) => s,
        _ => format!("{state:?}"),
    }
}

/// The reason a [`Store::reject`] of `ticket` records: `reason` trimmed, refused when nothing is
/// left. The next attempt reads it as what was wrong, so a rejection without one would send the
/// worker back blind. Every surface (CLI, TUI, `tm serve`) goes through `Store::reject`, so this
/// is the one rule; `tm serve` also calls it up front to answer 400 instead of 409.
///
/// # Errors
/// [`TmError::InvalidTransition`] when `reason` is empty or only whitespace.
pub fn rejection_reason(ticket: &TicketId, reason: &str) -> tm_types::Result<String> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(TmError::InvalidTransition(format!(
            "Rejecting ticket {ticket} needs a reason: describe what was wrong so the next attempt knows what to fix"
        )));
    }
    Ok(reason.to_string())
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

/// [`StoreTx::append`]'s redaction step: round-trip `draft.payload` through JSON, scrubbing
/// secret-shaped substrings via `tm_auth::redact_json`, and reconstruct the typed payload from
/// the result. A payload whose redaction touched a structured (non-freeform) field badly enough
/// that it no longer deserializes into its own typed shape fails the append with a `TmError`
/// rather than silently persisting a corrupted event; this module's pattern set is deliberately
/// conservative (a fixed provider-key prefix, or an explicit `key`/`token`/`secret`/`password`
/// label) specifically to keep this a theoretical rather than a practical risk — see
/// `docs/decisions/D-011-secret-redaction.md` for the fuller tradeoff.
fn redact_event_draft(mut draft: EventDraft) -> tm_types::Result<EventDraft> {
    let kind = draft.payload.kind();
    let json = draft.payload.to_json()?;
    let redacted = tm_auth::redact_json(&json);
    // `serde_json::to_value`/`from_value` is not a guaranteed byte-identity round-trip for every
    // payload shape (map key order, number representation) -- skip reconstructing the typed
    // payload entirely when nothing actually matched, so the overwhelming majority of appends
    // (no secret-shaped content at all) never risk a round-trip discrepancy in the first place.
    // See `event_payload_json_round_trip_is_exact_for_a_value_bearing_payload_when_nothing_matches`
    // and `docs/decisions/D-011-secret-redaction.md` for why this matters specifically for a
    // payload carrying an open-ended `serde_json::Value` field (`fields`/`from`/`to`).
    if redacted == json {
        return Ok(draft);
    }
    draft.payload = Payload::from_json(kind, redacted)?;
    Ok(draft)
}

/// [`Store::store_artifact`]'s redaction step: scrub secret-shaped substrings (`tm_auth::redact`)
/// out of `bytes` if (and only if) they decode as UTF-8 text. Bytes that fail to decode are a
/// real binary artifact and are returned untouched -- scanning/rewriting them would risk
/// corrupting content this module has no way to safely reinterpret.
fn redact_artifact_bytes(bytes: Vec<u8>) -> Vec<u8> {
    match std::str::from_utf8(&bytes) {
        Ok(text) => {
            let redacted = tm_auth::redact(text);
            if redacted == text {
                bytes
            } else {
                redacted.into_bytes()
            }
        }
        Err(_) => bytes,
    }
}

impl Store {
    /// Open (creating if absent) the project rooted at `project_root`, using the real wall clock
    /// and a fresh [`tm_types::CounterIds`] restored from persisted counters. Prefer
    /// [`Store::open_with`] in tests for deterministic clock/id injection.
    ///
    /// Shim over [`Store::open_at`] for the repo-scoped layout (`<project_root>/.tm`); prefer
    /// [`Store::open_at`] when the caller already knows the state directory (e.g. a global-scope
    /// project under `$TM_HOME/projects/<key>/`).
    pub fn open(project_root: &Path) -> tm_types::Result<Self> {
        Store::open_at(&project_root.join(".tm"))
    }

    /// Open (creating if absent) the project state directory at `state_dir`, using the real wall
    /// clock and a fresh [`tm_types::CounterIds`] restored from persisted counters. Prefer
    /// [`Store::open_with_at`] in tests for deterministic clock/id injection.
    pub fn open_at(state_dir: &Path) -> tm_types::Result<Self> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let db_path = state_dir.join("project.db");
        std::fs::create_dir_all(state_dir)?;
        let counters = {
            let mut conn = Connection::open(&db_path).map_err(storage_err)?;
            crate::schema::migrate(&mut conn, clock.as_ref())?;
            Self::read_counters(&conn)?
        };
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::with_counters(counters, 0));
        Store::open_with_at(state_dir, clock, ids)
    }

    /// Open with an injected clock and id source, the constructor tests and deterministic
    /// callers use.
    ///
    /// Shim over [`Store::open_with_at`] for the repo-scoped layout (`<project_root>/.tm`).
    pub fn open_with(
        project_root: &Path,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
    ) -> tm_types::Result<Self> {
        Store::open_with_at(&project_root.join(".tm"), clock, ids)
    }

    /// Open with an injected clock and id source at an explicit state directory (the directory
    /// that will hold `project.db` and `artifacts/`, not the workspace root).
    pub fn open_with_at(
        state_dir: &Path,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
    ) -> tm_types::Result<Self> {
        let db_path = state_dir.join("project.db");
        std::fs::create_dir_all(state_dir)?;
        let log = EventLog::open_with_clock(&db_path, clock.clone())?;
        {
            let mut conn = Connection::open(&db_path).map_err(storage_err)?;
            crate::schema::migrate(&mut conn, clock.as_ref())?;
        }
        Ok(Store {
            log,
            clock,
            ids,
            state_dir: state_dir.to_path_buf(),
        })
    }

    /// The directory holding this project's `project.db` and `artifacts/` (not necessarily the
    /// workspace root — see the `state_dir` vs. `root` distinction in D-003).
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
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
                        "This child ticket asks for more authority than its parent, ticket {parent_id}, has"
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
                    "Ticket {ticket} can't depend on {depends_on}: that would create a dependency cycle"
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
                "Submitting a ticket needs at least one piece of evidence",
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
                    "The auditor must be someone other than the worker who submitted this ticket",
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
    /// separate per-participant authority store), which answers "may this ticket's worker do
    /// this". A human participant is the project's owner, not the ticket's worker, and is not
    /// bound by it; an agent always is.
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
            if !actor.is_human() && !t.authority.tickets.close {
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

    /// A human accepts a submission as done: `Submitted -> Verifying -> Auditing -> Closed` in one
    /// atomic command, recorded as verified and audited by that human. This is the terminal path
    /// for submitted work when no verification ticket is in play, and it is deliberately
    /// human-only: an agent never certifies work (`SPEC.md` §11, "verification separation").
    pub fn accept(
        &self,
        ticket: &TicketId,
        note: Option<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        if !actor.is_human() {
            return Err(TmError::AuthorityDenied(format!(
                "{actor} cannot accept a submission: only a human can certify work directly"
            )));
        }
        let ticket = ticket.clone();
        self.run_command(move |view| {
            let t = view
                .tickets
                .get(&ticket)
                .ok_or_else(|| TmError::not_found("ticket", &ticket))?;
            let step = |from, trigger| {
                machine::transition(from, trigger)
                    .map_err(|e| TmError::InvalidTransition(e.to_string()))
            };
            let verifying = step(t.state, Trigger::VerificationStarted)?;
            let auditing = step(verifying, Trigger::VerificationPassed)?;
            let closed = step(auditing, Trigger::AuditPassed)?;
            Ok(vec![
                state_changed_draft(&ticket, t.state, verifying, actor.clone()),
                state_changed_draft(&ticket, verifying, auditing, actor.clone()),
                EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketVerifiedPayload {
                        ticket: ticket.clone(),
                        verifier: ticket.clone(),
                    }),
                ),
                state_changed_draft(&ticket, auditing, closed, actor.clone()),
                EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketAuditedPayload {
                        ticket: ticket.clone(),
                        auditor: ticket.clone(),
                    }),
                ),
                EventDraft::new(
                    actor,
                    Id::from(ticket.clone()),
                    Payload::from(TicketClosedPayload {
                        ticket: ticket.clone(),
                        reason: note.clone(),
                    }),
                ),
            ])
        })
    }

    /// A human rejects a submission: `Submitted -> Verifying -> Recovery`, recorded as a failed
    /// verification carrying `reason`, then the ticket's retry policy decides between another
    /// attempt (`-> Ready`) and escalation, exactly as for any other failure
    /// ([`Store::record_failure`]). The next attempt's context includes `reason` among the
    /// ticket's prior failures, so the worker sees what was wrong. The reason is required: an
    /// empty or whitespace-only one is refused ([`rejection_reason`]), and it is stored trimmed.
    pub fn reject(
        &self,
        ticket: &TicketId,
        reason: String,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        if !actor.is_human() {
            return Err(TmError::AuthorityDenied(format!(
                "{actor} cannot reject a submission directly: only a human can"
            )));
        }
        let reason = rejection_reason(ticket, &reason)?;
        let id = ticket.clone();
        let failure_reason = reason.clone();
        let mut events = self.run_command(move |view| {
            let t = view
                .tickets
                .get(&id)
                .ok_or_else(|| TmError::not_found("ticket", &id))?;
            let step = |from, trigger| {
                machine::transition(from, trigger)
                    .map_err(|e| TmError::InvalidTransition(e.to_string()))
            };
            let verifying = step(t.state, Trigger::VerificationStarted)?;
            let recovery = step(verifying, Trigger::VerificationFailed)?;
            Ok(vec![
                state_changed_draft(&id, t.state, verifying, actor.clone()),
                state_changed_draft(&id, verifying, recovery, actor.clone()),
                EventDraft::new(
                    actor.clone(),
                    Id::from(id.clone()),
                    Payload::from(TicketVerificationFailedPayload {
                        ticket: id.clone(),
                        verifier: id.clone(),
                        reason: reason.clone(),
                    }),
                ),
            ])
        })?;
        events.extend(self.record_failure(
            ticket,
            FailureClass::VerificationFailed,
            failure_reason,
            ParticipantId::system(),
        )?);
        Ok(events)
    }

    /// A human sends an escalated ticket back to work (`Escalated -> Blocked`, then `-> Ready`
    /// when its dependencies allow, as [`Store::activate`] does), with a fresh round of the
    /// attempts its retry policy allows. `guidance`, when given, is appended to the objective,
    /// the brief every attempt reads, so the next worker gets what the human said. Human-only,
    /// like [`Store::accept`]: escalation exists to reach a person.
    pub fn retry(
        &self,
        ticket: &TicketId,
        guidance: Option<String>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        if !actor.is_human() {
            return Err(TmError::AuthorityDenied(format!(
                "{actor} cannot retry an escalated ticket: escalation waits for a human"
            )));
        }
        let ticket = ticket.clone();
        self.run_command(move |view| {
            let t = view
                .tickets
                .get(&ticket)
                .ok_or_else(|| TmError::not_found("ticket", &ticket))?;
            if t.state != TicketState::Escalated {
                let state = format!("{:?}", t.state).to_ascii_lowercase();
                let instead = match t.state {
                    TicketState::Draft => " (activating it queues a draft)",
                    TicketState::Submitted => " (accept or reject a submission)",
                    _ => "",
                };
                return Err(TmError::InvalidTransition(format!(
                    "{ticket} is {state}; only an escalated ticket can be retried{instead}"
                )));
            }
            let blocked = machine::transition(t.state, Trigger::EscalationResolved)
                .map_err(|e| TmError::InvalidTransition(e.to_string()))?;
            let retry = RetryPolicy {
                max_attempts: t.attempts + t.retry.max_attempts.max(1),
                ..t.retry
            };
            let mut fields = serde_json::json!({ "retry": retry });
            if let Some(guidance) = guidance.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
                fields["objective"] = serde_json::json!(format!(
                    "{}\n\nFrom the user, after attempt {}: {guidance}",
                    t.objective, t.attempts
                ));
            }
            let mut drafts = vec![
                EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketUpdatedPayload {
                        ticket: ticket.clone(),
                        fields,
                    }),
                ),
                state_changed_draft(&ticket, t.state, blocked, actor.clone()),
            ];
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
                drafts.push(state_changed_draft(&ticket, blocked, ready, actor));
            }
            Ok(drafts)
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
            if !actor.is_human() && !t.authority.tickets.cancel {
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
            // The failure also joins the ticket's own history (`Ticket::failures`), which is what
            // the next attempt's context pack shows its worker ("prior failures") and what
            // `tm run` reports; `ticket.failed` alone only carries the reason for the log.
            let mut failures = t.failures.clone();
            failures.push(FailureRecord {
                class,
                detail: detail.clone(),
                at: self.clock.now(),
                attempt,
            });
            let mut drafts = vec![
                EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketFailedPayload {
                        ticket: ticket.clone(),
                        reason: detail.clone(),
                    }),
                ),
                EventDraft::new(
                    actor.clone(),
                    Id::from(ticket.clone()),
                    Payload::from(TicketUpdatedPayload {
                        ticket: ticket.clone(),
                        fields: serde_json::json!({ "failures": failures }),
                    }),
                ),
            ];
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
                    "Decision {supersedes} has already been superseded"
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
    ///
    /// `bytes` and `meta` are redacted for secret-shaped substrings (`tm_auth::redact`) before
    /// anything is hashed or written -- this is the one real choke point every artifact's bytes
    /// pass through regardless of `kind`, so it is where `docs/audit-2026-09-18-fable.md`'s
    /// "M-04" wires the durable-persistence half of its redaction guarantee (`Fabric::execute`,
    /// `crates/tm-provider/src/fabric.rs`, is the outbound-request half). Redaction happens
    /// before [`crate::artifact::plan_storage`] so the recorded `hash` matches the bytes that
    /// actually land on disk/in SQLite, not the pre-redaction bytes the caller passed in. `bytes`
    /// that do not decode as UTF-8 (a real binary artifact) are left untouched rather than risk
    /// corrupting them -- see `tm_auth::redact`'s docs for why this is safe: the pattern set is
    /// conservative enough that skipping non-text bytes costs no real coverage.
    pub fn store_artifact(
        &self,
        kind: ArtifactKind,
        media_type: String,
        bytes: Vec<u8>,
        meta: serde_json::Value,
        ticket: Option<TicketId>,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>> {
        let bytes = redact_artifact_bytes(bytes);
        let meta = tm_auth::redact_json(&meta);
        let id = ArtifactId::new(self.ids.next(IdKind::Artifact).as_str())?;
        let (hash, storage) = crate::artifact::plan_storage(&self.state_dir, &bytes);
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
    ///
    /// Applies the on-disk artifact read fallback: an [`ArtifactStorage::OnDisk`] path recorded
    /// at write time that no longer exists (e.g. because the state directory was copied
    /// elsewhere, as `tm init`'s promotion does) is re-resolved to
    /// `<state_dir>/artifacts/<hash>` before being handed back. This never rewrites what was
    /// persisted to the event log or the `artifacts` table — see this module's top-level note.
    pub fn view(&self) -> tm_types::Result<ProjectView> {
        let conn = tm_events::schema::open_read_connection(self.log.path())?;
        let mut view = Self::read_view(&conn)?;
        for artifact in view.artifacts.values_mut() {
            if let ArtifactStorage::OnDisk(path) = &artifact.storage {
                if !path.exists() {
                    artifact.storage = ArtifactStorage::OnDisk(
                        self.state_dir.join("artifacts").join(&artifact.hash),
                    );
                }
            }
        }
        Ok(view)
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
        read_goal_state(&conn, ticket)
    }

    /// Compute `source`'s [`Ticket`] row and [`GoalState`] (if any) exactly as they stood after
    /// replaying this project's own log through `seq` (inclusive) — the bounded-replay primitive
    /// [`Store::fork_ticket`] is built on (`docs/decisions/D-008-ticket-checkpoint-fork.md`).
    ///
    /// Replays `[1, seq]` into a throwaway, file-backed scratch schema (the same technique
    /// `crate::materialize`'s own tests use to exercise `apply` against a real schema without a
    /// live project) via [`crate::materialize::replay`] — the identical function both the live
    /// append path and [`Store::rebuild`]'s full-log replay go through — so "state as of seq" is
    /// derived by the same mechanism as "current state," just bounded. Never touches this
    /// project's own `project.db` or its materialized tables; the scratch file is discarded when
    /// this call returns.
    ///
    /// Must be called *before* opening any [`Store::transaction`]/[`Store::run_command`] on
    /// `self`, never from inside one: [`tm_events::EventLog`] serializes writers with one
    /// non-reentrant lock held for a transaction's whole lifetime, and this reads `self.log` via
    /// a fresh connection, which is safe on its own but would deadlock if nested inside a write
    /// this same `Store` already holds open.
    ///
    /// # Errors
    /// `TmError::invariant` if `seq` is `0` or exceeds the log's current head — a caller asking
    /// to fork from a point that cannot exist, rather than being silently clamped to whatever the
    /// log's actual extent happens to be.
    fn ticket_and_goal_as_of(
        &self,
        ticket: &TicketId,
        seq: u64,
    ) -> tm_types::Result<(Option<Ticket>, Option<GoalState>)> {
        let head = self.log.head()?;
        if seq == 0 || seq > head {
            return Err(TmError::invariant(format!(
                "Can't fork at step {seq}: this ticket's history only has {head} steps so far"
            )));
        }
        let events = self.log.read_range(1, seq)?;

        let scratch_file =
            tempfile::NamedTempFile::new().map_err(|e| TmError::storage(e.to_string()))?;
        let scratch_log = EventLog::open_with_clock(scratch_file.path(), Arc::clone(&self.clock))?;
        let tx = scratch_log.begin()?;
        crate::schema::create_views(tx.raw())?;
        crate::materialize::replay(&tx, &events)?;
        let view = Self::read_view(tx.raw())?;
        let goal = read_goal_state(tx.raw(), ticket)?;
        // Nothing written here was ever meant to be durable; the scratch file is deleted with
        // `scratch_file` regardless, but rolling back (rather than committing) says so plainly.
        tx.rollback()?;

        Ok((view.tickets.get(ticket).cloned(), goal))
    }

    /// Fork `source`'s materialized state as of `seq` into a brand-new ticket lineage in this
    /// same project's log (`docs/decisions/D-008-ticket-checkpoint-fork.md` has the full design
    /// reasoning). The new ticket:
    ///
    /// * Starts in [`TicketState::Draft`] like every other ticket, never teleported into
    ///   `source`'s historical machine state — a fork earns its own state transitions.
    /// * Inherits `source`'s objective, kind, milestone, authority, resources, executor
    ///   requirements, context refs, success predicates, verification policy, budget, retry
    ///   policy and priority as of `seq`, plus `source`'s goal text and step decomposition (if it
    ///   had one) — reconstructed via fresh `goal.set`/`goal.step_added`/`goal.step_completed`
    ///   events computed from the snapshot, not copied event rows.
    /// * Never inherits `parent`, `dependencies`, `attempts`, `failures` or `cycle`: those
    ///   describe `source`'s own execution history, not a definition a fresh lineage inherits.
    ///
    /// Emits `ticket.created` + `ticket.updated` for the new ticket (the same two-event
    /// convention [`Store::create_ticket`] itself uses, so the existing materializer arms do all
    /// the real work) followed by a purpose-built `ticket.forked` event recording
    /// `(new ticket, source, seq)` as durable, hash-chained provenance — all in one
    /// [`Store::transaction`], so the fork either lands completely or not at all.
    ///
    /// # Errors
    /// `TmError::not_found` if `source` did not exist as of `seq`. `TmError::invariant` if `seq`
    /// is `0` or exceeds the log's current head (see [`Store::ticket_and_goal_as_of`]).
    pub fn fork_ticket(
        &self,
        source: &TicketId,
        seq: u64,
        actor: ParticipantId,
    ) -> tm_types::Result<(TicketId, Vec<Event>)> {
        let (ticket, goal) = self.ticket_and_goal_as_of(source, seq)?;
        let ticket = ticket.ok_or_else(|| TmError::not_found("ticket", source))?;

        let new_id = TicketId::new(self.ids.next(IdKind::Ticket).as_str())?;
        let now = self.clock.now();
        let fields = serde_json::json!({
            "kind": ticket.kind,
            "milestone": ticket.milestone,
            "authority": ticket.authority,
            "resources": ticket.resources,
            "executor": ticket.executor,
            "context_refs": ticket.context_refs,
            "success": ticket.success,
            "verification": ticket.verification,
            "budget": ticket.budget,
            "retry": ticket.retry,
            "priority": ticket.priority,
            "created": now,
            "updated": now,
        });

        let events = self.transaction(|tx| {
            let mut events = tx.append_all(vec![
                EventDraft::new(
                    actor.clone(),
                    Id::from(new_id.clone()),
                    Payload::from(TicketCreatedPayload {
                        ticket: new_id.clone(),
                        title: ticket.objective.clone(),
                        parent: None,
                    }),
                ),
                EventDraft::new(
                    actor.clone(),
                    Id::from(new_id.clone()),
                    Payload::from(TicketUpdatedPayload {
                        ticket: new_id.clone(),
                        fields: fields.clone(),
                    }),
                ),
                EventDraft::new(
                    actor.clone(),
                    Id::from(new_id.clone()),
                    Payload::from(TicketForkedPayload {
                        ticket: new_id.clone(),
                        source: source.clone(),
                        source_seq: seq,
                    }),
                ),
            ])?;

            if let Some(goal) = &goal {
                events.push(tx.append(EventDraft::new(
                    actor.clone(),
                    Id::from(new_id.clone()),
                    Payload::from(GoalSetPayload {
                        ticket: new_id.clone(),
                        text: goal.text.clone(),
                    }),
                ))?);
                for step in &goal.steps {
                    events.push(tx.append(EventDraft::new(
                        actor.clone(),
                        Id::from(new_id.clone()),
                        Payload::from(GoalStepAddedPayload {
                            ticket: new_id.clone(),
                            step_id: step.id.clone(),
                            text: step.text.clone(),
                        }),
                    ))?);
                    if step.done {
                        events.push(tx.append(EventDraft::new(
                            actor.clone(),
                            Id::from(new_id.clone()),
                            Payload::from(GoalStepCompletedPayload {
                                ticket: new_id.clone(),
                                step_id: step.id.clone(),
                            }),
                        ))?);
                    }
                }
            }

            Ok(events)
        })?;

        Ok((new_id, events))
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

    /// The regression gate for M-04's "secret redaction ... an event": a real
    /// [`ProviderDegradedPayload`] carrying a known fake-secret-shaped canary in its free-text
    /// `reason` field, appended through the real [`Store::append`], must not reach either the
    /// returned in-memory [`Event`] or the raw `events.payload` text SQLite actually persisted.
    #[test]
    fn event_payload_redacts_a_secret_shaped_substring_before_it_is_persisted() {
        use tm_events::payload::ProviderDegradedPayload;

        const CANARY: &str = "sk-EVENTCANARY0123456789abcdefghijklmnopqr";

        let (_dir, store) = open_store();
        let events = store
            .append(vec![EventDraft::new(
                actor(),
                Id::none(),
                Payload::from(ProviderDegradedPayload {
                    provider: "openai".to_string(),
                    reason: format!("call failed, key was: {CANARY}"),
                }),
            )])
            .expect("append event");
        assert_eq!(events.len(), 1);

        // Not in the returned in-memory Event...
        let debug_text = format!("{:?}", events[0]);
        assert!(
            !debug_text.contains(CANARY),
            "canary leaked into the returned Event: {debug_text}"
        );
        let payload = events[0]
            .payload
            .as_provider_degraded()
            .expect("provider_degraded payload");
        assert!(!payload.reason.contains(CANARY), "{}", payload.reason);
        // The persistence path is the pure/keyless `tm_auth::redact` (no per-secret fingerprint,
        // unlike `Fabric::execute`'s `SessionRedactor`) -- see `tm_auth::redact`'s module docs.
        assert!(
            payload.reason.contains("<redacted:api_key>"),
            "{}",
            payload.reason
        );

        // ...and not in the raw bytes SQLite actually stored.
        let seq = events[0].seq;
        let persisted: String = store
            .transaction(|tx| {
                tx.raw()
                    .query_row(
                        "SELECT payload FROM events WHERE seq = ?1",
                        rusqlite::params![seq as i64],
                        |row| row.get(0),
                    )
                    .map_err(|e| TmError::storage(e.to_string()))
            })
            .expect("read raw payload column");
        assert!(
            !persisted.contains(CANARY),
            "canary leaked into the persisted events.payload row: {persisted}"
        );
    }

    /// The false-positive half of the same gate: a legitimate long identifier this codebase
    /// already generates (a blake3 content hash, with no `key`/`token`/`secret`/`password` label
    /// nearby) must survive a real append unredacted.
    #[test]
    fn event_payload_does_not_redact_a_real_content_hash() {
        use tm_events::payload::ProviderDegradedPayload;

        let hash = blake3::hash(b"some real content").to_hex().to_string();
        let (_dir, store) = open_store();
        let events = store
            .append(vec![EventDraft::new(
                actor(),
                Id::none(),
                Payload::from(ProviderDegradedPayload {
                    provider: "openai".to_string(),
                    reason: format!("mismatched content hash {hash}"),
                }),
            )])
            .expect("append event");

        let payload = events[0]
            .payload
            .as_provider_degraded()
            .expect("provider_degraded payload");
        assert!(
            payload.reason.contains(&hash),
            "a real content hash must survive redaction unchanged: {}",
            payload.reason
        );
    }

    /// `redact_event_draft`'s `to_json`/`from_json` round-trip is not a guaranteed
    /// byte-identity for every JSON shape (map ordering, number representation) -- this proves
    /// it is exact for the one payload in this workspace with an open-ended `serde_json::Value`
    /// field, both when nothing matches (the round-trip is skipped entirely) and when a sibling
    /// field does match (forcing the round-trip to actually run): a float, a nested object and
    /// array must all survive structurally unchanged.
    #[test]
    fn event_payload_value_field_round_trips_exactly_around_redaction() {
        use tm_events::payload::TicketUpdatedPayload;

        let (_dir, store) = open_store();
        let fields = serde_json::json!({
            "priority": 3,
            "burn_rate": 1.5,
            "nested": { "b": 2, "a": 1 },
            "tags": ["alpha", "beta"],
        });
        let events = store
            .append(vec![EventDraft::new(
                actor(),
                Id::none(),
                Payload::from(TicketUpdatedPayload {
                    ticket: TicketId::new("T-1").unwrap(),
                    fields: fields.clone(),
                }),
            )])
            .expect("append event with no secret-shaped content");
        let inner = events[0].payload.as_ticket_updated().expect("typed");
        assert_eq!(inner.fields, fields, "no-match path must be byte-exact");

        // Now force the round-trip to actually execute by adding a secret-shaped sibling value.
        const CANARY: &str = "AKIAABCDEFGHIJKLMNOP";
        let mut fields_with_secret = fields.clone();
        fields_with_secret["leaked"] = serde_json::Value::String(CANARY.to_string());
        let events = store
            .append(vec![EventDraft::new(
                actor(),
                Id::none(),
                Payload::from(TicketUpdatedPayload {
                    ticket: TicketId::new("T-1").unwrap(),
                    fields: fields_with_secret,
                }),
            )])
            .expect("append event with secret-shaped content");
        let inner = events[0].payload.as_ticket_updated().expect("typed");
        assert_eq!(
            inner.fields["priority"], fields["priority"],
            "integer survives the forced round-trip"
        );
        assert_eq!(
            inner.fields["burn_rate"], fields["burn_rate"],
            "float survives the forced round-trip"
        );
        assert_eq!(
            inner.fields["nested"], fields["nested"],
            "nested object survives the forced round-trip"
        );
        assert_eq!(
            inner.fields["tags"], fields["tags"],
            "array survives the forced round-trip"
        );
        assert_ne!(
            inner.fields["leaked"],
            serde_json::Value::String(CANARY.to_string())
        );
    }

    /// The artifact half of the same gate: [`Store::store_artifact`]'s bytes and `meta` both get
    /// scrubbed, but a legitimate ticket id survives.
    #[test]
    fn store_artifact_redacts_secret_shaped_bytes_and_meta() {
        const CANARY: &str = "AKIAABCDEFGHIJKLMNOP";

        let (_dir, store) = open_store();
        let events = store
            .store_artifact(
                ArtifactKind::CommandOutput,
                "text/plain".into(),
                format!("captured stdout:\naws_access_key_id={CANARY}\ndone (T-1)").into_bytes(),
                serde_json::json!({ "command": format!("printenv | grep {CANARY}"), "ticket": "T-1" }),
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
        let ArtifactStorage::Inline(bytes) = &artifact.storage else {
            panic!("small artifact should be stored inline");
        };
        let text = String::from_utf8(bytes.clone()).expect("utf8");
        assert!(!text.contains(CANARY), "{text}");
        assert!(
            text.contains("T-1"),
            "unrelated ticket id should survive: {text}"
        );
        let meta_text = artifact.meta.to_string();
        assert!(!meta_text.contains(CANARY), "{meta_text}");
    }

    #[test]
    fn open_at_creates_project_db_and_spills_large_artifacts_under_state_dir_with_no_dot_tm() {
        let tmp = TempDir::new().expect("tempdir");
        // `state_dir` here is deliberately *not* named `.tm` and is not nested under a project
        // root, the way a global-scope project's `$TM_HOME/projects/<key>/` would be.
        let state_dir = tmp.path().join("global-state");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store =
            Store::open_with_at(&state_dir, clock, ids).expect("open_with_at should succeed");

        assert!(
            state_dir.join("project.db").exists(),
            "open_at/open_with_at must create <state_dir>/project.db"
        );
        assert!(
            !tmp.path().join(".tm").exists(),
            "open_at/open_with_at must never create a .tm directory"
        );
        assert!(
            !state_dir.join(".tm").exists(),
            "open_at/open_with_at must never create a .tm directory under state_dir itself"
        );
        assert_eq!(store.state_dir(), state_dir.as_path());

        let big = vec![0xABu8; crate::artifact::INLINE_LIMIT_BYTES + 1024];
        let events = store
            .store_artifact(
                ArtifactKind::File,
                "application/octet-stream".into(),
                big.clone(),
                serde_json::json!({}),
                None,
                actor(),
            )
            .expect("store_artifact");
        let artifact_id = ArtifactId::new(events[0].subject.as_str()).unwrap();
        let view = store.view().expect("view");
        let artifact = view.artifacts.get(&artifact_id).expect("materialized");
        match &artifact.storage {
            ArtifactStorage::OnDisk(path) => {
                assert!(
                    path.starts_with(state_dir.join("artifacts")),
                    "spilled artifact should live under <state_dir>/artifacts, got {path:?}"
                );
                let on_disk = std::fs::read(path).expect("read spilled bytes");
                assert_eq!(on_disk, big);
            }
            ArtifactStorage::Inline(_) => panic!("bytes over INLINE_LIMIT_BYTES must spill"),
        }
        assert!(!tmp.path().join(".tm").exists());
    }

    #[test]
    fn relocated_state_dir_reads_spilled_artifact_bytes_back_via_the_fallback() {
        let tmp = TempDir::new().expect("tempdir");
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store_a =
            Store::open_with_at(&dir_a, clock, ids).expect("open_with_at A should succeed");

        let big = vec![0x5Au8; crate::artifact::INLINE_LIMIT_BYTES + 4096];
        let events = store_a
            .store_artifact(
                ArtifactKind::File,
                "application/octet-stream".into(),
                big.clone(),
                serde_json::json!({}),
                None,
                actor(),
            )
            .expect("store_artifact");
        let artifact_id = ArtifactId::new(events[0].subject.as_str()).unwrap();

        // The event/table-recorded storage descriptor is an absolute path under `dir_a`; it is
        // never rewritten. Copy the whole state directory tree to `dir_b` (as `tm init`'s
        // promotion will) and confirm a fresh `Store` opened at `dir_b` can still read the
        // artifact's bytes back, via the read-time fallback to `<state_dir>/artifacts/<hash>`.
        copy_dir_recursive(&dir_a, &dir_b);
        drop(store_a);
        // Simulate a genuine relocation (not just a copy that leaves the original in place, which
        // would let the stale absolute path still resolve and never exercise the fallback).
        std::fs::remove_dir_all(&dir_a).expect("remove original state dir");

        let clock_b: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids_b: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store_b =
            Store::open_with_at(&dir_b, clock_b, ids_b).expect("open_with_at B should succeed");
        let view_b = store_b.view().expect("view B");
        let artifact_b = view_b
            .artifacts
            .get(&artifact_id)
            .expect("artifact present in relocated store");
        let bytes_b = match &artifact_b.storage {
            ArtifactStorage::OnDisk(path) => {
                assert!(
                    path.starts_with(&dir_b),
                    "the read fallback must resolve to a path under the *new* state dir, got \
                     {path:?}"
                );
                std::fs::read(path).expect("read relocated artifact bytes")
            }
            ArtifactStorage::Inline(_) => panic!("bytes over INLINE_LIMIT_BYTES must spill"),
        };
        assert_eq!(bytes_b, big, "relocated bytes must round-trip identically");
    }

    fn copy_dir_recursive(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("create dest dir");
        for entry in std::fs::read_dir(from).expect("read dir") {
            let entry = entry.expect("dir entry");
            let file_type = entry.file_type().expect("file type");
            let dest = to.join(entry.file_name());
            if file_type.is_dir() {
                copy_dir_recursive(&entry.path(), &dest);
            } else {
                std::fs::copy(entry.path(), &dest).expect("copy file");
            }
        }
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

    /// A root ticket driven to `Submitted` by an agent worker, carrying one evidence artifact.
    fn submitted_ticket(store: &Store) -> TicketId {
        let ticket_id = create_root_ticket(store);
        let worker: ParticipantId = "agent:builtin/worker".parse().unwrap();
        store.activate(&ticket_id, actor()).expect("activate");
        store
            .acquire_lease(
                &ticket_id,
                worker.clone(),
                Authority::none(),
                vec![],
                60,
                actor(),
            )
            .expect("acquire lease");
        store
            .transition(&ticket_id, Trigger::WorkStarted, worker.clone())
            .expect("work started");
        let artifact = store
            .store_artifact(
                ArtifactKind::File,
                "text/plain".into(),
                b"tests pass".to_vec(),
                serde_json::json!({}),
                Some(ticket_id.clone()),
                worker.clone(),
            )
            .expect("store_artifact");
        let artifact_id = ArtifactId::new(artifact[0].subject.as_str()).expect("artifact id");
        store
            .submit(&ticket_id, "done".to_string(), vec![artifact_id], worker)
            .expect("submit");
        ticket_id
    }

    #[test]
    fn a_human_accepts_a_submission_and_it_closes_cleanly() {
        let (_dir, store) = open_store();
        let ticket_id = submitted_ticket(&store);
        let human: ParticipantId = "human:owner".parse().unwrap();
        store
            .accept(&ticket_id, Some("looks right".into()), human)
            .expect("accept");
        let view = store.view().unwrap();
        assert_eq!(view.tickets[&ticket_id].state, TicketState::Closed);
        assert!(
            crate::invariants::check_invariants(&view).is_empty(),
            "accepting leaves no invariant violations"
        );
    }

    #[test]
    fn an_agent_cannot_accept_and_only_submissions_can_be_accepted() {
        let (_dir, store) = open_store();
        let ticket_id = submitted_ticket(&store);
        let agent: ParticipantId = "agent:builtin/other".parse().unwrap();
        assert!(matches!(
            store.accept(&ticket_id, None, agent).unwrap_err(),
            TmError::AuthorityDenied(_)
        ));
        let draft = create_root_ticket(&store);
        let human: ParticipantId = "human:owner".parse().unwrap();
        assert!(matches!(
            store.accept(&draft, None, human).unwrap_err(),
            TmError::InvalidTransition(_)
        ));
    }

    #[test]
    fn a_human_rejection_sends_the_work_back_with_the_reason_recorded() {
        let (_dir, store) = open_store();
        let ticket_id = submitted_ticket(&store);
        let human: ParticipantId = "human:owner".parse().unwrap();
        store
            .reject(&ticket_id, "subtract is missing a test".into(), human)
            .expect("reject");
        let view = store.view().unwrap();
        let ticket = &view.tickets[&ticket_id];
        assert_eq!(ticket.state, TicketState::Ready, "retried, not dropped");
        let last = ticket
            .failures
            .last()
            .expect("the rejection is a recorded failure");
        assert_eq!(last.class, FailureClass::VerificationFailed);
        assert_eq!(last.detail, "subtract is missing a test");
    }

    #[test]
    fn a_rejection_needs_a_reason_and_stores_it_trimmed() {
        let (_dir, store) = open_store();
        let ticket_id = submitted_ticket(&store);
        let human: ParticipantId = "human:owner".parse().unwrap();
        for blank in ["", "   ", "\n\t "] {
            assert!(matches!(
                store
                    .reject(&ticket_id, blank.into(), human.clone())
                    .unwrap_err(),
                TmError::InvalidTransition(_)
            ));
        }
        assert_eq!(
            store.view().unwrap().tickets[&ticket_id].state,
            TicketState::Submitted,
            "a refused rejection changes nothing"
        );
        store
            .reject(&ticket_id, "  wrong file \n".into(), human)
            .expect("reject");
        let view = store.view().unwrap();
        let last = view.tickets[&ticket_id].failures.last().cloned().unwrap();
        assert_eq!(last.detail, "wrong file");
    }

    /// `updated` moves with every change to a ticket, including the terminal ones.
    #[test]
    fn closing_or_cancelling_a_ticket_bumps_its_updated_time() {
        let dir = TempDir::new().unwrap();
        let clock = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Store::open_with(dir.path(), clock.clone(), ids).unwrap();
        let human: ParticipantId = "human:owner".parse().unwrap();
        let ticket = |id: &TicketId| store.view().unwrap().tickets[id].clone();

        let cancelled = create_root_ticket(&store);
        let created = ticket(&cancelled).updated;
        assert_eq!(created, ticket(&cancelled).created);
        clock.advance_seconds(60);
        store.cancel(&cancelled, None, human.clone()).unwrap();
        assert_eq!(ticket(&cancelled).updated, created.plus_seconds(60));

        let closed = submitted_ticket(&store);
        clock.advance_seconds(60);
        let before = ticket(&closed).updated;
        store.accept(&closed, None, human.clone()).unwrap();
        assert_eq!(ticket(&closed).state, TicketState::Closed);
        assert_eq!(ticket(&closed).updated, before.plus_seconds(60));

        clock.advance_seconds(60);
        store
            .update_ticket(&closed, serde_json::json!({"priority": 3}), human)
            .unwrap();
        assert_eq!(ticket(&closed).updated, before.plus_seconds(120));
    }

    #[test]
    fn a_human_retries_an_escalated_ticket_with_guidance_and_fresh_attempts() {
        let (_dir, store) = open_store();
        let ticket_id = submitted_ticket(&store);
        let human: ParticipantId = "human:owner".parse().unwrap();
        store
            .reject(&ticket_id, "wrong file".into(), human.clone())
            .expect("reject");
        // A second attempt that runs out of budget, which is never retried: it escalates.
        let worker: ParticipantId = "agent:builtin/worker".parse().unwrap();
        store
            .acquire_lease(
                &ticket_id,
                worker.clone(),
                Authority::none(),
                vec![],
                60,
                actor(),
            )
            .expect("lease");
        store
            .transition(&ticket_id, Trigger::WorkStarted, worker)
            .expect("work started");
        store
            .record_failure(
                &ticket_id,
                FailureClass::BudgetExhausted,
                "ran dry".into(),
                ParticipantId::system(),
            )
            .expect("failure recorded");
        let before = store.view().unwrap().tickets[&ticket_id].clone();
        assert_eq!(before.state, TicketState::Escalated);

        let agent: ParticipantId = "agent:builtin/other".parse().unwrap();
        assert!(matches!(
            store.retry(&ticket_id, None, agent).unwrap_err(),
            TmError::AuthorityDenied(_)
        ));
        store
            .retry(
                &ticket_id,
                Some("edit src/lib.rs, not main.rs".into()),
                human.clone(),
            )
            .expect("retry");
        let after = store.view().unwrap().tickets[&ticket_id].clone();
        assert_eq!(after.state, TicketState::Ready);
        assert!(
            after
                .objective
                .ends_with("From the user, after attempt 2: edit src/lib.rs, not main.rs"),
            "{}",
            after.objective
        );
        assert!(after.objective.starts_with(&before.objective));
        assert_eq!(
            after.retry.max_attempts,
            before.attempts + before.retry.max_attempts
        );
        assert!(crate::invariants::check_invariants(&store.view().unwrap()).is_empty());
        assert!(
            matches!(
                store.retry(&ticket_id, None, human).unwrap_err(),
                TmError::InvalidTransition(_)
            ),
            "only an escalated ticket can be retried"
        );
    }

    #[test]
    fn a_human_can_cancel_a_ticket_whose_worker_could_not() {
        let (_dir, store) = open_store();
        let events = store
            .create_ticket(
                TicketKind::Work,
                "worker authority only".into(),
                None,
                None,
                Authority::worker(),
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
        let agent: ParticipantId = "agent:builtin/worker".parse().unwrap();
        assert!(matches!(
            store.cancel(&ticket_id, None, agent).unwrap_err(),
            TmError::AuthorityDenied(_)
        ));
        let human: ParticipantId = "human:owner".parse().unwrap();
        store
            .cancel(&ticket_id, Some("not needed".into()), human)
            .expect("the project owner can cancel any ticket");
        assert_eq!(
            store.view().unwrap().tickets[&ticket_id].state,
            TicketState::Cancelled
        );
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

    // --- `Store::fork_ticket` (`docs/decisions/D-008-ticket-checkpoint-fork.md`) ---

    #[test]
    fn fork_ticket_reads_source_state_as_of_seq_not_as_of_head() {
        let (_dir, store) = open_store();
        let source = create_root_ticket(&store);

        // Move the objective to "A", capture the seq right after, then move it again to "B"
        // before forking. If `fork_ticket` accidentally read HEAD instead of `seq`, the fork
        // would come back with "B" instead of "A" — this is the one assertion that actually
        // proves "--at seq" reads history, not the live view.
        let events_a = store
            .update_ticket(&source, serde_json::json!({"objective": "A"}), actor())
            .expect("update to A");
        let seq_after_a = events_a.last().expect("at least one event").seq;
        store
            .update_ticket(&source, serde_json::json!({"objective": "B"}), actor())
            .expect("update to B");

        let (forked_id, fork_events) = store
            .fork_ticket(&source, seq_after_a, actor())
            .expect("fork_ticket");
        assert!(!fork_events.is_empty());

        let view = store.view().expect("view");
        let forked = view
            .tickets
            .get(&forked_id)
            .expect("forked ticket materialized");
        assert_eq!(
            forked.objective, "A",
            "fork must reflect state as of seq, not HEAD"
        );
        let source_now = view.tickets.get(&source).expect("source ticket");
        assert_eq!(
            source_now.objective, "B",
            "the source ticket's own current state must be untouched by forking it"
        );
    }

    #[test]
    fn fork_ticket_starts_in_draft_with_no_lineage_or_history_carried_over() {
        let (_dir, store) = open_store();
        let source = create_root_ticket(&store);
        store.activate(&source, actor()).unwrap(); // source is now Ready, not Draft
        let head = store.log.head().expect("head");

        let (forked_id, _events) = store
            .fork_ticket(&source, head, actor())
            .expect("fork_ticket");

        let view = store.view().expect("view");
        let forked = view
            .tickets
            .get(&forked_id)
            .expect("forked ticket materialized");
        assert_eq!(
            forked.state,
            TicketState::Draft,
            "a fork always starts Draft, never teleported into the source's historical state"
        );
        assert_eq!(forked.objective, "do the thing");
        assert!(forked.parent.is_none());
        assert!(forked.children.is_empty());
        assert!(forked.dependencies.is_empty());
        assert_eq!(forked.attempts, 0);
        assert!(forked.failures.is_empty());
        assert!(forked.cycle.is_none());
    }

    #[test]
    fn fork_ticket_reconstructs_goal_state_as_of_seq_via_fresh_goal_events() {
        let (_dir, store) = open_store();
        let source = create_root_ticket(&store);
        store
            .set_goal(&source, "reach steady state".into(), actor())
            .expect("set_goal");
        store
            .add_goal_step(&source, "step-1".into(), "first".into(), actor())
            .expect("add_goal_step");
        let completed = store
            .complete_goal_step(&source, "step-1".into(), actor())
            .expect("complete_goal_step");
        let seq_after_step_1_done = completed.last().expect("event").seq;
        // A second step, added only *after* the snapshot point, must not appear in the fork.
        store
            .add_goal_step(&source, "step-2".into(), "second".into(), actor())
            .expect("add_goal_step");

        let (forked_id, _events) = store
            .fork_ticket(&source, seq_after_step_1_done, actor())
            .expect("fork_ticket");

        let forked_goal = store
            .goal_state(&forked_id)
            .expect("goal_state")
            .expect("fork carried a goal");
        assert_eq!(forked_goal.text, "reach steady state");
        assert_eq!(forked_goal.steps.len(), 1);
        assert_eq!(forked_goal.steps[0].id, "step-1");
        assert!(forked_goal.steps[0].done);
    }

    #[test]
    fn fork_ticket_seq_before_source_existed_is_not_found() {
        let (_dir, store) = open_store();
        let _early_ticket = create_root_ticket(&store); // occupies the log's earliest seqs
        let later_ticket = create_root_ticket(&store);
        let events = store.view().expect("view"); // sanity: both tickets exist now
        assert_eq!(events.tickets.len(), 2);

        // seq 1 predates `later_ticket`'s own `ticket.created` event.
        let err = store.fork_ticket(&later_ticket, 1, actor()).unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }

    #[test]
    fn fork_ticket_seq_zero_is_out_of_range() {
        let (_dir, store) = open_store();
        let source = create_root_ticket(&store);
        let err = store.fork_ticket(&source, 0, actor()).unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }

    #[test]
    fn fork_ticket_seq_past_head_is_out_of_range_not_silently_clamped() {
        let (_dir, store) = open_store();
        let source = create_root_ticket(&store);
        let head = store.log.head().expect("head");
        let err = store
            .fork_ticket(&source, head + 1000, actor())
            .unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }

    #[test]
    fn fork_ticket_preserves_hash_chain_integrity() {
        let (_dir, store) = open_store();
        let source = create_root_ticket(&store);
        let head = store.log.head().expect("head");
        store
            .fork_ticket(&source, head, actor())
            .expect("fork_ticket");

        let report = store.log.verify_chain().expect("verify_chain");
        assert!(
            report.is_valid(),
            "hash chain must stay valid across a fork: {report:?}"
        );

        let violations = store.check_invariants().expect("check_invariants");
        assert!(
            violations.is_empty(),
            "a fork must not leave the project in an invariant-violating state: {violations:?}"
        );
    }

    #[test]
    fn fork_ticket_records_forked_event_with_correct_provenance() {
        let (_dir, store) = open_store();
        let source = create_root_ticket(&store);
        let head = store.log.head().expect("head");
        let (forked_id, events) = store
            .fork_ticket(&source, head, actor())
            .expect("fork_ticket");

        let forked_event = events
            .iter()
            .find(|e| e.kind == tm_events::EventKind::TicketForked)
            .expect("a ticket.forked event was appended");
        let payload = forked_event
            .payload
            .as_ticket_forked()
            .expect("ticket.forked payload");
        assert_eq!(payload.ticket, forked_id);
        assert_eq!(payload.source, source);
        assert_eq!(payload.source_seq, head);
    }

    #[test]
    fn fork_ticket_of_a_milestone_member_succeeds_and_carries_the_milestone() {
        // `Store::create_ticket`'s own `fields` object already includes `"milestone"` for a
        // caller-supplied milestone, so `fork_ticket` copying `ticket.milestone` through is not
        // new behavior — this test's real job is proving it does not trip the post-write
        // invariant check `Store::transaction` runs (there is no cross-check today between a
        // ticket's own `milestone` field and `milestones.tickets`, per
        // `crate::milestone::MilestoneStore::membership_of`'s own doc: membership is computed
        // by scanning tickets, not read off a stored list — but a future invariant could change
        // that, and this test would catch a fork that stopped being legal under it).
        let (_dir, store) = open_store();
        let source = create_root_ticket(&store);
        let milestone_events = store
            .create_milestone("M1".into(), vec![source.clone()], vec![], actor())
            .expect("create_milestone");
        let milestone_id = milestone_events[0]
            .payload
            .as_milestone_created()
            .expect("milestone.created payload")
            .milestone
            .clone();
        let head = store.log.head().expect("head");

        let (forked_id, _events) = store
            .fork_ticket(&source, head, actor())
            .expect("fork_ticket must succeed for a ticket that belongs to a milestone");

        let view = store.view().expect("view");
        assert_eq!(
            view.tickets.get(&forked_id).unwrap().milestone,
            Some(milestone_id),
            "the fork carries the source's milestone the same way it carries priority/kind/..."
        );
        assert!(store
            .check_invariants()
            .expect("check_invariants")
            .is_empty());
    }

    #[test]
    fn fork_ticket_survives_rebuild() {
        // Matching this crate's own `effect_status_survives_rebuild`/`goal_state_survives_rebuild`
        // convention: a fork's state must be reproducible by a full from-`seq`-0 replay, not an
        // artifact of whatever `Store::ticket_and_goal_as_of`'s own scratch replay happened to
        // leave behind — the same "every row is derivable by replaying the log" guarantee this
        // crate's `materialize` module claims for everything else it writes.
        let (_dir, store) = open_store();
        let source = create_root_ticket(&store);
        store
            .set_goal(&source, "reach steady state".into(), actor())
            .expect("set_goal");
        store
            .add_goal_step(&source, "step-1".into(), "first".into(), actor())
            .expect("add_goal_step");
        let head = store.log.head().expect("head");

        let (forked_id, _events) = store
            .fork_ticket(&source, head, actor())
            .expect("fork_ticket");

        store.rebuild().expect("rebuild");

        let view = store.view().expect("view after rebuild");
        let forked = view
            .tickets
            .get(&forked_id)
            .expect("forked ticket survives rebuild");
        assert_eq!(forked.objective, "do the thing");
        assert_eq!(forked.state, TicketState::Draft);

        let goal = store
            .goal_state(&forked_id)
            .expect("goal_state after rebuild")
            .expect("forked goal survives rebuild");
        assert_eq!(goal.text, "reach steady state");
        assert_eq!(goal.steps.len(), 1);
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
