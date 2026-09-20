//! SQLite DDL, pragmas, anti-mutation triggers, and connection-opening helpers.
//!
//! Owns everything that turns an empty SQLite file into a valid `events` database and keeps it
//! that way: the table itself, the `BEFORE UPDATE`/`BEFORE DELETE` triggers that make
//! append-only a database-level guarantee rather than an application convention, the
//! `schema_version` table, and the forward-only migration list that brings an existing database
//! up to [`SCHEMA_VERSION`]. `log.rs` calls into this module to open connections; it never
//! touches DDL directly.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use tm_types::Clock;

/// The schema version this build of `tm-events` expects. Bump when adding a migration.
pub const SCHEMA_VERSION: i64 = 1;

/// `CREATE TABLE events (...)`, exactly matching `SPEC.md` §3.1.
pub const EVENTS_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS events (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    subject     TEXT    NOT NULL,
    actor       TEXT    NOT NULL,
    session     TEXT,
    causation   INTEGER,
    correlation TEXT,
    payload     TEXT    NOT NULL,
    hash        TEXT    NOT NULL
);
";

/// Indexes supporting `EventLog::read_subject` and correlation lookups.
pub const EVENTS_INDEXES_SQL: &str = "
CREATE INDEX IF NOT EXISTS events_subject_idx ON events (subject);
CREATE INDEX IF NOT EXISTS events_correlation_idx ON events (correlation);
CREATE INDEX IF NOT EXISTS events_causation_idx ON events (causation);
";

/// Raises on `UPDATE events`. Belt-and-suspenders alongside the source-grep test in
/// `SPEC.md` §3.1: the trigger holds even against a hand-crafted `sqlite3` session.
pub const EVENTS_NO_UPDATE_TRIGGER_SQL: &str = "
CREATE TRIGGER IF NOT EXISTS events_no_update
BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'events is append-only: UPDATE is forbidden');
END;
";

/// Raises on `DELETE FROM events`.
pub const EVENTS_NO_DELETE_TRIGGER_SQL: &str = "
CREATE TRIGGER IF NOT EXISTS events_no_delete
BEFORE DELETE ON events
BEGIN
    SELECT RAISE(ABORT, 'events is append-only: DELETE is forbidden');
END;
";

/// Tracks which schema migrations have been applied, in application order.
pub const SCHEMA_VERSION_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS schema_version (
    version     INTEGER NOT NULL PRIMARY KEY,
    applied_at  TEXT    NOT NULL
);
";

/// One forward migration: the version it brings the database to, and the DDL that does it.
/// Migrations never rewrite or delete prior migrations' effects; a new schema need is always a
/// new entry appended to [`MIGRATIONS`], never an edit to an existing one.
pub struct Migration {
    /// The `schema_version.version` this migration results in.
    pub version: i64,
    /// The DDL statements applied to reach `version`, run inside one transaction.
    pub ddl: &'static str,
}

/// Every migration, in order, from an empty database to [`SCHEMA_VERSION`].
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    ddl: "", // Composed from EVENTS_TABLE_SQL + EVENTS_INDEXES_SQL + the two trigger SQL
             // constants + SCHEMA_VERSION_TABLE_SQL by the migrate function; kept empty here
             // so each DDL fragment stays independently documented above and reusable.
}];

/// Convert a rusqlite error into `TmError::storage`, since `tm_types::TmError` cannot implement
/// `From<rusqlite::Error>` directly (orphan rules: neither type lives in this crate).
fn storage_err(e: rusqlite::Error) -> tm_types::TmError {
    tm_types::TmError::storage(e.to_string())
}

/// Set `journal_mode = WAL`, `foreign_keys = ON`, and a nonzero `busy_timeout` so concurrent
/// writers block-and-retry instead of failing with `SQLITE_BUSY`.
pub fn apply_pragmas(conn: &Connection) -> tm_types::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(storage_err)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(storage_err)?;
    conn.pragma_update(None, "busy_timeout", "5000")
        .map_err(storage_err)?;
    Ok(())
}

/// Bring `conn`'s schema forward to [`SCHEMA_VERSION`], applying any [`MIGRATIONS`] entries not
/// yet recorded in `schema_version`. Idempotent: calling this on an up-to-date database is a
/// no-op after one read of `schema_version`.
pub fn migrate(conn: &mut Connection, clock: &dyn Clock) -> tm_types::Result<()> {
    // Create schema_version table first (safe via IF NOT EXISTS)
    conn.execute_batch(SCHEMA_VERSION_TABLE_SQL)
        .map_err(storage_err)?;

    // Get current version: 0 if no migrations applied yet
    let mut current_version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(storage_err)?;

    // Apply each unapplied migration
    for migration in MIGRATIONS {
        if migration.version > current_version {
            let tx = conn.transaction().map_err(storage_err)?;

            // Compose migration 1's DDL from the documented fragments
            let ddl = if migration.version == 1 {
                format!(
                    "{}\n{}\n{}\n{}\n{}",
                    EVENTS_TABLE_SQL,
                    EVENTS_INDEXES_SQL,
                    EVENTS_NO_UPDATE_TRIGGER_SQL,
                    EVENTS_NO_DELETE_TRIGGER_SQL,
                    SCHEMA_VERSION_TABLE_SQL
                )
            } else {
                migration.ddl.to_string()
            };

            tx.execute_batch(&ddl).map_err(storage_err)?;

            let timestamp = clock.now();
            tx.execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (?, ?)",
                rusqlite::params![migration.version, timestamp.to_rfc3339()],
            )
            .map_err(storage_err)?;

            tx.commit().map_err(storage_err)?;
            current_version = migration.version;
        }
    }

    Ok(())
}

/// Open the single serialized write connection for `path`, applying pragmas and running any
/// pending migrations. Callers keep exactly one of these per project (`EventLog` owns it,
/// guarded by a mutex — see `log.rs`).
pub fn open_write_connection(path: &Path, clock: &dyn Clock) -> tm_types::Result<Connection> {
    let conn = Connection::open(path).map_err(storage_err)?;
    apply_pragmas(&conn)?;
    let mut conn = conn;
    migrate(&mut conn, clock)?;
    Ok(conn)
}

/// Open an additional read-only connection for `path`. Multiple of these may exist
/// concurrently alongside the one write connection (WAL mode); they never migrate the schema.
pub fn open_read_connection(path: &Path) -> tm_types::Result<Connection> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(storage_err)?;
    apply_pragmas(&conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_db_path(name: &str) -> std::path::PathBuf {
        // Suffixed with the test process's own pid so two concurrent `cargo test` invocations
        // (e.g. two parallel worktree builds on a shared machine) never collide on the same file
        // -- matching the convention every other temp-fixture in this workspace already follows
        // (`xtask::hygiene`'s test fixtures, `tm-computer`'s, `tm-browser`'s).
        let dir = std::env::temp_dir().join(format!("tm_events_test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.db", name));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn test_apply_pragmas_sets_wal_mode() {
        let path = temp_db_path("test_wal_mode");
        let conn = Connection::open(&path).expect("create db");

        apply_pragmas(&conn).expect("apply pragmas");

        let wal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("query journal_mode");
        assert_eq!(wal_mode.to_lowercase(), "wal");
    }

    #[test]
    fn test_apply_pragmas_enables_foreign_keys() {
        let path = temp_db_path("test_foreign_keys");
        let conn = Connection::open(&path).expect("create db");

        apply_pragmas(&conn).expect("apply pragmas");

        let fk_enabled: u8 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("query foreign_keys");
        assert_eq!(fk_enabled, 1);
    }

    #[test]
    fn test_apply_pragmas_sets_busy_timeout() {
        let path = temp_db_path("test_busy_timeout");
        let conn = Connection::open(&path).expect("create db");

        apply_pragmas(&conn).expect("apply pragmas");

        let timeout: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("query busy_timeout");
        assert_eq!(timeout, 5000);
    }

    #[test]
    fn test_migrate_creates_tables() {
        let path = temp_db_path("test_migrate_creates");
        let clock = tm_types::FixedClock::epoch();
        let mut conn = Connection::open(&path).expect("create db");

        migrate(&mut conn, &clock).expect("migrate");

        let events_exist: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='events'",
                [],
                |row| row.get(0),
            )
            .expect("query tables");
        assert_eq!(events_exist, 1);

        let schema_exist: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("query tables");
        assert_eq!(schema_exist, 1);
    }

    #[test]
    fn test_migrate_creates_indexes() {
        let path = temp_db_path("test_migrate_indexes");
        let clock = tm_types::FixedClock::epoch();
        let mut conn = Connection::open(&path).expect("create db");

        migrate(&mut conn, &clock).expect("migrate");

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name LIKE 'events_%'",
                [],
                |row| row.get(0),
            )
            .expect("query indexes");
        assert!(index_count >= 3); // subject, correlation, causation
    }

    #[test]
    fn test_migrate_idempotent_on_rerun() {
        let path = temp_db_path("test_migrate_idempotent");
        let clock = tm_types::FixedClock::epoch();
        let mut conn = Connection::open(&path).expect("create db");

        migrate(&mut conn, &clock).expect("first migrate");
        let first_version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .expect("query version after first");

        migrate(&mut conn, &clock).expect("second migrate");
        let second_version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .expect("query version after second");

        assert_eq!(first_version, 1);
        assert_eq!(second_version, 1);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .expect("query count");
        assert_eq!(count, 1); // Only one version entry after idempotent run
    }

    #[test]
    fn test_migrate_records_applied_timestamp() {
        let path = temp_db_path("test_migrate_timestamp");
        let clock = tm_types::FixedClock::epoch();
        let mut conn = Connection::open(&path).expect("create db");

        migrate(&mut conn, &clock).expect("migrate");

        let applied_at: String = conn
            .query_row(
                "SELECT applied_at FROM schema_version WHERE version=1",
                [],
                |row| row.get(0),
            )
            .expect("query timestamp");
        assert!(!applied_at.is_empty());
    }

    #[test]
    fn test_events_no_update_trigger() {
        let path = temp_db_path("test_no_update");
        let clock = tm_types::FixedClock::epoch();
        let mut conn = Connection::open(&path).expect("create db");
        migrate(&mut conn, &clock).expect("migrate");

        conn.execute(
            "INSERT INTO events (ts, kind, subject, actor, payload, hash) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["2025-01-01T00:00:00Z", "created", "T-1", "system", "{}", "hash1"],
        ).expect("insert event");

        let update_result = conn.execute("UPDATE events SET kind='modified' WHERE seq=1", []);
        assert!(
            update_result.is_err(),
            "UPDATE should be forbidden by trigger"
        );
    }

    #[test]
    fn test_events_no_delete_trigger() {
        let path = temp_db_path("test_no_delete");
        let clock = tm_types::FixedClock::epoch();
        let mut conn = Connection::open(&path).expect("create db");
        migrate(&mut conn, &clock).expect("migrate");

        conn.execute(
            "INSERT INTO events (ts, kind, subject, actor, payload, hash) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["2025-01-01T00:00:00Z", "created", "T-1", "system", "{}", "hash1"],
        ).expect("insert event");

        let delete_result = conn.execute("DELETE FROM events WHERE seq=1", []);
        assert!(
            delete_result.is_err(),
            "DELETE should be forbidden by trigger"
        );
    }

    #[test]
    fn test_open_write_connection_creates_schema() {
        let path = temp_db_path("test_open_write");
        let clock = tm_types::FixedClock::epoch();

        let conn = open_write_connection(&path, &clock).expect("open write connection");

        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
                [],
                |row| row.get(0),
            )
            .expect("query tables");
        assert!(table_count > 0);
    }

    #[test]
    fn test_open_write_connection_multiple_calls_idempotent() {
        let path = temp_db_path("test_open_write_multi");
        let clock = tm_types::FixedClock::epoch();

        let _conn1 = open_write_connection(&path, &clock).expect("first open");
        let conn2 = open_write_connection(&path, &clock).expect("second open");

        let version_count: i64 = conn2
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .expect("query version count");
        assert_eq!(
            version_count, 1,
            "Should only have one version entry after idempotent opens"
        );
    }

    #[test]
    fn test_open_read_connection_requires_existing_file() {
        let path = temp_db_path("test_open_read_missing");
        let result = open_read_connection(&path);
        assert!(
            result.is_err(),
            "Read connection should fail on missing file"
        );
    }

    #[test]
    fn test_open_read_connection_on_initialized_db() {
        let path = temp_db_path("test_open_read_exists");
        let clock = tm_types::FixedClock::epoch();

        let _write_conn = open_write_connection(&path, &clock).expect("create db");

        let read_conn = open_read_connection(&path).expect("open read connection");

        let version: i64 = read_conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .expect("query version");
        assert_eq!(version, 1);
    }

    #[test]
    fn test_open_read_connection_applies_pragmas() {
        let path = temp_db_path("test_read_pragmas");
        let clock = tm_types::FixedClock::epoch();

        let _write_conn = open_write_connection(&path, &clock).expect("create db");
        let read_conn = open_read_connection(&path).expect("open read connection");

        let fk_enabled: u8 = read_conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("query foreign_keys");
        assert_eq!(fk_enabled, 1);
    }

    #[test]
    fn test_events_table_append_only() {
        let path = temp_db_path("test_append_only");
        let clock = tm_types::FixedClock::epoch();
        let mut conn = Connection::open(&path).expect("create db");
        migrate(&mut conn, &clock).expect("migrate");

        conn.execute(
            "INSERT INTO events (ts, kind, subject, actor, payload, hash) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["2025-01-01T00:00:00Z", "created", "T-1", "system", "{}", "hash1"],
        ).expect("insert first event");

        conn.execute(
            "INSERT INTO events (ts, kind, subject, actor, payload, hash) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params!["2025-01-02T00:00:00Z", "created", "T-2", "system", "{}", "hash2"],
        ).expect("insert second event");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("count events");
        assert_eq!(count, 2);
    }
}
