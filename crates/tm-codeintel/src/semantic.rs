//! Owns semantic (vector) search: embed the query with the project's configured
//! [`crate::embed::Embedder`], prefilter candidate chunks through the inverted token index so
//! a query never has to cosine-rank the entire corpus, then rank survivors by cosine
//! similarity.
//!
//! Reads go through [`crate::store::Store::reader`], so this module stays correct under a
//! concurrent writer: a query either sees a chunk's vector fully written or not at all
//! (WAL + `BEGIN IMMEDIATE` on the write side guarantees no torn reads), never a partial row.

use std::sync::Arc;

use rusqlite::{Connection, ToSql};
use tm_types::{Result, TmError};

use crate::embed::{cosine_similarity, Embedder};
use crate::store::Store;

/// A chunk returned by semantic search, with its similarity score and text so callers don't
/// need a second round-trip to render it.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredChunk {
    /// Row id of the matching chunk.
    pub chunk_id: i64,
    /// Path of the owning file.
    pub path: String,
    /// The chunk's text.
    pub text: String,
    /// 1-based start line, inclusive.
    pub line_start: u32,
    /// 1-based end line, inclusive.
    pub line_end: u32,
    /// Cosine similarity to the query embedding, in `[-1.0, 1.0]`.
    pub score: f32,
}

/// How many candidate chunks the inverted-token prefilter admits before cosine ranking, and
/// how many results are ultimately returned.
#[derive(Debug, Clone, Copy)]
pub struct SemanticSearchOptions {
    /// Maximum candidates pulled from the token prefilter for cosine scoring.
    pub prefilter_limit: usize,
    /// Maximum results returned after ranking.
    pub top_k: usize,
}

impl Default for SemanticSearchOptions {
    fn default() -> Self {
        SemanticSearchOptions {
            prefilter_limit: 500,
            top_k: 20,
        }
    }
}

/// Vector search over one project's chunk index.
pub struct SemanticSearch {
    store: Arc<Store>,
    embedder: Arc<dyn Embedder>,
}

impl SemanticSearch {
    /// A searcher over `store`'s chunks, embedding queries with `embedder` (must match the
    /// embedder used to populate `vectors`, identified by [`Embedder::identifier`]).
    pub fn new(store: Arc<Store>, embedder: Arc<dyn Embedder>) -> Self {
        SemanticSearch { store, embedder }
    }

    /// Run a semantic query: embed `query_text`, prefilter through the inverted token index
    /// (see [`SemanticSearch::prefilter_candidates`]), cosine-rank survivors, return the top
    /// `options.top_k` as [`ScoredChunk`]s in descending score order.
    pub fn search(
        &self,
        query_text: &str,
        options: SemanticSearchOptions,
    ) -> Result<Vec<ScoredChunk>> {
        let mut query_vectors = self.embedder.embed(&[query_text.to_string()])?;
        let query_vector = query_vectors.pop().unwrap_or_default();

        let tokens = Self::query_tokens(query_text);
        let mut candidate_ids = if tokens.is_empty() {
            Vec::new()
        } else {
            self.prefilter_candidates(&tokens, options.prefilter_limit)?
        };

        if candidate_ids.is_empty() {
            candidate_ids = self.fallback_candidates(options.prefilter_limit)?;
        }

        if candidate_ids.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.store.reader()?;
        let mut scored = self.fetch_and_score(&conn, &candidate_ids, &query_vector)?;
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(options.top_k);
        Ok(scored)
    }

    /// Given the tokens of a query, look up candidate chunk ids from the `tokens` inverted
    /// index (union of postings for each query token, ranked by how many distinct query
    /// tokens each chunk matched, ties broken by total term frequency), bounded to `limit`.
    fn prefilter_candidates(&self, query_tokens: &[String], limit: usize) -> Result<Vec<i64>> {
        if query_tokens.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.store.reader()?;
        let placeholders = std::iter::repeat_n("?", query_tokens.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT chunk_id, COUNT(DISTINCT token) AS distinct_hits, SUM(term_frequency) AS tf \
             FROM tokens WHERE token IN ({placeholders}) GROUP BY chunk_id \
             ORDER BY distinct_hits DESC, tf DESC LIMIT ?"
        );

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| TmError::storage(e.to_string()))?;
        let limit_i64 = limit as i64;
        let mut params: Vec<&dyn ToSql> = query_tokens.iter().map(|t| t as &dyn ToSql).collect();
        params.push(&limit_i64);

        let rows = stmt
            .query_map(params.as_slice(), |row| row.get::<_, i64>(0))
            .map_err(|e| TmError::storage(e.to_string()))?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| TmError::storage(e.to_string()))?);
        }
        Ok(out)
    }

    /// Fallback candidate set when the query has no indexed tokens (or the prefilter matched
    /// nothing): every chunk with a vector for this searcher's embedder, bounded by `limit`.
    fn fallback_candidates(&self, limit: usize) -> Result<Vec<i64>> {
        let conn = self.store.reader()?;
        let mut stmt = conn
            .prepare("SELECT chunk_id FROM vectors WHERE embedder = ?1 LIMIT ?2")
            .map_err(|e| TmError::storage(e.to_string()))?;

        let rows = stmt
            .query_map(
                rusqlite::params![self.embedder.identifier(), limit as i64],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| TmError::storage(e.to_string()))?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| TmError::storage(e.to_string()))?);
        }
        Ok(out)
    }

    /// Fetch chunk text/location and stored vector for each candidate id (restricted to this
    /// searcher's embedder), and score each by cosine similarity to `query_vector`.
    fn fetch_and_score(
        &self,
        conn: &Connection,
        candidate_ids: &[i64],
        query_vector: &[f32],
    ) -> Result<Vec<ScoredChunk>> {
        let placeholders = std::iter::repeat_n("?", candidate_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT c.id, f.path, c.text, c.line_start, c.line_end, v.vector \
             FROM chunks c \
             JOIN files f ON f.id = c.file_id \
             JOIN vectors v ON v.chunk_id = c.id \
             WHERE v.embedder = ? AND c.id IN ({placeholders})"
        );

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| TmError::storage(e.to_string()))?;
        let embedder_id = self.embedder.identifier().to_string();
        let mut params: Vec<&dyn ToSql> = vec![&embedder_id];
        params.extend(candidate_ids.iter().map(|id| id as &dyn ToSql));

        let rows = stmt
            .query_map(params.as_slice(), |row| {
                let chunk_id: i64 = row.get(0)?;
                let path: String = row.get(1)?;
                let text: String = row.get(2)?;
                let line_start: u32 = row.get(3)?;
                let line_end: u32 = row.get(4)?;
                let vector_blob: Vec<u8> = row.get(5)?;
                Ok((chunk_id, path, text, line_start, line_end, vector_blob))
            })
            .map_err(|e| TmError::storage(e.to_string()))?;

        let mut out = Vec::new();
        for row in rows {
            let (chunk_id, path, text, line_start, line_end, vector_blob) =
                row.map_err(|e| TmError::storage(e.to_string()))?;
            let vector = vector_from_bytes(&vector_blob);
            let score = cosine_similarity(query_vector, &vector);
            out.push(ScoredChunk {
                chunk_id,
                path,
                text,
                line_start,
                line_end,
                score,
            });
        }
        Ok(out)
    }

    /// Tokenize query text the same way [`crate::embed::LocalHashEmbedder::features`]
    /// tokenizes for the token-unigram component, so prefilter lookups hit the same
    /// vocabulary the inverted index was built with.
    fn query_tokens(query_text: &str) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut tokens = Vec::new();
        for token in query_text
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
        {
            if token.is_empty() {
                continue;
            }
            if seen.insert(token.to_string()) {
                tokens.push(token.to_string());
            }
        }
        tokens
    }
}

/// Deserialize a chunk's stored vector blob (little-endian `f32` components, as written
/// alongside the index) into a `Vec<f32>`.
fn vector_from_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Serialize a vector to the same little-endian `f32` blob layout [`vector_from_bytes`]
/// expects, for use by tests (and any writer sharing this convention).
pub(crate) fn vector_to_bytes(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for component in vector {
        out.extend_from_slice(&component.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::LocalHashEmbedder;
    use tempfile::TempDir;

    /// Inserts a file, one chunk, its vector, and its token postings directly via raw SQL,
    /// bypassing the indexer so semantic search can be tested in isolation.
    fn insert_chunk(
        conn: &Connection,
        file_path: &str,
        chunk_id: i64,
        text: &str,
        vector: &[f32],
        embedder_id: &str,
        tokens: &[(&str, u32)],
    ) {
        conn.execute(
            "INSERT INTO files (id, path, blake3, size, lang, mtime) VALUES (?1, ?2, 'h', 0, NULL, 0)",
            rusqlite::params![chunk_id, file_path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chunks (id, file_id, byte_start, byte_end, line_start, line_end, symbol, text) \
             VALUES (?1, ?1, 0, ?2, 1, 1, NULL, ?3)",
            rusqlite::params![chunk_id, text.len() as i64, text],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO vectors (chunk_id, embedder, vector) VALUES (?1, ?2, ?3)",
            rusqlite::params![chunk_id, embedder_id, vector_to_bytes(vector)],
        )
        .unwrap();
        for (token, tf) in tokens {
            conn.execute(
                "INSERT INTO tokens (token, chunk_id, term_frequency) VALUES (?1, ?2, ?3)",
                rusqlite::params![token, chunk_id, tf],
            )
            .unwrap();
        }
    }

    fn build_store() -> (TempDir, Arc<Store>) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        (dir, store)
    }

    #[test]
    fn query_tokens_lowercases_and_splits_on_non_alphanumeric_boundaries() {
        let tokens = SemanticSearch::query_tokens("Hello, World! foo-bar_baz");
        assert_eq!(tokens, vec!["hello", "world", "foo", "bar", "baz"]);
    }

    #[test]
    fn query_tokens_deduplicates_repeated_words() {
        let tokens = SemanticSearch::query_tokens("dog dog cat dog");
        assert_eq!(tokens, vec!["dog", "cat"]);
    }

    #[test]
    fn query_tokens_of_empty_text_is_empty() {
        assert!(SemanticSearch::query_tokens("").is_empty());
    }

    #[test]
    fn search_ranks_the_most_similar_chunk_first() {
        let (_dir, store) = build_store();
        let embedder: Arc<dyn Embedder> = Arc::new(LocalHashEmbedder::new());
        let vectors = embedder
            .embed(&[
                "fn parse_widget() -> Widget".to_string(),
                "totally unrelated banana recipe".to_string(),
            ])
            .unwrap();

        let conn = store.writer().unwrap();
        insert_chunk(
            &conn,
            "a.rs",
            1,
            "fn parse_widget() -> Widget",
            &vectors[0],
            embedder.identifier(),
            &[("parse", 1), ("widget", 1)],
        );
        insert_chunk(
            &conn,
            "b.rs",
            2,
            "totally unrelated banana recipe",
            &vectors[1],
            embedder.identifier(),
            &[("banana", 1), ("recipe", 1)],
        );
        conn.execute_batch("COMMIT").unwrap();

        let search = SemanticSearch::new(store, embedder);
        let results = search
            .search("parse_widget", SemanticSearchOptions::default())
            .unwrap();

        assert!(!results.is_empty());
        assert_eq!(results[0].chunk_id, 1);
        assert_eq!(results[0].path, "a.rs");
        if results.len() > 1 {
            assert!(results[0].score >= results[1].score);
        }
    }

    #[test]
    fn search_truncates_to_top_k() {
        let (_dir, store) = build_store();
        let embedder: Arc<dyn Embedder> = Arc::new(LocalHashEmbedder::new());
        let texts: Vec<String> = (0..5)
            .map(|i| format!("shared token variant number {i}"))
            .collect();
        let vectors = embedder.embed(&texts).unwrap();

        let conn = store.writer().unwrap();
        for (i, (text, vector)) in texts.iter().zip(vectors.iter()).enumerate() {
            let id = i as i64 + 1;
            insert_chunk(
                &conn,
                &format!("f{i}.rs"),
                id,
                text,
                vector,
                embedder.identifier(),
                &[("shared", 1), ("token", 1)],
            );
        }
        conn.execute_batch("COMMIT").unwrap();

        let search = SemanticSearch::new(store, embedder);
        let options = SemanticSearchOptions {
            prefilter_limit: 500,
            top_k: 2,
        };
        let results = search.search("shared token", options).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_falls_back_to_all_vectors_when_query_has_no_indexed_tokens() {
        let (_dir, store) = build_store();
        let embedder: Arc<dyn Embedder> = Arc::new(LocalHashEmbedder::new());
        let vectors = embedder
            .embed(&["some indexed content".to_string()])
            .unwrap();

        let conn = store.writer().unwrap();
        insert_chunk(
            &conn,
            "a.rs",
            1,
            "some indexed content",
            &vectors[0],
            embedder.identifier(),
            &[("some", 1), ("indexed", 1), ("content", 1)],
        );
        conn.execute_batch("COMMIT").unwrap();

        let search = SemanticSearch::new(store, embedder);
        // "???" tokenizes to no alphanumeric tokens at all, so the prefilter can't run;
        // search should still fall back to scoring every indexed vector.
        let results = search
            .search("???", SemanticSearchOptions::default())
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_id, 1);
    }

    #[test]
    fn search_returns_empty_when_the_index_has_no_vectors() {
        let (_dir, store) = build_store();
        let embedder: Arc<dyn Embedder> = Arc::new(LocalHashEmbedder::new());
        let search = SemanticSearch::new(store, embedder);
        let results = search
            .search("anything at all", SemanticSearchOptions::default())
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_ignores_vectors_from_a_different_embedder() {
        let (_dir, store) = build_store();
        let embedder: Arc<dyn Embedder> = Arc::new(LocalHashEmbedder::new());
        let vectors = embedder.embed(&["match me please".to_string()]).unwrap();

        let conn = store.writer().unwrap();
        insert_chunk(
            &conn,
            "a.rs",
            1,
            "match me please",
            &vectors[0],
            "some-other-embedder",
            &[("match", 1), ("please", 1)],
        );
        conn.execute_batch("COMMIT").unwrap();

        let search = SemanticSearch::new(store, embedder);
        let results = search
            .search("match me please", SemanticSearchOptions::default())
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn prefilter_candidates_ranks_by_distinct_token_hits_then_term_frequency() {
        let (_dir, store) = build_store();
        let embedder: Arc<dyn Embedder> = Arc::new(LocalHashEmbedder::new());
        let vectors = embedder
            .embed(&[
                "chunk with both tokens".to_string(),
                "chunk with one token".to_string(),
            ])
            .unwrap();

        let conn = store.writer().unwrap();
        insert_chunk(
            &conn,
            "both.rs",
            1,
            "chunk with both tokens",
            &vectors[0],
            embedder.identifier(),
            &[("alpha", 1), ("beta", 1)],
        );
        insert_chunk(
            &conn,
            "one.rs",
            2,
            "chunk with one token",
            &vectors[1],
            embedder.identifier(),
            &[("alpha", 5)],
        );
        conn.execute_batch("COMMIT").unwrap();

        let search = SemanticSearch::new(store, embedder);
        let ids = search
            .prefilter_candidates(&["alpha".to_string(), "beta".to_string()], 10)
            .unwrap();
        assert_eq!(ids.first(), Some(&1));
    }

    #[test]
    fn prefilter_candidates_is_bounded_by_limit() {
        let (_dir, store) = build_store();
        let embedder: Arc<dyn Embedder> = Arc::new(LocalHashEmbedder::new());
        let texts: Vec<String> = (0..5).map(|i| format!("doc number {i}")).collect();
        let vectors = embedder.embed(&texts).unwrap();

        let conn = store.writer().unwrap();
        for (i, (text, vector)) in texts.iter().zip(vectors.iter()).enumerate() {
            insert_chunk(
                &conn,
                &format!("f{i}.rs"),
                i as i64 + 1,
                text,
                vector,
                embedder.identifier(),
                &[("doc", 1)],
            );
        }
        conn.execute_batch("COMMIT").unwrap();

        let search = SemanticSearch::new(store, embedder);
        let ids = search
            .prefilter_candidates(&["doc".to_string()], 3)
            .unwrap();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn vector_round_trips_through_byte_serialization() {
        let original = vec![0.5f32, -1.25, 3.0, 0.0];
        let bytes = vector_to_bytes(&original);
        assert_eq!(vector_from_bytes(&bytes), original);
    }
}
