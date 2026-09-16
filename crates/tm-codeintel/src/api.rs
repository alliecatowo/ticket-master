//! Owns the [`CodeIntel`] facade: the single entry point the rest of Ticketmaster uses for
//! code intelligence. Opens `index.db`, owns the writer lock (serializing
//! [`CodeIntel::update_incremental`] calls against each other) and the shared read pool, and
//! exposes the search/symbol/history entry points other crates call without touching
//! [`crate::store`], [`crate::embed`] or `git2` directly.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::{params, Connection};
use tm_types::{Clock, Result};

use crate::chunk::Chunker;
use crate::embed::{Embedder, LocalHashEmbedder};
use crate::exact::{ExactSearch, ExactSearchResult};
use crate::history::HistoryIndex;
use crate::hybrid::{
    hybrid, lexical_ranking, semantic_ranking, Query, RankedHit, RetrievalContext, Signal,
    SignalRanking, SignalWeights,
};
use crate::semantic::{ScoredChunk, SemanticSearch, SemanticSearchOptions};
use crate::store::{IndexDelta, Store};
use crate::symbols::{Range, Reference, Resolution, Symbol, SymbolIndex};
use crate::walk::{ChangeSet, FileRecord, Language, RepoWalker, MAX_INDEXABLE_BYTES};

/// The code-intelligence facade for one project.
///
/// Holds the [`Store`] (which itself opens fresh connections per call rather than holding one
/// open), the configured [`Embedder`], and a [`Mutex`] serializing writers so
/// `update_incremental` calls from multiple threads queue rather than race for the
/// `BEGIN IMMEDIATE` lock at the SQLite level (SQLite would already serialize them, but
/// queuing in-process avoids every-but-one thread burning a retry loop against
/// `SQLITE_BUSY`).
pub struct CodeIntel {
    project_root: PathBuf,
    store: Arc<Store>,
    embedder: Arc<dyn Embedder>,
    write_lock: Mutex<()>,
}

/// Tokenize `text` into normalized (lowercased, alphanumeric-run) tokens with per-token
/// occurrence counts, for populating the `tokens` inverted index alongside a chunk.
fn tokenize_for_index(text: &str) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for token in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        *counts.entry(token.to_string()).or_insert(0u32) += 1;
    }
    counts
}

/// Read and chunk one file's current on-disk content, embed each chunk, and write its
/// file/chunk/vector/token rows inside `conn`'s already-open writer transaction. Returns the
/// number of chunks written (zero if the file could not be read as text or was over the
/// indexable size ceiling).
///
/// Syntax-aware chunk boundaries are not sourced from a symbol extraction pass here:
/// [`crate::symbols::SymbolIndex::outline`] (the only public, by-path symbol accessor)
/// reports rendered lines and depth but not byte ranges, so it cannot be converted into
/// [`crate::chunk::SyntaxBoundary`]s without widening that module's public surface. Chunking
/// therefore always falls back to the sliding window; this is a documented tradeoff, not an
/// oversight.
fn write_file_content(
    conn: &Connection,
    project_root: &Path,
    embedder: &dyn Embedder,
    record: &FileRecord,
    file_id: i64,
) -> Result<u64> {
    if record.size > MAX_INDEXABLE_BYTES {
        return Ok(0);
    }

    let full_path = project_root.join(&record.path);
    let text = match fs::read_to_string(&full_path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("skipping content of {}: failed to read: {}", record.path, e);
            return Ok(0);
        }
    };

    let lang = record.lang.unwrap_or(Language::Other);
    let chunks = Chunker::new().chunk(&record.path, &text, lang, &[]);
    if chunks.is_empty() {
        return Ok(0);
    }

    let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
    let vectors = embedder.embed(&texts)?;

    for (chunk, vector) in chunks.iter().zip(vectors.iter()) {
        conn.execute(
            "INSERT INTO chunks(file_id, byte_start, byte_end, line_start, line_end, symbol, text) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                file_id,
                chunk.range.byte_start as i64,
                chunk.range.byte_end as i64,
                chunk.range.line_start,
                chunk.range.line_end,
                chunk.symbol,
                chunk.text,
            ],
        )
        .map_err(|e| tm_types::TmError::storage(format!("Failed to insert chunk: {e}")))?;
        let chunk_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO vectors(chunk_id, embedder, vector) VALUES (?1, ?2, ?3)",
            params![
                chunk_id,
                embedder.identifier(),
                crate::semantic::vector_to_bytes(vector)
            ],
        )
        .map_err(|e| tm_types::TmError::storage(format!("Failed to insert vector: {e}")))?;

        for (token, term_frequency) in tokenize_for_index(&chunk.text) {
            conn.execute(
                "INSERT INTO tokens(token, chunk_id, term_frequency) VALUES (?1, ?2, ?3)",
                params![token, chunk_id, term_frequency],
            )
            .map_err(|e| tm_types::TmError::storage(format!("Failed to insert token: {e}")))?;
        }
    }

    Ok(chunks.len() as u64)
}

impl CodeIntel {
    /// Open (creating if absent) the code-intelligence index for the project at
    /// `project_root`, using the default no-network [`LocalHashEmbedder`].
    pub fn open(project_root: &Path) -> Result<CodeIntel> {
        Self::open_with_embedder(project_root, Arc::new(LocalHashEmbedder::new()))
    }

    /// Open with an explicit embedder (e.g. an API-backed one supplied by a higher layer),
    /// otherwise identical to [`CodeIntel::open`].
    pub fn open_with_embedder(
        project_root: &Path,
        embedder: Arc<dyn Embedder>,
    ) -> Result<CodeIntel> {
        let store = Arc::new(Store::open(project_root)?);
        Ok(CodeIntel {
            project_root: project_root.to_path_buf(),
            store,
            embedder,
            write_lock: Mutex::new(()),
        })
    }

    /// The project root this instance indexes.
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Walk the project, diff against stored file records, and for every added or modified
    /// file: re-chunk, re-embed, and re-extract symbols (invalidating and replacing that
    /// file's prior chunks/vectors/tokens/symbols/refs); for every removed file, delete its
    /// rows (cascading via foreign keys). Also runs an incremental git history ingest. Files
    /// whose blake3 is unchanged are touched nowhere. Serialized against concurrent callers
    /// by `write_lock`; the actual SQLite mutation is further serialized by `BEGIN IMMEDIATE`.
    ///
    /// Symbol/reference persistence: [`crate::symbols::SymbolIndex`]'s public API does not
    /// expose a by-path accessor returning full [`Symbol`] rows (byte ranges, kind, container,
    /// doc) -- only name-keyed lookups and a rendered [`crate::symbols::OutlineEntry`] list --
    /// so the `symbols`/`refs` tables are not written here. [`CodeIntel::symbol_index`] parses
    /// fresh from disk on demand instead, which keeps symbol/reference queries correct without
    /// requiring `index.db` to hold data this module cannot populate soundly. `symbols_written`
    /// in the returned [`IndexDelta`] is therefore always zero.
    pub fn update_incremental(&self, clock: &dyn Clock) -> Result<IndexDelta> {
        let _guard = self.write_lock.lock();

        let current = RepoWalker::new(&self.project_root).walk()?;
        let previous = self.store.list_files()?;
        let previous_records: Vec<FileRecord> = previous
            .iter()
            .map(|f| FileRecord {
                path: f.path.clone(),
                blake3: f.blake3.clone(),
                size: f.size,
                lang: f.lang,
                mtime: f.mtime,
            })
            .collect();
        let changes: ChangeSet = RepoWalker::diff(&current, &previous_records);

        let mut delta = IndexDelta::default();

        if !changes.is_empty() {
            let conn = self.store.writer()?;

            for record in &changes.added {
                conn.execute("DELETE FROM files WHERE path = ?1", params![record.path])
                    .map_err(|e| {
                        tm_types::TmError::storage(format!("Failed to clear stale file row: {e}"))
                    })?;
                let lang_str = crate::store::language_to_str(record.lang);
                conn.execute(
                    "INSERT INTO files(path, blake3, size, lang, mtime) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![record.path, record.blake3, record.size as i64, lang_str, record.mtime],
                )
                .map_err(|e| tm_types::TmError::storage(format!("Failed to insert file: {e}")))?;
                let file_id = conn.last_insert_rowid();
                delta.chunks_written += write_file_content(
                    &conn,
                    &self.project_root,
                    self.embedder.as_ref(),
                    record,
                    file_id,
                )?;
                delta.files_added += 1;
            }

            for record in &changes.modified {
                conn.execute("DELETE FROM files WHERE path = ?1", params![record.path])
                    .map_err(|e| {
                        tm_types::TmError::storage(format!("Failed to clear stale file row: {e}"))
                    })?;
                let lang_str = crate::store::language_to_str(record.lang);
                conn.execute(
                    "INSERT INTO files(path, blake3, size, lang, mtime) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![record.path, record.blake3, record.size as i64, lang_str, record.mtime],
                )
                .map_err(|e| tm_types::TmError::storage(format!("Failed to insert file: {e}")))?;
                let file_id = conn.last_insert_rowid();
                delta.chunks_written += write_file_content(
                    &conn,
                    &self.project_root,
                    self.embedder.as_ref(),
                    record,
                    file_id,
                )?;
                delta.files_modified += 1;
            }

            for path in &changes.removed {
                conn.execute("DELETE FROM files WHERE path = ?1", params![path])
                    .map_err(|e| {
                        tm_types::TmError::storage(format!("Failed to delete removed file: {e}"))
                    })?;
                delta.files_removed += 1;
            }

            conn.execute_batch("COMMIT").map_err(|e| {
                tm_types::TmError::storage(format!("Failed to commit index update: {e}"))
            })?;
        }

        delta.commits_ingested = HistoryIndex::new(Arc::clone(&self.store), &self.project_root)
            .ingest_incremental(clock)?;

        Ok(delta)
    }

    /// Literal substring search. Thin pass-through to [`ExactSearch`], always current since
    /// it walks the tree directly rather than reading from the index.
    pub fn search_exact(&self, needle: &str) -> Result<ExactSearchResult> {
        ExactSearch::new(&self.project_root).literal(needle)
    }

    /// Regex search. Thin pass-through to [`ExactSearch`].
    pub fn search_regex(&self, pattern: &str) -> Result<ExactSearchResult> {
        ExactSearch::new(&self.project_root).regex(pattern)
    }

    /// Semantic vector search over the current index. Thin pass-through to
    /// [`SemanticSearch`]; reflects whatever [`CodeIntel::update_incremental`] last wrote.
    pub fn search_semantic(
        &self,
        query_text: &str,
        options: SemanticSearchOptions,
    ) -> Result<Vec<ScoredChunk>> {
        SemanticSearch::new(Arc::clone(&self.store), Arc::clone(&self.embedder))
            .search(query_text, options)
    }

    /// Fused hybrid search across all signals. Gathers per-signal rankings (semantic, lexical,
    /// symbol-proximity, path-affinity, edit-recency, co-change) and calls
    /// [`crate::hybrid::hybrid`].
    ///
    /// Symbol-proximity is built from `query.seed_symbols` via a freshly parsed
    /// [`CodeIntel::symbol_index`] (reparsed on demand rather than cached, for the same
    /// staleness reason [`CodeIntel::symbol_index`] documents): each seed's callers and
    /// callees contribute their defining locations. Path-affinity ranks every currently
    /// indexed file by prefix match against `query.seed_paths` and `ctx.claimed_paths`.
    /// Edit-recency reuses `ctx.recently_edited` verbatim (already most-recent-first).
    /// Co-change comes from [`HistoryIndex::co_change`] over `query.seed_paths`, merged and
    /// sorted by co-commit count.
    pub fn search_hybrid(
        &self,
        query: &Query,
        ctx: &RetrievalContext,
        weights: SignalWeights,
    ) -> Result<Vec<RankedHit>> {
        let semantic_hits = self.search_semantic(&query.text, SemanticSearchOptions::default())?;
        let semantic_sig = semantic_ranking(&semantic_hits);

        let exact_result = self.search_exact(&query.text)?;
        let lexical_sig = lexical_ranking(&exact_result.hits);

        let symbol_idx = self.symbol_index()?;
        let mut symbol_proximity_ranked: Vec<(String, Option<u32>)> = Vec::new();
        let mut seen_symbol_hits = HashSet::new();
        for &seed in &query.seed_symbols {
            for sym in symbol_idx
                .callers(seed)
                .into_iter()
                .chain(symbol_idx.callees(seed))
            {
                let key = (sym.path.clone(), Some(sym.range.line_start));
                if seen_symbol_hits.insert(key.clone()) {
                    symbol_proximity_ranked.push(key);
                }
            }
        }
        let symbol_proximity_sig = SignalRanking {
            signal: Signal::SymbolProximity,
            ranked: symbol_proximity_ranked,
        };

        let mut affinity_prefixes: Vec<String> = query.seed_paths.clone();
        affinity_prefixes.extend(ctx.claimed_paths.iter().cloned());
        let files = self.store.list_files()?;
        let mut path_affinity_ranked: Vec<(String, Option<u32>)> = Vec::new();
        let mut seen_paths = HashSet::new();
        for prefix in &affinity_prefixes {
            for file in &files {
                if file.path.starts_with(prefix.as_str()) && seen_paths.insert(file.path.clone()) {
                    path_affinity_ranked.push((file.path.clone(), None));
                }
            }
        }
        let path_affinity_sig = SignalRanking {
            signal: Signal::PathAffinity,
            ranked: path_affinity_ranked,
        };

        let edit_recency_sig = SignalRanking {
            signal: Signal::EditRecency,
            ranked: ctx
                .recently_edited
                .iter()
                .map(|p| (p.clone(), None))
                .collect(),
        };

        let history = HistoryIndex::new(Arc::clone(&self.store), &self.project_root);
        let mut co_change: Vec<(String, u64)> = Vec::new();
        let mut seen_co_change = HashSet::new();
        for seed_path in &query.seed_paths {
            if let Ok(entries) = history.co_change(seed_path) {
                for entry in entries {
                    if seen_co_change.insert(entry.path.clone()) {
                        co_change.push((entry.path, entry.co_commits));
                    }
                }
            }
        }
        co_change.sort_by_key(|c| std::cmp::Reverse(c.1));
        let co_change_sig = SignalRanking {
            signal: Signal::CoChange,
            ranked: co_change.into_iter().map(|(p, _)| (p, None)).collect(),
        };

        let signals = vec![
            semantic_sig,
            lexical_sig,
            symbol_proximity_sig,
            path_affinity_sig,
            edit_recency_sig,
            co_change_sig,
        ];

        let store = Arc::clone(&self.store);
        let snippet_lookup = move |path: &str, line_start: Option<u32>| -> (String, Option<u32>) {
            let conn = match store.reader() {
                Ok(conn) => conn,
                Err(_) => return (String::new(), None),
            };
            let result = if let Some(line) = line_start {
                conn.query_row(
                    "SELECT c.text, c.line_end FROM chunks c JOIN files f ON f.id = c.file_id \
                     WHERE f.path = ?1 AND c.line_start <= ?2 AND c.line_end >= ?2 \
                     ORDER BY c.line_start DESC LIMIT 1",
                    params![path, line],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
                )
            } else {
                conn.query_row(
                    "SELECT c.text, c.line_end FROM chunks c JOIN files f ON f.id = c.file_id \
                     WHERE f.path = ?1 ORDER BY c.line_start LIMIT 1",
                    params![path],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
                )
            };
            match result {
                Ok((text, line_end)) => (text, Some(line_end)),
                Err(_) => (String::new(), None),
            }
        };

        Ok(hybrid(query, ctx, &signals, weights, &snippet_lookup))
    }

    /// Load a fresh [`SymbolIndex`] by parsing every currently-indexed file with a grammar.
    /// Symbol/reference queries (`definition`, `references`, `outline`, etc.) go through the
    /// returned index directly; this facade does not cache it, since staleness after an
    /// `update_incremental` would be surprising.
    pub fn symbol_index(&self) -> Result<SymbolIndex> {
        let files = self.store.list_files()?;
        let mut index = SymbolIndex::new();
        for file in files {
            let Some(lang) = file.lang else { continue };
            if !lang.has_grammar() {
                continue;
            }
            let full_path = self.project_root.join(&file.path);
            let text = match fs::read_to_string(&full_path) {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!(
                        "symbol_index: skipping {}: failed to read: {}",
                        file.path,
                        e
                    );
                    continue;
                }
            };
            if let Err(e) = index.parse_file(&file.path, &text, lang) {
                tracing::warn!(
                    "symbol_index: skipping {}: failed to parse: {}",
                    file.path,
                    e
                );
            }
        }
        Ok(index)
    }

    /// Resolve a reference to `name` from `from_path`. Convenience wrapper over
    /// [`CodeIntel::symbol_index`] + [`SymbolIndex::resolve`] for callers that don't need to
    /// hold the index themselves.
    pub fn resolve_symbol(&self, name: &str, from_path: &str) -> Result<Resolution> {
        let index = self.symbol_index()?;
        let reference = Reference {
            symbol_hint: name.to_string(),
            path: from_path.to_string(),
            range: Range {
                byte_start: 0,
                byte_end: 0,
                line_start: 0,
                line_end: 0,
            },
        };
        Ok(index.resolve(&reference))
    }

    /// The symbol defining `name` as seen from `from_path`.
    pub fn definition(&self, name: &str, from_path: &str) -> Result<Option<Symbol>> {
        let index = self.symbol_index()?;
        Ok(index.definition(name, from_path).cloned())
    }

    /// A rendered outline of `path`.
    pub fn outline(&self, path: &str) -> Result<Vec<crate::symbols::OutlineEntry>> {
        let index = self.symbol_index()?;
        Ok(index.outline(path))
    }

    /// Git history: why a line range looks the way it does.
    pub fn history_why(
        &self,
        path: &str,
        line_start: u32,
        line_end: u32,
    ) -> Result<crate::history::WhyAnswer> {
        HistoryIndex::new(Arc::clone(&self.store), &self.project_root)
            .why(path, line_start, line_end)
    }

    /// Git history: search commit messages and diffs.
    pub fn history_search(&self, query: &str) -> Result<Vec<crate::history::HistoryHit>> {
        HistoryIndex::new(Arc::clone(&self.store), &self.project_root).search(query)
    }

    /// Git history: find implementations matching `query` that were deleted and never
    /// reintroduced.
    pub fn history_deleted(
        &self,
        query: &str,
    ) -> Result<Vec<crate::history::DeletedImplementation>> {
        HistoryIndex::new(Arc::clone(&self.store), &self.project_root).deleted(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tm_types::FixedClock;

    /// Initialize a git repo with one empty commit at `path`, so
    /// [`HistoryIndex::ingest_incremental`] (invoked by every `update_incremental` call) has a
    /// valid `HEAD` to walk instead of failing on a repo with no commits.
    fn init_git_repo(path: &Path) {
        let repo = git2::Repository::init(path).expect("git init");
        let sig = git2::Signature::new("Test", "test@example.com", &git2::Time::new(0, 0))
            .expect("signature");
        let tree_id = {
            let mut index = repo.index().expect("repo index");
            index.write_tree().expect("write tree")
        };
        let tree = repo.find_tree(tree_id).expect("find tree");
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .expect("initial commit");
    }

    fn new_project() -> TempDir {
        let dir = TempDir::new().expect("temp dir");
        init_git_repo(dir.path());
        dir
    }

    #[test]
    fn open_creates_index_db_under_project_root() {
        let dir = new_project();
        let intel = CodeIntel::open(dir.path()).expect("open");
        assert!(dir.path().join(".tm/index.db").exists());
        assert_eq!(intel.project_root(), dir.path());
    }

    #[test]
    fn open_with_embedder_uses_the_supplied_embedder() {
        let dir = new_project();
        let embedder: Arc<dyn Embedder> = Arc::new(LocalHashEmbedder::new());
        let intel = CodeIntel::open_with_embedder(dir.path(), Arc::clone(&embedder)).expect("open");
        assert_eq!(intel.embedder.identifier(), embedder.identifier());
    }

    #[test]
    fn update_incremental_indexes_a_new_text_file() {
        let dir = new_project();
        fs::write(
            dir.path().join("hello.py"),
            "def greet():\n    return 'hi'\n",
        )
        .expect("write");

        let intel = CodeIntel::open(dir.path()).expect("open");
        let clock = FixedClock::epoch();
        let delta = intel.update_incremental(&clock).expect("update");

        assert_eq!(delta.files_added, 1);
        assert_eq!(delta.files_modified, 0);
        assert_eq!(delta.files_removed, 0);
        assert!(delta.chunks_written >= 1);
    }

    #[test]
    fn update_incremental_is_a_no_op_when_nothing_changed() {
        let dir = new_project();
        fs::write(dir.path().join("hello.py"), "print('hi')\n").expect("write");

        let intel = CodeIntel::open(dir.path()).expect("open");
        let clock = FixedClock::epoch();
        intel.update_incremental(&clock).expect("first update");
        let delta = intel.update_incremental(&clock).expect("second update");

        assert_eq!(delta.files_added, 0);
        assert_eq!(delta.files_modified, 0);
        assert_eq!(delta.files_removed, 0);
        assert_eq!(delta.chunks_written, 0);
        assert_eq!(delta.commits_ingested, 0);
    }

    #[test]
    fn update_incremental_detects_a_modified_file() {
        let dir = new_project();
        let path = dir.path().join("hello.py");
        fs::write(&path, "print('hi')\n").expect("write");

        let intel = CodeIntel::open(dir.path()).expect("open");
        let clock = FixedClock::epoch();
        intel.update_incremental(&clock).expect("first update");

        fs::write(&path, "print('changed')\n").expect("rewrite");
        let delta = intel.update_incremental(&clock).expect("second update");

        assert_eq!(delta.files_added, 0);
        assert_eq!(delta.files_modified, 1);
        assert_eq!(delta.files_removed, 0);
    }

    #[test]
    fn update_incremental_detects_a_removed_file() {
        let dir = new_project();
        let path = dir.path().join("hello.py");
        fs::write(&path, "print('hi')\n").expect("write");

        let intel = CodeIntel::open(dir.path()).expect("open");
        let clock = FixedClock::epoch();
        intel.update_incremental(&clock).expect("first update");

        fs::remove_file(&path).expect("remove");
        let delta = intel.update_incremental(&clock).expect("second update");

        assert_eq!(delta.files_added, 0);
        assert_eq!(delta.files_modified, 0);
        assert_eq!(delta.files_removed, 1);

        let files = intel.store.list_files().expect("list files");
        assert!(files.is_empty());
    }

    #[test]
    fn update_incremental_skips_files_over_the_size_ceiling() {
        let dir = new_project();
        let oversized = "x".repeat((MAX_INDEXABLE_BYTES + 1) as usize);
        fs::write(dir.path().join("big.txt"), oversized).expect("write");

        let intel = CodeIntel::open(dir.path()).expect("open");
        let clock = FixedClock::epoch();
        let delta = intel.update_incremental(&clock).expect("update");

        assert_eq!(delta.files_added, 1);
        assert_eq!(delta.chunks_written, 0);
    }

    #[test]
    fn search_exact_finds_a_literal_substring_without_indexing_first() {
        let dir = new_project();
        fs::write(dir.path().join("hello.py"), "def unique_marker(): pass\n").expect("write");

        let intel = CodeIntel::open(dir.path()).expect("open");
        let result = intel.search_exact("unique_marker").expect("search");
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].path, "hello.py");
    }

    #[test]
    fn search_semantic_returns_results_after_update_incremental() {
        let dir = new_project();
        fs::write(
            dir.path().join("hello.py"),
            "def greet_the_world():\n    return 'hello world'\n",
        )
        .expect("write");

        let intel = CodeIntel::open(dir.path()).expect("open");
        let clock = FixedClock::epoch();
        intel.update_incremental(&clock).expect("update");

        let hits = intel
            .search_semantic("greet the world", SemanticSearchOptions::default())
            .expect("search");
        assert!(!hits.is_empty());
    }

    #[test]
    fn outline_of_an_unindexed_project_is_empty() {
        let dir = new_project();
        let intel = CodeIntel::open(dir.path()).expect("open");
        let outline = intel.outline("nonexistent.rs").expect("outline");
        assert!(outline.is_empty());
    }

    #[test]
    fn definition_is_none_when_no_files_are_indexed() {
        let dir = new_project();
        let intel = CodeIntel::open(dir.path()).expect("open");
        let def = intel
            .definition("anything", "nonexistent.rs")
            .expect("definition");
        assert!(def.is_none());
    }

    #[test]
    fn resolve_symbol_is_none_when_no_files_are_indexed() {
        let dir = new_project();
        let intel = CodeIntel::open(dir.path()).expect("open");
        let resolution = intel
            .resolve_symbol("anything", "nonexistent.rs")
            .expect("resolve_symbol");
        assert!(resolution.symbol_id.is_none());
    }

    #[test]
    fn search_hybrid_returns_no_hits_over_an_empty_index() {
        let dir = new_project();
        let intel = CodeIntel::open(dir.path()).expect("open");
        let query = Query {
            text: "nothing to find".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let hits = intel
            .search_hybrid(&query, &ctx, SignalWeights::default())
            .expect("search_hybrid");
        assert!(hits.is_empty());
    }

    #[test]
    fn history_search_finds_the_initial_commit_message() {
        let dir = new_project();
        let intel = CodeIntel::open(dir.path()).expect("open");
        let clock = FixedClock::epoch();
        intel.update_incremental(&clock).expect("update");
        let hits = intel.history_search("init").expect("history_search");
        assert!(!hits.is_empty());
    }
}
