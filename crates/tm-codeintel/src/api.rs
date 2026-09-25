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
use rusqlite::{params, Connection, OptionalExtension};
use tm_types::{Clock, Result};

use crate::chunk::Chunker;
use crate::embed::{Embedder, LocalHashEmbedder};
use crate::exact::{ExactSearch, ExactSearchResult, SearchOptions};
use crate::history::HistoryIndex;
use crate::hybrid::{
    hybrid, semantic_ranking, Query, RankedHit, RetrievalContext, Signal, SignalRanking,
    SignalWeights,
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
    ///
    /// Shim over [`CodeIntel::open_at`] for the repo-scoped layout (index at
    /// `<project_root>/.tm/index.db`, workspace = `project_root`); prefer [`CodeIntel::open_at`]
    /// when the caller already knows the index directory (e.g. a global-scope project under
    /// `$TM_HOME/projects/<key>/`).
    pub fn open(project_root: &Path) -> Result<CodeIntel> {
        Self::open_at(&project_root.join(".tm"), project_root)
    }

    /// Open with an explicit embedder (e.g. an API-backed one supplied by a higher layer),
    /// otherwise identical to [`CodeIntel::open`].
    pub fn open_with_embedder(
        project_root: &Path,
        embedder: Arc<dyn Embedder>,
    ) -> Result<CodeIntel> {
        Self::open_at_with_embedder(&project_root.join(".tm"), project_root, embedder)
    }

    /// Open using [`crate::embed::build_default_embedder`]'s choice: the real semantic
    /// [`crate::potion::PotionEmbedder`] when `minishlab/potion-code-16M-v2` is already
    /// cached locally, [`LocalHashEmbedder`] otherwise (never downloads; see D-025) — or
    /// whatever `TM_EMBEDDER`/`config_override` force. This is the entry point real command
    /// paths should prefer over [`CodeIntel::open`] going forward; `open`/`open_at` keep
    /// defaulting to [`LocalHashEmbedder`] unconditionally so the many existing `#[test]`s
    /// across this workspace that call them stay exactly as deterministic and network-free as
    /// they are today.
    pub fn open_auto(project_root: &Path, config_override: Option<&str>) -> Result<CodeIntel> {
        Self::open_at_with_embedder(
            &project_root.join(".tm"),
            project_root,
            crate::embed::build_default_embedder(config_override),
        )
    }

    /// [`CodeIntel::open_auto`], at an explicit index directory (see [`CodeIntel::open_at`]).
    pub fn open_at_auto(
        index_dir: &Path,
        workspace_root: &Path,
        config_override: Option<&str>,
    ) -> Result<CodeIntel> {
        Self::open_at_with_embedder(
            index_dir,
            workspace_root,
            crate::embed::build_default_embedder(config_override),
        )
    }

    /// Open (creating if absent) the code-intelligence index at `<index_dir>/index.db`, indexing
    /// the workspace at `workspace_root`, using the default no-network [`LocalHashEmbedder`].
    /// `index_dir` and `workspace_root` are independent: `index_dir` is where `index.db` lives
    /// (a project's state directory), `workspace_root` is what gets walked/indexed (the git
    /// toplevel or cwd) — see the `state_dir` vs. `root` distinction in D-003.
    pub fn open_at(index_dir: &Path, workspace_root: &Path) -> Result<CodeIntel> {
        Self::open_at_with_embedder(
            index_dir,
            workspace_root,
            Arc::new(LocalHashEmbedder::new()),
        )
    }

    /// Open at an explicit index directory and workspace root with an explicit embedder,
    /// otherwise identical to [`CodeIntel::open_at`].
    pub fn open_at_with_embedder(
        index_dir: &Path,
        workspace_root: &Path,
        embedder: Arc<dyn Embedder>,
    ) -> Result<CodeIntel> {
        let store = Arc::new(Store::open_at(index_dir)?);
        Ok(CodeIntel {
            project_root: workspace_root.to_path_buf(),
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
        let mut changes: ChangeSet = RepoWalker::diff(&current, &previous_records);

        // The vectors table is keyed by `embedder.identifier()`. If the configured embedder
        // has changed since the last `update_incremental` (e.g. Potion just became available,
        // or the project's `index.embedder` config flipped), a blake3-unchanged file's vector
        // was still produced by the *old* embedder and would otherwise never get re-embedded
        // (that's the whole point of the blake3 skip), silently mixing vector spaces in one
        // corpus. Detect the mismatch via a single meta row and, when it fires, promote every
        // currently-walked file not already in `added`/`modified` into `modified` so it goes
        // through `write_file_content` (and therefore gets re-embedded) this run.
        const EMBEDDER_META_KEY: &str = "embedder_identifier";
        let stored_embedder_id = self.store.get_meta(EMBEDDER_META_KEY)?;
        let embedder_changed = stored_embedder_id.as_deref() != Some(self.embedder.identifier());
        if embedder_changed {
            let already_covered: HashSet<&str> = changes
                .added
                .iter()
                .chain(changes.modified.iter())
                .map(|r| r.path.as_str())
                .collect();
            let promoted: Vec<FileRecord> = current
                .iter()
                .filter(|record| !already_covered.contains(record.path.as_str()))
                .cloned()
                .collect();
            drop(already_covered);
            changes.modified.extend(promoted);
        }

        let mut delta = IndexDelta::default();

        if !changes.is_empty() {
            let conn = self.store.writer()?;

            if embedder_changed {
                conn.execute("DELETE FROM vectors", []).map_err(|e| {
                    tm_types::TmError::storage(format!(
                        "Failed to clear vectors for embedder change: {e}"
                    ))
                })?;
            }

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

            Store::set_meta(&conn, EMBEDDER_META_KEY, self.embedder.identifier())?;

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

    /// Literal substring search bounded by `options` (a result-count limit and optional
    /// `path_glob`), for callers that hand results to a model rather than a human — an agent
    /// tool or the MCP server — where an unbounded [`CodeIntel::search_exact`] can be tens of
    /// thousands of tokens of `line_text` for one query on a real repo.
    pub fn search_exact_with(
        &self,
        needle: &str,
        options: &SearchOptions,
    ) -> Result<ExactSearchResult> {
        ExactSearch::new(&self.project_root).literal_with(needle, options)
    }

    /// Regex search bounded by `options`. See [`CodeIntel::search_exact_with`].
    pub fn search_regex_with(
        &self,
        pattern: &str,
        options: &SearchOptions,
    ) -> Result<ExactSearchResult> {
        ExactSearch::new(&self.project_root).regex_with(pattern, options)
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

    /// BM25-ranked lexical search over the `tokens` inverted index populated by
    /// `write_file_content`, used as the [`Signal::Lexical`] input to
    /// [`CodeIntel::search_hybrid`].
    ///
    /// Unlike [`CodeIntel::search_exact`] (a literal substring match on the *whole* query
    /// string, which is effectively dead for a multi-word natural-language query like "where
    /// are tickets moved between states" -- no file contains that exact substring), this
    /// tokenizes the query the same way chunk text is tokenized at index time and scores
    /// every chunk sharing at least one token, using the standard Okapi BM25 formula
    /// (`k1 = 1.2`, `b = 0.75`) over each token's term frequency, document frequency and the
    /// corpus's average chunk length. Ties broken by chunk id for determinism.
    fn search_lexical_bm25(&self, query_text: &str) -> Result<SignalRanking> {
        let empty = || SignalRanking {
            signal: Signal::Lexical,
            ranked: Vec::new(),
        };

        let query_tokens: Vec<String> = tokenize_for_index(query_text).into_keys().collect();
        if query_tokens.is_empty() {
            return Ok(empty());
        }

        let conn = self.store.reader()?;

        let chunk_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
            .map_err(|e| tm_types::TmError::storage(format!("Failed to count chunks: {e}")))?;
        if chunk_count == 0 {
            return Ok(empty());
        }

        // Chunk lengths (in indexed tokens), computed once for the whole corpus rather than
        // per query token, since BM25's length-normalization term needs every candidate
        // chunk's length and the corpus average regardless of how many tokens the query has.
        let mut doc_len: HashMap<i64, i64> = HashMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT chunk_id, SUM(term_frequency) FROM tokens GROUP BY chunk_id")
                .map_err(|e| {
                    tm_types::TmError::storage(format!("Failed to prepare doc-length query: {e}"))
                })?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
                .map_err(|e| {
                    tm_types::TmError::storage(format!("Failed to query doc lengths: {e}"))
                })?;
            for row in rows {
                let (chunk_id, len) = row.map_err(|e| {
                    tm_types::TmError::storage(format!("Failed to read doc-length row: {e}"))
                })?;
                doc_len.insert(chunk_id, len);
            }
        }
        let avg_doc_len = if doc_len.is_empty() {
            1.0
        } else {
            doc_len.values().sum::<i64>() as f64 / doc_len.len() as f64
        }
        .max(1.0);

        const K1: f64 = 1.2;
        const B: f64 = 0.75;

        let mut scores: HashMap<i64, f64> = HashMap::new();
        for token in &query_tokens {
            let df: i64 = conn
                .query_row(
                    "SELECT COUNT(DISTINCT chunk_id) FROM tokens WHERE token = ?1",
                    params![token],
                    |row| row.get(0),
                )
                .map_err(|e| tm_types::TmError::storage(format!("Failed to read token df: {e}")))?;
            if df == 0 {
                continue;
            }
            let idf = ((chunk_count as f64 - df as f64 + 0.5) / (df as f64 + 0.5) + 1.0).ln();

            let mut stmt = conn
                .prepare("SELECT chunk_id, term_frequency FROM tokens WHERE token = ?1")
                .map_err(|e| {
                    tm_types::TmError::storage(format!("Failed to prepare token query: {e}"))
                })?;
            let rows = stmt
                .query_map(params![token], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(|e| tm_types::TmError::storage(format!("Failed to query tokens: {e}")))?;
            for row in rows {
                let (chunk_id, tf) = row.map_err(|e| {
                    tm_types::TmError::storage(format!("Failed to read token row: {e}"))
                })?;
                let tf = tf as f64;
                let len = doc_len.get(&chunk_id).copied().unwrap_or(0) as f64;
                let denom = tf + K1 * (1.0 - B + B * len / avg_doc_len);
                if denom <= 0.0 {
                    continue;
                }
                let score = idf * (tf * (K1 + 1.0)) / denom;
                *scores.entry(chunk_id).or_insert(0.0) += score;
            }
        }

        if scores.is_empty() {
            return Ok(empty());
        }

        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        let mut out = Vec::with_capacity(ranked.len());
        for (chunk_id, _score) in ranked {
            let hit: Option<(String, u32)> = conn
                .query_row(
                    "SELECT f.path, c.line_start FROM chunks c JOIN files f ON f.id = c.file_id \
                     WHERE c.id = ?1",
                    params![chunk_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
                )
                .optional()
                .map_err(|e| {
                    tm_types::TmError::storage(format!("Failed to resolve chunk path: {e}"))
                })?;
            if let Some((path, line_start)) = hit {
                out.push((path, Some(line_start)));
            }
        }

        Ok(SignalRanking {
            signal: Signal::Lexical,
            ranked: out,
        })
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

        let lexical_sig = self.search_lexical_bm25(&query.text)?;

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
        let project_root = self.project_root.clone();
        let snippet_lookup = move |path: &str, line_start: Option<u32>| -> (String, Option<u32>) {
            // Try to read the line from the source file first, like exact search does.
            if let Some(line_num) = line_start {
                let full_path = project_root.join(path);
                if let Ok(contents) = fs::read_to_string(&full_path) {
                    let lines: Vec<&str> = contents.lines().collect();
                    // line_num is 1-based
                    if let Some(line_idx) = line_num.checked_sub(1) {
                        if let Some(line_text) = lines.get(line_idx as usize) {
                            return (line_text.to_string(), Some(line_num));
                        }
                    }
                }
            }

            // Fall back to reading from the database chunks if file read fails or no line_start.
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
    /// `update_incremental` would be surprising. Not caching costs a full re-parse of every
    /// indexed file on each call, but a caller that holds a [`SymbolIndex`] across turns (e.g.
    /// tm-mcp's `symbol_def`, whose id needs to round-trip into a later
    /// `symbol_references`/`symbol_callers`/`symbol_callees` call within the same session) no
    /// longer needs *this* facade to cache anything to get a stable id: symbol ids are now
    /// content-derived (nav-design-symbol-index-caching-stable-ids, see
    /// `crate::symbols::stable_symbol_id` and docs/decisions/D-029-stable-symbol-ids.md), so two
    /// independently-parsed `SymbolIndex`es of the same files agree on a symbol's id without
    /// needing to be the same index instance. Incremental re-parsing of only the files an
    /// `IndexDelta` touched, keyed by that stability, is a real option `D-029` leaves open but
    /// this task does not implement, given `self.store.list_files()` here already returns
    /// every indexed file each call and there is no cached [`SymbolIndex`] instance for a delta
    /// to patch into.
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
    fn open_at_separates_index_dir_from_workspace_and_writes_only_index_db_there() {
        let workspace = new_project();
        fs::write(
            workspace.path().join("hello.py"),
            "def greet():\n    return 'hi'\n",
        )
        .expect("write");
        // `index_dir` is deliberately outside `workspace` and not named `.tm`, the way a
        // global-scope project's `$TM_HOME/projects/<key>/` would be relative to the repo it
        // indexes.
        let state = TempDir::new().expect("state dir");
        let index_dir = state.path().join("global-state");

        let intel =
            CodeIntel::open_at(&index_dir, workspace.path()).expect("open_at should succeed");
        assert_eq!(intel.project_root(), workspace.path());
        assert!(
            index_dir.join("index.db").exists(),
            "open_at must create <index_dir>/index.db"
        );
        assert!(
            !workspace.path().join(".tm").exists(),
            "open_at must never create a .tm directory under the workspace"
        );

        let clock = FixedClock::epoch();
        let delta = intel
            .update_incremental(&clock)
            .expect("update_incremental should index the workspace");
        assert_eq!(delta.files_added, 1);
        assert!(delta.chunks_written >= 1);

        // Still true after indexing: nothing was written under the workspace, only under
        // `index_dir`.
        assert!(!workspace.path().join(".tm").exists());
        assert!(index_dir.join("index.db").exists());
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

    /// A second, distinctly-identified no-network embedder purely for testing the
    /// embedder-mismatch re-embed path without depending on Potion or the network.
    struct OtherTestEmbedder;
    impl Embedder for OtherTestEmbedder {
        fn dims(&self) -> usize {
            crate::embed::LOCAL_HASH_DIMS
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            LocalHashEmbedder::new().embed(texts)
        }
        fn identifier(&self) -> &str {
            "test-other-embedder"
        }
    }

    #[test]
    fn update_incremental_re_embeds_unchanged_files_when_the_embedder_identifier_changes() {
        let dir = new_project();
        fs::write(dir.path().join("hello.py"), "print('hi')\n").expect("write");

        let clock = FixedClock::epoch();
        let hash_embedder: Arc<dyn Embedder> = Arc::new(LocalHashEmbedder::new());
        let intel =
            CodeIntel::open_with_embedder(dir.path(), Arc::clone(&hash_embedder)).expect("open");
        let first = intel.update_incremental(&clock).expect("first update");
        assert_eq!(first.files_added, 1);

        // Reopen the same index with a different-identified embedder; the file on disk is
        // completely unchanged (same blake3), so a plain walker diff would report no changes
        // at all -- but the stored vector space no longer matches this embedder's identifier,
        // so this must still re-embed it.
        let other_embedder: Arc<dyn Embedder> = Arc::new(OtherTestEmbedder);
        let intel_other =
            CodeIntel::open_with_embedder(dir.path(), Arc::clone(&other_embedder)).expect("open");
        let second = intel_other
            .update_incremental(&clock)
            .expect("second update, embedder changed");

        assert_eq!(
            second.files_added, 0,
            "the file itself is not new to the file-tracking table"
        );
        assert_eq!(
            second.files_modified, 1,
            "an embedder-identifier mismatch must force a re-embed even though blake3 is unchanged"
        );
        assert!(second.chunks_written >= 1);

        // A third, unchanged open with the same (new) embedder must now be a true no-op again.
        let intel_other_again =
            CodeIntel::open_with_embedder(dir.path(), Arc::clone(&other_embedder)).expect("open");
        let third = intel_other_again
            .update_incremental(&clock)
            .expect("third update, embedder unchanged");
        assert_eq!(third.files_added, 0);
        assert_eq!(third.files_modified, 0);
    }

    #[test]
    fn search_lexical_bm25_ranks_a_multi_word_query_no_file_contains_as_a_literal_substring() {
        let dir = new_project();
        fs::write(
            dir.path().join("machine.py"),
            "def move_ticket_between_states(ticket, new_state):\n    ticket.state = new_state\n",
        )
        .expect("write");
        fs::write(
            dir.path().join("unrelated.py"),
            "def totally_unrelated():\n    return 42\n",
        )
        .expect("write");

        let intel = CodeIntel::open(dir.path()).expect("open");
        let clock = FixedClock::epoch();
        intel.update_incremental(&clock).expect("update");

        // No file contains this exact phrase as a literal substring, so `search_exact` would
        // report zero hits; the tokenized BM25 ranking must still surface the relevant file.
        let ranking = intel
            .search_lexical_bm25("moving tickets between states")
            .expect("lexical search");
        assert!(!ranking.ranked.is_empty());
        assert_eq!(ranking.ranked[0].0, "machine.py");
    }

    #[test]
    fn search_lexical_bm25_returns_no_hits_over_an_empty_index() {
        let dir = new_project();
        let intel = CodeIntel::open(dir.path()).expect("open");
        let ranking = intel
            .search_lexical_bm25("anything at all")
            .expect("lexical search");
        assert!(ranking.ranked.is_empty());
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
    fn symbol_id_survives_update_incremental_adding_a_file_that_sorts_earlier() {
        // nav-design-symbol-index-caching-stable-ids: symbol_index() re-parses the whole
        // workspace on every call (staleness would be surprising, see symbol_index()'s own doc
        // comment), in path-sorted order (Store::list_files()'s `ORDER BY path`). Before
        // docs/decisions/D-029-stable-symbol-ids.md, an id was a per-parse positional counter,
        // so adding a new file that sorts before an existing one would have shifted every id
        // after it. Content-derived ids must not move here.
        let dir = new_project();
        fs::write(dir.path().join("z.rs"), "fn only() {}\n").expect("write");

        let intel = CodeIntel::open(dir.path()).expect("open");
        let clock = FixedClock::epoch();
        intel.update_incremental(&clock).expect("first update");

        let id_before = intel
            .definition("only", "z.rs")
            .expect("symbol_index")
            .expect("only is defined in z.rs")
            .id;

        // a.rs sorts before z.rs.
        fs::write(dir.path().join("a.rs"), "fn other() {}\n").expect("write");
        let delta = intel.update_incremental(&clock).expect("second update");
        assert_eq!(delta.files_added, 1, "a.rs must be a newly-added file");

        let id_after = intel
            .definition("only", "z.rs")
            .expect("symbol_index")
            .expect("only is still defined in z.rs")
            .id;

        assert_eq!(id_before, id_after);
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
    fn search_hybrid_populates_snippet_from_source_file() {
        let dir = new_project();
        fs::write(
            dir.path().join("math.rs"),
            "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .expect("write math.rs");

        let intel = CodeIntel::open(dir.path()).expect("open");
        let clock = FixedClock::epoch();
        intel.update_incremental(&clock).expect("update");

        let query = Query {
            text: "add".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let hits = intel
            .search_hybrid(&query, &ctx, SignalWeights::default())
            .expect("search_hybrid");

        // Verify we found results with non-empty snippets
        assert!(!hits.is_empty(), "should find 'add' in the file");
        for hit in &hits {
            if hit.path == "math.rs" && hit.line_start == Some(1) {
                // The function definition line should have a non-empty snippet
                assert!(
                    !hit.snippet.is_empty(),
                    "snippet should not be empty for line with code"
                );
                assert!(
                    hit.snippet.contains("fn add"),
                    "snippet should contain the matched function"
                );
            }
        }
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
