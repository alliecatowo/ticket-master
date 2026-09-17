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
use tm_core::ResourceMode;
use tm_types::{LeaseId, ParticipantId, TicketId, Timestamp};

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
        let participant = entry.participant.clone();
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries.insert(participant, entry);
    }

    /// Drop `participant`'s entry outright (explicit "I'm done" signal, distinct from TTL
    /// expiry).
    pub fn remove(&self, participant: &ParticipantId) {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries.remove(participant);
    }

    /// Every live (not necessarily unexpired — callers sweep first if they want only-fresh)
    /// entry, for `GET /presence`.
    pub fn snapshot(&self) -> Vec<PresenceEntry> {
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries.values().cloned().collect()
    }

    /// Remove every entry expired as of `now` (per [`sweep_expired_at`]), returning the
    /// participants that were dropped.
    pub fn sweep_expired(&self, now: Timestamp) -> Vec<ParticipantId> {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        sweep_expired_at(&mut entries, now)
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
    let expired: Vec<ParticipantId> = entries
        .iter()
        .filter(|(_, e)| e.is_expired(now))
        .map(|(k, _)| k.clone())
        .collect();
    entries.retain(|_, e| !e.is_expired(now));
    expired
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
    let mut summaries = Vec::new();
    for lease in view.leases.values() {
        for claim in &lease.resources {
            let paths = claim
                .paths
                .patterns()
                .iter()
                .map(|p| p.as_str().to_string())
                .collect();
            summaries.push(PathLeaseSummary {
                lease: lease.id.clone(),
                ticket: lease.ticket.clone(),
                holder: lease.holder.clone(),
                mode: claim.mode,
                paths,
            });
        }
    }
    summaries
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

/// Type alias for public API export.
pub type PresenceStore = PresenceTable;

/// Type alias for public API export.
pub type SurfacedLease = PathLeaseSummary;

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::lease::Lease;
    use tm_core::ticket::ResourceClaim;
    use tm_core::view::ProjectView;
    use tm_types::{LeaseId, ParticipantId, PatternSet, TicketId, Timestamp};

    fn make_participant(id: &str) -> ParticipantId {
        ParticipantId::new(format!("human:{id}")).expect("valid participant id")
    }

    fn make_ticket(id: &str) -> TicketId {
        TicketId::new(id).expect("valid ticket id")
    }

    fn make_lease_id(id: &str) -> LeaseId {
        LeaseId::new(id).expect("valid lease id")
    }

    fn make_entry(
        participant: ParticipantId,
        ticket: Option<TicketId>,
        file: Option<String>,
        action: String,
        last_seen: Timestamp,
        ttl_seconds: u32,
    ) -> PresenceEntry {
        PresenceEntry {
            participant,
            ticket,
            file,
            action,
            last_seen,
            ttl_seconds,
        }
    }

    #[test]
    fn upsert_inserts_new_entry() {
        let table = PresenceTable::new();
        let participant = make_participant("alice");
        let ticket = Some(make_ticket("T-1"));
        let entry = make_entry(
            participant.clone(),
            ticket.clone(),
            Some("main.rs".to_string()),
            "editing".to_string(),
            Timestamp::EPOCH,
            60,
        );

        table.upsert(entry.clone());
        let snapshot = table.snapshot();

        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0], entry);
    }

    #[test]
    fn upsert_replaces_existing_entry() {
        let table = PresenceTable::new();
        let participant = make_participant("alice");

        let entry1 = make_entry(
            participant.clone(),
            Some(make_ticket("T-1")),
            Some("main.rs".to_string()),
            "editing".to_string(),
            Timestamp::EPOCH,
            60,
        );
        table.upsert(entry1);

        let entry2 = make_entry(
            participant.clone(),
            Some(make_ticket("T-2")),
            Some("lib.rs".to_string()),
            "reviewing".to_string(),
            Timestamp::EPOCH.plus_seconds(10),
            120,
        );
        table.upsert(entry2.clone());

        let snapshot = table.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0], entry2);
    }

    #[test]
    fn remove_deletes_entry() {
        let table = PresenceTable::new();
        let participant = make_participant("alice");
        let entry = make_entry(
            participant.clone(),
            Some(make_ticket("T-1")),
            None,
            "idle".to_string(),
            Timestamp::EPOCH,
            60,
        );

        table.upsert(entry);
        assert_eq!(table.snapshot().len(), 1);

        table.remove(&participant);
        assert_eq!(table.snapshot().len(), 0);
    }

    #[test]
    fn remove_no_op_when_not_present() {
        let table = PresenceTable::new();
        let participant = make_participant("alice");

        table.remove(&participant);
        assert_eq!(table.snapshot().len(), 0);
    }

    #[test]
    fn snapshot_empty_table() {
        let table = PresenceTable::new();
        let snapshot = table.snapshot();
        assert_eq!(snapshot.len(), 0);
    }

    #[test]
    fn snapshot_multiple_entries() {
        let table = PresenceTable::new();
        let alice = make_participant("alice");
        let bob = make_participant("bob");

        let entry1 = make_entry(
            alice.clone(),
            Some(make_ticket("T-1")),
            Some("main.rs".to_string()),
            "editing".to_string(),
            Timestamp::EPOCH,
            60,
        );
        let entry2 = make_entry(
            bob.clone(),
            Some(make_ticket("T-2")),
            Some("lib.rs".to_string()),
            "reviewing".to_string(),
            Timestamp::EPOCH.plus_seconds(5),
            120,
        );

        table.upsert(entry1.clone());
        table.upsert(entry2.clone());

        let snapshot = table.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.contains(&entry1));
        assert!(snapshot.contains(&entry2));
    }

    #[test]
    fn sweep_expired_at_removes_expired_entries() {
        let now = Timestamp::EPOCH.plus_seconds(100);
        let mut entries = BTreeMap::new();

        let alice = make_participant("alice");
        let bob = make_participant("bob");

        // Alice's entry expires at 60 seconds (EPOCH + 30 + 30)
        let alice_entry = make_entry(
            alice.clone(),
            Some(make_ticket("T-1")),
            None,
            "idle".to_string(),
            Timestamp::EPOCH.plus_seconds(30),
            30,
        );

        // Bob's entry expires at 200 seconds (EPOCH + 50 + 150)
        let bob_entry = make_entry(
            bob.clone(),
            Some(make_ticket("T-2")),
            None,
            "editing".to_string(),
            Timestamp::EPOCH.plus_seconds(50),
            150,
        );

        entries.insert(alice.clone(), alice_entry);
        entries.insert(bob.clone(), bob_entry);

        let removed = sweep_expired_at(&mut entries, now);

        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0], alice);
        assert_eq!(entries.len(), 1);
        assert!(entries.contains_key(&bob));
        assert!(!entries.contains_key(&alice));
    }

    #[test]
    fn sweep_expired_at_preserves_fresh_entries() {
        let now = Timestamp::EPOCH.plus_seconds(50);
        let mut entries = BTreeMap::new();

        let participant = make_participant("alice");
        let entry = make_entry(
            participant.clone(),
            Some(make_ticket("T-1")),
            None,
            "editing".to_string(),
            Timestamp::EPOCH.plus_seconds(30),
            60,
        );

        entries.insert(participant.clone(), entry);

        let removed = sweep_expired_at(&mut entries, now);

        assert_eq!(removed.len(), 0);
        assert_eq!(entries.len(), 1);
        assert!(entries.contains_key(&participant));
    }

    #[test]
    fn sweep_expired_at_empty_table() {
        let mut entries = BTreeMap::new();
        let removed = sweep_expired_at(&mut entries, Timestamp::EPOCH.plus_seconds(100));

        assert_eq!(removed.len(), 0);
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn sweep_expired_preserves_order() {
        let now = Timestamp::EPOCH.plus_seconds(100);
        let mut entries = BTreeMap::new();

        // Insert in non-alphabetical order
        for id in &["charlie", "alice", "bob"] {
            let participant = make_participant(id);
            let entry = make_entry(
                participant.clone(),
                None,
                None,
                "idle".to_string(),
                Timestamp::EPOCH,
                50,
            );
            entries.insert(participant, entry);
        }

        let removed = sweep_expired_at(&mut entries, now);

        // BTreeMap iteration is ordered by key, so we should get them sorted
        assert_eq!(removed.len(), 3);
        assert_eq!(removed[0], make_participant("alice"));
        assert_eq!(removed[1], make_participant("bob"));
        assert_eq!(removed[2], make_participant("charlie"));
    }

    #[test]
    fn sweep_expired_table_method() {
        let table = PresenceTable::new();
        let alice = make_participant("alice");
        let bob = make_participant("bob");
        let now = Timestamp::EPOCH.plus_seconds(100);

        let alice_entry = make_entry(
            alice.clone(),
            None,
            None,
            "idle".to_string(),
            Timestamp::EPOCH.plus_seconds(30),
            30,
        );
        let bob_entry = make_entry(
            bob.clone(),
            None,
            None,
            "idle".to_string(),
            Timestamp::EPOCH.plus_seconds(50),
            150,
        );

        table.upsert(alice_entry);
        table.upsert(bob_entry);

        let removed = table.sweep_expired(now);

        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0], alice);

        let snapshot = table.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].participant, bob);
    }

    #[test]
    fn presence_entry_is_expired_at_boundary() {
        let entry = make_entry(
            make_participant("alice"),
            None,
            None,
            "idle".to_string(),
            Timestamp::EPOCH,
            60,
        );

        // Not expired just before TTL
        assert!(!entry.is_expired(Timestamp::EPOCH.plus_seconds(59)));

        // Expired exactly at TTL
        assert!(entry.is_expired(Timestamp::EPOCH.plus_seconds(60)));

        // Expired after TTL
        assert!(entry.is_expired(Timestamp::EPOCH.plus_seconds(61)));
    }

    #[test]
    fn path_leases_empty_view() {
        let view = ProjectView::empty();
        let summaries = path_leases(&view);
        assert_eq!(summaries.len(), 0);
    }

    #[test]
    fn path_leases_single_claim() {
        let mut view = ProjectView::empty();
        let lease_id = make_lease_id("L-aaaaaaaaaaaa");
        let ticket_id = make_ticket("T-1");
        let holder = make_participant("alice");

        let patterns = PatternSet::parse(["src/**/*.rs"]).expect("valid set");

        let claim = ResourceClaim {
            paths: patterns,
            mode: ResourceMode::Exclusive,
        };

        let lease = Lease {
            id: lease_id.clone(),
            ticket: ticket_id.clone(),
            holder: holder.clone(),
            authority: tm_types::Authority::none(),
            resources: vec![claim],
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 60,
            epoch: 1,
        };

        view.leases.insert(lease_id.clone(), lease);

        let summaries = path_leases(&view);

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].lease, lease_id);
        assert_eq!(summaries[0].ticket, ticket_id);
        assert_eq!(summaries[0].holder, holder);
        assert_eq!(summaries[0].mode, ResourceMode::Exclusive);
        assert_eq!(summaries[0].paths.len(), 1);
        assert_eq!(summaries[0].paths[0], "src/**/*.rs");
    }

    #[test]
    fn path_leases_multiple_claims_one_lease() {
        let mut view = ProjectView::empty();
        let lease_id = make_lease_id("L-bbbbbbbbbbbb");
        let ticket_id = make_ticket("T-2");
        let holder = make_participant("bob");

        let patterns1 = PatternSet::parse(["src/**/*.rs"]).expect("valid set");
        let patterns2 = PatternSet::parse(["tests/**/*.rs"]).expect("valid set");

        let claim1 = ResourceClaim {
            paths: patterns1,
            mode: ResourceMode::Exclusive,
        };
        let claim2 = ResourceClaim {
            paths: patterns2,
            mode: ResourceMode::Shared,
        };

        let lease = Lease {
            id: lease_id.clone(),
            ticket: ticket_id.clone(),
            holder: holder.clone(),
            authority: tm_types::Authority::none(),
            resources: vec![claim1, claim2],
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 60,
            epoch: 1,
        };

        view.leases.insert(lease_id.clone(), lease);

        let summaries = path_leases(&view);

        assert_eq!(summaries.len(), 2);

        // Both summaries share the same lease/ticket/holder
        for summary in &summaries {
            assert_eq!(summary.lease, lease_id);
            assert_eq!(summary.ticket, ticket_id);
            assert_eq!(summary.holder, holder);
        }

        // First claim: exclusive, src/**/*.rs
        assert_eq!(summaries[0].mode, ResourceMode::Exclusive);
        assert_eq!(summaries[0].paths.len(), 1);
        assert_eq!(summaries[0].paths[0], "src/**/*.rs");

        // Second claim: shared, tests/**/*.rs
        assert_eq!(summaries[1].mode, ResourceMode::Shared);
        assert_eq!(summaries[1].paths.len(), 1);
        assert_eq!(summaries[1].paths[0], "tests/**/*.rs");
    }

    #[test]
    fn path_leases_multiple_patterns_one_claim() {
        let mut view = ProjectView::empty();
        let lease_id = make_lease_id("L-cccccccccccc");
        let ticket_id = make_ticket("T-3");
        let holder = make_participant("charlie");

        let patterns =
            PatternSet::parse(["src/*.rs", "lib/*.rs", "tests/*.rs"]).expect("valid set");

        let claim = ResourceClaim {
            paths: patterns,
            mode: ResourceMode::Shared,
        };

        let lease = Lease {
            id: lease_id.clone(),
            ticket: ticket_id.clone(),
            holder: holder.clone(),
            authority: tm_types::Authority::none(),
            resources: vec![claim],
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 60,
            epoch: 1,
        };

        view.leases.insert(lease_id.clone(), lease);

        let summaries = path_leases(&view);

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].paths.len(), 3);
        assert!(summaries[0].paths.contains(&"src/*.rs".to_string()));
        assert!(summaries[0].paths.contains(&"lib/*.rs".to_string()));
        assert!(summaries[0].paths.contains(&"tests/*.rs".to_string()));
    }

    #[test]
    fn default_creates_empty_table() {
        let table = PresenceTable::default();
        assert_eq!(table.snapshot().len(), 0);
    }
}
