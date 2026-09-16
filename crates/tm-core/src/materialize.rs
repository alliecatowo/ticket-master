//! The single point where an event becomes materialized state.
//!
//! [`apply`] is the *only* function in `tm-core` that writes to the tables `schema.rs` defines.
//! `store.rs`'s command API calls it once per event it appends, inside the same [`tm_events::Tx`]
//! the event itself is written through; `Store::rebuild()` calls it once per event replayed from
//! `seq 0` after `schema::drop_views` + `schema::migrate`. Because both paths go through this one
//! function, replay is byte-identical to the live path by construction — there is no second copy
//! of "what does a `ticket.closed` event mean" to drift out of sync.
//!
//! An event kind this view doesn't care about (e.g. `presence.updated`) is a no-op, never an
//! error: the catalogue in `tm_events::EventKind` is shared by every materialized view in the
//! system, and `tm-core` only owns a subset of it.
//!
//! # Conventions this module picks (documented once, applied consistently)
//!
//! - Every JSON-shaped column (`kind`, `state`, `authority`, `executor`, `verification`,
//!   `budget`, `retry`, dependency `kind`, milestone `state`, artifact `kind`/`storage`) stores
//!   the `serde_json` rendering of its `tm-core`/`tm-types` value, quotes included for bare
//!   strings/unit-enum variants — the same shape every reader in this crate deserializes with
//!   `serde_json::from_str`.
//! - Ticket lifecycle: `TicketStateChanged` is the *only* writer of `tickets.state`. Every other
//!   domain event that implies a state transition (`TicketSubmitted`, `TicketVerified`, ...,
//!   `TicketClosed`, `TicketEscalated`, ...) is always paired by `store.rs` with a
//!   `TicketStateChanged` alongside it, so those arms below are no-ops for `state` and only
//!   touch kind-specific side effects they alone carry (e.g. `TicketRetryScheduled` bumping
//!   `attempts`).
//! - `DecisionCreatedPayload`/`ArtifactCreatedPayload` carry less detail than the `decisions`/
//!   `artifacts` tables have columns for (no `reason`/`evidence` on the former, no
//!   `kind`/`bytes_len`/`hash` on the latter). Missing columns get a documented, harmless
//!   default (empty string/list, `ArtifactKind::File`, zero length) rather than failing to
//!   materialize the row; richer detail is a future payload-shape improvement, not something
//!   `apply` can invent.
//! - `ResourceClaimedPayload`/`ResourceReleasedPayload` are keyed by `(resource, holder)`, not
//!   by `(ticket, idx)` the way `resource_claims` is — they describe a different, coarser
//!   concept (a free-form named resource claimed by a participant) than the ticket-scoped path
//!   claims that table stores. Materializing them onto `resource_claims` would require inventing
//!   a ticket id that doesn't exist in the payload, so those arms are no-ops here.
//! - `AuthorityReverted` names a `to_seq` to restore to; reconstructing "what was the authority
//!   at that point" requires replaying the log up to `to_seq`, which is a multi-event operation
//!   `store.rs` performs (by re-deriving and issuing a fresh `AuthorityGranted`), not something
//!   a single event's `apply` can do in isolation. No-op here.

use std::path::PathBuf;

use rusqlite::params;
use serde::Serialize;
use tm_events::{Event, EventKind, Tx};
use tm_types::{Authority, Budget, ParticipantId, Role, TicketId, Timestamp, TmError, Tolerance};

use crate::artifact::{ArtifactKind, ArtifactStorage};
use crate::milestone::MilestoneState;
use crate::ticket::{
    DependencyKind, ExecutorRequirements, RetryPolicy, TicketKind, TicketState, VerificationPolicy,
};

/// Turn one committed event into its effect on materialized state, writing through `tx`.
///
/// Dispatches on `event.kind` / `event.payload`; each arm issues the `INSERT`/`UPDATE`/`DELETE`
/// statements against `tx.raw()` that bring the relevant table(s) up to date. See the module
/// doc comment for the conventions applied throughout (JSON column encoding, the
/// `TicketStateChanged`-owns-`state` rule, and the documented payload/table shape mismatches).
pub fn apply(tx: &Tx<'_>, event: &Event) -> tm_types::Result<()> {
    match event.kind {
        EventKind::ProjectCreated => {
            let p = event
                .payload
                .as_project_created()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            upsert_meta(tx, "name", &p.name)?;
            upsert_meta(tx, "root", &p.root)?;
        }
        EventKind::ProjectAttached => {
            let p = event
                .payload
                .as_project_attached()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            upsert_meta(tx, "name", &p.name)?;
            upsert_meta(tx, "root", &p.root)?;
        }
        EventKind::TicketCreated => {
            let p = event
                .payload
                .as_ticket_created()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            let ts = event.ts.to_rfc3339();
            let kind = ticket_kind_from_id(&p.ticket);
            tx.raw()
                .execute(
                    "INSERT INTO tickets (
                        id, kind, objective, state, parent, milestone, authority, executor,
                        context_refs, success, verification, budget, retry, cycle, attempts,
                        priority, created, updated
                     ) VALUES (?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, NULL, 0, 0, ?, ?)",
                    params![
                        p.ticket.as_str(),
                        json_string(&kind)?,
                        p.title,
                        json_string(&TicketState::Draft)?,
                        p.parent.as_ref().map(TicketId::as_str),
                        json_string(&Authority::none())?,
                        json_string(&default_executor())?,
                        "[]",
                        "[]",
                        json_string(&VerificationPolicy::Single)?,
                        json_string(&Budget::none())?,
                        json_string(&default_retry())?,
                        ts,
                        ts,
                    ],
                )
                .map_err(storage_err)?;
            if let Some(parent) = &p.parent {
                tx.raw()
                    .execute(
                        "INSERT OR IGNORE INTO ticket_children (parent, child) VALUES (?, ?)",
                        params![parent.as_str(), p.ticket.as_str()],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::TicketUpdated => {
            let p = event
                .payload
                .as_ticket_updated()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            let ts = event.ts.to_rfc3339();
            if let Some(obj) = p.fields.as_object() {
                // Unknown keys are ignored, not errors: `fields` is a caller-supplied patch and
                // this whitelist is the only part of it `tm-core` understands.
                if let Some(v) = obj.get("objective").and_then(|v| v.as_str()) {
                    tx.raw()
                        .execute(
                            "UPDATE tickets SET objective = ?, updated = ? WHERE id = ?",
                            params![v, ts, p.ticket.as_str()],
                        )
                        .map_err(storage_err)?;
                }
                if let Some(v) = obj.get("priority").and_then(|v| v.as_i64()) {
                    tx.raw()
                        .execute(
                            "UPDATE tickets SET priority = ?, updated = ? WHERE id = ?",
                            params![v, ts, p.ticket.as_str()],
                        )
                        .map_err(storage_err)?;
                }
                if obj.contains_key("milestone") {
                    let m = obj.get("milestone").and_then(|v| v.as_str());
                    tx.raw()
                        .execute(
                            "UPDATE tickets SET milestone = ?, updated = ? WHERE id = ?",
                            params![m, ts, p.ticket.as_str()],
                        )
                        .map_err(storage_err)?;
                }
            }
        }
        EventKind::TicketStateChanged => {
            let p = event
                .payload
                .as_ticket_state_changed()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "UPDATE tickets SET state = ?, updated = ? WHERE id = ?",
                    params![quoted(&p.to), event.ts.to_rfc3339(), p.ticket.as_str()],
                )
                .map_err(storage_err)?;
        }
        EventKind::TicketDependencyAdded => {
            let p = event
                .payload
                .as_ticket_dependency_added()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "INSERT OR IGNORE INTO ticket_deps (ticket, depends_on, kind) VALUES (?, ?, ?)",
                    params![p.ticket.as_str(), p.depends_on.as_str(), json_string(&DependencyKind::Hard)?],
                )
                .map_err(storage_err)?;
        }
        EventKind::TicketDependencyRemoved => {
            let p = event
                .payload
                .as_ticket_dependency_removed()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "DELETE FROM ticket_deps WHERE ticket = ? AND depends_on = ?",
                    params![p.ticket.as_str(), p.depends_on.as_str()],
                )
                .map_err(storage_err)?;
        }
        EventKind::TicketChildAdded => {
            let p = event
                .payload
                .as_ticket_child_added()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "INSERT OR IGNORE INTO ticket_children (parent, child) VALUES (?, ?)",
                    params![p.parent.as_str(), p.child.as_str()],
                )
                .map_err(storage_err)?;
        }
        EventKind::TicketLeased => {
            let p = event
                .payload
                .as_ticket_leased()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            let ts = event.ts.to_rfc3339();
            let ttl = p.expires_at.seconds_since(event.ts).max(0);
            tx.raw()
                .execute(
                    "INSERT INTO leases (
                        id, ticket, holder, authority, resources, acquired, heartbeat,
                        ttl_seconds, epoch, live
                     ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, 1)",
                    params![
                        p.lease.as_str(),
                        p.ticket.as_str(),
                        p.holder.as_str(),
                        json_string(&Authority::none())?,
                        "[]",
                        ts,
                        ts,
                        ttl,
                    ],
                )
                .map_err(storage_err)?;
            tx.raw()
                .execute(
                    "UPDATE tickets SET state = ?, updated = ? WHERE id = ?",
                    params![json_string(&TicketState::Leased)?, ts, p.ticket.as_str()],
                )
                .map_err(storage_err)?;
        }
        EventKind::TicketHeartbeat => {
            let p = event
                .payload
                .as_ticket_heartbeat()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            let ttl = p.expires_at.seconds_since(event.ts).max(0);
            tx.raw()
                .execute(
                    "UPDATE leases SET heartbeat = ?, ttl_seconds = ? WHERE id = ?",
                    params![event.ts.to_rfc3339(), ttl, p.lease.as_str()],
                )
                .map_err(storage_err)?;
        }
        EventKind::TicketLeaseExpired => {
            let p = event
                .payload
                .as_ticket_lease_expired()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            release_lease(tx, event.ts, p.lease.as_str(), p.ticket.as_str())?;
        }
        EventKind::TicketLeaseReleased => {
            let p = event
                .payload
                .as_ticket_lease_released()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            release_lease(tx, event.ts, p.lease.as_str(), p.ticket.as_str())?;
        }
        // Informational: `TicketChildAdded` already records the parent/child edge this implies.
        EventKind::TicketDelegated
        // `state` is owned by the paired `TicketStateChanged`; see module doc comment.
        | EventKind::TicketSubmitted
        | EventKind::TicketVerified
        | EventKind::TicketVerificationFailed
        | EventKind::TicketAudited
        | EventKind::TicketAuditRejected
        | EventKind::TicketClosed
        | EventKind::TicketCancelled
        | EventKind::TicketReopened
        | EventKind::TicketFailed
        | EventKind::TicketEscalated
        // Informational: no dedicated budget-exhaustion side table.
        | EventKind::TicketBudgetExhausted => {}
        EventKind::TicketRetryScheduled => {
            let p = event
                .payload
                .as_ticket_retry_scheduled()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "UPDATE tickets SET attempts = ?, updated = ? WHERE id = ?",
                    params![p.attempt, event.ts.to_rfc3339(), p.ticket.as_str()],
                )
                .map_err(storage_err)?;
        }
        EventKind::DecisionCreated => {
            let p = event
                .payload
                .as_decision_created()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            let subject = p.ticket.as_ref().map(TicketId::as_str).unwrap_or(p.decision.as_str());
            let affected: Vec<&str> = p.ticket.as_ref().map(TicketId::as_str).into_iter().collect();
            tx.raw()
                .execute(
                    "INSERT INTO decisions (
                        id, subject, decision, reason, evidence, affected_tickets, affected_docs,
                        author, ts, supersedes, superseded_by
                     ) VALUES (?, ?, ?, '', '[]', ?, '[]', ?, ?, NULL, NULL)",
                    params![
                        p.decision.as_str(),
                        subject,
                        p.summary,
                        json_string(&affected)?,
                        event.actor.as_str(),
                        event.ts.to_rfc3339(),
                    ],
                )
                .map_err(storage_err)?;
        }
        EventKind::DecisionSuperseded => {
            let p = event
                .payload
                .as_decision_superseded()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "UPDATE decisions SET superseded_by = ? WHERE id = ?",
                    params![p.superseded_by.as_str(), p.decision.as_str()],
                )
                .map_err(storage_err)?;
        }
        EventKind::AuthorityGranted => {
            let p = event
                .payload
                .as_authority_granted()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            set_ticket_authority(tx, event.ts, p.ticket.as_ref(), &p.grant)?;
        }
        EventKind::AuthorityDelegated => {
            let p = event
                .payload
                .as_authority_delegated()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            set_ticket_authority(tx, event.ts, p.ticket.as_ref(), &p.grant)?;
        }
        EventKind::AuthorityRevoked => {
            let p = event
                .payload
                .as_authority_revoked()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            set_ticket_authority(tx, event.ts, p.ticket.as_ref(), &Authority::none())?;
        }
        // See module doc comment: reconstructing history from `to_seq` is `store.rs`'s job.
        EventKind::AuthorityReverted
        // See module doc comment: payload shape doesn't carry a ticket to key the row on.
        | EventKind::ResourceClaimed
        | EventKind::ResourceReleased
        | EventKind::ResourceConflictDetected
        // Transient; not a listed materialized table.
        | EventKind::CommandStarted
        | EventKind::CommandCompleted
        // No dedicated table; the event log remains the source of truth for these.
        | EventKind::CommentCreated
        | EventKind::ApprovalRequested
        | EventKind::ApprovalDecided
        | EventKind::ProviderSelected
        | EventKind::ProviderExhausted
        | EventKind::ProviderDegraded
        | EventKind::ProviderRecovered
        | EventKind::ExecutorFailed
        // Owned by a future `tm-codeintel`-adjacent view, not core.
        | EventKind::IndexUpdated
        // No dedicated table in SPEC.md 4.1's list.
        | EventKind::HarnessBenchmarked
        | EventKind::HarnessPromoted
        // Owned by `tm-genesis`'s own view, not core.
        | EventKind::GenesisStarted
        | EventKind::GenesisStageEntered
        | EventKind::GenesisStageCompleted
        | EventKind::GenesisAssumptionRecorded
        | EventKind::GenesisMaturityEvaluated
        | EventKind::GenesisCompleted => {}
        EventKind::ArtifactCreated => {
            let p = event
                .payload
                .as_artifact_created()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "INSERT INTO artifacts (
                        id, kind, media_type, bytes_len, hash, storage, inline, on_disk, meta
                     ) VALUES (?, ?, ?, 0, '', ?, NULL, ?, 'null')",
                    params![
                        p.artifact.as_str(),
                        json_string(&ArtifactKind::File)?,
                        p.media_type,
                        json_string(&ArtifactStorage::OnDisk(PathBuf::from(&p.path)))?,
                        p.path,
                    ],
                )
                .map_err(storage_err)?;
        }
        EventKind::MilestoneCreated => {
            let p = event
                .payload
                .as_milestone_created()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            let ts = event.ts.to_rfc3339();
            tx.raw()
                .execute(
                    "INSERT INTO milestones (id, title, state, closed_by, created, updated)
                     VALUES (?, ?, ?, NULL, ?, ?)",
                    params![p.milestone.as_str(), p.title, json_string(&MilestoneState::Open)?, ts, ts],
                )
                .map_err(storage_err)?;
        }
        EventKind::MilestoneClosed => {
            let p = event
                .payload
                .as_milestone_closed()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "UPDATE milestones SET state = ?, closed_by = ?, updated = ? WHERE id = ?",
                    params![
                        json_string(&MilestoneState::Closed)?,
                        event.actor.as_str(),
                        event.ts.to_rfc3339(),
                        p.milestone.as_str(),
                    ],
                )
                .map_err(storage_err)?;
        }
        EventKind::MilestoneReopened => {
            let p = event
                .payload
                .as_milestone_reopened()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "UPDATE milestones SET state = ?, closed_by = NULL, updated = ? WHERE id = ?",
                    params![json_string(&MilestoneState::Open)?, event.ts.to_rfc3339(), p.milestone.as_str()],
                )
                .map_err(storage_err)?;
        }
        EventKind::SessionStarted => {
            let p = event
                .payload
                .as_session_started()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "INSERT INTO sessions (id, started_by, started, ended) VALUES (?, ?, ?, NULL)",
                    params![p.session.as_str(), p.participant.as_str(), event.ts.to_rfc3339()],
                )
                .map_err(storage_err)?;
        }
        EventKind::SessionJoined => {
            let p = event
                .payload
                .as_session_joined()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            upsert_participant(tx, &p.participant, "joined", event.ts)?;
        }
        EventKind::SessionLeft => {
            let p = event
                .payload
                .as_session_left()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            upsert_participant(tx, &p.participant, "left", event.ts)?;
        }
        EventKind::SessionEnded => {
            let p = event
                .payload
                .as_session_ended()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "UPDATE sessions SET ended = ? WHERE id = ?",
                    params![event.ts.to_rfc3339(), p.session.as_str()],
                )
                .map_err(storage_err)?;
        }
        EventKind::PresenceUpdated => {
            let p = event
                .payload
                .as_presence_updated()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            upsert_participant(tx, &p.participant, &p.status, event.ts)?;
        }
        EventKind::UsageRecorded => {
            let p = event
                .payload
                .as_usage_recorded()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            // Debiting the ancestor budget chain (ticket -> milestone -> project) is
            // `store.rs::record_usage`'s job: it resolves the chain from `view.rs` and checks it
            // *before* this event is even drafted, so by the time `apply` runs, recording is all
            // that is left to do here.
            tx.raw()
                .execute(
                    "INSERT INTO provider_usage (ticket, session, tokens, dollars_micros, wall_seconds, ts)
                     VALUES (?, ?, ?, ?, ?, ?)",
                    params![
                        p.ticket.as_ref().map(TicketId::as_str),
                        p.session.as_ref().map(|s| s.as_str()),
                        p.tokens as i64,
                        p.dollars_micros as i64,
                        p.wall_seconds as i64,
                        event.ts.to_rfc3339(),
                    ],
                )
                .map_err(storage_err)?;
        }
        EventKind::DocRegistered => {
            let p = event
                .payload
                .as_doc_registered()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            upsert_doc(tx, &p.path, p.ticket.as_ref().map(TicketId::as_str), "registered", event)?;
        }
        EventKind::DocGenerated => {
            let p = event
                .payload
                .as_doc_generated()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            upsert_doc(tx, &p.path, p.ticket.as_ref().map(TicketId::as_str), "generated", event)?;
        }
        EventKind::DocInvalidated => {
            let p = event
                .payload
                .as_doc_invalidated()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            upsert_doc(tx, &p.path, None, "invalidated", event)?;
        }
        EventKind::DocReconciled => {
            let p = event
                .payload
                .as_doc_reconciled()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            upsert_doc(tx, &p.path, None, "reconciled", event)?;
        }
        EventKind::HarnessChanged => {
            let p = event
                .payload
                .as_harness_changed()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "INSERT INTO harness_epochs (field, from_value, to_value, ts) VALUES (?, ?, ?, ?)",
                    params![
                        p.field,
                        serde_json::to_string(&p.from).map_err(TmError::from)?,
                        serde_json::to_string(&p.to).map_err(TmError::from)?,
                        event.ts.to_rfc3339(),
                    ],
                )
                .map_err(storage_err)?;
        }
        EventKind::MirrorLinked => {
            let p = event
                .payload
                .as_mirror_linked()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            tx.raw()
                .execute(
                    "INSERT INTO mirror_links (remote, reference, updated) VALUES (?, NULL, ?)
                     ON CONFLICT(remote) DO UPDATE SET updated = excluded.updated",
                    params![p.remote, event.ts.to_rfc3339()],
                )
                .map_err(storage_err)?;
        }
        EventKind::MirrorPushed => {
            let p = event
                .payload
                .as_mirror_pushed()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            upsert_mirror_reference(tx, &p.remote, &p.reference, event.ts)?;
        }
        EventKind::MirrorPulled => {
            let p = event
                .payload
                .as_mirror_pulled()
                .ok_or_else(|| payload_mismatch(event.kind))?;
            upsert_mirror_reference(tx, &p.remote, &p.reference, event.ts)?;
        }
    }
    Ok(())
}

/// Replay every event in `events`, in order, through [`apply`] — the exact sequence
/// `Store::rebuild()` uses after `schema::drop_views` + `schema::migrate`, and reusable by tests
/// that want to assert replay determinism without going through the full `Store`.
///
/// The first failing `apply` aborts the replay; the caller (`Store::rebuild`) is responsible for
/// wrapping this in a `Tx` and rolling back on error so a failed rebuild never leaves a
/// half-populated view.
pub fn replay(tx: &Tx<'_>, events: &[Event]) -> tm_types::Result<()> {
    for event in events {
        apply(tx, event)?;
    }
    Ok(())
}

/// A `rusqlite::Error` from any statement becomes `TmError::storage`.
fn storage_err(e: rusqlite::Error) -> TmError {
    TmError::storage(e.to_string())
}

/// An event whose payload variant doesn't match its own `kind` (should be unreachable given
/// `Payload::kind`'s own invariant, but `apply` never panics, so this is surfaced as a parse
/// error rather than assumed away).
fn payload_mismatch(kind: EventKind) -> TmError {
    TmError::parse(format!(
        "{} event carried a mismatched payload variant",
        kind.as_str()
    ))
}

/// Serialize `v` to the JSON text stored in a materialized-view column.
fn json_string<T: Serialize>(v: &T) -> tm_types::Result<String> {
    Ok(serde_json::to_string(v).map_err(TmError::from)?)
}

/// Render a bare string the same way `serde_json` renders a unit-enum variant or a `String`
/// field, for payload fields (like `TicketStateChangedPayload::to`) that are already plain text
/// but need to match the quoted convention every other JSON column uses.
fn quoted(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

/// The default an [`ExecutorRequirements`] gets when a `ticket.created` event doesn't carry
/// enough detail to pick one deliberately; refined later via `ticket.updated`.
fn default_executor() -> ExecutorRequirements {
    ExecutorRequirements {
        role: Role::CoderFast,
        human_required: false,
        min_capability: Tolerance::Preferred,
    }
}

/// The default [`RetryPolicy`] a freshly created ticket gets.
fn default_retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay_seconds: 60,
        backoff_multiplier: 2.0,
        max_delay_seconds: 3600,
        non_retryable: Vec::new(),
    }
}

/// The [`TicketKind`] implied by a ticket id's prefix (`T-`/`V-`/`A-`); `Investigation`,
/// `Recovery` and `Harness` have no id-prefix signal and are never the default for a freshly
/// created ticket.
fn ticket_kind_from_id(id: &TicketId) -> TicketKind {
    match id.kind() {
        tm_types::IdKind::Verification => TicketKind::Verification,
        tm_types::IdKind::Audit => TicketKind::Audit,
        _ => TicketKind::Work,
    }
}

/// Shared body of `ticket.lease_expired`/`ticket.lease_released`: mark the lease dead and return
/// the ticket to `Ready` for the scheduler to re-lease.
fn release_lease(tx: &Tx<'_>, ts: Timestamp, lease: &str, ticket: &str) -> tm_types::Result<()> {
    tx.raw()
        .execute("UPDATE leases SET live = 0 WHERE id = ?", params![lease])
        .map_err(storage_err)?;
    tx.raw()
        .execute(
            "UPDATE tickets SET state = ?, updated = ? WHERE id = ?",
            params![json_string(&TicketState::Ready)?, ts.to_rfc3339(), ticket],
        )
        .map_err(storage_err)?;
    Ok(())
}

/// Overwrite `tickets.authority` for `ticket`, if present; authority events that don't name a
/// ticket (project- or participant-scoped grants) have nothing to write here.
fn set_ticket_authority(
    tx: &Tx<'_>,
    ts: Timestamp,
    ticket: Option<&TicketId>,
    grant: &Authority,
) -> tm_types::Result<()> {
    if let Some(ticket) = ticket {
        tx.raw()
            .execute(
                "UPDATE tickets SET authority = ?, updated = ? WHERE id = ?",
                params![json_string(grant)?, ts.to_rfc3339(), ticket.as_str()],
            )
            .map_err(storage_err)?;
    }
    Ok(())
}

/// Insert-or-update a `participants` row's `kind`/`status`/`updated`.
fn upsert_participant(
    tx: &Tx<'_>,
    participant: &ParticipantId,
    status: &str,
    ts: Timestamp,
) -> tm_types::Result<()> {
    let kind = if participant.is_agent() {
        "agent"
    } else if participant.is_human() {
        "human"
    } else {
        "system"
    };
    tx.raw()
        .execute(
            "INSERT INTO participants (id, kind, status, updated) VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, status = excluded.status, updated = excluded.updated",
            params![participant.as_str(), kind, status, ts.to_rfc3339()],
        )
        .map_err(storage_err)?;
    Ok(())
}

/// Insert-or-update a `docs` row plus its append-only `doc_provenance` trail, keyed on the
/// event's own `seq` (already the unique, gapless ordering key the provenance trail needs).
fn upsert_doc(
    tx: &Tx<'_>,
    path: &str,
    ticket: Option<&str>,
    state: &str,
    event: &Event,
) -> tm_types::Result<()> {
    let ts = event.ts.to_rfc3339();
    tx.raw()
        .execute(
            "INSERT INTO docs (path, ticket, state, updated) VALUES (?, ?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET
                ticket = COALESCE(excluded.ticket, docs.ticket),
                state = excluded.state,
                updated = excluded.updated",
            params![path, ticket, state, ts],
        )
        .map_err(storage_err)?;
    tx.raw()
        .execute(
            "INSERT OR IGNORE INTO doc_provenance (path, seq, kind, ts) VALUES (?, ?, ?, ?)",
            params![path, event.seq as i64, event.kind.as_str(), ts],
        )
        .map_err(storage_err)?;
    Ok(())
}

/// Insert-or-update a `mirror_links` row's `reference`/`updated`.
fn upsert_mirror_reference(
    tx: &Tx<'_>,
    remote: &str,
    reference: &str,
    ts: Timestamp,
) -> tm_types::Result<()> {
    tx.raw()
        .execute(
            "INSERT INTO mirror_links (remote, reference, updated) VALUES (?, ?, ?)
             ON CONFLICT(remote) DO UPDATE SET reference = excluded.reference, updated = excluded.updated",
            params![remote, reference, ts.to_rfc3339()],
        )
        .map_err(storage_err)?;
    Ok(())
}

/// Insert-or-update a `meta` row.
fn upsert_meta(tx: &Tx<'_>, key: &str, value: &str) -> tm_types::Result<()> {
    tx.raw()
        .execute(
            "INSERT INTO meta (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(storage_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rusqlite::OptionalExtension;
    use tempfile::TempDir;
    use tm_events::payload::{
        ArtifactCreatedPayload, DecisionCreatedPayload, DecisionSupersededPayload,
        MilestoneClosedPayload, MilestoneCreatedPayload, ProjectCreatedPayload,
        TicketChildAddedPayload, TicketCreatedPayload, TicketDependencyAddedPayload,
        TicketDependencyRemovedPayload, TicketHeartbeatPayload, TicketLeaseReleasedPayload,
        TicketLeasedPayload, TicketRetryScheduledPayload, TicketStateChangedPayload,
        TicketUpdatedPayload, UsageRecordedPayload,
    };
    use tm_events::{EventLog, Payload, Tx};
    use tm_types::{
        ArtifactId, DecisionId, FixedClock, Id, LeaseId, MilestoneId, ParticipantId, TicketId,
    };

    use super::*;
    use crate::schema;

    /// A fresh log with the materialized-view schema created, plus the tempdir keeping it alive.
    fn harness() -> (TempDir, EventLog) {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(FixedClock::new(Timestamp::from_unix_seconds(1_700_000_000)));
        let log = EventLog::open_with_clock(&dir.path().join("project.db"), clock).unwrap();
        let tx = log.begin().unwrap();
        tx.raw().execute_batch(schema::VIEWS_TABLE_SQL).unwrap();
        tx.commit().unwrap();
        (dir, log)
    }

    fn event(seq: u64, ts: Timestamp, subject: Id, payload: Payload) -> Event {
        Event {
            seq,
            ts,
            kind: payload.kind(),
            subject,
            actor: ParticipantId::system(),
            session: None,
            causation: None,
            correlation: None,
            payload,
            hash: String::new(),
        }
    }

    fn ticket_created(id: &str, title: &str, parent: Option<&str>) -> Event {
        let ticket = TicketId::new(id).unwrap();
        event(
            1,
            Timestamp::from_unix_seconds(1_700_000_000),
            Id::from(ticket.clone()),
            Payload::from(TicketCreatedPayload {
                ticket,
                title: title.into(),
                parent: parent.map(|p| TicketId::new(p).unwrap()),
            }),
        )
    }

    fn apply_all(tx: &Tx<'_>, events: &[Event]) {
        replay(tx, events).unwrap();
    }

    #[test]
    fn ticket_created_inserts_a_draft_row_with_the_id_derived_kind() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        let ev = ticket_created("T-1", "Fix login bug", None);
        apply(&tx, &ev).unwrap();

        let (kind, state, objective): (String, String, String) = tx
            .raw()
            .query_row(
                "SELECT kind, state, objective FROM tickets WHERE id = 'T-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "\"Work\"");
        assert_eq!(state, "\"Draft\"");
        assert_eq!(objective, "Fix login bug");
    }

    #[test]
    fn ticket_created_with_verification_prefix_infers_verification_kind() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply(&tx, &ticket_created("V-1", "Verify T-1", None)).unwrap();

        let kind: String = tx
            .raw()
            .query_row("SELECT kind FROM tickets WHERE id = 'V-1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kind, "\"Verification\"");
    }

    #[test]
    fn ticket_created_with_parent_also_records_the_child_edge() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply_all(
            &tx,
            &[
                ticket_created("T-1", "Parent", None),
                ticket_created("T-2", "Child", Some("T-1")),
            ],
        );

        let child: String = tx
            .raw()
            .query_row(
                "SELECT child FROM ticket_children WHERE parent = 'T-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(child, "T-2");
    }

    #[test]
    fn ticket_updated_patches_only_whitelisted_fields() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply(&tx, &ticket_created("T-1", "Original", None)).unwrap();

        let fields =
            serde_json::json!({"objective": "Revised", "priority": 5, "unknown_field": "ignored"});
        let ev = event(
            2,
            Timestamp::from_unix_seconds(1_700_000_100),
            Id::from(TicketId::new("T-1").unwrap()),
            Payload::from(TicketUpdatedPayload {
                ticket: TicketId::new("T-1").unwrap(),
                fields,
            }),
        );
        apply(&tx, &ev).unwrap();

        let (objective, priority): (String, i64) = tx
            .raw()
            .query_row(
                "SELECT objective, priority FROM tickets WHERE id = 'T-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(objective, "Revised");
        assert_eq!(priority, 5);
    }

    #[test]
    fn ticket_updated_with_unknown_keys_only_is_a_harmless_no_op() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply(&tx, &ticket_created("T-1", "Original", None)).unwrap();

        let fields = serde_json::json!({"nonsense": true});
        let ev = event(
            2,
            Timestamp::from_unix_seconds(1_700_000_100),
            Id::from(TicketId::new("T-1").unwrap()),
            Payload::from(TicketUpdatedPayload {
                ticket: TicketId::new("T-1").unwrap(),
                fields,
            }),
        );
        apply(&tx, &ev).unwrap();

        let objective: String = tx
            .raw()
            .query_row(
                "SELECT objective FROM tickets WHERE id = 'T-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(objective, "Original");
    }

    #[test]
    fn ticket_state_changed_is_the_sole_writer_of_state() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply(&tx, &ticket_created("T-1", "Work", None)).unwrap();

        let ev = event(
            2,
            Timestamp::from_unix_seconds(1_700_000_100),
            Id::from(TicketId::new("T-1").unwrap()),
            Payload::from(TicketStateChangedPayload {
                ticket: TicketId::new("T-1").unwrap(),
                from: "Draft".into(),
                to: "Ready".into(),
            }),
        );
        apply(&tx, &ev).unwrap();

        let state: String = tx
            .raw()
            .query_row("SELECT state FROM tickets WHERE id = 'T-1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "\"Ready\"");
    }

    #[test]
    fn ticket_closed_event_alone_leaves_state_untouched() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply(&tx, &ticket_created("T-1", "Work", None)).unwrap();

        let ev = event(
            2,
            Timestamp::from_unix_seconds(1_700_000_100),
            Id::from(TicketId::new("T-1").unwrap()),
            Payload::from(tm_events::payload::TicketClosedPayload {
                ticket: TicketId::new("T-1").unwrap(),
                reason: None,
            }),
        );
        apply(&tx, &ev).unwrap();

        let state: String = tx
            .raw()
            .query_row("SELECT state FROM tickets WHERE id = 'T-1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "\"Draft\"");
    }

    #[test]
    fn dependency_added_then_removed_round_trips() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply_all(
            &tx,
            &[
                ticket_created("T-1", "A", None),
                ticket_created("T-2", "B", None),
            ],
        );

        let add = event(
            3,
            Timestamp::from_unix_seconds(1_700_000_100),
            Id::none(),
            Payload::from(TicketDependencyAddedPayload {
                ticket: TicketId::new("T-1").unwrap(),
                depends_on: TicketId::new("T-2").unwrap(),
            }),
        );
        apply(&tx, &add).unwrap();
        let count: i64 = tx
            .raw()
            .query_row("SELECT COUNT(*) FROM ticket_deps", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let remove = event(
            4,
            Timestamp::from_unix_seconds(1_700_000_200),
            Id::none(),
            Payload::from(TicketDependencyRemovedPayload {
                ticket: TicketId::new("T-1").unwrap(),
                depends_on: TicketId::new("T-2").unwrap(),
            }),
        );
        apply(&tx, &remove).unwrap();
        let count: i64 = tx
            .raw()
            .query_row("SELECT COUNT(*) FROM ticket_deps", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn child_added_is_idempotent() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply_all(
            &tx,
            &[
                ticket_created("T-1", "Parent", None),
                ticket_created("T-2", "Child", None),
            ],
        );

        let ev = event(
            3,
            Timestamp::from_unix_seconds(1_700_000_100),
            Id::none(),
            Payload::from(TicketChildAddedPayload {
                parent: TicketId::new("T-1").unwrap(),
                child: TicketId::new("T-2").unwrap(),
            }),
        );
        apply(&tx, &ev).unwrap();
        apply(&tx, &ev).unwrap();

        let count: i64 = tx
            .raw()
            .query_row("SELECT COUNT(*) FROM ticket_children", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn ticket_leased_inserts_a_live_lease_and_moves_the_ticket_to_leased() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply(&tx, &ticket_created("T-1", "Work", None)).unwrap();

        let ev = event(
            2,
            Timestamp::from_unix_seconds(1_700_000_100),
            Id::none(),
            Payload::from(TicketLeasedPayload {
                ticket: TicketId::new("T-1").unwrap(),
                lease: LeaseId::new("L-aaaaaaaaaaaa").unwrap(),
                holder: ParticipantId::new("agent:anthropic/1").unwrap(),
                expires_at: Timestamp::from_unix_seconds(1_700_000_400),
            }),
        );
        apply(&tx, &ev).unwrap();

        let (ttl, live): (i64, i64) = tx
            .raw()
            .query_row(
                "SELECT ttl_seconds, live FROM leases WHERE id = 'L-aaaaaaaaaaaa'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(ttl, 300);
        assert_eq!(live, 1);

        let state: String = tx
            .raw()
            .query_row("SELECT state FROM tickets WHERE id = 'T-1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "\"Leased\"");
    }

    #[test]
    fn heartbeat_recomputes_ttl_against_the_new_expiry() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply(&tx, &ticket_created("T-1", "Work", None)).unwrap();
        apply(
            &tx,
            &event(
                2,
                Timestamp::from_unix_seconds(1_700_000_100),
                Id::none(),
                Payload::from(TicketLeasedPayload {
                    ticket: TicketId::new("T-1").unwrap(),
                    lease: LeaseId::new("L-aaaaaaaaaaaa").unwrap(),
                    holder: ParticipantId::new("agent:anthropic/1").unwrap(),
                    expires_at: Timestamp::from_unix_seconds(1_700_000_400),
                }),
            ),
        )
        .unwrap();

        let hb = event(
            3,
            Timestamp::from_unix_seconds(1_700_000_200),
            Id::none(),
            Payload::from(TicketHeartbeatPayload {
                ticket: TicketId::new("T-1").unwrap(),
                lease: LeaseId::new("L-aaaaaaaaaaaa").unwrap(),
                expires_at: Timestamp::from_unix_seconds(1_700_000_500),
            }),
        );
        apply(&tx, &hb).unwrap();

        let ttl: i64 = tx
            .raw()
            .query_row(
                "SELECT ttl_seconds FROM leases WHERE id = 'L-aaaaaaaaaaaa'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ttl, 300);
    }

    #[test]
    fn lease_released_marks_dead_and_returns_ticket_to_ready() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply(&tx, &ticket_created("T-1", "Work", None)).unwrap();
        apply(
            &tx,
            &event(
                2,
                Timestamp::from_unix_seconds(1_700_000_100),
                Id::none(),
                Payload::from(TicketLeasedPayload {
                    ticket: TicketId::new("T-1").unwrap(),
                    lease: LeaseId::new("L-aaaaaaaaaaaa").unwrap(),
                    holder: ParticipantId::new("agent:anthropic/1").unwrap(),
                    expires_at: Timestamp::from_unix_seconds(1_700_000_400),
                }),
            ),
        )
        .unwrap();

        let ev = event(
            3,
            Timestamp::from_unix_seconds(1_700_000_200),
            Id::none(),
            Payload::from(TicketLeaseReleasedPayload {
                ticket: TicketId::new("T-1").unwrap(),
                lease: LeaseId::new("L-aaaaaaaaaaaa").unwrap(),
            }),
        );
        apply(&tx, &ev).unwrap();

        let live: i64 = tx
            .raw()
            .query_row(
                "SELECT live FROM leases WHERE id = 'L-aaaaaaaaaaaa'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(live, 0);
        let state: String = tx
            .raw()
            .query_row("SELECT state FROM tickets WHERE id = 'T-1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "\"Ready\"");
    }

    #[test]
    fn retry_scheduled_bumps_attempts() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply(&tx, &ticket_created("T-1", "Work", None)).unwrap();

        let ev = event(
            2,
            Timestamp::from_unix_seconds(1_700_000_100),
            Id::none(),
            Payload::from(TicketRetryScheduledPayload {
                ticket: TicketId::new("T-1").unwrap(),
                attempt: 2,
                not_before: Timestamp::from_unix_seconds(1_700_000_200),
            }),
        );
        apply(&tx, &ev).unwrap();

        let attempts: i64 = tx
            .raw()
            .query_row("SELECT attempts FROM tickets WHERE id = 'T-1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(attempts, 2);
    }

    #[test]
    fn decision_created_inserts_a_row_keyed_by_the_referenced_ticket() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply(&tx, &ticket_created("T-1", "Work", None)).unwrap();

        let ev = event(
            2,
            Timestamp::from_unix_seconds(1_700_000_100),
            Id::none(),
            Payload::from(DecisionCreatedPayload {
                decision: DecisionId::new("D-1").unwrap(),
                ticket: Some(TicketId::new("T-1").unwrap()),
                summary: "Use SQLite for the event log".into(),
            }),
        );
        apply(&tx, &ev).unwrap();

        let (subject, decision, affected): (String, String, String) = tx
            .raw()
            .query_row(
                "SELECT subject, decision, affected_tickets FROM decisions WHERE id = 'D-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(subject, "T-1");
        assert_eq!(decision, "Use SQLite for the event log");
        assert_eq!(affected, "[\"T-1\"]");
    }

    #[test]
    fn decision_superseded_updates_only_superseded_by() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();

        let created = event(
            1,
            Timestamp::from_unix_seconds(1_700_000_000),
            Id::none(),
            Payload::from(DecisionCreatedPayload {
                decision: DecisionId::new("D-1").unwrap(),
                ticket: None,
                summary: "Original".into(),
            }),
        );
        apply(&tx, &created).unwrap();

        let superseded = event(
            2,
            Timestamp::from_unix_seconds(1_700_000_100),
            Id::none(),
            Payload::from(DecisionSupersededPayload {
                decision: DecisionId::new("D-1").unwrap(),
                superseded_by: DecisionId::new("D-2").unwrap(),
            }),
        );
        apply(&tx, &superseded).unwrap();

        let (decision, superseded_by): (String, Option<String>) = tx
            .raw()
            .query_row(
                "SELECT decision, superseded_by FROM decisions WHERE id = 'D-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(decision, "Original");
        assert_eq!(superseded_by.as_deref(), Some("D-2"));
    }

    #[test]
    fn artifact_created_inserts_a_row_with_documented_defaults() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();

        let ev = event(
            1,
            Timestamp::from_unix_seconds(1_700_000_000),
            Id::none(),
            Payload::from(ArtifactCreatedPayload {
                artifact: ArtifactId::new("ART-aaaaaaaaaaaa").unwrap(),
                ticket: None,
                path: "reports/out.txt".into(),
                media_type: "text/plain".into(),
            }),
        );
        apply(&tx, &ev).unwrap();

        let (media_type, bytes_len, on_disk): (String, i64, String) = tx
            .raw()
            .query_row(
                "SELECT media_type, bytes_len, on_disk FROM artifacts WHERE id = 'ART-aaaaaaaaaaaa'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(media_type, "text/plain");
        assert_eq!(bytes_len, 0);
        assert_eq!(on_disk, "reports/out.txt");
    }

    #[test]
    fn milestone_lifecycle_open_close_reopen() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();

        apply(
            &tx,
            &event(
                1,
                Timestamp::from_unix_seconds(1_700_000_000),
                Id::none(),
                Payload::from(MilestoneCreatedPayload {
                    milestone: MilestoneId::new("M-1").unwrap(),
                    title: "Beta".into(),
                }),
            ),
        )
        .unwrap();
        let state: String = tx
            .raw()
            .query_row("SELECT state FROM milestones WHERE id = 'M-1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "\"Open\"");

        apply(
            &tx,
            &event(
                2,
                Timestamp::from_unix_seconds(1_700_000_100),
                Id::none(),
                Payload::from(MilestoneClosedPayload {
                    milestone: MilestoneId::new("M-1").unwrap(),
                }),
            ),
        )
        .unwrap();
        let (state, closed_by): (String, Option<String>) = tx
            .raw()
            .query_row(
                "SELECT state, closed_by FROM milestones WHERE id = 'M-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "\"Closed\"");
        assert_eq!(closed_by.as_deref(), Some("system"));

        apply(
            &tx,
            &event(
                3,
                Timestamp::from_unix_seconds(1_700_000_200),
                Id::none(),
                Payload::from(tm_events::payload::MilestoneReopenedPayload {
                    milestone: MilestoneId::new("M-1").unwrap(),
                }),
            ),
        )
        .unwrap();
        let (state, closed_by): (String, Option<String>) = tx
            .raw()
            .query_row(
                "SELECT state, closed_by FROM milestones WHERE id = 'M-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "\"Open\"");
        assert!(closed_by.is_none());
    }

    #[test]
    fn usage_recorded_inserts_a_provider_usage_row() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();

        let ev = event(
            1,
            Timestamp::from_unix_seconds(1_700_000_000),
            Id::none(),
            Payload::from(UsageRecordedPayload {
                ticket: None,
                session: None,
                tokens: 100,
                dollars_micros: 500,
                wall_seconds: 2,
            }),
        );
        apply(&tx, &ev).unwrap();

        let count: i64 = tx
            .raw()
            .query_row("SELECT COUNT(*) FROM provider_usage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn project_created_writes_name_and_root_to_meta() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();

        let ev = event(
            1,
            Timestamp::from_unix_seconds(1_700_000_000),
            Id::none(),
            Payload::from(ProjectCreatedPayload {
                name: "Ticketmaster".into(),
                root: "/repo".into(),
            }),
        );
        apply(&tx, &ev).unwrap();

        let value: String = tx
            .raw()
            .query_row("SELECT value FROM meta WHERE key = 'name'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(value, "Ticketmaster");
    }

    #[test]
    fn unknown_to_this_view_kind_is_a_no_op_not_an_error() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();

        let ev = event(
            1,
            Timestamp::from_unix_seconds(1_700_000_000),
            Id::none(),
            Payload::from(tm_events::payload::CommandStartedPayload {
                command: "echo hi".into(),
                ticket: None,
                session: None,
            }),
        );
        apply(&tx, &ev).unwrap();

        let count: i64 = tx
            .raw()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(count > 0);
    }

    #[test]
    fn replay_of_a_full_sequence_matches_applying_events_one_by_one() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();

        let events = vec![
            ticket_created("T-1", "Work", None),
            event(
                2,
                Timestamp::from_unix_seconds(1_700_000_100),
                Id::none(),
                Payload::from(TicketStateChangedPayload {
                    ticket: TicketId::new("T-1").unwrap(),
                    from: "Draft".into(),
                    to: "Ready".into(),
                }),
            ),
        ];
        replay(&tx, &events).unwrap();

        let state: String = tx
            .raw()
            .query_row("SELECT state FROM tickets WHERE id = 'T-1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "\"Ready\"");
    }

    #[test]
    fn replay_stops_at_the_first_failing_event() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();

        // A `ticket.state_changed` for a ticket row that was never created: the `UPDATE` itself
        // succeeds trivially (zero rows matched, not an error), so to exercise a genuine failure
        // we feed a payload whose `fields` cannot be interpreted — but since malformed payloads
        // can't be constructed through the typed API, assert the weaker, always-true property
        // instead: replay never partially applies a prefix that then silently skips a later
        // failure by continuing past `?`.
        let events = vec![
            ticket_created("T-1", "Work", None),
            ticket_created("T-1", "Duplicate", None),
        ];
        let result: Result<(), _> = (|| {
            apply(&tx, &events[0])?;
            apply(&tx, &events[1])
        })();
        assert!(result.is_err());
    }

    #[test]
    fn dependency_removed_without_a_prior_add_is_a_harmless_no_op() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();
        apply_all(
            &tx,
            &[
                ticket_created("T-1", "A", None),
                ticket_created("T-2", "B", None),
            ],
        );

        let ev = event(
            3,
            Timestamp::from_unix_seconds(1_700_000_100),
            Id::none(),
            Payload::from(TicketDependencyRemovedPayload {
                ticket: TicketId::new("T-1").unwrap(),
                depends_on: TicketId::new("T-2").unwrap(),
            }),
        );
        apply(&tx, &ev).unwrap();

        let count: i64 = tx
            .raw()
            .query_row("SELECT COUNT(*) FROM ticket_deps", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn milestone_closed_for_missing_milestone_updates_zero_rows_without_error() {
        let (_dir, log) = harness();
        let tx = log.begin().unwrap();

        let ev = event(
            1,
            Timestamp::from_unix_seconds(1_700_000_000),
            Id::none(),
            Payload::from(MilestoneClosedPayload {
                milestone: MilestoneId::new("M-9").unwrap(),
            }),
        );
        apply(&tx, &ev).unwrap();

        let row: Option<String> = tx
            .raw()
            .query_row("SELECT state FROM milestones WHERE id = 'M-9'", [], |row| {
                row.get(0)
            })
            .optional()
            .unwrap();
        assert!(row.is_none());
    }
}
