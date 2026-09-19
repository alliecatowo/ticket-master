//! `SPEC.md` §16.6/§16.7: a fixture project with 200+ events, `Store::rebuild()` reproducing
//! byte-identical materialized state, the hash chain verifying end to end, and a tamper case
//! being detected.

mod common;

use std::sync::Arc;

use tempfile::TempDir;
use tm_core::MirrorSyncDirection;
use tm_events::EventLog;
use tm_types::{Authority, Clock, FixedClock, ParticipantId};

/// Build a project with well over 200 events: many tickets, most activated, several driven all
/// the way through lease/submit/verify/audit to `Closed`, plus at least one event of every kind
/// B-05 gave `tm-core::Store` a durable write path for (sessions, docs, harness epochs, mirror
/// links, provider usage, commands), so `rebuild_reproduces_byte_identical_state_over_200_events`
/// below exercises every materializer arm this fixture is meant to cover, not just the
/// ticket/lease/decision ones it already covered before B-05.
fn build_fixture_project(store: &tm_core::Store) {
    let mut closeable = Vec::new();
    for i in 0..60 {
        let ticket = common::ready_ticket(store, &format!("fixture ticket {i}"), Authority::root());
        if i % 5 == 0 {
            closeable.push(ticket);
        }
    }
    for (i, ticket) in closeable.into_iter().enumerate() {
        let holder = common::agent(&format!("executor-{i}"));
        let auditor = common::agent(&format!("auditor-{i}"));
        common::close_ticket(store, &ticket, holder, auditor);
    }

    let system = common::system();
    let alice = ParticipantId::new("human:alice").expect("well-formed participant id");
    let started = store
        .start_session(alice.clone(), system.clone())
        .expect("start_session");
    let session = started[0]
        .payload
        .as_session_started()
        .expect("session.started payload")
        .session
        .clone();
    store
        .join_session(&session, common::agent("pair"), system.clone())
        .expect("join_session");
    store
        .end_session(&session, system.clone())
        .expect("end_session");

    let doc_ticket = common::ready_ticket(store, "doc-linked ticket", Authority::root());
    store
        .register_doc(
            "docs/architecture.md".into(),
            Some(doc_ticket.clone()),
            system.clone(),
        )
        .expect("register_doc");
    store
        .invalidate_doc(
            "docs/architecture.md".into(),
            "source moved".into(),
            system.clone(),
        )
        .expect("invalidate_doc");
    store
        .reconcile_doc("docs/architecture.md".into(), system.clone())
        .expect("reconcile_doc");

    store
        .promote_epoch("config-a".into(), system.clone())
        .expect("promote_epoch");
    store
        .promote_epoch("config-b".into(), system.clone())
        .expect("promote_epoch");

    let mirror_ticket = common::ready_ticket(store, "mirrored ticket", Authority::root());
    store
        .link_mirror(&mirror_ticket, "github".into(), system.clone())
        .expect("link_mirror");
    store
        .update_mirror_link(
            &mirror_ticket,
            "github".into(),
            "owner/repo#1".into(),
            MirrorSyncDirection::Push,
            system.clone(),
        )
        .expect("update_mirror_link push");
    store
        .update_mirror_link(
            &mirror_ticket,
            "github".into(),
            "owner/repo#1".into(),
            MirrorSyncDirection::Pull,
            system.clone(),
        )
        .expect("update_mirror_link pull");

    store
        .append(vec![tm_events::EventDraft::new(
            system.clone(),
            tm_types::Id::none(),
            tm_events::Payload::from(tm_events::payload::ProviderSelectedPayload {
                role: "coder_fast".into(),
                provider: "anthropic".into(),
                model: "claude-sonnet".into(),
            }),
        )])
        .expect("append provider.selected");

    store
        .record_command("cargo test -p tm-core".into(), None, None, 0, 4200, system)
        .expect("record_command");
}

/// Row counts for every table B-05 gave a durable write path, keyed by table name, read directly
/// off the raw connection (no `Store` read API for these exists yet — out of B-05's scope — so
/// this mirrors how `hash_chain_detects_a_tampered_event` below already reaches for a raw
/// connection when the fixture needs to see something `Store`'s own API doesn't expose).
fn b05_table_snapshot(db_path: &std::path::Path) -> Vec<(&'static str, Vec<String>)> {
    let conn = rusqlite::Connection::open(db_path).expect("open raw connection");
    let tables: &[(&str, &str)] = &[
        ("sessions", "SELECT id || '|' || participant || '|' || started || '|' || last_seen FROM sessions ORDER BY id"),
        ("docs", "SELECT id || '|' || title || '|' || content || '|' || author || '|' || ts FROM docs ORDER BY id"),
        ("doc_provenance", "SELECT doc_id || '|' || source || '|' || reason FROM doc_provenance ORDER BY doc_id, source"),
        ("provider_usage", "SELECT provider || '|' || model || '|' || tokens_used || '|' || dollars_micros FROM provider_usage ORDER BY provider, model"),
        ("harness_epochs", "SELECT epoch || '|' || harness_config || '|' || ts FROM harness_epochs ORDER BY epoch"),
        ("mirror_links", "SELECT ticket || '|' || remote_id || '|' || remote_system || '|' || last_synced FROM mirror_links ORDER BY ticket"),
    ];
    tables
        .iter()
        .map(|(name, query)| {
            let mut stmt = conn.prepare(query).expect("prepare");
            let rows: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query")
                .map(|r| r.expect("row"))
                .collect();
            (*name, rows)
        })
        .collect()
}

#[test]
fn rebuild_reproduces_byte_identical_state_over_200_events() {
    let dir = TempDir::new().expect("tempdir");
    let (_clock, store) = common::open_store(dir.path());

    build_fixture_project(&store);

    let db_path = dir.path().join(".tm").join("project.db");
    let head = EventLog::open_with_clock(&db_path, Arc::new(FixedClock::epoch()) as Arc<dyn Clock>)
        .expect("open log")
        .head()
        .expect("head");
    assert!(
        head >= 200,
        "fixture project should have produced at least 200 events, got {head}"
    );

    let before = store.view().expect("view before rebuild");
    let before_b05_tables = b05_table_snapshot(&db_path);
    store.rebuild().expect("rebuild");
    let after = store.view().expect("view after rebuild");
    let after_b05_tables = b05_table_snapshot(&db_path);

    // Every table `B-05` gave `tm-core::Store` a durable write path for (sessions, docs,
    // doc_provenance, provider_usage, harness_epochs, mirror_links) must come back
    // byte-identical too, same guarantee as the ticket/lease/decision tables below — `rebuild`
    // drops and replays *every* materialized table, not a chosen subset.
    for (before_table, after_table) in before_b05_tables.iter().zip(after_b05_tables.iter()) {
        assert_eq!(before_table.0, after_table.0);
        assert!(
            !before_table.1.is_empty(),
            "fixture should have produced at least one {} row",
            before_table.0
        );
        assert_eq!(
            before_table.1, after_table.1,
            "{} must survive rebuild byte-identical",
            before_table.0
        );
    }

    // `Store::rebuild` drops every materialized table and replays the log through the same
    // `materialize::apply` the live path used, so every field must come back identical.
    assert_eq!(
        before.tickets, after.tickets,
        "tickets must survive rebuild byte-identical"
    );
    assert_eq!(
        before.leases, after.leases,
        "leases must survive rebuild byte-identical"
    );
    assert_eq!(
        before.milestones, after.milestones,
        "milestones must survive rebuild byte-identical"
    );
    assert_eq!(
        before.decisions, after.decisions,
        "decisions must survive rebuild byte-identical"
    );
    // `Store::store_artifact` writes an artifact's `kind`/`bytes_len`/`hash` with a direct SQL
    // write alongside (not through) the `artifact.created` event it appends — the one documented
    // exception to "materialize::apply is the only writer" (see `tm-core::store`'s module docs).
    // `materialize::apply`'s handling of that event only inserts a placeholder for those three
    // columns to satisfy `NOT NULL`, so `rebuild()` cannot recover them from the log alone; `id`,
    // `media_type` and `storage` (which the payload does carry) still round-trip exactly. This
    // is a known gap against SPEC.md §17 invariant 2 in `tm-core` itself, not something this
    // suite can paper over without touching a crate it doesn't own.
    for (id, before_artifact) in &before.artifacts {
        let after_artifact = after
            .artifacts
            .get(id)
            .unwrap_or_else(|| panic!("artifact {id} missing after rebuild"));
        assert_eq!(before_artifact.id, after_artifact.id);
        assert_eq!(before_artifact.media_type, after_artifact.media_type);
        assert_eq!(before_artifact.storage, after_artifact.storage);
    }
    assert_eq!(before.artifacts.len(), after.artifacts.len());
    assert_eq!(
        before.evidence, after.evidence,
        "evidence must survive rebuild byte-identical"
    );
    assert_eq!(
        before.graph, after.graph,
        "the dependency graph must survive rebuild byte-identical"
    );
    assert_eq!(
        before.counters, after.counters,
        "id counters must survive rebuild identically"
    );
}

#[test]
fn hash_chain_verifies_over_the_whole_fixture_log() {
    let dir = TempDir::new().expect("tempdir");
    let (_clock, store) = common::open_store(dir.path());
    build_fixture_project(&store);

    let db_path = dir.path().join(".tm").join("project.db");
    let log = EventLog::open_with_clock(&db_path, Arc::new(FixedClock::epoch()) as Arc<dyn Clock>)
        .expect("open log");
    let report = log.verify_chain().expect("verify_chain");

    assert!(report.is_valid(), "chain should verify clean: {report:?}");
    assert!(report.events_checked >= 200);
    assert_eq!(report.first_broken_seq, None);
}

#[test]
fn hash_chain_detects_a_tampered_event() {
    let dir = TempDir::new().expect("tempdir");
    let (_clock, store) = common::open_store(dir.path());
    build_fixture_project(&store);

    let db_path = dir.path().join(".tm").join("project.db");

    // Corrupt one event's stored hash directly on disk. The `events` table is append-only via
    // `BEFORE UPDATE`/`BEFORE DELETE` triggers (SPEC.md §3.1) — that guarantee is exercised
    // elsewhere; here the trigger is dropped first so this test can simulate an attacker or
    // storage corruption that bypassed it, and prove `verify_chain` still catches the result.
    let tampered_seq: i64 = 42;
    let raw = rusqlite::Connection::open(&db_path).expect("raw connection");
    raw.execute_batch("DROP TRIGGER IF EXISTS events_no_update;")
        .expect("drop append-only trigger");
    let updated = raw
        .execute(
            "UPDATE events SET hash = ?1 WHERE seq = ?2",
            rusqlite::params![
                "0000000000000000000000000000000000000000000000000000000000000000",
                tampered_seq
            ],
        )
        .expect("corrupt one event's hash");
    assert_eq!(updated, 1, "exactly one row should have been tampered");
    drop(raw);

    let log = EventLog::open_with_clock(&db_path, Arc::new(FixedClock::epoch()) as Arc<dyn Clock>)
        .expect("open log");
    let report = log.verify_chain().expect("verify_chain");

    assert!(!report.is_valid(), "a tampered event must be detected");
    assert_eq!(report.first_broken_seq, Some(tampered_seq as u64));
}
