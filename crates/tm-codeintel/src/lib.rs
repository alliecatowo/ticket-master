//! Code intelligence: the reason agents stop rediscovering the codebase.
//!
//! `tm-codeintel` maintains a local index of a project's working tree, stored under that
//! project's state directory (`<project>/.tm/index.db` in repo scope,
//! `$TM_HOME/projects/<key>/index.db` in global scope -- see D-003) and fuses four retrieval
//! modes over it:
//!
//! - **exact** ([`exact`]) — literal and regex search over the walked file set.
//! - **semantic** ([`semantic`]) — embedding-vector search prefiltered by an inverted token
//!   index, using the deterministic, no-network [`embed::LocalHashEmbedder`] by default.
//! - **symbols** ([`symbols`]) — tree-sitter-derived definitions, references, and resolution.
//! - **history** ([`history`]) — git commits, blame, and message/diff search via `git2`.
//!
//! [`hybrid`] fuses these signals with weighted reciprocal-rank fusion and an explainable
//! per-hit contribution breakdown. [`api`] is the facade the rest of Ticketmaster calls:
//! it owns the single writer lock and the shared read pool over `index.db`.
//!
//! The index is local-first and cheap: SQLite in WAL mode, `BEGIN IMMEDIATE` for the single
//! writer, dozens of concurrent readers. Indexing is incremental — a file's blake3 hash
//! decides whether it needs re-chunking, re-embedding and re-extracting symbols; unchanged
//! files are left untouched. See `SPEC.md` §7.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Repository walking, language detection and change detection.
pub mod walk;

/// `index.db` schema, migrations and typed row accessors.
pub mod store;

/// The [`embed::Embedder`] trait, the default local hash embedder, and embedding cache.
pub mod embed;

/// Syntax-aware and sliding-window chunking of file contents.
pub mod chunk;

/// Tree-sitter symbol and reference extraction, resolution, and outlines.
pub mod symbols;

/// Streaming literal and regex search.
pub mod exact;

/// Vector search over the chunk index.
pub mod semantic;

/// Git history ingestion and history-aware queries.
pub mod history;

/// Weighted reciprocal-rank fusion across all retrieval signals.
pub mod hybrid;

/// The [`api::CodeIntel`] facade used by the rest of Ticketmaster.
pub mod api;

pub use api::CodeIntel;
pub use chunk::{Chunk, ChunkRange, Chunker};
pub use embed::{cosine_similarity, Embedder, LocalHashEmbedder};
pub use exact::{ExactSearch, Hit};
pub use history::{CoChange, HistoryIndex, WhyAnswer};
pub use hybrid::{Query, RankedHit, RetrievalContext, SignalContribution, SignalWeights};
pub use semantic::{ScoredChunk, SemanticSearch};
pub use store::{IndexDelta, Store};
pub use symbols::{Reference, Symbol, SymbolIndex};
pub use walk::{FileRecord, RepoWalker};
