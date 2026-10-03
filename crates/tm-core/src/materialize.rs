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
//! participant/session/doc/harness-epoch/mirror-link/provider-usage state (`SPEC.md` §4.1's
//! table list) with columns [`crate::schema`] actually has room for. A few kinds in `tm_events`'
//! closed payload catalogue (`resource.claimed`/`released`, `usage.recorded`, `harness.changed`,
//! `harness.benchmarked`, `command.*`) name fields with genuinely nowhere to land in this
//! crate's tables at all (no per-command, per-benchmark-run or per-field-diff table exists); since
//! `tm_events` isn't this crate's to change, those are deliberate no-ops, documented at each such
//! arm below rather than left to the catch-all. A few more (`doc.registered`, `provider.selected`
//! vs. `usage.recorded`, `harness.promoted`, `mirror.linked`) name fields that line up only
//! partially with their table's columns (e.g. `doc.registered` carries a `path`, not the `docs`
//! table's `id`); those *are* materialized here, under a documented mapping decision at each such
//! arm, rather than skipped — partial information is still real information.

use rusqlite::{params, OptionalExtension};
use tm_events::{Event, EventKind, Tx};
use tm_types::{IdKind, Timestamp, TmError};

use crate::goal::GoalStep;

fn storage_err(e: rusqlite::Error) -> TmError {
    TmError::storage(e.to_string())
}

/// One past the current value of `counter` in `counters` (0 if the counter has never been
/// bumped), for a table whose primary key is an internally-allocated sequence number rather than
/// a number parsed off an id string (see [`bump_counter`], which this pairs with: the caller
/// reads the next value here, then persists it via `bump_counter` once the row it names has been
/// written). Deterministic across the live path and replay because both process events through
/// [`apply`] in the same total order.
fn next_counter(tx: &Tx<'_>, counter: &str) -> tm_types::Result<u64> {
    let current: Option<i64> = tx
        .raw()
        .query_row(
            "SELECT value FROM counters WHERE counter_name = ?1",
            params![counter],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_err)?;
    Ok(current.unwrap_or(0) as u64 + 1)
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
            params![counter, i64::try_from(number).unwrap_or(i64::MAX)],
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
                // A change to the ticket is a change: `updated` follows it, unless the event
                // sets `updated` itself (`create_ticket`/`fork_ticket`'s initial field set).
                if p.fields.get("updated").is_none() {
                    touch_ticket(tx, &p.ticket, event.ts)?;
                }
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
                // Every lifecycle step (close, cancel, lease, failure, ...) travels as a state
                // change, so this is what keeps `updated` honest across all of them.
                touch_ticket(tx, &p.ticket, event.ts)?;
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
        | EventKind::TicketBudgetExhausted
        // `ticket.budget_handoff` travels alongside its own `ticket.state_changed` event too
        // (`Store::budget_handoff`/`Store::record_usage`'s handoff-eligible branch), same
        // convention as every other ticket lifecycle kind in this arm.
        | EventKind::TicketBudgetHandoff => {}
        // `ticket.forked` (`docs/decisions/D-008-ticket-checkpoint-fork.md`) is pure provenance:
        // `Store::fork_ticket` always appends it alongside its own `ticket.created`/
        // `ticket.updated` pair, which is what actually populates the new ticket's row (the same
        // two-event convention `Store::create_ticket` itself uses, per this module's top-level
        // note). `tickets` has no `forked_from`/`forked_from_seq` columns, so this event carries
        // nothing further to materialize here — a deliberate no-op, not a gap: the fact is
        // durable and hash-chained in the raw log, recoverable via `EventLog::read_subject` (its
        // `source`/`source_seq` fields specifically are not, today, surfaced by any CLI verb —
        // `tm events show <seq>` renders `kind`/`subject`/`ts` only, for every event kind, not
        // payload) even though it is not (yet) a queryable column, the same tradeoff
        // `harness.changed`/`harness.benchmarked` above already make.
        EventKind::TicketForked => {}
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
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)
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
                            p.supersedes.as_ref().map(|d| d.as_str()),
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
        EventKind::CommandStarted | EventKind::CommandCompleted => {
            // No `commands` table exists in `crate::schema` — every command run is already
            // recoverable from the raw event log itself (that *is* the durable record `B-05`
            // asks for: `Store::record_command` gets these onto the log at all, which is the gap
            // that mattered; `tm-context::command::run`'s caller previously had nowhere to commit
            // them). Materializing a redundant projection of the log into a new table is out of
            // this item's scope (it owns durable homes for the tables `crate::schema` already
            // declares, not new tables), so this is a deliberate no-op.
        }
        EventKind::DocRegistered => {
            if let Some(p) = event.payload.as_doc_registered() {
                // `doc.registered`'s payload carries `path` (and an optional originating
                // `ticket`), not a separate doc id — the closed catalogue has no room for one.
                // `path` is a doc's de facto stable identity elsewhere in this codebase too (see
                // `tm-docs::registry::DocTomlEntry`, which always pairs one `path` with one
                // `id`), so `path` is used as `docs.id` here. `title`/`content` have no source in
                // this payload either — a doc's prose lives on disk, mirroring how artifact bytes
                // live outside the log (see `store.rs`'s module note on `store_artifact`) — so
                // `title` defaults to `path` and `content` to empty; a real doc-content event
                // kind would be needed to do better, and adding one is out of this item's scope.
                // `state`/`last_verified` are deliberately untouched by the `ON CONFLICT`
                // branch: re-registering an already-known doc (`load_and_sync_doc_registry`
                // calls `register_doc` on every `docs list`/`check`/`reconcile`) must not reset
                // a persisted `Stale`/`Reconciling`/`Fresh` doc back to the fresh-row default —
                // only a real `doc.invalidated`/`doc.reconciled`/`doc.reconciling` event may
                // change state (docs-persist-state-across-invocations).
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "INSERT INTO docs (id, title, content, author, ts)
                         VALUES (?1, ?1, '', ?2, ?3)
                         ON CONFLICT(id) DO UPDATE SET author = excluded.author, ts = excluded.ts",
                        params![p.path, event.actor.as_str(), now],
                    )
                    .map_err(storage_err)?;
                if let Some(ticket) = &p.ticket {
                    // `doc_provenance` is keyed `(doc_id, source)`; the originating ticket is
                    // recorded as one provenance source among potentially several, exactly like
                    // `tm-docs::registry::DocFrontMatter::derived_from` entries would be.
                    tx.raw()
                        .execute(
                            "INSERT OR IGNORE INTO doc_provenance (doc_id, source, reason)
                             VALUES (?1, ?2, 'doc.registered')",
                            params![p.path, ticket.as_str()],
                        )
                        .map_err(storage_err)?;
                }
            }
        }
        EventKind::DocInvalidated => {
            if let Some(p) = event.payload.as_doc_invalidated() {
                // `docs.state`/`docs.last_verified` (docs-persist-state-across-invocations) give
                // this a real home now: an invalidation moves the doc to `Stale`, `ts` is
                // touched as a durable freshness signal ("this doc was last acted on at..."),
                // the same convention `doc.reconciled`/`doc.reconciling` below use. `reason` still
                // has nowhere to persist under the existing schema and is intentionally dropped
                // here (still recoverable from the raw event log). Upserts rather than a plain
                // `UPDATE` so replay is order-tolerant if an invalidation is ever logged for a
                // path this view hasn't seen a `doc.registered` for yet.
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "INSERT INTO docs (id, title, content, author, ts, state)
                         VALUES (?1, ?1, '', ?2, ?3, 'stale')
                         ON CONFLICT(id) DO UPDATE SET ts = excluded.ts, state = 'stale'",
                        params![p.path, event.actor.as_str(), now],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::DocReconciled => {
            if let Some(p) = event.payload.as_doc_reconciled() {
                // Same mapping as `doc.invalidated` above; a reconciliation also sets
                // `last_verified` to this event's own timestamp, since the doc's content is
                // confirmed to match its basis again as of now.
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "INSERT INTO docs (id, title, content, author, ts, state, last_verified)
                         VALUES (?1, ?1, '', ?2, ?3, 'fresh', ?3)
                         ON CONFLICT(id) DO UPDATE SET
                             ts = excluded.ts, state = 'fresh', last_verified = excluded.ts",
                        params![p.path, event.actor.as_str(), now],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::DocReconciling => {
            if let Some(p) = event.payload.as_doc_reconciling() {
                // A regeneration/review ticket just opened against this doc
                // (`Store::mark_doc_reconciling`, `tm docs reconcile`). Same upsert shape as
                // `doc.invalidated`/`doc.reconciled` above, plus a `doc_provenance` row for the
                // opened ticket (same convention `doc.registered`'s optional `ticket` uses
                // above), so a later caller (`tm docs attest`, `docs-wire-attestation-cli-path`)
                // can find the doc's open reconciliation ticket from `Store::docs`' provenance
                // list without a separate lookup.
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "INSERT INTO docs (id, title, content, author, ts, state)
                         VALUES (?1, ?1, '', ?2, ?3, 'reconciling')
                         ON CONFLICT(id) DO UPDATE SET ts = excluded.ts, state = 'reconciling'",
                        params![p.path, event.actor.as_str(), now],
                    )
                    .map_err(storage_err)?;
                if let Some(ticket) = &p.ticket {
                    tx.raw()
                        .execute(
                            "INSERT OR IGNORE INTO doc_provenance (doc_id, source, reason)
                             VALUES (?1, ?2, 'doc.reconciling')",
                            params![p.path, ticket.as_str()],
                        )
                        .map_err(storage_err)?;
                }
            }
        }
        EventKind::ProviderSelected => {
            if let Some(p) = event.payload.as_provider_selected() {
                // `provider_usage`'s primary key is `(provider, model)`. `usage.recorded` (below)
                // does now carry optional `provider`/`model` attribution
                // (`tel-usage-payload-model-field`), but rolling its `tokens`/`dollars_micros`
                // into this table's running counters is deliberately out of that task's scope —
                // see the `UsageRecorded` arm below for why this table stays a freshness table,
                // not a precise spend ledger, for now. `provider.selected` therefore seeds/
                // refreshes an identity row per
                // `(provider, model)` pair with zeroed counters rather than a running total; real
                // spend accounting already happens per ticket/session via `budgets`
                // (`Store::record_usage` -> `crate::budget::BudgetLedger::record_usage`), which is
                // what actually enforces `SPEC.md`'s budget invariants. `provider_usage` is
                // therefore a "which providers/models have been selected, and how recently"
                // freshness table here, not a precise spend ledger.
                tx.raw()
                    .execute(
                        "INSERT INTO provider_usage (provider, model, tokens_used, dollars_micros, last_updated)
                         VALUES (?1, ?2, 0, 0, ?3)
                         ON CONFLICT(provider, model) DO UPDATE SET last_updated = excluded.last_updated",
                        params![p.provider, p.model, event.ts.to_rfc3339()],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::UsageRecorded => {
            // Deliberate no-op against `provider_usage`, even though the payload can now name
            // `provider`/`model` (`tel-usage-payload-model-field`): rolling per-event spend into
            // this table's running `tokens_used`/`dollars_micros` counters, keyed and aggregated
            // correctly across concurrent writers, is a separate task, not a side effect to sneak
            // into a field-addition. Nothing is silently dropped that matters in the meantime —
            // the budget debit this event accompanies already happened synchronously at the call
            // site before the event was even built (see `store.rs`'s `record_usage`), and the
            // attribution itself is preserved verbatim in the immutable event log for a future
            // reader (or task) to fold.
        }
        EventKind::HarnessChanged => {
            // A field-level harness config diff (`field`/`from`/`to`) has no dedicated table:
            // `harness_epochs` (`crate::schema`) stores one full `harness_config` snapshot per
            // *promoted* epoch, not a diff log of the draft config leading up to a promotion.
            // Adding a diff-history table is out of this item's scope (durable homes for the
            // tables that already exist); deliberate no-op.
        }
        EventKind::HarnessBenchmarked => {
            // Same reasoning as `harness.changed`: no per-suite benchmark-result table exists in
            // `crate::schema` to hold `suite`/`score` against. Deliberate no-op (already covered
            // by this module's own `unknown_to_this_view_kind_is_a_no_op_not_an_error` test).
        }
        EventKind::HarnessPromoted => {
            if let Some(p) = event.payload.as_harness_promoted() {
                // `harness.promoted`'s payload carries only `candidate` (an opaque harness-config
                // identifier), not the `harness_epochs.epoch` integer primary key `crate::schema`
                // gives that table — the closed catalogue has no room for a numeric epoch.
                // `epoch` is therefore derived here from a private `harness_epoch` row in
                // `counters`, bumped by one on every promotion; deterministic because `apply`
                // sees promotions in the same total order on the live path and on replay.
                let epoch = next_counter(tx, "harness_epoch")? as i64;
                // `benchmark_json` is deliberately left NULL here (never named in this INSERT's
                // column list, so it takes the column's own default) -- `harness.promoted`'s
                // payload carries no benchmark report (see `crate::schema`'s `SCHEMA_VERSION` `7`
                // doc comment), so it is set afterward, outside replay, by
                // `Store::set_harness_epoch_benchmark`. A rebuild therefore resets it to `None`
                // for every epoch, same as `mirror_links.content_hash` after a rebuild.
                tx.raw()
                    .execute(
                        "INSERT INTO harness_epochs (epoch, harness_config, ts)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(epoch) DO UPDATE SET harness_config = excluded.harness_config,
                            ts = excluded.ts",
                        params![epoch, p.candidate, event.ts.to_rfc3339()],
                    )
                    .map_err(storage_err)?;
                bump_counter(tx, "harness_epoch", epoch as u64)?;
            }
        }
        EventKind::MirrorLinked => {
            if let Some(p) = event.payload.as_mirror_linked() {
                // `mirror.linked`'s payload carries only `remote` (the adapter/tracker name,
                // matching `tm-mirror::sync::SyncEngine::push`'s convention of putting
                // `tracker.name()` there) — no ticket. The ticket this link is for is instead
                // read from `event.subject`, the same convention `tm-mirror::sync` already uses
                // when building this event's `EventDraft` (`subject = Id::from(ticket)`). No
                // `remote_id` is known yet at link time (that arrives with the first
                // `mirror.pushed`/`mirror.pulled`), so it is left empty on first insert and never
                // clobbered by a later `mirror.linked` replay for the same ticket.
                //
                // `mirror_links.ticket` is the table's primary key, so a `mirror.linked` with no
                // subject (e.g. a malformed draft built outside `Store::link_mirror`, which
                // always sets one) must not silently collide every such event onto one `ticket =
                // ''` row; skip rather than corrupt the table.
                if !event.subject.is_empty() {
                    let now = event.ts.to_rfc3339();
                    tx.raw()
                        .execute(
                            "INSERT INTO mirror_links (ticket, remote_id, remote_system, last_synced)
                             VALUES (?1, '', ?2, ?3)
                             ON CONFLICT(ticket) DO UPDATE SET remote_system = excluded.remote_system,
                                last_synced = excluded.last_synced",
                            params![event.subject.as_str(), p.remote, now],
                        )
                        .map_err(storage_err)?;
                }
            }
        }
        EventKind::MirrorPushed | EventKind::MirrorPulled => {
            let pushed = event
                .payload
                .as_mirror_pushed()
                .map(|p| (&p.remote, &p.reference));
            let pulled = event
                .payload
                .as_mirror_pulled()
                .map(|p| (&p.remote, &p.reference));
            if let Some((remote, reference)) = pushed.or(pulled) {
                // Same `event.subject`-carries-the-ticket convention (and same empty-subject
                // guard) as `mirror.linked` above. Unlike `mirror.linked`, `mirror.pushed`/
                // `mirror.pulled` do carry `reference` (the external id), so `remote_id` is
                // written for real here.
                if !event.subject.is_empty() {
                    let now = event.ts.to_rfc3339();
                    tx.raw()
                        .execute(
                            "INSERT INTO mirror_links (ticket, remote_id, remote_system, last_synced)
                             VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(ticket) DO UPDATE SET remote_id = excluded.remote_id,
                                remote_system = excluded.remote_system,
                                last_synced = excluded.last_synced",
                            params![event.subject.as_str(), reference, remote, now],
                        )
                        .map_err(storage_err)?;
                }
            }
        }
        EventKind::EffectJournaled => {
            if let Some(p) = event.payload.as_effect_journaled() {
                // Fresh journal row. `Store::begin_effect` only appends this event when no row
                // for `key` exists yet (it checks first), so a plain `INSERT` is correct on the
                // live path; on replay the same is true because events replay in the same total
                // order they were appended in, so this key has never been seen before either.
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "INSERT INTO effects (key, ticket, attempt, kind, status, receipt_artifact, started, completed)
                         VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, NULL)
                         ON CONFLICT(key) DO UPDATE SET status = excluded.status, started = excluded.started",
                        params![
                            p.key,
                            p.ticket.as_str(),
                            p.attempt,
                            p.kind,
                            crate::effect::EffectStatus::Journaled.as_str(),
                            now
                        ],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::EffectCompleted => {
            if let Some(p) = event.payload.as_effect_completed() {
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "UPDATE effects SET status = ?2, receipt_artifact = ?3, completed = ?4
                         WHERE key = ?1",
                        params![
                            p.key,
                            crate::effect::EffectStatus::Completed.as_str(),
                            p.receipt_artifact,
                            now
                        ],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::EffectFailed => {
            if let Some(p) = event.payload.as_effect_failed() {
                // `p.reason` has nowhere to persist under `effects`' schema (no column for it,
                // per the audit's proposed column set) — same convention as `doc.invalidated`'s
                // dropped `reason` above; still recoverable from the raw event log.
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "UPDATE effects SET status = ?2, completed = ?3 WHERE key = ?1",
                        params![p.key, crate::effect::EffectStatus::Failed.as_str(), now],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::GoalSet => {
            if let Some(p) = event.payload.as_goal_set() {
                // A fresh `goal.set` restarts the decomposition: `steps` resets to `[]` and
                // `claimed_complete` resets to false, exactly as it would if this were the first
                // `goal.set` for this ticket (`ON CONFLICT` re-applies the same reset on an
                // existing row, which is what makes this idempotent under replaying the same
                // event twice, per this module's own B-11-shaped idempotence test below).
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "INSERT INTO goals (ticket, text, steps, claimed_complete, last_reoriented_step, set_at, updated_at)
                         VALUES (?1, ?2, '[]', 0, 0, ?3, ?3)
                         ON CONFLICT(ticket) DO UPDATE SET
                             text = excluded.text,
                             steps = '[]',
                             claimed_complete = 0,
                             last_reoriented_step = 0,
                             set_at = excluded.set_at,
                             updated_at = excluded.updated_at",
                        params![p.ticket.as_str(), p.text, now],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::GoalStepAdded => {
            if let Some(p) = event.payload.as_goal_step_added() {
                // Read-modify-write, deduped by `step_id`: re-applying the same `goal.step_added`
                // event twice (replay of an already-materialized log, or B-11's own
                // apply-twice idempotence shape) updates the existing entry's `text` in place
                // rather than appending a duplicate.
                let now = event.ts.to_rfc3339();
                let mut steps = read_goal_steps(tx, &p.ticket)?;
                match steps.iter_mut().find(|s| s.id == p.step_id) {
                    Some(existing) => existing.text = p.text.clone(),
                    None => steps.push(GoalStep {
                        id: p.step_id.clone(),
                        text: p.text.clone(),
                        done: false,
                    }),
                }
                write_goal_steps(tx, &p.ticket, &steps, &now)?;
            }
        }
        EventKind::GoalStepCompleted => {
            if let Some(p) = event.payload.as_goal_step_completed() {
                // Plain assignment of `done = true` on the matching step — idempotent by
                // construction; a step_id with no matching row (goal never set, or step never
                // added) is a no-op rather than an error, mirroring this module's general
                // "unmaterializable detail is dropped, not fatal" convention.
                let now = event.ts.to_rfc3339();
                let mut steps = read_goal_steps(tx, &p.ticket)?;
                if let Some(step) = steps.iter_mut().find(|s| s.id == p.step_id) {
                    step.done = true;
                    write_goal_steps(tx, &p.ticket, &steps, &now)?;
                }
            }
        }
        EventKind::GoalReoriented => {
            if let Some(p) = event.payload.as_goal_reoriented() {
                // Last-write-wins assignment, not a counter: re-applying the same event during
                // replay sets `last_reoriented_step` to the same `at_step` both times.
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "UPDATE goals SET last_reoriented_step = ?2, updated_at = ?3 WHERE ticket = ?1",
                        params![p.ticket.as_str(), p.at_step, now],
                    )
                    .map_err(storage_err)?;
            }
        }
        EventKind::GoalClaimedComplete => {
            if let Some(p) = event.payload.as_goal_claimed_complete() {
                // `p.summary` has nowhere to persist under `goals`' schema (no column for it) —
                // same convention as `effect.failed`'s dropped `reason` above; still recoverable
                // from the raw event log.
                let now = event.ts.to_rfc3339();
                tx.raw()
                    .execute(
                        "UPDATE goals SET claimed_complete = 1, updated_at = ?2 WHERE ticket = ?1",
                        params![p.ticket.as_str(), now],
                    )
                    .map_err(storage_err)?;
            }
        }
        // The remaining catalogued kinds aren't part of this crate's materialized view at all
        // (authority.granted/delegated/revoked/reverted, resource.conflict_detected,
        // executor.failed, index.updated, comment/approval events, genesis.*) — no table in
        // `crate::schema` names anything for them, and (per this module's doc comment)
        // unrecognized/unmaterialized kinds are a deliberate no-op, not an error, so replay never
        // fails on a kind this build doesn't project.
        _ => {}
    }
    Ok(())
}

/// Read `goals.steps` for `ticket` back into typed [`GoalStep`]s, or an empty `Vec` if no `goals`
/// row exists yet for it (the `goal.step_added` materializer arm is the only caller today, and it
/// always runs after a `goal.set` in practice, but treating a missing row as "no steps yet" rather
/// than erroring keeps this pure read-modify helper safe to call regardless of ordering).
fn read_goal_steps(tx: &Tx<'_>, ticket: &tm_types::TicketId) -> tm_types::Result<Vec<GoalStep>> {
    let steps_json: Option<String> = tx
        .raw()
        .query_row(
            "SELECT steps FROM goals WHERE ticket = ?1",
            params![ticket.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_err)?;
    match steps_json {
        Some(json) => serde_json::from_str(&json).map_err(TmError::from),
        None => Ok(Vec::new()),
    }
}

/// Write `steps` back to `goals.steps` for `ticket`, bumping `updated_at` to `now` (an already
/// RFC3339-formatted timestamp). An `INSERT ... ON CONFLICT` rather than a bare `UPDATE`: a
/// `goal.step_added` is expected to always follow a `goal.set` in practice (`AgentLoop` never
/// emits one without the other), but a bare `UPDATE` would silently drop the step if a `goals`
/// row somehow didn't exist yet, rather than surfacing or recovering from it — the fresh-row
/// branch here fills `text`/`claimed_complete`/`last_reoriented_step`/`set_at` with the same
/// defaults `goal.set`'s own fresh-row branch uses, so a step is never lost even in that case.
fn write_goal_steps(
    tx: &Tx<'_>,
    ticket: &tm_types::TicketId,
    steps: &[GoalStep],
    now: &str,
) -> tm_types::Result<()> {
    let steps_json = serde_json::to_string(steps).map_err(TmError::from)?;
    tx.raw()
        .execute(
            "INSERT INTO goals (ticket, text, steps, claimed_complete, last_reoriented_step, set_at, updated_at)
             VALUES (?1, '', ?2, 0, 0, ?3, ?3)
             ON CONFLICT(ticket) DO UPDATE SET steps = excluded.steps, updated_at = excluded.updated_at",
            params![ticket.as_str(), steps_json, now],
        )
        .map_err(storage_err)?;
    Ok(())
}

/// Set `ticket`'s `updated` column to `at`, the timestamp of the event that changed it, in the
/// same serialized form `Store::create_ticket` writes (a JSON-serialized `Timestamp`).
fn touch_ticket(tx: &Tx<'_>, ticket: &tm_types::TicketId, at: Timestamp) -> tm_types::Result<()> {
    let at = serde_json::to_value(at)
        .map_err(TmError::from)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            TmError::invariant("internal error: a timestamp did not serialize as expected")
        })?;
    tx.raw()
        .execute(
            "UPDATE tickets SET updated = ?2 WHERE id = ?1",
            params![ticket.as_str(), at],
        )
        .map_err(storage_err)?;
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
            "due" => {
                // `value` is either a `"YYYY-MM-DD"` string (set) or JSON `null` (`--due none`
                // clears it); `value.as_str()` is `None` for `null`, which binds SQL `NULL`, same
                // convention as the `milestone` arm just above.
                tx.raw()
                    .execute(
                        "UPDATE tickets SET due = ?2 WHERE id = ?1",
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

    /// As [`draft_event`], but with an explicit `subject` — needed for the mirror-link arms,
    /// which (per `tm-mirror::sync::SyncEngine`'s own event-building convention) read the ticket
    /// a mirror event is about off `event.subject` rather than the payload.
    fn draft_event_with_subject(
        seq: u64,
        kind: EK,
        subject: Id,
        payload: tm_events::Payload,
    ) -> Event {
        Event {
            subject,
            ..draft_event(seq, kind, payload)
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

            let counter: i64 = tx
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
    fn ticket_updated_due_sets_then_clears_the_due_column() {
        with_tx(|tx| {
            let ticket = TicketId::new("T-1").unwrap();
            let created = tm_events::Payload::from(TicketCreatedPayload {
                ticket: ticket.clone(),
                title: "T".into(),
                parent: None,
            });
            apply(tx, &draft_event(1, EK::TicketCreated, created)).unwrap();

            let set_due = tm_events::Payload::from(TicketUpdatedPayload {
                ticket: ticket.clone(),
                fields: serde_json::json!({ "due": "2026-10-01" }),
            });
            apply(tx, &draft_event(2, EK::TicketUpdated, set_due)).unwrap();

            let due: Option<String> = tx
                .raw()
                .query_row("SELECT due FROM tickets WHERE id = 'T-1'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(due, Some("2026-10-01".to_string()));

            // `--due none` sends JSON `null`, which must clear the column, same as the
            // `milestone` field's convention just above.
            let clear_due = tm_events::Payload::from(TicketUpdatedPayload {
                ticket: ticket.clone(),
                fields: serde_json::json!({ "due": null }),
            });
            apply(tx, &draft_event(3, EK::TicketUpdated, clear_due)).unwrap();

            let due: Option<String> = tx
                .raw()
                .query_row("SELECT due FROM tickets WHERE id = 'T-1'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(due, None);
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
                supersedes: None,
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

    #[test]
    fn doc_registered_upserts_docs_row_keyed_by_path() {
        with_tx(|tx| {
            let payload = tm_events::Payload::from(DocRegisteredPayload {
                path: "docs/architecture.md".into(),
                ticket: None,
            });
            apply(tx, &draft_event(1, EK::DocRegistered, payload)).unwrap();

            let (title, content): (String, String) = tx
                .raw()
                .query_row(
                    "SELECT title, content FROM docs WHERE id = 'docs/architecture.md'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(title, "docs/architecture.md");
            assert_eq!(content, "");
        });
    }

    #[test]
    fn doc_registered_with_ticket_records_provenance() {
        with_tx(|tx| {
            let ticket = TicketId::new("T-1").unwrap();
            let payload = tm_events::Payload::from(DocRegisteredPayload {
                path: "docs/architecture.md".into(),
                ticket: Some(ticket),
            });
            apply(tx, &draft_event(1, EK::DocRegistered, payload)).unwrap();

            let source: String = tx
                .raw()
                .query_row(
                    "SELECT source FROM doc_provenance WHERE doc_id = 'docs/architecture.md'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(source, "T-1");
        });
    }

    #[test]
    fn doc_invalidated_then_reconciled_touch_ts_without_a_prior_registration() {
        with_tx(|tx| {
            let invalidated = tm_events::Payload::from(DocInvalidatedPayload {
                path: "docs/stale.md".into(),
                reason: "source moved".into(),
            });
            apply(tx, &draft_event(1, EK::DocInvalidated, invalidated)).unwrap();

            let count: i64 = tx
                .raw()
                .query_row(
                    "SELECT COUNT(*) FROM docs WHERE id = 'docs/stale.md'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                count, 1,
                "invalidation upserts a docs row even with no prior registration"
            );

            let reconciled = tm_events::Payload::from(DocReconciledPayload {
                path: "docs/stale.md".into(),
            });
            apply(tx, &draft_event(2, EK::DocReconciled, reconciled)).unwrap();

            let count: i64 = tx
                .raw()
                .query_row(
                    "SELECT COUNT(*) FROM docs WHERE id = 'docs/stale.md'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "reconciliation must not duplicate the row");
        });
    }

    #[test]
    fn doc_state_transitions_persist_across_registered_invalidated_reconciling_reconciled() {
        with_tx(|tx| {
            let registered = tm_events::Payload::from(DocRegisteredPayload {
                path: "docs/architecture.md".into(),
                ticket: None,
            });
            apply(tx, &draft_event(1, EK::DocRegistered, registered)).unwrap();
            let state = |tx: &Tx<'_>| -> String {
                tx.raw()
                    .query_row(
                        "SELECT state FROM docs WHERE id = 'docs/architecture.md'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap()
            };
            assert_eq!(state(tx), "unverified");

            let invalidated = tm_events::Payload::from(DocInvalidatedPayload {
                path: "docs/architecture.md".into(),
                reason: "source moved".into(),
            });
            apply(tx, &draft_event(2, EK::DocInvalidated, invalidated)).unwrap();
            assert_eq!(state(tx), "stale");

            let ticket = TicketId::new("T-1").unwrap();
            let reconciling = tm_events::Payload::from(DocReconcilingPayload {
                path: "docs/architecture.md".into(),
                ticket: Some(ticket),
            });
            apply(tx, &draft_event(3, EK::DocReconciling, reconciling)).unwrap();
            assert_eq!(state(tx), "reconciling");
            let source: String = tx
                .raw()
                .query_row(
                    "SELECT source FROM doc_provenance
                     WHERE doc_id = 'docs/architecture.md' AND source = 'T-1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(source, "T-1");

            let reconciled = tm_events::Payload::from(DocReconciledPayload {
                path: "docs/architecture.md".into(),
            });
            apply(tx, &draft_event(4, EK::DocReconciled, reconciled)).unwrap();
            assert_eq!(state(tx), "fresh");
            let last_verified: Option<String> = tx
                .raw()
                .query_row(
                    "SELECT last_verified FROM docs WHERE id = 'docs/architecture.md'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(last_verified.is_some());

            // Re-registering an already-known doc (`load_and_sync_doc_registry` calls
            // `register_doc` on every `docs list`/`check`) must not reset the persisted state
            // back to the fresh-row default.
            let reregistered = tm_events::Payload::from(DocRegisteredPayload {
                path: "docs/architecture.md".into(),
                ticket: None,
            });
            apply(tx, &draft_event(5, EK::DocRegistered, reregistered)).unwrap();
            assert_eq!(state(tx), "fresh");
        });
    }

    #[test]
    fn provider_selected_seeds_a_zeroed_provider_usage_row() {
        with_tx(|tx| {
            let payload = tm_events::Payload::from(ProviderSelectedPayload {
                role: "coder_fast".into(),
                provider: "anthropic".into(),
                model: "claude-sonnet".into(),
            });
            apply(tx, &draft_event(1, EK::ProviderSelected, payload)).unwrap();

            let (tokens, dollars): (i64, i64) = tx
                .raw()
                .query_row(
                    "SELECT tokens_used, dollars_micros FROM provider_usage
                     WHERE provider = 'anthropic' AND model = 'claude-sonnet'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(tokens, 0);
            assert_eq!(dollars, 0);
        });
    }

    #[test]
    fn usage_recorded_is_a_no_op_not_an_error() {
        with_tx(|tx| {
            let payload = tm_events::Payload::from(UsageRecordedPayload {
                ticket: None,
                session: None,
                tokens: 100,
                dollars_micros: 5,
                wall_seconds: 1,
                provider: None,
                model: None,
            });
            let event = draft_event(1, EK::UsageRecorded, payload);
            assert!(apply(tx, &event).is_ok());

            let count: i64 = tx
                .raw()
                .query_row("SELECT COUNT(*) FROM provider_usage", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0);
        });
    }

    #[test]
    fn harness_promoted_allocates_sequential_epochs() {
        with_tx(|tx| {
            let first = tm_events::Payload::from(HarnessPromotedPayload {
                candidate: "config-a".into(),
            });
            apply(tx, &draft_event(1, EK::HarnessPromoted, first)).unwrap();
            let second = tm_events::Payload::from(HarnessPromotedPayload {
                candidate: "config-b".into(),
            });
            apply(tx, &draft_event(2, EK::HarnessPromoted, second)).unwrap();

            let mut stmt = tx
                .raw()
                .prepare("SELECT epoch, harness_config FROM harness_epochs ORDER BY epoch")
                .unwrap();
            let rows: Vec<(i64, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert_eq!(
                rows,
                vec![(1, "config-a".to_string()), (2, "config-b".to_string())]
            );
        });
    }

    #[test]
    fn mirror_linked_then_pushed_populates_remote_id() {
        with_tx(|tx| {
            let ticket = TicketId::new("T-1").unwrap();
            let subject = Id::from(ticket.clone());
            let linked = tm_events::Payload::from(MirrorLinkedPayload {
                remote: "github".into(),
            });
            apply(
                tx,
                &draft_event_with_subject(1, EK::MirrorLinked, subject.clone(), linked),
            )
            .unwrap();

            let (remote_id, remote_system): (String, String) = tx
                .raw()
                .query_row(
                    "SELECT remote_id, remote_system FROM mirror_links WHERE ticket = 'T-1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(remote_id, "");
            assert_eq!(remote_system, "github");

            let pushed = tm_events::Payload::from(MirrorPushedPayload {
                remote: "github".into(),
                reference: "owner/repo#42".into(),
            });
            apply(
                tx,
                &draft_event_with_subject(2, EK::MirrorPushed, subject, pushed),
            )
            .unwrap();

            let remote_id: String = tx
                .raw()
                .query_row(
                    "SELECT remote_id FROM mirror_links WHERE ticket = 'T-1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(remote_id, "owner/repo#42");
        });
    }

    #[test]
    fn mirror_events_with_no_subject_do_not_collide_on_an_empty_ticket_row() {
        with_tx(|tx| {
            let linked = tm_events::Payload::from(MirrorLinkedPayload {
                remote: "github".into(),
            });
            apply(tx, &draft_event(1, EK::MirrorLinked, linked)).unwrap();

            let pushed = tm_events::Payload::from(MirrorPushedPayload {
                remote: "github".into(),
                reference: "owner/repo#1".into(),
            });
            apply(tx, &draft_event(2, EK::MirrorPushed, pushed)).unwrap();

            let count: i64 = tx
                .raw()
                .query_row("SELECT COUNT(*) FROM mirror_links", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                count, 0,
                "a mirror event with no subject must not write a ticket = '' row"
            );
        });
    }

    #[test]
    fn command_started_and_completed_are_a_no_op_not_an_error() {
        with_tx(|tx| {
            let started = tm_events::Payload::from(CommandStartedPayload {
                command: "cargo test".into(),
                ticket: None,
                session: None,
            });
            assert!(apply(tx, &draft_event(1, EK::CommandStarted, started)).is_ok());

            let completed = tm_events::Payload::from(CommandCompletedPayload {
                command: "cargo test".into(),
                ticket: None,
                session: None,
                exit_code: 0,
                duration_ms: 10,
            });
            assert!(apply(tx, &draft_event(2, EK::CommandCompleted, completed)).is_ok());
        });
    }

    // ---- effect.journaled / effect.completed / effect.failed (SPEC.md §21.5, audit B-11) ---

    fn ticket(s: &str) -> TicketId {
        s.parse().unwrap()
    }

    fn effect_row(tx: &Tx<'_>, key: &str) -> (String, Option<String>, i64, bool) {
        tx.raw()
            .query_row(
                "SELECT status, receipt_artifact, attempt, completed IS NOT NULL FROM effects WHERE key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }

    #[test]
    fn effect_journaled_inserts_a_journaled_row() {
        with_tx(|tx| {
            let journaled = tm_events::Payload::from(EffectJournaledPayload {
                key: "key-1".into(),
                ticket: ticket("T-1"),
                attempt: 0,
                kind: "git.push".into(),
            });
            apply(tx, &draft_event(1, EK::EffectJournaled, journaled)).unwrap();

            let (status, receipt, attempt, completed) = effect_row(tx, "key-1");
            assert_eq!(status, "journaled");
            assert_eq!(receipt, None);
            assert_eq!(attempt, 0);
            assert!(!completed);
        });
    }

    #[test]
    fn effect_completed_updates_status_and_receipt() {
        with_tx(|tx| {
            let journaled = tm_events::Payload::from(EffectJournaledPayload {
                key: "key-1".into(),
                ticket: ticket("T-1"),
                attempt: 0,
                kind: "git.push".into(),
            });
            apply(tx, &draft_event(1, EK::EffectJournaled, journaled)).unwrap();

            let completed = tm_events::Payload::from(EffectCompletedPayload {
                key: "key-1".into(),
                ticket: ticket("T-1"),
                receipt_artifact: Some("deadbeef".into()),
            });
            apply(tx, &draft_event(2, EK::EffectCompleted, completed)).unwrap();

            let (status, receipt, _attempt, has_completed) = effect_row(tx, "key-1");
            assert_eq!(status, "completed");
            assert_eq!(receipt.as_deref(), Some("deadbeef"));
            assert!(has_completed);
        });
    }

    #[test]
    fn effect_failed_updates_status_without_a_receipt() {
        with_tx(|tx| {
            let journaled = tm_events::Payload::from(EffectJournaledPayload {
                key: "key-1".into(),
                ticket: ticket("T-1"),
                attempt: 0,
                kind: "mirror.push:github".into(),
            });
            apply(tx, &draft_event(1, EK::EffectJournaled, journaled)).unwrap();

            let failed = tm_events::Payload::from(EffectFailedPayload {
                key: "key-1".into(),
                ticket: ticket("T-1"),
                reason: "github returned 503".into(),
            });
            apply(tx, &draft_event(2, EK::EffectFailed, failed)).unwrap();

            let (status, receipt, _attempt, has_completed) = effect_row(tx, "key-1");
            assert_eq!(status, "failed");
            assert_eq!(receipt, None);
            assert!(has_completed, "failed also stamps `completed` (terminal timestamp column, not the `Completed` status)");
        });
    }

    #[test]
    fn effect_journaled_replay_is_idempotent_for_the_same_key() {
        // `apply` is shared by the live path and `replay`; re-applying the same
        // `effect.journaled` event (as replay would) must not error or duplicate the row.
        with_tx(|tx| {
            let journaled = tm_events::Payload::from(EffectJournaledPayload {
                key: "key-1".into(),
                ticket: ticket("T-1"),
                attempt: 0,
                kind: "git.push".into(),
            });
            apply(tx, &draft_event(1, EK::EffectJournaled, journaled.clone())).unwrap();
            apply(tx, &draft_event(1, EK::EffectJournaled, journaled)).unwrap();

            let count: i64 = tx
                .raw()
                .query_row("SELECT COUNT(*) FROM effects", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1);
        });
    }

    // ---- goal.set / goal.step_added / goal.step_completed / goal.reoriented /
    // goal.claimed_complete (SPEC.md §29, audit B-09) ----

    fn goal_row(tx: &Tx<'_>, ticket: &str) -> (String, String, bool, i64) {
        tx.raw()
            .query_row(
                "SELECT text, steps, claimed_complete, last_reoriented_step FROM goals WHERE ticket = ?1",
                params![ticket],
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?)),
            )
            .unwrap()
    }

    #[test]
    fn goal_set_inserts_a_fresh_row() {
        with_tx(|tx| {
            let set = tm_events::Payload::from(GoalSetPayload {
                ticket: ticket("T-1"),
                text: "get the login form to validate emails".into(),
            });
            apply(tx, &draft_event(1, EK::GoalSet, set)).unwrap();

            let (text, steps, claimed, reoriented) = goal_row(tx, "T-1");
            assert_eq!(text, "get the login form to validate emails");
            assert_eq!(steps, "[]");
            assert!(!claimed);
            assert_eq!(reoriented, 0);
        });
    }

    #[test]
    fn goal_set_replay_is_idempotent() {
        with_tx(|tx| {
            let set = tm_events::Payload::from(GoalSetPayload {
                ticket: ticket("T-1"),
                text: "goal text".into(),
            });
            apply(tx, &draft_event(1, EK::GoalSet, set.clone())).unwrap();
            apply(tx, &draft_event(1, EK::GoalSet, set)).unwrap();

            let count: i64 = tx
                .raw()
                .query_row("SELECT COUNT(*) FROM goals", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1);
        });
    }

    #[test]
    fn goal_set_again_resets_steps_and_claimed_complete() {
        with_tx(|tx| {
            let set = tm_events::Payload::from(GoalSetPayload {
                ticket: ticket("T-1"),
                text: "first goal".into(),
            });
            apply(tx, &draft_event(1, EK::GoalSet, set)).unwrap();

            let added = tm_events::Payload::from(GoalStepAddedPayload {
                ticket: ticket("T-1"),
                step_id: "step-1".into(),
                text: "do a thing".into(),
            });
            apply(tx, &draft_event(2, EK::GoalStepAdded, added)).unwrap();

            let claimed = tm_events::Payload::from(GoalClaimedCompletePayload {
                ticket: ticket("T-1"),
                summary: "done".into(),
            });
            apply(tx, &draft_event(3, EK::GoalClaimedComplete, claimed)).unwrap();

            let (_, steps, is_claimed, _) = goal_row(tx, "T-1");
            assert_ne!(steps, "[]");
            assert!(is_claimed);

            let reset = tm_events::Payload::from(GoalSetPayload {
                ticket: ticket("T-1"),
                text: "second goal, restarted".into(),
            });
            apply(tx, &draft_event(4, EK::GoalSet, reset)).unwrap();

            let (text, steps, is_claimed, _) = goal_row(tx, "T-1");
            assert_eq!(text, "second goal, restarted");
            assert_eq!(steps, "[]", "a fresh goal.set restarts the decomposition");
            assert!(!is_claimed);
        });
    }

    #[test]
    fn goal_step_added_appends_a_step() {
        with_tx(|tx| {
            let set = tm_events::Payload::from(GoalSetPayload {
                ticket: ticket("T-1"),
                text: "goal".into(),
            });
            apply(tx, &draft_event(1, EK::GoalSet, set)).unwrap();

            let added = tm_events::Payload::from(GoalStepAddedPayload {
                ticket: ticket("T-1"),
                step_id: "step-1".into(),
                text: "read the file".into(),
            });
            apply(tx, &draft_event(2, EK::GoalStepAdded, added)).unwrap();

            let (_, steps_json, _, _) = goal_row(tx, "T-1");
            let steps: Vec<crate::goal::GoalStep> = serde_json::from_str(&steps_json).unwrap();
            assert_eq!(steps.len(), 1);
            assert_eq!(steps[0].id, "step-1");
            assert_eq!(steps[0].text, "read the file");
            assert!(!steps[0].done);
        });
    }

    #[test]
    fn goal_step_added_replay_is_idempotent_for_the_same_step_id() {
        // Re-applying the same `goal.step_added` event (as replay would) must update the
        // existing step in place, never append a duplicate.
        with_tx(|tx| {
            let set = tm_events::Payload::from(GoalSetPayload {
                ticket: ticket("T-1"),
                text: "goal".into(),
            });
            apply(tx, &draft_event(1, EK::GoalSet, set)).unwrap();

            let added = tm_events::Payload::from(GoalStepAddedPayload {
                ticket: ticket("T-1"),
                step_id: "step-1".into(),
                text: "read the file".into(),
            });
            apply(tx, &draft_event(2, EK::GoalStepAdded, added.clone())).unwrap();
            apply(tx, &draft_event(2, EK::GoalStepAdded, added)).unwrap();

            let (_, steps_json, _, _) = goal_row(tx, "T-1");
            let steps: Vec<crate::goal::GoalStep> = serde_json::from_str(&steps_json).unwrap();
            assert_eq!(steps.len(), 1, "must not duplicate the step on replay");
        });
    }

    #[test]
    fn goal_step_completed_marks_the_matching_step_done() {
        with_tx(|tx| {
            let set = tm_events::Payload::from(GoalSetPayload {
                ticket: ticket("T-1"),
                text: "goal".into(),
            });
            apply(tx, &draft_event(1, EK::GoalSet, set)).unwrap();
            let added = tm_events::Payload::from(GoalStepAddedPayload {
                ticket: ticket("T-1"),
                step_id: "step-1".into(),
                text: "read the file".into(),
            });
            apply(tx, &draft_event(2, EK::GoalStepAdded, added)).unwrap();

            let completed = tm_events::Payload::from(GoalStepCompletedPayload {
                ticket: ticket("T-1"),
                step_id: "step-1".into(),
            });
            apply(
                tx,
                &draft_event(3, EK::GoalStepCompleted, completed.clone()),
            )
            .unwrap();
            // Replay-idempotence: applying the same completion twice must not error.
            apply(tx, &draft_event(3, EK::GoalStepCompleted, completed)).unwrap();

            let (_, steps_json, _, _) = goal_row(tx, "T-1");
            let steps: Vec<crate::goal::GoalStep> = serde_json::from_str(&steps_json).unwrap();
            assert_eq!(steps.len(), 1);
            assert!(steps[0].done);
        });
    }

    #[test]
    fn goal_reoriented_sets_last_reoriented_step_and_is_idempotent() {
        with_tx(|tx| {
            let set = tm_events::Payload::from(GoalSetPayload {
                ticket: ticket("T-1"),
                text: "goal".into(),
            });
            apply(tx, &draft_event(1, EK::GoalSet, set)).unwrap();

            let reoriented = tm_events::Payload::from(GoalReorientedPayload {
                ticket: ticket("T-1"),
                at_step: 3,
            });
            apply(tx, &draft_event(2, EK::GoalReoriented, reoriented.clone())).unwrap();
            apply(tx, &draft_event(2, EK::GoalReoriented, reoriented)).unwrap();

            let (_, _, _, last_reoriented_step) = goal_row(tx, "T-1");
            assert_eq!(
                last_reoriented_step, 3,
                "re-applying the same reorient event must not double-count"
            );
        });
    }

    #[test]
    fn goal_claimed_complete_sets_the_flag_and_is_idempotent() {
        with_tx(|tx| {
            let set = tm_events::Payload::from(GoalSetPayload {
                ticket: ticket("T-1"),
                text: "goal".into(),
            });
            apply(tx, &draft_event(1, EK::GoalSet, set)).unwrap();

            let claimed = tm_events::Payload::from(GoalClaimedCompletePayload {
                ticket: ticket("T-1"),
                summary: "all done".into(),
            });
            apply(
                tx,
                &draft_event(2, EK::GoalClaimedComplete, claimed.clone()),
            )
            .unwrap();
            apply(tx, &draft_event(2, EK::GoalClaimedComplete, claimed)).unwrap();

            let (_, _, is_claimed, _) = goal_row(tx, "T-1");
            assert!(is_claimed);

            let count: i64 = tx
                .raw()
                .query_row("SELECT COUNT(*) FROM goals", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1);
        });
    }
}
