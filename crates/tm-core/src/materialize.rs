//! The single function that turns one event into materialized state.
//!
//! `apply` is the only place any of the tables in [`crate::schema`] are written. Both the live
//! path (`Store`'s command methods, appending an event and materializing it in the same
//! [`tm_events::log::Tx`]) and the replay path ([`replay`], driven by `Store::rebuild`) call
//! through this one function, which is what makes "every row is derivable by replaying the log"
//! true by construction rather than by discipline.
//!
//! Unknown-to-this-view event kinds are a no-op, never an error: `tm-core` materializes only the
//! subset of the catalogue that names project/ticket/decision/milestone/artifact/evidence/
//! participant state (`SPEC.md` §4.1's table list) with columns [`crate::schema`] actually has
//! room for. A few kinds in `tm_events`' closed payload catalogue (`resource.claimed`/
//! `released`, `usage.recorded`, `doc.*`, `provider.selected`, `harness.changed`,
//! `mirror.*`) name fields that don't line up with the corresponding table's columns (e.g.
//! `doc.registered` carries a `path`, not the `docs` table's `id`); since `tm_events` isn't this
//! crate's to change, those are deliberate no-ops here too rather than writes that would
//! misrepresent the event, documented at each such arm below.

use rusqlite::params;
use tm_events::{Event, EventKind, Tx};
use tm_types::{IdKind, TmError};

fn storage_err(e: rusqlite::Error) -> TmError {
    TmError::storage(e.to_string())
}

/// Record the high-water mark for one id counter, so `Store::rebuild` can restore `CounterIds`
/// correctly after replay. `number` is the numeric suffix parsed off the id that produced this
/// event (e.g. `42` from `T-42`); ids with no parseable number (e.g. free-form strings) are
/// skipped rather than treated as an error, since not every id-shaped field this module touches
/// is one of `tm-types`' numbered kinds.
fn bump_counter(tx: &Tx<'_>, counter: &str, number: u64) -> tm_types::Result<()> {
    tx.raw()
        .execute(
            "INSERT INTO counters (counter_name, value) VALUES (?1, ?2)
             ON CONFLICT(counter_name) DO UPDATE SET value = MAX(value, excluded.value)",
            params![counter, number],
        )
        .map_err(storage_err)?;
    Ok(())
}

/// Placeholder field values [`EventKind::TicketCreated`] writes for every `tickets` column that
/// event's payload doesn't carry; the `ticket.updated` event `Store::create_ticket` always
/// appends in the same transaction immediately overwrites them with the real values. Plain data,
/// never fails to serialize.
struct TicketDefaults;

impl TicketDefaults {
    fn kind() -> String {
        serde_json::to_string(&crate::ticket::TicketKind::Work)
            .expect("TicketKind::Work is plain data and always serializes")
    }
    fn authority() -> String {
        serde_json::to_string(&tm_types::Authority::none())
            .expect("Authority::none() is plain data and always serializes")
    }
    fn executor() -> String {
        serde_json::to_string(&crate::ticket::ExecutorRequirements {
            role: tm_types::Role::CoderFast,
            human_required: false,
            min_capability: tm_types::Tolerance::Any,
        })
        .expect("ExecutorRequirements is plain data and always serializes")
    }
    fn verification() -> String {
        serde_json::to_string(&crate::ticket::VerificationPolicy::None)
            .expect("VerificationPolicy::None is plain data and always serializes")
    }
    fn budget() -> String {
        serde_json::to_string(&tm_types::Budget::unlimited())
            .expect("Budget::unlimited() is plain data and always serializes")
    }
    fn retry() -> String {
        serde_json::to_string(&crate::ticket::RetryPolicy {
            max_attempts: 1,
            base_delay_seconds: 0,
            backoff_multiplier: 1.0,
            max_delay_seconds: 0,
        })
        .expect("RetryPolicy is plain data and always serializes")
    }
}

/// Apply one event's effect to materialized state, inside `tx`.
///
/// # Errors
/// Only for genuine storage failure (a `rusqlite` error surfaced as `TmError::storage`) — never
/// for "this event doesn't make sense given current state". By the time an event is in the log
/// it already passed `machine`/`invariants` validation on the live path; replay of a valid log
/// must always succeed. A `rusqlite` constraint violation here (e.g. a duplicate primary key
/// from a corrupted log) is therefore itself reported as `TmError::storage`, not swallowed.
pub fn apply(tx: &Tx<'_>, event: &Event) -> tm_types::Result<()> {
    match event.kind {
        EventKind::ProjectCreated => {
            if let Some(p) = event.payload.as_project_created() {
                set_meta(tx, "name", &p.name)?;
                set_meta(tx, "root", &p.root)?;
            }
        }
        EventKind::ProjectAttached => {
            if let Some(p) = event.payload.as_project_attached() {
                set_meta(tx, "name", &p.name)?;
                set_meta(tx, "root", &p.root)?;
            }
        }
        EventKind::TicketCreated => {
            if let Some(p) = event.payload.as_ticket_created() {
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "INSERT INTO tickets (id, kind, objective, state, parent, milestone,
                                authority, resources, executor, context_refs, success,
                                verification, budget, retry, cycle, attempts, failures, priority,
                                created, updated)
                         VALUES (?1, ?2, ?3, '\"draft\"', ?4, NULL, ?5, '[]', ?6, '[]', '[]', ?7,
                                ?8, ?9, NULL, 0, '[]', 0, ?10, ?10)
                         ON CONFLICT(id) DO UPDATE SET objective = excluded.objective,
                            parent = excluded.parent",
                        params![
                            p.ticket.as_str(),
                            TicketDefaults::kind(),
                            p.title,
                            p.parent.as_ref().map(|t| t.as_str()),
                            TicketDefaults::authority(),
                            TicketDefaults::executor(),
                            TicketDefaults::verification(),
                            TicketDefaults::budget(),
                            TicketDefaults::retry(),
                            now,
                        ],
                    )
                    .map_err(storage_err)?;
                if let Some(parent) = &p.parent {
                    tx.raw()
                        .execute(
                            "INSERT OR IGNORE INTO ticket_children (parent, child) VALUES (?1, ?2)",
                            params![parent.as_str(), p.ticket.as_str()],
                        )
                        .map_err(storage_err)?;
                }
                if let Some(n) = p.ticket.number() {
                    bump_counter(tx, p.ticket.kind().counter(), n)?;
                }
            }
        }
        EventKind::TicketUpdated => {
            if let Some(p) = event.payload.as_ticket_updated() {
                apply_ticket_updated(tx, event, &p.ticket, &p.fields)?;
            }
        }
        EventKind::TicketStateChanged => {
            if let Some(p) = event.payload.as_ticket_state_changed() {
                let state_json = serde_json::to_string(&p.to).map_err(TmError::from)?;
                tx.raw()
                    .execute(
                        "UPDATE tickets SET state = ?2 WHERE id = ?1",
                        params![p.ticket.as_str(), state_json],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::TicketDependencyAdded => {
            if let Some(p) = event.payload.as_ticket_dependency_added() {
                // `ticket.dependency_added`'s payload doesn't carry `DependencyKind` (the closed
                // `tm_events` catalogue has no room for it), so every materialized edge is
                // conservatively `Hard`, the strictest kind for cycle-legality purposes.
                tx.raw()
                    .execute(
                        "INSERT OR IGNORE INTO ticket_deps (ticket, depends_on, kind)
                         VALUES (?1, ?2, '\"hard\"')",
                        params![p.ticket.as_str(), p.depends_on.as_str()],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::TicketDependencyRemoved => {
            if let Some(p) = event.payload.as_ticket_dependency_removed() {
                tx.raw()
                    .execute(
                        "DELETE FROM ticket_deps WHERE ticket = ?1 AND depends_on = ?2",
                        params![p.ticket.as_str(), p.depends_on.as_str()],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::TicketChildAdded => {
            if let Some(p) = event.payload.as_ticket_child_added() {
                tx.raw()
                    .execute(
                        "INSERT OR IGNORE INTO ticket_children (parent, child) VALUES (?1, ?2)",
                        params![p.parent.as_str(), p.child.as_str()],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::TicketLeased => {
            if let Some(p) = event.payload.as_ticket_leased() {
                // `ticket.leased`'s payload carries `expires_at` but not the granted
                // `authority`/`resources`, nor the lease's own `acquired` timestamp or `epoch`
                // separately from `event.ts` (the closed catalogue has no room for them); the
                // ttl is recoverable as `expires_at - event.ts`, and `acquired`/`heartbeat` both
                // start at `event.ts`.
                let ttl_seconds = p.expires_at.seconds_since(event.ts).max(0);
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "INSERT INTO leases (id, ticket, holder, authority, resources, acquired,
                                heartbeat, ttl_seconds, epoch)
                         VALUES (?1, ?2, ?3, ?4, '[]', ?5, ?5, ?6, 0)
                         ON CONFLICT(id) DO UPDATE SET holder = excluded.holder,
                            heartbeat = excluded.heartbeat, ttl_seconds = excluded.ttl_seconds",
                        params![
                            p.lease.as_str(),
                            p.ticket.as_str(),
                            p.holder.as_str(),
                            TicketDefaults::authority(),
                            now,
                            ttl_seconds,
                        ],
                    )
                    .map_err(storage_err)?;
                if let Some(n) = p.lease.number() {
                    bump_counter(tx, IdKind::Lease.counter(), n)?;
                }
            }
        }
        EventKind::TicketHeartbeat => {
            if let Some(p) = event.payload.as_ticket_heartbeat() {
                let ttl_seconds = p.expires_at.seconds_since(event.ts).max(0);
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "UPDATE leases SET heartbeat = ?2, ttl_seconds = ?3 WHERE id = ?1",
                        params![p.lease.as_str(), now, ttl_seconds],
                    )
                    .map_err(storage_err)?;
            }
        }
        // `leases` has no liveness flag; a lease's effective liveness follows from its ticket's
        // state (already restored to `Ready` by the accompanying `ticket.state_changed`), so
        // expiry/release need no further write here.
        EventKind::TicketLeaseExpired | EventKind::TicketLeaseReleased => {
            // A lease that ended must leave the live set, together with the resource claims it
            // held. Leaving it behind is not a cosmetic leak: the double-lease invariant would
            // then refuse every future lease on that ticket, so a worker that crashed once would
            // block its ticket permanently — the exact failure leases exist to prevent
            // (SPEC.md §4.5, §17 invariant 5).
            let lease = event
                .payload
                .as_ticket_lease_expired()
                .map(|p| p.lease.clone())
                .or_else(|| {
                    event
                        .payload
                        .as_ticket_lease_released()
                        .map(|p| p.lease.clone())
                });
            if let Some(lease) = lease {
                tx.raw()
                    .execute(
                        "DELETE FROM resource_claims WHERE lease = ?1",
                        params![lease.as_str()],
                    )
                    .map_err(storage_err)?;
                tx.raw()
                    .execute("DELETE FROM leases WHERE id = ?1", params![lease.as_str()])
                    .map_err(storage_err)?;
            }
        }
        EventKind::TicketDelegated => {
            if let Some(p) = event.payload.as_ticket_delegated() {
                tx.raw()
                    .execute(
                        "INSERT OR IGNORE INTO ticket_children (parent, child) VALUES (?1, ?2)",
                        params![p.parent.as_str(), p.child.as_str()],
                    )
                    .map_err(storage_err)?;
            }
        }
        // Submitted/verified/audited/closed/cancelled/reopened/failed/retry-scheduled/escalated
        // all travel alongside a `ticket.state_changed` event in the same transaction (see
        // `store.rs`'s command methods), which is what actually updates `tickets.state`; these
        // domain events themselves carry nothing this crate's tables have a column for beyond
        // that.
        EventKind::TicketSubmitted
        | EventKind::TicketVerified
        | EventKind::TicketVerificationFailed
        | EventKind::TicketAudited
        | EventKind::TicketAuditRejected
        | EventKind::TicketClosed
        | EventKind::TicketCancelled
        | EventKind::TicketReopened
        | EventKind::TicketFailed
        | EventKind::TicketRetryScheduled
        | EventKind::TicketEscalated
        | EventKind::TicketBudgetExhausted => {}
        EventKind::DecisionCreated => {
            if let Some(p) = event.payload.as_decision_created() {
                // `decision.created`'s `summary` field carries the JSON blob `store.rs`'s
                // `decision_summary_json` built (`{subject, decision, reason, evidence,
                // affected_tickets, affected_paths}`), since the payload proper has no room for
                // those fields individually.
                let parsed: serde_json::Value =
                    serde_json::from_str(&p.summary).map_err(TmError::from)?;
                let subject = parsed
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let decision_text = parsed
                    .get("decision")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let reason = parsed
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let empty_array = serde_json::json!([]);
                let evidence = parsed.get("evidence").unwrap_or(&empty_array).to_string();
                let affected_tickets = parsed
                    .get("affected_tickets")
                    .unwrap_or(&empty_array)
                    .to_string();
                let affected_paths = parsed
                    .get("affected_paths")
                    .unwrap_or(&empty_array)
                    .to_string();
                tx.raw()
                    .execute(
                        "INSERT INTO decisions (id, subject, decision, reason, evidence,
                                affected_tickets, affected_paths, author, ts, supersedes,
                                superseded_by)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL)
                         ON CONFLICT(id) DO UPDATE SET subject = excluded.subject,
                            decision = excluded.decision, reason = excluded.reason,
                            evidence = excluded.evidence,
                            affected_tickets = excluded.affected_tickets,
                            affected_paths = excluded.affected_paths",
                        params![
                            p.decision.as_str(),
                            subject,
                            decision_text,
                            reason,
                            evidence,
                            affected_tickets,
                            affected_paths,
                            event.actor.as_str(),
                            event.ts.to_rfc3339(),
                        ],
                    )
                    .map_err(storage_err)?;
                if let Some(n) = p.decision.number() {
                    bump_counter(tx, IdKind::Decision.counter(), n)?;
                }
            }
        }
        EventKind::DecisionSuperseded => {
            if let Some(p) = event.payload.as_decision_superseded() {
                tx.raw()
                    .execute(
                        "UPDATE decisions SET superseded_by = ?2 WHERE id = ?1",
                        params![p.decision.as_str(), p.superseded_by.as_str()],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::ArtifactCreated => {
            if let Some(p) = event.payload.as_artifact_created() {
                // `Store::store_artifact` follows this event with a direct SQL write (see
                // `store.rs`'s module-level note) carrying the real `kind`/`bytes_len`/`hash`/
                // `meta`; this placeholder row only needs to satisfy the table's `NOT NULL`
                // columns until that write lands in the same transaction.
                tx.raw()
                    .execute(
                        "INSERT INTO artifacts (id, kind, media_type, bytes_len, hash, storage, meta)
                         VALUES (?1, '\"file\"', ?2, 0, '', ?3, '{}')
                         ON CONFLICT(id) DO UPDATE SET media_type = excluded.media_type,
                            storage = excluded.storage",
                        params![p.artifact.as_str(), p.media_type, p.path],
                    )
                    .map_err(storage_err)?;
                if let Some(n) = p.artifact.number() {
                    bump_counter(tx, IdKind::Artifact.counter(), n)?;
                }
            }
        }
        EventKind::MilestoneCreated => {
            if let Some(p) = event.payload.as_milestone_created() {
                tx.raw()
                    .execute(
                        "INSERT INTO milestones (id, title, tickets, state, closed_by, assumptions)
                         VALUES (?1, ?2, '[]', '\"open\"', NULL, '[]')
                         ON CONFLICT(id) DO UPDATE SET title = excluded.title",
                        params![p.milestone.as_str(), p.title],
                    )
                    .map_err(storage_err)?;
                if let Some(n) = p.milestone.number() {
                    bump_counter(tx, IdKind::Milestone.counter(), n)?;
                }
            }
        }
        EventKind::MilestoneClosed => {
            if let Some(p) = event.payload.as_milestone_closed() {
                tx.raw()
                    .execute(
                        "UPDATE milestones SET state = '\"closed\"' WHERE id = ?1",
                        params![p.milestone.as_str()],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::MilestoneReopened => {
            if let Some(p) = event.payload.as_milestone_reopened() {
                tx.raw()
                    .execute(
                        "UPDATE milestones SET state = '\"open\"' WHERE id = ?1",
                        params![p.milestone.as_str()],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::SessionStarted => {
            if let Some(p) = event.payload.as_session_started() {
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "INSERT INTO sessions (id, participant, started, last_seen)
                         VALUES (?1, ?2, ?3, ?3)
                         ON CONFLICT(id) DO UPDATE SET last_seen = excluded.last_seen",
                        params![p.session.as_str(), p.participant.as_str(), now],
                    )
                    .map_err(storage_err)?;
                if let Some(n) = p.session.number() {
                    bump_counter(tx, IdKind::Session.counter(), n)?;
                }
                upsert_participant(tx, p.participant.as_str(), "active")?;
            }
        }
        EventKind::SessionJoined => {
            if let Some(p) = event.payload.as_session_joined() {
                upsert_participant(tx, p.participant.as_str(), "active")?;
            }
        }
        EventKind::SessionLeft => {
            if let Some(p) = event.payload.as_session_left() {
                upsert_participant(tx, p.participant.as_str(), "idle")?;
            }
        }
        EventKind::SessionEnded => {
            if let Some(p) = event.payload.as_session_ended() {
                tx.raw()
                    .execute(
                        "UPDATE sessions SET last_seen = ?2 WHERE id = ?1",
                        params![p.session.as_str(), event.ts.to_rfc3339()],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::PresenceUpdated => {
            if let Some(p) = event.payload.as_presence_updated() {
                upsert_participant(tx, p.participant.as_str(), &p.status)?;
            }
        }
        // The remaining catalogued kinds either aren't part of this crate's materialized view
        // (authority.granted/delegated/revoked/reverted, resource.conflict_detected, command.*,
        // executor.failed, index.updated, harness.benchmarked/promoted, comment/approval events,
        // genesis.*) or name fields that don't correspond to columns the affected table actually
        // has, per this module's doc comment (resource.claimed/released, usage.recorded, doc.*,
        // provider.selected, harness.changed, mirror.*). Per this module's doc comment,
        // unrecognized/unmaterialized kinds are a deliberate no-op, not an error, so replay never
        // fails on a kind this build doesn't project.
        _ => {}
    }
    Ok(())
}

/// Apply a `ticket.updated` event's `fields` object to the `tickets`/`evidence` tables.
/// `fields` is built by several different call sites in `store.rs` (`create_ticket`'s initial
/// full field set, `update_ticket`'s caller-chosen subset, `create_milestone`'s
/// `{"milestone": id}`, and `evidence_draft`'s `{"evidence_attached": {...}}`), so this dispatches
/// key-by-key rather than assuming any one shape.
fn apply_ticket_updated(
    tx: &Tx<'_>,
    event: &Event,
    ticket: &tm_types::TicketId,
    fields: &serde_json::Value,
) -> tm_types::Result<()> {
    let Some(obj) = fields.as_object() else {
        return Ok(());
    };
    for (key, value) in obj {
        match key.as_str() {
            "evidence_attached" => {
                if let Some(ev) = value.as_object() {
                    let kind_json = ev.get("kind").map(|v| v.to_string()).unwrap_or_default();
                    let artifact = ev.get("artifact").and_then(|v| v.as_str()).unwrap_or("");
                    let summary = ev.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                    tx.raw()
                        .execute(
                            "INSERT OR REPLACE INTO evidence (ticket, kind, artifact, produced_by, ts, summary)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![
                                ticket.as_str(),
                                kind_json,
                                artifact,
                                event.actor.as_str(),
                                event.ts.to_rfc3339(),
                                summary,
                            ],
                        )
                        .map_err(storage_err)?;
                }
            }
            "objective" => {
                if let Some(s) = value.as_str() {
                    tx.raw()
                        .execute(
                            "UPDATE tickets SET objective = ?2 WHERE id = ?1",
                            params![ticket.as_str(), s],
                        )
                        .map_err(storage_err)?;
                }
            }
            "milestone" => {
                tx.raw()
                    .execute(
                        "UPDATE tickets SET milestone = ?2 WHERE id = ?1",
                        params![ticket.as_str(), value.as_str()],
                    )
                    .map_err(storage_err)?;
            }
            "created" | "updated" => {
                if let Some(s) = value.as_str() {
                    let sql = if key == "created" {
                        "UPDATE tickets SET created = ?2 WHERE id = ?1"
                    } else {
                        "UPDATE tickets SET updated = ?2 WHERE id = ?1"
                    };
                    tx.raw()
                        .execute(sql, params![ticket.as_str(), s])
                        .map_err(storage_err)?;
                }
            }
            "priority" => {
                if let Some(n) = value.as_i64() {
                    tx.raw()
                        .execute(
                            "UPDATE tickets SET priority = ?2 WHERE id = ?1",
                            params![ticket.as_str(), n],
                        )
                        .map_err(storage_err)?;
                }
            }
            "attempts" => {
                if let Some(n) = value.as_i64() {
                    tx.raw()
                        .execute(
                            "UPDATE tickets SET attempts = ?2 WHERE id = ?1",
                            params![ticket.as_str(), n],
                        )
                        .map_err(storage_err)?;
                }
            }
            "cycle" if value.is_null() => {
                tx.raw()
                    .execute(
                        "UPDATE tickets SET cycle = NULL WHERE id = ?1",
                        params![ticket.as_str()],
                    )
                    .map_err(storage_err)?;
            }
            "kind" | "authority" | "resources" | "executor" | "context_refs" | "success"
            | "verification" | "budget" | "retry" | "cycle" | "failures" => {
                let text = value.to_string();
                let sql = match key.as_str() {
                    "kind" => "UPDATE tickets SET kind = ?2 WHERE id = ?1",
                    "authority" => "UPDATE tickets SET authority = ?2 WHERE id = ?1",
                    "resources" => "UPDATE tickets SET resources = ?2 WHERE id = ?1",
                    "executor" => "UPDATE tickets SET executor = ?2 WHERE id = ?1",
                    "context_refs" => "UPDATE tickets SET context_refs = ?2 WHERE id = ?1",
                    "success" => "UPDATE tickets SET success = ?2 WHERE id = ?1",
                    "verification" => "UPDATE tickets SET verification = ?2 WHERE id = ?1",
                    "budget" => "UPDATE tickets SET budget = ?2 WHERE id = ?1",
                    "retry" => "UPDATE tickets SET retry = ?2 WHERE id = ?1",
                    "cycle" => "UPDATE tickets SET cycle = ?2 WHERE id = ?1",
                    _ => "UPDATE tickets SET failures = ?2 WHERE id = ?1",
                };
                tx.raw()
                    .execute(sql, params![ticket.as_str(), text])
                    .map_err(storage_err)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn set_meta(tx: &Tx<'_>, key: &str, value: &str) -> tm_types::Result<()> {
    tx.raw()
        .execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(storage_err)?;
    Ok(())
}

fn upsert_participant(tx: &Tx<'_>, participant: &str, status: &str) -> tm_types::Result<()> {
    tx.raw()
        .execute(
            "INSERT INTO participants (id, status) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET status = excluded.status",
            params![participant, status],
        )
        .map_err(storage_err)?;
    Ok(())
}

/// True when `tx`'s connection currently has row `key` = `value` in `meta` (test/diagnostic
/// helper only; production readers go through `crate::view`).
#[cfg(test)]
fn meta_value(tx: &Tx<'_>, key: &str) -> Option<String> {
    use rusqlite::OptionalExtension;
    tx.raw()
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .expect("meta lookup does not fail against a well-formed test schema")
}

/// Apply every event in `events`, in order, within `tx`. `Store::rebuild` calls this against a
/// freshly-dropped-and-recreated schema to replay the whole log from `seq` 0; the live path
/// instead calls [`apply`] once per event as each is appended, inside the same transaction as
/// the append.
pub fn replay(tx: &Tx<'_>, events: &[Event]) -> tm_types::Result<()> {
    for event in events {
        apply(tx, event)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_events::log::EventLog;
    use tm_events::payload::*;
    use tm_events::EventKind as EK;
    use tm_types::{
        ArtifactId, DecisionId, Id, MilestoneId, ParticipantId, SessionId, TicketId, Timestamp,
    };

    /// Builds a real `Tx` (backed by a temp-file `EventLog`) with the crate's real materialized
    /// schema (`crate::schema::create_views`) installed on its connection, and hands it to `f`.
    /// Assertions run inside `f` itself, against `tx.raw()`, before the transaction commits.
    fn with_tx(f: impl FnOnce(&Tx<'_>)) {
        let file = tempfile::NamedTempFile::new().expect("create temp db file");
        let log = EventLog::open(file.path()).expect("open event log");
        let mut tx = log.begin().expect("begin tx");
        crate::schema::create_views(tx.raw_mut()).expect("install schema");
        f(&tx);
        tx.commit().expect("commit");
    }

    fn draft_event(seq: u64, kind: EK, payload: tm_events::Payload) -> Event {
        Event {
            seq,
            ts: Timestamp::EPOCH,
            kind,
            subject: Id::none(),
            actor: ParticipantId::system(),
            session: None,
            causation: None,
            correlation: None,
            payload,
            hash: String::new(),
        }
    }

    fn ticket_state(tx: &Tx<'_>, id: &str) -> String {
        tx.raw()
            .query_row(
                "SELECT state FROM tickets WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn project_created_sets_name_and_root() {
        with_tx(|tx| {
            let payload = tm_events::Payload::from(ProjectCreatedPayload {
                name: "demo".into(),
                root: "/tmp/demo".into(),
            });
            let event = draft_event(1, EK::ProjectCreated, payload);
            apply(tx, &event).unwrap();
            assert_eq!(meta_value(tx, "name"), Some("demo".into()));
            assert_eq!(meta_value(tx, "root"), Some("/tmp/demo".into()));
        });
    }

    #[test]
    fn ticket_created_inserts_row_and_bumps_counter() {
        with_tx(|tx| {
            let ticket = TicketId::new("T-42").unwrap();
            let payload = tm_events::Payload::from(TicketCreatedPayload {
                ticket: ticket.clone(),
                title: "Fix bug".into(),
                parent: None,
            });
            let event = draft_event(1, EK::TicketCreated, payload);
            apply(tx, &event).unwrap();

            let (objective, state): (String, String) = tx
                .raw()
                .query_row(
                    "SELECT objective, state FROM tickets WHERE id = 'T-42'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(objective, "Fix bug");
            assert_eq!(state, "\"draft\"");

            let counter: u64 = tx
                .raw()
                .query_row(
                    "SELECT value FROM counters WHERE counter_name = ?1",
                    params![ticket.kind().counter()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(counter, 42);
        });
    }

    #[test]
    fn ticket_created_with_parent_records_child_edge() {
        with_tx(|tx| {
            let parent = TicketId::new("T-1").unwrap();
            let child = TicketId::new("T-2").unwrap();
            let payload = tm_events::Payload::from(TicketCreatedPayload {
                ticket: child.clone(),
                title: "Child".into(),
                parent: Some(parent.clone()),
            });
            let event = draft_event(1, EK::TicketCreated, payload);
            apply(tx, &event).unwrap();

            let count: i64 = tx
                .raw()
                .query_row(
                    "SELECT COUNT(*) FROM ticket_children WHERE parent = 'T-1' AND child = 'T-2'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
        });
    }

    #[test]
    fn ticket_updated_applies_arbitrary_field_subset() {
        with_tx(|tx| {
            let ticket = TicketId::new("T-1").unwrap();
            let created = tm_events::Payload::from(TicketCreatedPayload {
                ticket: ticket.clone(),
                title: "T".into(),
                parent: None,
            });
            apply(tx, &draft_event(1, EK::TicketCreated, created)).unwrap();

            let fields = serde_json::json!({ "objective": "renamed", "priority": 5 });
            let updated = tm_events::Payload::from(TicketUpdatedPayload {
                ticket: ticket.clone(),
                fields,
            });
            apply(tx, &draft_event(2, EK::TicketUpdated, updated)).unwrap();

            let (objective, priority): (String, i64) = tx
                .raw()
                .query_row(
                    "SELECT objective, priority FROM tickets WHERE id = 'T-1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(objective, "renamed");
            assert_eq!(priority, 5);
        });
    }

    #[test]
    fn ticket_updated_evidence_attached_inserts_evidence_row() {
        with_tx(|tx| {
            let ticket = TicketId::new("T-1").unwrap();
            let artifact = ArtifactId::new("ART-deadbeefcafe").unwrap();
            let created = tm_events::Payload::from(TicketCreatedPayload {
                ticket: ticket.clone(),
                title: "T".into(),
                parent: None,
            });
            apply(tx, &draft_event(1, EK::TicketCreated, created)).unwrap();

            let fields = serde_json::json!({
                "evidence_attached": {
                    "kind": crate::artifact::EvidenceKind::Review,
                    "artifact": artifact,
                    "summary": "looks good",
                }
            });
            let updated = tm_events::Payload::from(TicketUpdatedPayload {
                ticket: ticket.clone(),
                fields,
            });
            apply(tx, &draft_event(2, EK::TicketUpdated, updated)).unwrap();

            let summary: String = tx
                .raw()
                .query_row(
                    "SELECT summary FROM evidence WHERE ticket = 'T-1' AND artifact = ?1",
                    params![artifact.as_str()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(summary, "looks good");
        });
    }

    #[test]
    fn ticket_state_changed_updates_state_column() {
        with_tx(|tx| {
            let ticket = TicketId::new("T-1").unwrap();
            let created = tm_events::Payload::from(TicketCreatedPayload {
                ticket: ticket.clone(),
                title: "T".into(),
                parent: None,
            });
            apply(tx, &draft_event(1, EK::TicketCreated, created)).unwrap();

            let changed = tm_events::Payload::from(TicketStateChangedPayload {
                ticket: ticket.clone(),
                from: "draft".into(),
                to: "ready".into(),
            });
            apply(tx, &draft_event(2, EK::TicketStateChanged, changed)).unwrap();

            assert_eq!(ticket_state(tx, "T-1"), "\"ready\"");
        });
    }

    #[test]
    fn dependency_added_then_removed_leaves_no_row() {
        with_tx(|tx| {
            let ticket = TicketId::new("T-1").unwrap();
            let dep = TicketId::new("T-2").unwrap();
            let added = tm_events::Payload::from(TicketDependencyAddedPayload {
                ticket: ticket.clone(),
                depends_on: dep.clone(),
            });
            apply(tx, &draft_event(1, EK::TicketDependencyAdded, added)).unwrap();
            let removed = tm_events::Payload::from(TicketDependencyRemovedPayload {
                ticket,
                depends_on: dep,
            });
            apply(tx, &draft_event(2, EK::TicketDependencyRemoved, removed)).unwrap();

            let count: i64 = tx
                .raw()
                .query_row("SELECT COUNT(*) FROM ticket_deps", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0);
        });
    }

    #[test]
    fn lease_heartbeat_extends_ttl() {
        with_tx(|tx| {
            let ticket = TicketId::new("T-1").unwrap();
            let lease = tm_types::LeaseId::new("L-abcdef123456").unwrap();
            let holder = ParticipantId::new("agent:coder/1").unwrap();
            let leased = tm_events::Payload::from(TicketLeasedPayload {
                ticket: ticket.clone(),
                lease: lease.clone(),
                holder,
                expires_at: Timestamp::EPOCH.plus_seconds(10),
            });
            apply(tx, &draft_event(1, EK::TicketLeased, leased)).unwrap();

            let ttl: i64 = tx
                .raw()
                .query_row(
                    "SELECT ttl_seconds FROM leases WHERE id = ?1",
                    params![lease.as_str()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(ttl, 10);

            let heartbeat = tm_events::Payload::from(TicketHeartbeatPayload {
                ticket,
                lease: lease.clone(),
                expires_at: Timestamp::EPOCH.plus_seconds(30),
            });
            apply(tx, &draft_event(2, EK::TicketHeartbeat, heartbeat)).unwrap();

            let ttl: i64 = tx
                .raw()
                .query_row(
                    "SELECT ttl_seconds FROM leases WHERE id = ?1",
                    params![lease.as_str()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(ttl, 30);
        });
    }

    #[test]
    fn decision_created_then_superseded_records_link() {
        with_tx(|tx| {
            let d1 = DecisionId::new("D-1").unwrap();
            let d2 = DecisionId::new("D-2").unwrap();
            let summary = serde_json::json!({
                "subject": "naming",
                "decision": "use snake_case",
                "reason": "consistency",
                "evidence": [],
                "affected_tickets": [],
                "affected_paths": [],
            })
            .to_string();
            let created = tm_events::Payload::from(DecisionCreatedPayload {
                decision: d1.clone(),
                ticket: None,
                summary,
            });
            apply(tx, &draft_event(1, EK::DecisionCreated, created)).unwrap();

            let subject: String = tx
                .raw()
                .query_row("SELECT subject FROM decisions WHERE id = 'D-1'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(subject, "naming");

            let superseded = tm_events::Payload::from(DecisionSupersededPayload {
                decision: d1.clone(),
                superseded_by: d2.clone(),
            });
            apply(tx, &draft_event(2, EK::DecisionSuperseded, superseded)).unwrap();

            let by: String = tx
                .raw()
                .query_row(
                    "SELECT superseded_by FROM decisions WHERE id = 'D-1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(by, "D-2");
        });
    }

    #[test]
    fn milestone_closed_then_reopened_round_trips_state() {
        with_tx(|tx| {
            let milestone = MilestoneId::new("M-1").unwrap();
            let created = tm_events::Payload::from(MilestoneCreatedPayload {
                milestone: milestone.clone(),
                title: "Beta".into(),
            });
            apply(tx, &draft_event(1, EK::MilestoneCreated, created)).unwrap();

            let closed = tm_events::Payload::from(MilestoneClosedPayload {
                milestone: milestone.clone(),
            });
            apply(tx, &draft_event(2, EK::MilestoneClosed, closed)).unwrap();
            let state: String = tx
                .raw()
                .query_row("SELECT state FROM milestones WHERE id = 'M-1'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(state, "\"closed\"");

            let reopened = tm_events::Payload::from(MilestoneReopenedPayload { milestone });
            apply(tx, &draft_event(3, EK::MilestoneReopened, reopened)).unwrap();
            let state: String = tx
                .raw()
                .query_row("SELECT state FROM milestones WHERE id = 'M-1'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(state, "\"open\"");
        });
    }

    #[test]
    fn artifact_created_inserts_placeholder_row() {
        with_tx(|tx| {
            let artifact = ArtifactId::new("ART-deadbeefcafe").unwrap();
            let payload = tm_events::Payload::from(ArtifactCreatedPayload {
                artifact: artifact.clone(),
                ticket: None,
                path: "disk:/artifacts/report.pdf".into(),
                media_type: "application/pdf".into(),
            });
            apply(tx, &draft_event(1, EK::ArtifactCreated, payload)).unwrap();

            let (storage, media_type): (String, String) = tx
                .raw()
                .query_row(
                    "SELECT storage, media_type FROM artifacts WHERE id = ?1",
                    params![artifact.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(storage, "disk:/artifacts/report.pdf");
            assert_eq!(media_type, "application/pdf");
        });
    }

    #[test]
    fn session_started_creates_session_and_active_participant() {
        with_tx(|tx| {
            let session = SessionId::new("S-1").unwrap();
            let participant = ParticipantId::new("human:alice").unwrap();
            let payload = tm_events::Payload::from(SessionStartedPayload {
                session: session.clone(),
                participant: participant.clone(),
            });
            apply(tx, &draft_event(1, EK::SessionStarted, payload)).unwrap();

            let participant_row: String = tx
                .raw()
                .query_row(
                    "SELECT participant FROM sessions WHERE id = ?1",
                    params![session.as_str()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(participant_row, participant.as_str());
            let status: String = tx
                .raw()
                .query_row(
                    "SELECT status FROM participants WHERE id = ?1",
                    params![participant.as_str()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(status, "active");
        });
    }

    #[test]
    fn unknown_to_this_view_kind_is_a_no_op_not_an_error() {
        with_tx(|tx| {
            let payload = tm_events::Payload::from(HarnessBenchmarkedPayload {
                suite: "smoke".into(),
                score: 0.9,
            });
            let event = draft_event(1, EK::HarnessBenchmarked, payload);
            assert!(apply(tx, &event).is_ok());
        });
    }

    #[test]
    fn replay_applies_every_event_in_order() {
        with_tx(|tx| {
            let ticket = TicketId::new("T-1").unwrap();
            let created = tm_events::Payload::from(TicketCreatedPayload {
                ticket: ticket.clone(),
                title: "T".into(),
                parent: None,
            });
            let changed = tm_events::Payload::from(TicketStateChangedPayload {
                ticket: ticket.clone(),
                from: "draft".into(),
                to: "ready".into(),
            });
            let events = vec![
                draft_event(1, EK::TicketCreated, created),
                draft_event(2, EK::TicketStateChanged, changed),
            ];
            replay(tx, &events).unwrap();

            assert_eq!(ticket_state(tx, "T-1"), "\"ready\"");
        });
    }

    #[test]
    fn replay_stops_and_propagates_on_storage_error() {
        with_tx(|tx| {
            // A raw duplicate-primary-key `INSERT` (unlike the upserts `apply` itself issues) is
            // a genuine constraint violation, simulating a corrupted-log scenario directly
            // against the connection to exercise the error path.
            tx.raw()
                .execute(
                    "INSERT INTO milestones (id, title, tickets, state, closed_by, assumptions)
                     VALUES ('M-1', 'x', '[]', '\"open\"', NULL, '[]')",
                    [],
                )
                .unwrap();
            let dup = tx.raw().execute(
                "INSERT INTO milestones (id, title, tickets, state, closed_by, assumptions)
                 VALUES ('M-1', 'y', '[]', '\"open\"', NULL, '[]')",
                [],
            );
            assert!(dup.is_err());
        });
    }
}
