//! SQLite DDL and forward migrations for every materialized-view table (`SPEC.md` §4.1).
//!
//! Owns the schema for `tickets`, `ticket_deps`, `ticket_children`, `leases`, `resource_claims`,
//! `decisions`, `milestones`, `artifacts`, `evidence`, `budgets`, `participants`, `sessions`,
//! `counters`, `docs`, `doc_provenance`, `provider_usage`, `harness_epochs`, `mirror_links`, and
//! `meta`, plus [`drop_views`] which `Store::rebuild` uses to blow away every one of these
//! tables (never the `tm-events` `events` table itself) before replaying from `seq` 0.
//!
//! Every table here stores denormalized, derived state: the event log is the truth, these are a
//! cache of it. Columns therefore favor the shapes [`crate::materialize::apply`] needs to write
//! quickly and [`crate::view`] needs to read cheaply, not any particular normal form.

use rusqlite::Connection;
use tm_types::TmError;

/// The schema version this build of `tm-core` expects. Bump when adding a migration.
pub const SCHEMA_VERSION: i64 = 1;

/// One materialized table's name, paired with the `CREATE TABLE IF NOT EXISTS` DDL for it.
pub struct TableDef {
    /// The table name, matching `SPEC.md` §4.1's list exactly.
    pub name: &'static str,
    /// `CREATE TABLE IF NOT EXISTS ...` DDL.
    pub create_sql: &'static str,
}

/// Every materialized table this crate owns, in dependency order (tables with foreign-key-style
/// references to `tickets`/`milestones`/etc. after the tables they reference), so
/// [`create_views`] can execute them in order without a deferred-constraint dance.
pub const TABLES: &[TableDef] = &[
    TableDef {
        name: "counters",
        create_sql: "
            CREATE TABLE IF NOT EXISTS counters (
                counter_name TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            )
        ",
    },
    TableDef {
        name: "participants",
        create_sql: "
            CREATE TABLE IF NOT EXISTS participants (
                id TEXT PRIMARY KEY,
                status TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "sessions",
        create_sql: "
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                participant TEXT NOT NULL,
                started TEXT NOT NULL,
                last_seen TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "artifacts",
        create_sql: "
            CREATE TABLE IF NOT EXISTS artifacts (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                media_type TEXT NOT NULL,
                bytes_len INTEGER NOT NULL,
                hash TEXT NOT NULL,
                storage TEXT NOT NULL,
                meta TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "decisions",
        create_sql: "
            CREATE TABLE IF NOT EXISTS decisions (
                id TEXT PRIMARY KEY,
                subject TEXT NOT NULL,
                decision TEXT NOT NULL,
                reason TEXT NOT NULL,
                evidence TEXT NOT NULL,
                affected_tickets TEXT NOT NULL,
                affected_paths TEXT NOT NULL,
                author TEXT NOT NULL,
                ts TEXT NOT NULL,
                supersedes TEXT,
                superseded_by TEXT
            )
        ",
    },
    TableDef {
        name: "milestones",
        create_sql: "
            CREATE TABLE IF NOT EXISTS milestones (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                tickets TEXT NOT NULL,
                state TEXT NOT NULL,
                closed_by TEXT,
                assumptions TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "tickets",
        create_sql: "
            CREATE TABLE IF NOT EXISTS tickets (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                objective TEXT NOT NULL,
                state TEXT NOT NULL,
                parent TEXT,
                milestone TEXT,
                authority TEXT NOT NULL,
                resources TEXT NOT NULL,
                executor TEXT NOT NULL,
                context_refs TEXT NOT NULL,
                success TEXT NOT NULL,
                verification TEXT NOT NULL,
                budget TEXT NOT NULL,
                retry TEXT NOT NULL,
                cycle TEXT,
                attempts INTEGER NOT NULL,
                failures TEXT NOT NULL,
                priority INTEGER NOT NULL,
                created TEXT NOT NULL,
                updated TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "ticket_deps",
        create_sql: "
            CREATE TABLE IF NOT EXISTS ticket_deps (
                ticket TEXT NOT NULL,
                depends_on TEXT NOT NULL,
                kind TEXT NOT NULL,
                PRIMARY KEY (ticket, depends_on)
            )
        ",
    },
    TableDef {
        name: "ticket_children",
        create_sql: "
            CREATE TABLE IF NOT EXISTS ticket_children (
                parent TEXT NOT NULL,
                child TEXT NOT NULL,
                PRIMARY KEY (parent, child)
            )
        ",
    },
    TableDef {
        name: "leases",
        create_sql: "
            CREATE TABLE IF NOT EXISTS leases (
                id TEXT PRIMARY KEY,
                ticket TEXT NOT NULL,
                holder TEXT NOT NULL,
                authority TEXT NOT NULL,
                resources TEXT NOT NULL,
                acquired TEXT NOT NULL,
                heartbeat TEXT NOT NULL,
                ttl_seconds INTEGER NOT NULL,
                epoch INTEGER NOT NULL
            )
        ",
    },
    TableDef {
        name: "resource_claims",
        create_sql: "
            CREATE TABLE IF NOT EXISTS resource_claims (
                lease TEXT NOT NULL,
                paths TEXT NOT NULL,
                mode TEXT NOT NULL,
                PRIMARY KEY (lease)
            )
        ",
    },
    TableDef {
        name: "evidence",
        create_sql: "
            CREATE TABLE IF NOT EXISTS evidence (
                ticket TEXT NOT NULL,
                kind TEXT NOT NULL,
                artifact TEXT NOT NULL,
                produced_by TEXT NOT NULL,
                ts TEXT NOT NULL,
                summary TEXT NOT NULL,
                PRIMARY KEY (ticket, artifact)
            )
        ",
    },
    TableDef {
        name: "budgets",
        create_sql: "
            CREATE TABLE IF NOT EXISTS budgets (
                scope TEXT NOT NULL,
                scope_id TEXT,
                limits TEXT NOT NULL,
                spent TEXT NOT NULL,
                PRIMARY KEY (scope, scope_id)
            )
        ",
    },
    TableDef {
        name: "docs",
        create_sql: "
            CREATE TABLE IF NOT EXISTS docs (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                content TEXT NOT NULL,
                author TEXT NOT NULL,
                ts TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "doc_provenance",
        create_sql: "
            CREATE TABLE IF NOT EXISTS doc_provenance (
                doc_id TEXT NOT NULL,
                source TEXT NOT NULL,
                reason TEXT NOT NULL,
                PRIMARY KEY (doc_id, source)
            )
        ",
    },
    TableDef {
        name: "provider_usage",
        create_sql: "
            CREATE TABLE IF NOT EXISTS provider_usage (
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                tokens_used INTEGER NOT NULL,
                dollars_micros INTEGER NOT NULL,
                last_updated TEXT NOT NULL,
                PRIMARY KEY (provider, model)
            )
        ",
    },
    TableDef {
        name: "harness_epochs",
        create_sql: "
            CREATE TABLE IF NOT EXISTS harness_epochs (
                epoch INTEGER PRIMARY KEY,
                harness_config TEXT NOT NULL,
                ts TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "mirror_links",
        create_sql: "
            CREATE TABLE IF NOT EXISTS mirror_links (
                ticket TEXT PRIMARY KEY,
                remote_id TEXT NOT NULL,
                remote_system TEXT NOT NULL,
                last_synced TEXT NOT NULL
            )
        ",
    },
    TableDef {
        name: "meta",
        create_sql: "
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )
        ",
    },
];

/// Index DDL supporting the read patterns `view.rs` and `graph.rs` need: dependency/child lookup
/// by both ends of an edge, lease lookup by ticket and by holder, resource-claim overlap
/// candidates by path prefix, decision lookup by subject and by affected path, evidence lookup
/// by ticket.
pub const INDEXES_SQL: &str = "
CREATE INDEX IF NOT EXISTS ticket_deps_depends_on_idx ON ticket_deps (depends_on);
CREATE INDEX IF NOT EXISTS ticket_children_child_idx ON ticket_children (child);
CREATE INDEX IF NOT EXISTS leases_ticket_idx ON leases (ticket);
CREATE INDEX IF NOT EXISTS leases_holder_idx ON leases (holder);
CREATE INDEX IF NOT EXISTS decisions_subject_idx ON decisions (subject);
CREATE INDEX IF NOT EXISTS evidence_ticket_idx ON evidence (ticket);
CREATE INDEX IF NOT EXISTS sessions_participant_idx ON sessions (participant);
CREATE INDEX IF NOT EXISTS tickets_parent_idx ON tickets (parent);
CREATE INDEX IF NOT EXISTS tickets_milestone_idx ON tickets (milestone);
";

/// Bring `conn`'s materialized-view schema forward to [`SCHEMA_VERSION`]. Shares the
/// `schema_version` table with `tm-events`' own migration bookkeeping is *not* assumed here —
/// this crate tracks its own version in a `tm_core_schema_version` table so the two crates'
/// migration histories never collide inside one SQLite file.
pub fn migrate(conn: &mut Connection, clock: &dyn tm_types::Clock) -> tm_types::Result<()> {
    // Create tm_core schema version table if not present
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS tm_core_schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
        )
    ",
    )
    .map_err(storage_err)?;

    // Get current version: 0 if no migrations applied yet
    let current_version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM tm_core_schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(storage_err)?;

    // Apply migration to version 1 if not yet applied
    if current_version < SCHEMA_VERSION {
        let tx = conn.transaction().map_err(storage_err)?;

        // Create all tables
        for table in TABLES {
            tx.execute_batch(table.create_sql).map_err(storage_err)?;
        }

        // Create all indexes
        tx.execute_batch(INDEXES_SQL).map_err(storage_err)?;

        // Record the migration
        let now = clock.now();
        tx.execute(
            "INSERT INTO tm_core_schema_version (version, applied_at) VALUES (?, ?)",
            rusqlite::params![SCHEMA_VERSION, now.to_rfc3339()],
        )
        .map_err(storage_err)?;

        tx.commit().map_err(storage_err)?;
    }

    Ok(())
}

/// Create every materialized table and index if absent, without touching `tm_core_schema_version`
/// bookkeeping. Used by [`drop_views`]'s caller (`Store::rebuild`) to recreate a clean schema
/// after dropping.
pub fn create_views(conn: &Connection) -> tm_types::Result<()> {
    // Create all tables
    for table in TABLES {
        conn.execute_batch(table.create_sql).map_err(storage_err)?;
    }

    // Create all indexes
    conn.execute_batch(INDEXES_SQL).map_err(storage_err)?;

    Ok(())
}

/// Drop every materialized table (never `events`), for `Store::rebuild` to replay onto a clean
/// schema. Order matters only insofar as SQLite requires it for any `FOREIGN KEY` constraints;
/// since [`TABLES`] deliberately avoids those, drop order is simply the reverse of [`TABLES`].
pub fn drop_views(conn: &Connection) -> tm_types::Result<()> {
    // Drop in reverse order of TABLES
    for table in TABLES.iter().rev() {
        conn.execute(&format!("DROP TABLE IF EXISTS {}", table.name), [])
            .map_err(storage_err)?;
    }

    Ok(())
}

fn storage_err(e: rusqlite::Error) -> TmError {
    TmError::storage(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_db_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("tm_core_schema_test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.db", name));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn test_migrate_creates_schema_version_table() {
        let path = temp_db_path("test_schema_version");
        let mut conn = rusqlite::Connection::open(&path).expect("create db");

        migrate(&mut conn, &tm_types::FixedClock::epoch()).expect("migrate");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='tm_core_schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("query tables");
        assert_eq!(count, 1);
    }

    #[test]
    fn test_migrate_creates_all_tables() {
        let path = temp_db_path("test_all_tables");
        let mut conn = rusqlite::Connection::open(&path).expect("create db");

        migrate(&mut conn, &tm_types::FixedClock::epoch()).expect("migrate");

        for table in TABLES {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    [table.name],
                    |row| row.get(0),
                )
                .expect("query tables");
            assert_eq!(count, 1, "Table {} was not created", table.name);
        }
    }

    #[test]
    fn test_migrate_creates_indexes() {
        let path = temp_db_path("test_indexes");
        let mut conn = rusqlite::Connection::open(&path).expect("create db");

        migrate(&mut conn, &tm_types::FixedClock::epoch()).expect("migrate");

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index'",
                [],
                |row| row.get(0),
            )
            .expect("query indexes");
        assert!(index_count >= 9);
    }

    #[test]
    fn test_migrate_idempotent_on_rerun() {
        let path = temp_db_path("test_idempotent");
        let mut conn = rusqlite::Connection::open(&path).expect("create db");

        migrate(&mut conn, &tm_types::FixedClock::epoch()).expect("first migrate");
        let first_version: i64 = conn
            .query_row(
                "SELECT MAX(version) FROM tm_core_schema_version",
                [],
                |row| row.get(0),
            )
            .expect("query version after first");

        migrate(&mut conn, &tm_types::FixedClock::epoch()).expect("second migrate");
        let second_version: i64 = conn
            .query_row(
                "SELECT MAX(version) FROM tm_core_schema_version",
                [],
                |row| row.get(0),
            )
            .expect("query version after second");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tm_core_schema_version", [], |row| {
                row.get(0)
            })
            .expect("query count");

        assert_eq!(first_version, SCHEMA_VERSION);
        assert_eq!(second_version, SCHEMA_VERSION);
        assert_eq!(count, 1);
    }

    #[test]
    fn test_create_views_creates_all_tables() {
        let path = temp_db_path("test_create_views");
        let conn = rusqlite::Connection::open(&path).expect("create db");

        create_views(&conn).expect("create_views");

        for table in TABLES {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    [table.name],
                    |row| row.get(0),
                )
                .expect("query tables");
            assert_eq!(count, 1, "Table {} was not created", table.name);
        }
    }

    #[test]
    fn test_create_views_is_idempotent() {
        let path = temp_db_path("test_create_views_idempotent");
        let conn = rusqlite::Connection::open(&path).expect("create db");

        create_views(&conn).expect("first create_views");
        create_views(&conn).expect("second create_views");

        for table in TABLES {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    [table.name],
                    |row| row.get(0),
                )
                .expect("query tables");
            assert_eq!(
                count, 1,
                "Table {} count incorrect after idempotent call",
                table.name
            );
        }
    }

    #[test]
    fn test_drop_views_drops_all_tables() {
        let path = temp_db_path("test_drop_views");
        let conn = rusqlite::Connection::open(&path).expect("create db");

        create_views(&conn).expect("create_views");

        for table in TABLES {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    [table.name],
                    |row| row.get(0),
                )
                .expect("query before drop");
            assert_eq!(count, 1, "Table {} not created initially", table.name);
        }

        drop_views(&conn).expect("drop_views");

        for table in TABLES {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    [table.name],
                    |row| row.get(0),
                )
                .expect("query after drop");
            assert_eq!(count, 0, "Table {} was not dropped", table.name);
        }
    }

    #[test]
    fn test_drop_views_is_idempotent() {
        let path = temp_db_path("test_drop_idempotent");
        let conn = rusqlite::Connection::open(&path).expect("create db");

        create_views(&conn).expect("create_views");
        drop_views(&conn).expect("first drop_views");
        drop_views(&conn).expect("second drop_views");

        for table in TABLES {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    [table.name],
                    |row| row.get(0),
                )
                .expect("query after idempotent drops");
            assert_eq!(
                count, 0,
                "Table {} still exists after idempotent drop",
                table.name
            );
        }
    }

    #[test]
    fn test_create_and_drop_are_inverses() {
        let path = temp_db_path("test_create_drop_cycle");
        let conn = rusqlite::Connection::open(&path).expect("create db");

        create_views(&conn).expect("create_views");
        drop_views(&conn).expect("drop_views");
        create_views(&conn).expect("create_views again");

        for table in TABLES {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
                    [table.name],
                    |row| row.get(0),
                )
                .expect("query after cycle");
            assert_eq!(
                count, 1,
                "Table {} missing after create-drop-create cycle",
                table.name
            );
        }
    }

    #[test]
    fn test_tables_in_dependency_order() {
        // Verify that counters comes before tickets (counters are read when restoring state)
        let counters_idx = TABLES.iter().position(|t| t.name == "counters").unwrap();
        let tickets_idx = TABLES.iter().position(|t| t.name == "tickets").unwrap();
        assert!(counters_idx < tickets_idx);

        // Verify that decisions comes before milestones which may reference them
        let decisions_idx = TABLES.iter().position(|t| t.name == "decisions").unwrap();
        let milestones_idx = TABLES.iter().position(|t| t.name == "milestones").unwrap();
        assert!(decisions_idx < milestones_idx);

        // Verify that tickets comes before ticket_deps and ticket_children
        let deps_idx = TABLES.iter().position(|t| t.name == "ticket_deps").unwrap();
        let children_idx = TABLES
            .iter()
            .position(|t| t.name == "ticket_children")
            .unwrap();
        assert!(tickets_idx < deps_idx);
        assert!(tickets_idx < children_idx);
    }
}
