//! SQLite DDL and forward migrations for every materialized view table.
//!
//! Owns the table/index definitions for `tickets`, `ticket_deps`, `ticket_children`, `leases`,
//! `resource_claims`, `decisions`, `milestones`, `artifacts`, `evidence`, `budgets`,
//! `participants`, `sessions`, `counters`, `docs`, `doc_provenance`, `provider_usage`,
//! `harness_epochs`, `mirror_links`, `meta`, plus [`drop_views`] (used by `Store::rebuild`) and
//! [`migrate`] (forward-only, mirroring `tm_events::schema`'s shape). This module never touches
//! the `events` table — that belongs to `tm-events` — and it never writes rows; row writes are
//! `materialize.rs`'s job exclusively.

use rusqlite::Connection;

/// The schema version this build of `tm-core` expects. Bump when adding a migration.
pub const SCHEMA_VERSION: i64 = 1;

/// `CREATE TABLE` statements for every materialized view table, in dependency order (referenced
/// tables before their foreign keys). One row per logical entity per `SPEC.md` §4.1.
pub const VIEWS_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS counters (
    kind  TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS milestones (
    id        TEXT PRIMARY KEY,
    title     TEXT NOT NULL,
    state     TEXT NOT NULL,
    closed_by TEXT,
    created   TEXT NOT NULL,
    updated   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tickets (
    id           TEXT PRIMARY KEY,
    kind         TEXT NOT NULL,
    objective    TEXT NOT NULL,
    state        TEXT NOT NULL,
    parent       TEXT,
    milestone    TEXT,
    authority    TEXT NOT NULL,
    executor     TEXT NOT NULL,
    context_refs TEXT NOT NULL,
    success      TEXT NOT NULL,
    verification TEXT NOT NULL,
    budget       TEXT NOT NULL,
    retry        TEXT NOT NULL,
    cycle        TEXT,
    attempts     INTEGER NOT NULL,
    priority     INTEGER NOT NULL,
    created      TEXT NOT NULL,
    updated      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ticket_deps (
    ticket     TEXT NOT NULL,
    depends_on TEXT NOT NULL,
    kind       TEXT NOT NULL,
    PRIMARY KEY (ticket, depends_on)
);

CREATE TABLE IF NOT EXISTS ticket_children (
    parent TEXT NOT NULL,
    child  TEXT NOT NULL,
    PRIMARY KEY (parent, child)
);

CREATE TABLE IF NOT EXISTS resource_claims (
    ticket  TEXT NOT NULL,
    idx     INTEGER NOT NULL,
    pattern TEXT NOT NULL,
    mode    TEXT NOT NULL,
    PRIMARY KEY (ticket, idx)
);

CREATE TABLE IF NOT EXISTS leases (
    id         TEXT PRIMARY KEY,
    ticket     TEXT NOT NULL,
    holder     TEXT NOT NULL,
    authority  TEXT NOT NULL,
    resources  TEXT NOT NULL,
    acquired   TEXT NOT NULL,
    heartbeat  TEXT NOT NULL,
    ttl_seconds INTEGER NOT NULL,
    epoch      INTEGER NOT NULL,
    live       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS decisions (
    id             TEXT PRIMARY KEY,
    subject        TEXT NOT NULL,
    decision       TEXT NOT NULL,
    reason         TEXT NOT NULL,
    evidence       TEXT NOT NULL,
    affected_tickets TEXT NOT NULL,
    affected_docs  TEXT NOT NULL,
    author         TEXT NOT NULL,
    ts             TEXT NOT NULL,
    supersedes     TEXT,
    superseded_by  TEXT
);

CREATE TABLE IF NOT EXISTS artifacts (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    media_type TEXT NOT NULL,
    bytes_len  INTEGER NOT NULL,
    hash       TEXT NOT NULL,
    storage    TEXT NOT NULL,
    inline     BLOB,
    on_disk    TEXT,
    meta       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS evidence (
    id          TEXT PRIMARY KEY,
    ticket      TEXT NOT NULL,
    kind        TEXT NOT NULL,
    artifact    TEXT NOT NULL,
    produced_by TEXT NOT NULL,
    ts          TEXT NOT NULL,
    summary     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS budgets (
    scope_kind TEXT NOT NULL,
    scope_id   TEXT NOT NULL,
    tokens     INTEGER NOT NULL,
    dollars_micros INTEGER NOT NULL,
    wall_seconds INTEGER NOT NULL,
    spent_tokens INTEGER NOT NULL,
    spent_dollars_micros INTEGER NOT NULL,
    spent_wall_seconds INTEGER NOT NULL,
    PRIMARY KEY (scope_kind, scope_id)
);

CREATE TABLE IF NOT EXISTS participants (
    id       TEXT PRIMARY KEY,
    kind     TEXT NOT NULL,
    status   TEXT,
    updated  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id        TEXT PRIMARY KEY,
    started_by TEXT NOT NULL,
    started   TEXT NOT NULL,
    ended     TEXT
);

CREATE TABLE IF NOT EXISTS docs (
    path       TEXT PRIMARY KEY,
    ticket     TEXT,
    state      TEXT NOT NULL,
    updated    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS doc_provenance (
    path       TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    ts         TEXT NOT NULL,
    PRIMARY KEY (path, seq)
);

CREATE TABLE IF NOT EXISTS provider_usage (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    ticket         TEXT,
    session        TEXT,
    tokens         INTEGER NOT NULL,
    dollars_micros INTEGER NOT NULL,
    wall_seconds   INTEGER NOT NULL,
    ts             TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS harness_epochs (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    field      TEXT NOT NULL,
    from_value TEXT NOT NULL,
    to_value   TEXT NOT NULL,
    ts         TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS mirror_links (
    remote    TEXT PRIMARY KEY,
    reference TEXT,
    updated   TEXT NOT NULL
);
";

/// Indexes supporting the query patterns `graph.rs`, `lease.rs`, `decision.rs`, `invariants.rs`
/// and the views need: dependency/child lookups by parent, live-lease lookups by ticket and by
/// resource, decisions by subject/affected path, evidence by ticket.
pub const VIEWS_INDEXES_SQL: &str = "
CREATE INDEX IF NOT EXISTS tickets_state_idx ON tickets (state);
CREATE INDEX IF NOT EXISTS tickets_milestone_idx ON tickets (milestone);
CREATE INDEX IF NOT EXISTS ticket_deps_depends_on_idx ON ticket_deps (depends_on);
CREATE INDEX IF NOT EXISTS ticket_children_child_idx ON ticket_children (child);
CREATE INDEX IF NOT EXISTS leases_ticket_idx ON leases (ticket);
CREATE INDEX IF NOT EXISTS leases_live_idx ON leases (live);
CREATE INDEX IF NOT EXISTS resource_claims_ticket_idx ON resource_claims (ticket);
CREATE INDEX IF NOT EXISTS decisions_subject_idx ON decisions (subject);
CREATE INDEX IF NOT EXISTS decisions_superseded_by_idx ON decisions (superseded_by);
CREATE INDEX IF NOT EXISTS evidence_ticket_idx ON evidence (ticket);
CREATE INDEX IF NOT EXISTS provider_usage_ticket_idx ON provider_usage (ticket);
CREATE INDEX IF NOT EXISTS doc_provenance_path_idx ON doc_provenance (path);
";

/// Tracks which `tm-core` schema migrations have been applied, independent of `tm_events`'s own
/// `schema_version` table (different database concern, same pattern).
pub const SCHEMA_VERSION_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS tm_core_schema_version (
    version    INTEGER NOT NULL PRIMARY KEY,
    applied_at TEXT    NOT NULL
);
";

/// One forward migration: the version it brings the database to, and the DDL that does it.
/// Never rewrites a prior migration's effect; new schema needs are new entries appended here.
pub struct Migration {
    /// The `tm_core_schema_version.version` this migration results in.
    pub version: i64,
    /// The DDL statements applied to reach `version`, run inside one transaction.
    pub ddl: &'static str,
}

/// Every migration, in order, from an empty (but `events`-populated) database to
/// [`SCHEMA_VERSION`].
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    ddl: "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS counters (
    kind  TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS milestones (
    id        TEXT PRIMARY KEY,
    title     TEXT NOT NULL,
    state     TEXT NOT NULL,
    closed_by TEXT,
    created   TEXT NOT NULL,
    updated   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tickets (
    id           TEXT PRIMARY KEY,
    kind         TEXT NOT NULL,
    objective    TEXT NOT NULL,
    state        TEXT NOT NULL,
    parent       TEXT,
    milestone    TEXT,
    authority    TEXT NOT NULL,
    executor     TEXT NOT NULL,
    context_refs TEXT NOT NULL,
    success      TEXT NOT NULL,
    verification TEXT NOT NULL,
    budget       TEXT NOT NULL,
    retry        TEXT NOT NULL,
    cycle        TEXT,
    attempts     INTEGER NOT NULL,
    priority     INTEGER NOT NULL,
    created      TEXT NOT NULL,
    updated      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ticket_deps (
    ticket     TEXT NOT NULL,
    depends_on TEXT NOT NULL,
    kind       TEXT NOT NULL,
    PRIMARY KEY (ticket, depends_on)
);

CREATE TABLE IF NOT EXISTS ticket_children (
    parent TEXT NOT NULL,
    child  TEXT NOT NULL,
    PRIMARY KEY (parent, child)
);

CREATE TABLE IF NOT EXISTS resource_claims (
    ticket  TEXT NOT NULL,
    idx     INTEGER NOT NULL,
    pattern TEXT NOT NULL,
    mode    TEXT NOT NULL,
    PRIMARY KEY (ticket, idx)
);

CREATE TABLE IF NOT EXISTS leases (
    id         TEXT PRIMARY KEY,
    ticket     TEXT NOT NULL,
    holder     TEXT NOT NULL,
    authority  TEXT NOT NULL,
    resources  TEXT NOT NULL,
    acquired   TEXT NOT NULL,
    heartbeat  TEXT NOT NULL,
    ttl_seconds INTEGER NOT NULL,
    epoch      INTEGER NOT NULL,
    live       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS decisions (
    id             TEXT PRIMARY KEY,
    subject        TEXT NOT NULL,
    decision       TEXT NOT NULL,
    reason         TEXT NOT NULL,
    evidence       TEXT NOT NULL,
    affected_tickets TEXT NOT NULL,
    affected_docs  TEXT NOT NULL,
    author         TEXT NOT NULL,
    ts             TEXT NOT NULL,
    supersedes     TEXT,
    superseded_by  TEXT
);

CREATE TABLE IF NOT EXISTS artifacts (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    media_type TEXT NOT NULL,
    bytes_len  INTEGER NOT NULL,
    hash       TEXT NOT NULL,
    storage    TEXT NOT NULL,
    inline     BLOB,
    on_disk    TEXT,
    meta       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS evidence (
    id          TEXT PRIMARY KEY,
    ticket      TEXT NOT NULL,
    kind        TEXT NOT NULL,
    artifact    TEXT NOT NULL,
    produced_by TEXT NOT NULL,
    ts          TEXT NOT NULL,
    summary     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS budgets (
    scope_kind TEXT NOT NULL,
    scope_id   TEXT NOT NULL,
    tokens     INTEGER NOT NULL,
    dollars_micros INTEGER NOT NULL,
    wall_seconds INTEGER NOT NULL,
    spent_tokens INTEGER NOT NULL,
    spent_dollars_micros INTEGER NOT NULL,
    spent_wall_seconds INTEGER NOT NULL,
    PRIMARY KEY (scope_kind, scope_id)
);

CREATE TABLE IF NOT EXISTS participants (
    id       TEXT PRIMARY KEY,
    kind     TEXT NOT NULL,
    status   TEXT,
    updated  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id        TEXT PRIMARY KEY,
    started_by TEXT NOT NULL,
    started   TEXT NOT NULL,
    ended     TEXT
);

CREATE TABLE IF NOT EXISTS docs (
    path       TEXT PRIMARY KEY,
    ticket     TEXT,
    state      TEXT NOT NULL,
    updated    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS doc_provenance (
    path       TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    ts         TEXT NOT NULL,
    PRIMARY KEY (path, seq)
);

CREATE TABLE IF NOT EXISTS provider_usage (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    ticket         TEXT,
    session        TEXT,
    tokens         INTEGER NOT NULL,
    dollars_micros INTEGER NOT NULL,
    wall_seconds   INTEGER NOT NULL,
    ts             TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS harness_epochs (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    field      TEXT NOT NULL,
    from_value TEXT NOT NULL,
    to_value   TEXT NOT NULL,
    ts         TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS mirror_links (
    remote    TEXT PRIMARY KEY,
    reference TEXT,
    updated   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS tickets_state_idx ON tickets (state);
CREATE INDEX IF NOT EXISTS tickets_milestone_idx ON tickets (milestone);
CREATE INDEX IF NOT EXISTS ticket_deps_depends_on_idx ON ticket_deps (depends_on);
CREATE INDEX IF NOT EXISTS ticket_children_child_idx ON ticket_children (child);
CREATE INDEX IF NOT EXISTS leases_ticket_idx ON leases (ticket);
CREATE INDEX IF NOT EXISTS leases_live_idx ON leases (live);
CREATE INDEX IF NOT EXISTS resource_claims_ticket_idx ON resource_claims (ticket);
CREATE INDEX IF NOT EXISTS decisions_subject_idx ON decisions (subject);
CREATE INDEX IF NOT EXISTS decisions_superseded_by_idx ON decisions (superseded_by);
CREATE INDEX IF NOT EXISTS evidence_ticket_idx ON evidence (ticket);
CREATE INDEX IF NOT EXISTS provider_usage_ticket_idx ON provider_usage (ticket);
CREATE INDEX IF NOT EXISTS doc_provenance_path_idx ON doc_provenance (path);
",
}];

/// Bring `conn`'s materialized-view schema forward to [`SCHEMA_VERSION`], applying any
/// [`MIGRATIONS`] entries not yet recorded in `tm_core_schema_version`. Idempotent.
pub fn migrate(conn: &mut Connection) -> tm_types::Result<()> {
    use tm_types::TmError;

    conn.execute_batch(SCHEMA_VERSION_TABLE_SQL)
        .map_err(|e| TmError::storage(format!("Failed to create schema version table: {}", e)))?;

    let current_version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM tm_core_schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|e| TmError::storage(format!("Failed to read current schema version: {}", e)))?;

    for migration in MIGRATIONS {
        if migration.version > current_version {
            let mut tx = conn
                .transaction()
                .map_err(|e| TmError::storage(format!("Failed to start transaction: {}", e)))?;

            tx.execute_batch(migration.ddl)
                .map_err(|e| TmError::storage(format!("Failed to execute migration DDL: {}", e)))?;

            tx.execute(
                "INSERT INTO tm_core_schema_version (version, applied_at) VALUES (?, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                rusqlite::params![migration.version],
            )
            .map_err(|e| TmError::storage(format!("Failed to record migration: {}", e)))?;

            tx.commit()
                .map_err(|e| TmError::storage(format!("Failed to commit migration: {}", e)))?;
        }
    }

    Ok(())
}

/// Drop every materialized view table (but never `events` or `tm_core_schema_version`), for
/// `Store::rebuild` to recreate via [`migrate`] and repopulate via `materialize::replay`.
pub fn drop_views(conn: &mut Connection) -> tm_types::Result<()> {
    use tm_types::TmError;

    let mut tx = conn
        .transaction()
        .map_err(|e| TmError::storage(format!("Failed to start transaction: {}", e)))?;

    let drop_order = [
        "mirror_links",
        "harness_epochs",
        "provider_usage",
        "doc_provenance",
        "docs",
        "sessions",
        "participants",
        "budgets",
        "evidence",
        "artifacts",
        "decisions",
        "leases",
        "resource_claims",
        "ticket_children",
        "ticket_deps",
        "tickets",
        "milestones",
        "counters",
        "meta",
    ];

    for table_name in &drop_order {
        tx.execute(&format!("DROP TABLE IF EXISTS {}", table_name), [])
            .map_err(|e| TmError::storage(format!("Failed to drop table {}: {}", table_name, e)))?;
    }

    tx.commit()
        .map_err(|e| TmError::storage(format!("Failed to commit drop transaction: {}", e)))?;

    migrate(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_memory_db() -> rusqlite::Result<Connection> {
        let mut conn = Connection::open_in_memory()?;
        conn.pragma_update(rusqlite::params!["journal_mode"], "WAL")?;
        Ok(conn)
    }

    #[test]
    fn migrate_creates_schema_version_table() {
        let mut conn = open_memory_db().expect("open in-memory db");

        let result = migrate(&mut conn);
        assert!(result.is_ok());

        let version_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='tm_core_schema_version')",
                [],
                |row| row.get(0),
            )
            .expect("check version table exists");
        assert!(version_exists, "tm_core_schema_version table should exist");
    }

    #[test]
    fn migrate_creates_all_view_tables() {
        let mut conn = open_memory_db().expect("open in-memory db");

        let result = migrate(&mut conn);
        assert!(result.is_ok());

        let expected_tables = [
            "meta",
            "counters",
            "milestones",
            "tickets",
            "ticket_deps",
            "ticket_children",
            "resource_claims",
            "leases",
            "decisions",
            "artifacts",
            "evidence",
            "budgets",
            "participants",
            "sessions",
            "docs",
            "doc_provenance",
            "provider_usage",
            "harness_epochs",
            "mirror_links",
        ];

        for table_name in &expected_tables {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
                    rusqlite::params![table_name],
                    |row| row.get(0),
                )
                .expect("check table exists");
            assert!(exists, "Table {} should exist after migrate", table_name);
        }
    }

    #[test]
    fn migrate_creates_indexes() {
        let mut conn = open_memory_db().expect("open in-memory db");

        let result = migrate(&mut conn);
        assert!(result.is_ok());

        let expected_indexes = [
            "tickets_state_idx",
            "tickets_milestone_idx",
            "ticket_deps_depends_on_idx",
            "ticket_children_child_idx",
            "leases_ticket_idx",
            "leases_live_idx",
            "resource_claims_ticket_idx",
            "decisions_subject_idx",
            "decisions_superseded_by_idx",
            "evidence_ticket_idx",
            "provider_usage_ticket_idx",
            "doc_provenance_path_idx",
        ];

        for index_name in &expected_indexes {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name=?)",
                    rusqlite::params![index_name],
                    |row| row.get(0),
                )
                .expect("check index exists");
            assert!(exists, "Index {} should exist after migrate", index_name);
        }
    }

    #[test]
    fn migrate_is_idempotent() {
        let mut conn = open_memory_db().expect("open in-memory db");

        let first_result = migrate(&mut conn);
        assert!(first_result.is_ok());

        let second_result = migrate(&mut conn);
        assert!(second_result.is_ok());

        let version: i64 = conn
            .query_row("SELECT COUNT(*) FROM tm_core_schema_version", [], |row| {
                row.get(0)
            })
            .expect("count schema versions");
        assert_eq!(version, 1, "Migration should only be recorded once");
    }

    #[test]
    fn migrate_records_version_with_timestamp() {
        let mut conn = open_memory_db().expect("open in-memory db");

        let result = migrate(&mut conn);
        assert!(result.is_ok());

        let (recorded_version, has_timestamp): (i64, bool) = conn
            .query_row(
                "SELECT version, applied_at IS NOT NULL FROM tm_core_schema_version WHERE version = ?",
                rusqlite::params![1],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read recorded migration");

        assert_eq!(recorded_version, 1);
        assert!(
            has_timestamp,
            "Migration should record applied_at timestamp"
        );
    }

    #[test]
    fn drop_views_removes_all_tables() {
        let mut conn = open_memory_db().expect("open in-memory db");

        let setup_result = migrate(&mut conn);
        assert!(setup_result.is_ok());

        let drop_result = drop_views(&mut conn);
        assert!(drop_result.is_ok());

        let expected_tables = [
            "meta",
            "counters",
            "milestones",
            "tickets",
            "ticket_deps",
            "ticket_children",
            "resource_claims",
            "leases",
            "decisions",
            "artifacts",
            "evidence",
            "budgets",
            "participants",
            "sessions",
            "docs",
            "doc_provenance",
            "provider_usage",
            "harness_epochs",
            "mirror_links",
        ];

        for table_name in &expected_tables {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
                    rusqlite::params![table_name],
                    |row| row.get(0),
                )
                .expect("check table exists");
            assert!(
                exists,
                "Table {} should be recreated after drop_views",
                table_name
            );
        }
    }

    #[test]
    fn drop_views_preserves_schema_version_table() {
        let mut conn = open_memory_db().expect("open in-memory db");

        let setup_result = migrate(&mut conn);
        assert!(setup_result.is_ok());

        let version_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM tm_core_schema_version", [], |row| {
                row.get(0)
            })
            .expect("count schema versions before drop");

        let drop_result = drop_views(&mut conn);
        assert!(drop_result.is_ok());

        let version_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM tm_core_schema_version", [], |row| {
                row.get(0)
            })
            .expect("count schema versions after drop");

        assert_eq!(
            version_before, version_after,
            "Schema version table should be preserved"
        );
    }

    #[test]
    fn drop_views_creates_empty_tables() {
        let mut conn = open_memory_db().expect("open in-memory db");

        let setup_result = migrate(&mut conn);
        assert!(setup_result.is_ok());

        conn.execute("INSERT INTO meta (key, value) VALUES ('test', 'data')", [])
            .expect("insert test data");

        let drop_result = drop_views(&mut conn);
        assert!(drop_result.is_ok());

        let meta_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM meta", [], |row| row.get(0))
            .expect("count meta rows");
        assert_eq!(meta_count, 0, "Tables should be empty after drop_views");
    }

    #[test]
    fn migrate_applies_only_newer_migrations() {
        let mut conn = open_memory_db().expect("open in-memory db");

        let first_result = migrate(&mut conn);
        assert!(first_result.is_ok());

        let applied_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tm_core_schema_version", [], |row| {
                row.get(0)
            })
            .expect("count applied migrations");
        assert_eq!(applied_count, 1);

        let second_result = migrate(&mut conn);
        assert!(second_result.is_ok());

        let final_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tm_core_schema_version", [], |row| {
                row.get(0)
            })
            .expect("count applied migrations after second call");
        assert_eq!(final_count, 1, "Should not apply the same migration twice");
    }

    #[test]
    fn migration_1_includes_all_required_columns() {
        let mut conn = open_memory_db().expect("open in-memory db");

        let result = migrate(&mut conn);
        assert!(result.is_ok());

        // Verify tickets table has all expected columns
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(tickets)")
            .expect("prepare pragma")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query columns")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect columns");

        let expected_cols = vec![
            "id",
            "kind",
            "objective",
            "state",
            "parent",
            "milestone",
            "authority",
            "executor",
            "context_refs",
            "success",
            "verification",
            "budget",
            "retry",
            "cycle",
            "attempts",
            "priority",
            "created",
            "updated",
        ];

        for expected in expected_cols {
            assert!(
                columns.contains(&expected.to_string()),
                "Column {} should exist in tickets table",
                expected
            );
        }
    }
}
