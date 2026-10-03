//! Owns `index.db`: schema, migrations, connection setup (WAL, `busy_timeout`, foreign keys),
//! the single-writer/many-reader concurrency contract, and typed row accessors for every
//! table. No other module opens a `rusqlite::Connection` directly.
//!
//! Concurrency contract: readers open the database with default (deferred) transactions and
//! never block a writer for long; the writer always begins with `BEGIN IMMEDIATE` so a second
//! writer fails fast (`SQLITE_BUSY`) instead of deadlocking against a reader.
//! `busy_timeout` gives readers a bounded wait instead of an immediate error. WAL mode lets
//! readers see either the pre- or post-commit state of a table, never a torn write, which is
//! what "concurrent readers must never see a partial update" requires: every mutation that
//! touches more than one table happens inside a single `BEGIN IMMEDIATE ... COMMIT`.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use tm_types::Result;

use crate::walk::Language;

/// Current schema version. Bump alongside a new migration step in [`Store::open_at`].
pub const SCHEMA_VERSION: i64 = 1;

/// Relative path, from a project root, to the index database under the repo-scoped layout
/// (`.tm/index.db`, the directory [`Store::open`] and [`Store::open_at`]'s shim compute). Not
/// meaningful for a global-scope project, whose index lives at an arbitrary `<index_dir>/index.db`
/// via [`Store::open_at`] directly — kept only as a documented convenience/back-compat constant
/// for the repo-scoped case, not read by [`Store::open_at`] itself.
pub const INDEX_DB_RELATIVE_PATH: &str = ".tm/index.db";

/// A row in the `files` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    /// Row id.
    pub id: i64,
    /// Path relative to the project root.
    pub path: String,
    /// BLAKE3 hash of the file's contents at last index.
    pub blake3: String,
    /// Size in bytes at last index.
    pub size: u64,
    /// Detected language, if any.
    pub lang: Option<Language>,
    /// Filesystem mtime in Unix seconds at last index.
    pub mtime: i64,
}

/// A row in the `chunks` table: a contiguous span of one file's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRow {
    /// Row id, also the id used by `vectors` and `tokens` to reference this chunk.
    pub id: i64,
    /// Owning file's row id.
    pub file_id: i64,
    /// Byte offset range within the file, start inclusive, end exclusive.
    pub byte_start: u64,
    /// End of the byte range, exclusive.
    pub byte_end: u64,
    /// 1-based start line, inclusive.
    pub line_start: u32,
    /// 1-based end line, inclusive.
    pub line_end: u32,
    /// Name of the enclosing symbol, if the chunk was cut at a syntax boundary.
    pub symbol: Option<String>,
    /// The chunk's raw text, stored so semantic search can return it without a re-read.
    pub text: String,
}

/// A row in the `vectors` table: one embedding for one chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorRow {
    /// Owning chunk's row id.
    pub chunk_id: i64,
    /// Embedder identifier (e.g. `"local-hash-512"`) the vector was produced by.
    pub embedder: String,
    /// Raw `f32` components, length equal to the embedder's `dims()`.
    pub vector: Vec<f32>,
}

/// A row in the `tokens` inverted index: one (token, chunk) posting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenPosting {
    /// The token text (already normalized: lowercased, stripped).
    pub token: String,
    /// Chunk this token occurs in.
    pub chunk_id: i64,
    /// Number of occurrences within the chunk, for sublinear-TF scoring at query time.
    pub term_frequency: u32,
}

/// A row in the `symbols` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    /// Row id.
    pub id: i64,
    /// Owning file's row id.
    pub file_id: i64,
    /// Symbol name.
    pub name: String,
    /// Symbol kind as a string tag (mirrors [`crate::symbols::SymbolKind`]).
    pub kind: String,
    /// Byte range within the file.
    pub byte_start: u64,
    /// End of byte range, exclusive.
    pub byte_end: u64,
    /// Enclosing symbol's row id, if nested.
    pub container_id: Option<i64>,
    /// Rendered signature, if applicable (functions, methods).
    pub signature: Option<String>,
    /// Extracted doc comment, if present.
    pub doc: Option<String>,
}

/// A row in the `refs` table: one reference to a symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefRow {
    /// Row id.
    pub id: i64,
    /// File the reference occurs in.
    pub file_id: i64,
    /// Byte range of the reference occurrence.
    pub byte_start: u64,
    /// End of byte range, exclusive.
    pub byte_end: u64,
    /// Textual hint of what's referenced (identifier as written), before resolution.
    pub symbol_hint: String,
    /// Resolved symbol row id, if resolution succeeded unambiguously.
    pub resolved_symbol_id: Option<i64>,
}

/// A row in the `commits` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRow {
    /// Row id.
    pub id: i64,
    /// Full 40-character commit SHA.
    pub sha: String,
    /// Author name as recorded in the commit.
    pub author: String,
    /// Commit timestamp, Unix seconds, as recorded in the commit (not wall clock).
    pub authored_at: i64,
    /// Full commit message.
    pub message: String,
}

/// A row in the `commit_files` table: one path touched by one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFileRow {
    /// Owning commit's row id.
    pub commit_id: i64,
    /// Path touched, relative to the project root.
    pub path: String,
    /// Unified-diff hunk text for this path in this commit.
    pub hunk_text: String,
    /// Lines added.
    pub additions: u32,
    /// Lines removed.
    pub deletions: u32,
}

/// Arbitrary key/value document metadata (schema version, last full-walk timestamp, embedder
/// identifier in use, IDF corpus stats, etc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocMeta {
    /// Metadata key.
    pub key: String,
    /// JSON-encoded value.
    pub value: String,
}

/// Counts of what an incremental update touched, returned to callers (and eventually emitted
/// as an `index.updated` event upstream).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexDelta {
    /// Files newly seen.
    pub files_added: u64,
    /// Files whose content changed.
    pub files_modified: u64,
    /// Files no longer present.
    pub files_removed: u64,
    /// Chunks (re)created.
    pub chunks_written: u64,
    /// Symbols (re)extracted.
    pub symbols_written: u64,
    /// Commits newly ingested.
    pub commits_ingested: u64,
}

/// A handle to `index.db`. Cheap to clone-by-reopen: SQLite connections are not `Send` across
/// the WAL boundary in the way that matters here, so [`Store`] hands out fresh connections
/// from [`Store::reader`] rather than sharing one across threads.
pub struct Store {
    db_path: PathBuf,
}

/// Convert Language enum to its string representation for storage.
pub(crate) fn language_to_str(lang: Option<Language>) -> Option<String> {
    lang.map(|l| match l {
        Language::Rust => "rust".to_string(),
        Language::Go => "go".to_string(),
        Language::TypeScript => "typescript".to_string(),
        Language::Tsx => "tsx".to_string(),
        Language::JavaScript => "javascript".to_string(),
        Language::Python => "python".to_string(),
        Language::Swift => "swift".to_string(),
        Language::Other => "other".to_string(),
    })
}

/// Convert stored string back to Language enum.
fn str_to_language(s: Option<String>) -> Option<Language> {
    s.and_then(|l| match l.as_str() {
        "rust" => Some(Language::Rust),
        "go" => Some(Language::Go),
        "typescript" => Some(Language::TypeScript),
        "tsx" => Some(Language::Tsx),
        "javascript" => Some(Language::JavaScript),
        "python" => Some(Language::Python),
        "swift" => Some(Language::Swift),
        "other" => Some(Language::Other),
        _ => None,
    })
}

/// Apply standard pragmas to a connection for consistent behavior.
fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| tm_types::TmError::storage(format!("Failed to set journal_mode: {}", e)))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| tm_types::TmError::storage(format!("Failed to set synchronous: {}", e)))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| tm_types::TmError::storage(format!("Failed to set foreign_keys: {}", e)))?;
    conn.pragma_update(None, "busy_timeout", Store::BUSY_TIMEOUT_MS.to_string())
        .map_err(|e| tm_types::TmError::storage(format!("Failed to set busy_timeout: {}", e)))?;
    Ok(())
}

impl Store {
    /// Open (creating if absent) the index at `<project_root>/.tm/index.db`, running any
    /// pending migrations. Creates the `.tm` directory if missing.
    ///
    /// Shim over [`Store::open_at`] for the repo-scoped layout (`<project_root>/.tm`); prefer
    /// [`Store::open_at`] when the caller already knows the index directory (e.g. a
    /// global-scope project under `$TM_HOME/projects/<key>/`).
    pub fn open(project_root: &Path) -> Result<Store> {
        Store::open_at(&project_root.join(".tm"))
    }

    /// Open (creating if absent) the index at `<index_dir>/index.db`, running any pending
    /// migrations. Creates `index_dir` if missing.
    ///
    /// Tables created by migration: `files`, `chunks`, `vectors`, `tokens`, `symbols`, `refs`,
    /// `commits`, `commit_files`, `doc_meta`. Foreign keys cascade from `files` to
    /// `chunks`/`symbols`, from `chunks` to `vectors`/`tokens`, from `symbols` to `refs`, from
    /// `commits` to `commit_files`, so deleting a file row (on removal) cleans up its chunks,
    /// vectors, tokens and symbols in one statement.
    pub fn open_at(index_dir: &Path) -> Result<Store> {
        let db_path = index_dir.join("index.db");

        std::fs::create_dir_all(index_dir).map_err(|e| {
            tm_types::TmError::storage(format!("Failed to create index directory: {}", e))
        })?;

        let conn = Connection::open(&db_path)
            .map_err(|e| tm_types::TmError::storage(format!("Failed to open index.db: {}", e)))?;

        apply_pragmas(&conn)?;

        let user_version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|e| {
                tm_types::TmError::storage(format!("Failed to read user_version: {}", e))
            })?;

        if user_version == 0 {
            let schema = r#"
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT UNIQUE NOT NULL,
    blake3 TEXT NOT NULL,
    size INTEGER NOT NULL,
    lang TEXT,
    mtime INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_files_path ON files(path);

CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    byte_start INTEGER NOT NULL,
    byte_end INTEGER NOT NULL,
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    symbol TEXT,
    text TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_chunks_file_id ON chunks(file_id);

CREATE TABLE IF NOT EXISTS vectors (
    chunk_id INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    embedder TEXT NOT NULL,
    vector BLOB NOT NULL,
    PRIMARY KEY (chunk_id, embedder)
);

CREATE TABLE IF NOT EXISTS tokens (
    token TEXT NOT NULL,
    chunk_id INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    term_frequency INTEGER NOT NULL,
    PRIMARY KEY (token, chunk_id)
);

CREATE INDEX IF NOT EXISTS idx_tokens_token ON tokens(token);

CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    byte_start INTEGER NOT NULL,
    byte_end INTEGER NOT NULL,
    container_id INTEGER REFERENCES symbols(id) ON DELETE CASCADE,
    signature TEXT,
    doc TEXT
);

CREATE INDEX IF NOT EXISTS idx_symbols_file_id ON symbols(file_id);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(file_id, name);

CREATE TABLE IF NOT EXISTS refs (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    byte_start INTEGER NOT NULL,
    byte_end INTEGER NOT NULL,
    symbol_hint TEXT NOT NULL,
    resolved_symbol_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_refs_symbol_hint ON refs(symbol_hint);

CREATE TABLE IF NOT EXISTS commits (
    id INTEGER PRIMARY KEY,
    sha TEXT UNIQUE NOT NULL,
    author TEXT NOT NULL,
    authored_at INTEGER NOT NULL,
    message TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_commits_sha ON commits(sha);

CREATE TABLE IF NOT EXISTS commit_files (
    commit_id INTEGER NOT NULL REFERENCES commits(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    hunk_text TEXT NOT NULL,
    additions INTEGER NOT NULL,
    deletions INTEGER NOT NULL,
    PRIMARY KEY (commit_id, path)
);

CREATE INDEX IF NOT EXISTS idx_commit_files_commit_id ON commit_files(commit_id);
CREATE INDEX IF NOT EXISTS idx_commit_files_path ON commit_files(path);

CREATE TABLE IF NOT EXISTS doc_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
            "#;

            conn.execute_batch(schema).map_err(|e| {
                tm_types::TmError::storage(format!("Failed to create schema: {}", e))
            })?;

            conn.pragma_update(None, "user_version", SCHEMA_VERSION.to_string())
                .map_err(|e| {
                    tm_types::TmError::storage(format!("Failed to set user_version: {}", e))
                })?;
        } else if user_version != SCHEMA_VERSION {
            return Err(tm_types::TmError::invariant(format!(
                "index.db schema version {} is newer than this build supports (current: {})",
                user_version, SCHEMA_VERSION
            )));
        }

        drop(conn);

        Ok(Store { db_path })
    }

    /// Path to the underlying database file.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Open a fresh reader connection. Many of these may be live concurrently, including
    /// while a writer transaction is in flight; WAL guarantees each sees a consistent
    /// snapshot.
    pub fn reader(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path).map_err(|e| {
            tm_types::TmError::storage(format!("Failed to open reader connection: {}", e))
        })?;
        apply_pragmas(&conn)?;
        Ok(conn)
    }

    /// Begin a write transaction with `BEGIN IMMEDIATE`, ensuring only one writer proceeds at
    /// a time and other writers fail fast with `SQLITE_BUSY` (bounded by `busy_timeout`)
    /// rather than deadlocking against a reader.
    ///
    /// Callers must commit or roll back the returned connection's transaction themselves
    /// (rusqlite's `Connection` does not expose a standalone `Transaction` type across this
    /// boundary cleanly without lifetimes tied to the connection); the convention in this
    /// crate is `let conn = store.writer()?; conn.execute_batch("BEGIN IMMEDIATE")?; ... ;
    /// conn.execute_batch("COMMIT")?;` wrapped so a returned `Err` rolls back.
    pub fn writer(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path).map_err(|e| {
            tm_types::TmError::storage(format!("Failed to open writer connection: {}", e))
        })?;
        apply_pragmas(&conn)?;
        conn.execute_batch("BEGIN IMMEDIATE").map_err(|e| {
            tm_types::TmError::storage(format!("Failed to begin transaction: {}", e))
        })?;
        Ok(conn)
    }

    /// Fetch the stored record for every file, ordered by path. Used to diff against a fresh
    /// [`crate::walk::RepoWalker::walk`] result.
    pub fn list_files(&self) -> Result<Vec<FileRow>> {
        let conn = self.reader()?;
        let mut stmt = conn
            .prepare("SELECT id, path, blake3, size, lang, mtime FROM files ORDER BY path")
            .map_err(|e| tm_types::TmError::storage(format!("Failed to prepare query: {}", e)))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(FileRow {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    blake3: row.get(2)?,
                    size: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                    lang: str_to_language(row.get::<_, Option<String>>(4)?),
                    mtime: row.get(5)?,
                })
            })
            .map_err(|e| tm_types::TmError::storage(format!("Failed to query files: {}", e)))?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(|e| {
                tm_types::TmError::storage(format!("Failed to read file row: {}", e))
            })?);
        }
        Ok(result)
    }

    /// Look up a single file row by path.
    pub fn file_by_path(&self, path: &str) -> Result<Option<FileRow>> {
        let conn = self.reader()?;
        let mut stmt = conn
            .prepare("SELECT id, path, blake3, size, lang, mtime FROM files WHERE path = ?1")
            .map_err(|e| tm_types::TmError::storage(format!("Failed to prepare query: {}", e)))?;

        let result = stmt
            .query_row(params![path], |row| {
                Ok(FileRow {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    blake3: row.get(2)?,
                    size: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                    lang: str_to_language(row.get::<_, Option<String>>(4)?),
                    mtime: row.get(5)?,
                })
            })
            .optional()
            .map_err(|e| {
                tm_types::TmError::storage(format!("Failed to query file by path: {}", e))
            })?;

        Ok(result)
    }

    /// Read a document-metadata value by key.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.reader()?;
        let mut stmt = conn
            .prepare("SELECT value FROM doc_meta WHERE key = ?1")
            .map_err(|e| tm_types::TmError::storage(format!("Failed to prepare query: {}", e)))?;

        let result = stmt
            .query_row(params![key], |row| row.get::<_, String>(0))
            .optional()
            .map_err(|e| tm_types::TmError::storage(format!("Failed to query metadata: {}", e)))?;

        Ok(result)
    }

    /// Write a document-metadata value, replacing any existing value for that key. Must be
    /// called within a writer transaction (`conn` from [`Store::writer`]).
    pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO doc_meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )
        .map_err(|e| tm_types::TmError::storage(format!("Failed to set metadata: {}", e)))?;
        Ok(())
    }

    /// Milliseconds SQLite will wait for a lock before returning `SQLITE_BUSY`.
    pub const BUSY_TIMEOUT_MS: u32 = 5_000;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_store() -> (TempDir, Store) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let store = Store::open(temp_dir.path()).expect("Failed to open test store");
        (temp_dir, store)
    }

    #[test]
    fn test_store_open_creates_db() {
        let (_temp, store) = create_test_store();
        assert!(
            store.db_path().exists(),
            "Database file should exist after open"
        );
    }

    #[test]
    fn test_store_open_creates_tm_directory() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let tm_dir = temp_dir.path().join(".tm");
        assert!(
            !tm_dir.exists(),
            ".tm directory should not exist before open"
        );

        let store = Store::open(temp_dir.path()).expect("Failed to open test store");
        assert!(tm_dir.exists(), ".tm directory should be created");
        assert!(
            store.db_path().exists(),
            "Database file should exist in .tm directory"
        );
    }

    #[test]
    fn test_store_open_idempotent() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let store1 = Store::open(temp_dir.path()).expect("First open failed");
        let store2 = Store::open(temp_dir.path()).expect("Second open failed");

        assert_eq!(store1.db_path(), store2.db_path());
        assert!(store1.db_path().exists());
    }

    #[test]
    fn test_reader_connection() {
        let (_temp, store) = create_test_store();
        let conn = store.reader().expect("Failed to get reader connection");
        let _: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .expect("Failed to query files table");
    }

    #[test]
    fn test_writer_connection() {
        let (_temp, store) = create_test_store();
        let conn = store.writer().expect("Failed to get writer connection");
        conn.execute(
            "INSERT INTO doc_meta(key, value) VALUES (?1, ?2)",
            params!["test_key", "test_value"],
        )
        .expect("Failed to insert metadata");
        conn.execute_batch("COMMIT").expect("Failed to commit");
    }

    #[test]
    fn test_list_files_empty() {
        let (_temp, store) = create_test_store();
        let files = store.list_files().expect("Failed to list files");
        assert_eq!(files.len(), 0);
    }

    #[test]
    fn test_file_by_path_not_found() {
        let (_temp, store) = create_test_store();
        let result = store
            .file_by_path("nonexistent.rs")
            .expect("Failed to query by path");
        assert_eq!(result, None);
    }

    #[test]
    fn test_get_meta_not_found() {
        let (_temp, store) = create_test_store();
        let result = store
            .get_meta("missing_key")
            .expect("Failed to get metadata");
        assert_eq!(result, None);
    }

    #[test]
    fn test_set_and_get_meta() {
        let (_temp, store) = create_test_store();
        let conn = store.writer().expect("Failed to get writer connection");

        Store::set_meta(&conn, "test_key", "test_value").expect("Failed to set metadata");
        conn.execute_batch("COMMIT").expect("Failed to commit");

        let result = store.get_meta("test_key").expect("Failed to get metadata");
        assert_eq!(result, Some("test_value".to_string()));
    }

    #[test]
    fn test_set_meta_overwrites_existing() {
        let (_temp, store) = create_test_store();

        let conn = store.writer().expect("Failed to get writer connection");
        Store::set_meta(&conn, "key", "value1").expect("Failed to set metadata");
        conn.execute_batch("COMMIT").expect("Failed to commit");

        let conn = store.writer().expect("Failed to get writer connection");
        Store::set_meta(&conn, "key", "value2").expect("Failed to set metadata");
        conn.execute_batch("COMMIT").expect("Failed to commit");

        let result = store.get_meta("key").expect("Failed to get metadata");
        assert_eq!(result, Some("value2".to_string()));
    }

    #[test]
    fn test_set_multiple_metadata_entries() {
        let (_temp, store) = create_test_store();

        let conn = store.writer().expect("Failed to get writer connection");
        Store::set_meta(&conn, "key1", "value1").expect("Failed to set metadata");
        Store::set_meta(&conn, "key2", "value2").expect("Failed to set metadata");
        Store::set_meta(&conn, "key3", "value3").expect("Failed to set metadata");
        conn.execute_batch("COMMIT").expect("Failed to commit");

        assert_eq!(
            store.get_meta("key1").expect("Failed to get key1"),
            Some("value1".to_string())
        );
        assert_eq!(
            store.get_meta("key2").expect("Failed to get key2"),
            Some("value2".to_string())
        );
        assert_eq!(
            store.get_meta("key3").expect("Failed to get key3"),
            Some("value3".to_string())
        );
    }

    #[test]
    fn test_language_rust_roundtrip() {
        assert_eq!(
            str_to_language(language_to_str(Some(Language::Rust))),
            Some(Language::Rust)
        );
    }

    #[test]
    fn test_language_go_roundtrip() {
        assert_eq!(
            str_to_language(language_to_str(Some(Language::Go))),
            Some(Language::Go)
        );
    }

    #[test]
    fn test_language_typescript_roundtrip() {
        assert_eq!(
            str_to_language(language_to_str(Some(Language::TypeScript))),
            Some(Language::TypeScript)
        );
    }

    #[test]
    fn test_language_tsx_roundtrip() {
        assert_eq!(
            str_to_language(language_to_str(Some(Language::Tsx))),
            Some(Language::Tsx)
        );
    }

    #[test]
    fn test_language_javascript_roundtrip() {
        assert_eq!(
            str_to_language(language_to_str(Some(Language::JavaScript))),
            Some(Language::JavaScript)
        );
    }

    #[test]
    fn test_language_python_roundtrip() {
        assert_eq!(
            str_to_language(language_to_str(Some(Language::Python))),
            Some(Language::Python)
        );
    }

    #[test]
    fn test_language_swift_roundtrip() {
        assert_eq!(
            str_to_language(language_to_str(Some(Language::Swift))),
            Some(Language::Swift)
        );
    }

    #[test]
    fn test_language_other_roundtrip() {
        assert_eq!(
            str_to_language(language_to_str(Some(Language::Other))),
            Some(Language::Other)
        );
    }

    #[test]
    fn test_language_none_roundtrip() {
        assert_eq!(str_to_language(language_to_str(None)), None);
    }

    #[test]
    fn test_busy_timeout_constant() {
        assert_eq!(Store::BUSY_TIMEOUT_MS, 5_000);
    }
}
