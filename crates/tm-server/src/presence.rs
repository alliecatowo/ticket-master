//! Presence: participant → current ticket/file/action, TTL'd, plus the surfaced path leases so
//! humans and agents can see who is holding what before they collide.
//!
//! Presence and path leases share one substrate (`SPEC.md` §14): both answer "who else is
//! touching this?", just at different granularity and durability. Presence
//! ([`PresenceTable`]) is server-local and TTL'd — a heartbeat away from being wrong, and lost
//! on restart, by design (it is a hint, not truth). Path leases ([`path_leases`]) are read
//! straight out of durable [`tm_core::view::ProjectView`] leases, so they survive restarts and
//! are the actual collision-prevention mechanism; presence is the UI layer on top.
//!
//! The TTL sweep ([`sweep_expired_at`]) is pure over `(entries, now)`, so it is unit-testable
//! without a running clock; [`PresenceTable::sweep_expired`] is the thin `Mutex`-holding wrapper
//! that applies it.

use std::collections::BTreeMap;
use std::sync::Mutex;

use tm_core::view::ProjectView;
use tm_types::{LeaseId, ParticipantId, ResourceMode, TicketId, Timestamp};

/// One participant's last-reported location in the project.
#[derive(Debug, Clone, PartialEq)]
pub struct PresenceEntry {
    /// Who this entry is about.
    pub participant: ParticipantId,
    /// The ticket they're currently working, if any.
    pub ticket: Option<TicketId>,
    /// The file/path they're currently touching, if any.
    pub file: Option<String>,
    /// Free-form action label (e.g. `"editing"`, `"reviewing"`, `"idle"`).
    pub action: String,
    /// When this entry was last refreshed.
    pub last_seen: Timestamp,
    /// Seconds after `last_seen` this entry is considered stale.
    pub ttl_seconds: u32,
}

impl PresenceEntry {
    /// True when `now` is at or past `last_seen + ttl_seconds`.
    pub fn is_expired(&self, now: Timestamp) -> bool {
        now.seconds_since(self.last_seen) >= i64::from(self.ttl_seconds)
    }
}

/// What `GET /presence` reports for one live path lease: durable state, not server-local.
#[derive(Debug, Clone, PartialEq)]
pub struct PathLeaseSummary {
    /// The lease this claim belongs to.
    pub lease: LeaseId,
    /// The ticket the lease is held for.
    pub ticket: TicketId,
    /// Who holds it.
    pub holder: ParticipantId,
    /// Exclusive or shared.
    pub mode: ResourceMode,
    /// The path patterns claimed, as their source strings (`tm_types::PathPattern::as_str`).
    pub paths: Vec<String>,
}

/// Presence storage: one entry per participant, guarded by a plain `Mutex` (presence writes are
/// small and infrequent enough that a `Mutex` over `BTreeMap` beats the complexity of a
/// concurrent map here).
pub struct PresenceTable {
    entries: Mutex<BTreeMap<ParticipantId, PresenceEntry>>,
}

impl PresenceTable {
    /// An empty presence table.
    pub fn new() -> Self {
        PresenceTable {
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Insert or refresh `entry`, keyed by `entry.participant`.
    pub fn upsert(&self, entry: PresenceEntry) {
        // IMPL: lock, insert(entry.participant.clone(), entry). Never panics: poisoned-lock
        // recovery via `.unwrap_or_else(|p| p.into_inner())` is acceptable here since presence
        // is advisory, not durable.
        todo!("insert/replace this participant's presence entry")
    }

    /// Drop `participant`'s entry outright (explicit "I'm done" signal, distinct from TTL
    /// expiry).
    pub fn remove(&self, participant: &ParticipantId) {
        todo!("remove this participant's presence entry if present")
    }

    /// Every live (not necessarily unexpired — callers sweep first if they want only-fresh)
    /// entry, for `GET /presence`.
    pub fn snapshot(&self) -> Vec<PresenceEntry> {
        todo!("clone every entry out of the table")
    }

    /// Remove every entry expired as of `now` (per [`sweep_expired_at`]), returning the
    /// participants that were dropped.
    pub fn sweep_expired(&self, now: Timestamp) -> Vec<ParticipantId> {
        // IMPL: lock, call sweep_expired_at(&mut *guard, now), return its result. Kept as a
        // separate pure function so the sweep policy is testable without a Mutex.
        todo!("apply sweep_expired_at under the lock")
    }
}

impl Default for PresenceTable {
    fn default() -> Self {
        PresenceTable::new()
    }
}

/// Pure sweep: remove and return the id of every entry in `entries` for which
/// [`PresenceEntry::is_expired`] is true as of `now`.
pub fn sweep_expired_at(
    entries: &mut BTreeMap<ParticipantId, PresenceEntry>,
    now: Timestamp,
) -> Vec<ParticipantId> {
    // IMPL: entries.iter().filter(|(_, e)| e.is_expired(now)).map(|(k, _)| k.clone()).collect(),
    // then entries.retain(|_, e| !e.is_expired(now)) (or remove each collected key). Return the
    // removed-key list in stable (BTreeMap iteration) order.
    todo!("remove and return every participant whose entry is expired as of now")
}

/// Surface every live lease's resource claims as path-lease summaries, for `GET /presence`
/// (`SPEC.md` §14: "Path leases are surfaced so humans and agents collide on the same
/// substrate").
///
/// # Invariant
/// Purely derived from `view.leases`; never mutates or infers beyond what's already
/// materialized. One [`PathLeaseSummary`] per `(lease, resource claim)` pair, so a lease with
/// multiple claims produces multiple summary rows sharing the same `lease`/`ticket`/`holder`.
pub fn path_leases(view: &ProjectView) -> Vec<PathLeaseSummary> {
    // IMPL: for each lease in view.leases.values(), for each claim in &lease.resources, push a
    // PathLeaseSummary { lease: lease.id.clone(), ticket: lease.ticket.clone(), holder:
    // lease.holder.clone(), mode: claim.mode, paths: claim.paths.patterns().map(|p|
    // p.as_str().to_string()).collect() }.
    todo!("flatten each live lease's resource claims into PathLeaseSummary rows")
}

/// The `GET /presence` response shape: participants plus the path leases they should be shown
/// alongside.
#[derive(Debug, Clone, PartialEq)]
pub struct Presence {
    /// Unexpired presence entries.
    pub participants: Vec<PresenceEntry>,
    /// Live path leases, from [`path_leases`].
    pub path_leases: Vec<PathLeaseSummary>,
}
