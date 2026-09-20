+++
[doc]
id = "wiki/architecture/tm-codeintel"
mode = "generated"
derived_from = ["crates/tm-codeintel/src/**"]
+++

# Architecture: tm-codeintel

## Module tree

- `crates/tm-codeintel/src/api.rs`
- `crates/tm-codeintel/src/chunk.rs`
- `crates/tm-codeintel/src/embed.rs`
- `crates/tm-codeintel/src/exact.rs`
- `crates/tm-codeintel/src/history.rs`
- `crates/tm-codeintel/src/hybrid.rs`
- `crates/tm-codeintel/src/lib.rs`
- `crates/tm-codeintel/src/semantic.rs`
- `crates/tm-codeintel/src/store.rs`
- `crates/tm-codeintel/src/symbols.rs`
- `crates/tm-codeintel/src/walk.rs`

## Public symbols

### `crates/tm-codeintel/src/api.rs`

- `pub struct CodeIntel`
- `impl CodeIntel`
  - `pub fn open(project_root: &Path) -> Result<CodeIntel>`
  - `pub fn open_with_embedder(
        project_root: &Path,
        embedder: Arc<dyn Embedder>,
    ) -> Result<CodeIntel>`
  - `pub fn open_at(index_dir: &Path, workspace_root: &Path) -> Result<CodeIntel>`
  - `pub fn open_at_with_embedder(
        index_dir: &Path,
        workspace_root: &Path,
        embedder: Arc<dyn Embedder>,
    ) -> Result<CodeIntel>`
  - `pub fn project_root(&self) -> &Path`
  - `pub fn update_incremental(&self, clock: &dyn Clock) -> Result<IndexDelta>`
  - `pub fn search_exact(&self, needle: &str) -> Result<ExactSearchResult>`
  - `pub fn search_regex(&self, pattern: &str) -> Result<ExactSearchResult>`
  - `pub fn search_semantic(
        &self,
        query_text: &str,
        options: SemanticSearchOptions,
    ) -> Result<Vec<ScoredChunk>>`
  - `pub fn search_hybrid(
        &self,
        query: &Query,
        ctx: &RetrievalContext,
        weights: SignalWeights,
    ) -> Result<Vec<RankedHit>>`
  - `pub fn symbol_index(&self) -> Result<SymbolIndex>`
  - `pub fn resolve_symbol(&self, name: &str, from_path: &str) -> Result<Resolution>`
  - `pub fn definition(&self, name: &str, from_path: &str) -> Result<Option<Symbol>>`
  - `pub fn outline(&self, path: &str) -> Result<Vec<crate::symbols::OutlineEntry>>`
  - `pub fn history_why(
        &self,
        path: &str,
        line_start: u32,
        line_end: u32,
    ) -> Result<crate::history::WhyAnswer>`
  - `pub fn history_search(&self, query: &str) -> Result<Vec<crate::history::HistoryHit>>`
  - `pub fn history_deleted(
        &self,
        query: &str,
    ) -> Result<Vec<crate::history::DeletedImplementation>>`

### `crates/tm-codeintel/src/chunk.rs`

- `pub const TARGET_CHUNK_TOKENS: usize = 400;`
- `pub const OVERLAP_FRACTION: f32 = 0.15;`
- `pub struct ChunkRange`
- `pub struct Chunk`
- `pub struct SyntaxBoundary`
- `pub struct Chunker`
- `impl Chunker`
  - `pub fn new() -> Self`
  - `pub fn with_sizing(target_tokens: usize, overlap_fraction: f32) -> Self`
  - `pub fn chunk(
        &self,
        path: &str,
        text: &str,
        lang: Language,
        boundaries: &[SyntaxBoundary],
    ) -> Vec<Chunk>`

### `crates/tm-codeintel/src/embed.rs`

- `pub const LOCAL_HASH_DIMS: usize = 512;`
- `pub trait Embedder: Send + Sync`
- `pub struct LocalHashEmbedder`
- `impl LocalHashEmbedder`
  - `pub fn new() -> Self`
  - `pub fn fit_idf(&mut self, corpus: &[String])`
- `pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32`

### `crates/tm-codeintel/src/exact.rs`

- `pub struct Hit`
- `pub const DEFAULT_HIT_CAP: usize = 1000;`
- `pub struct ExactSearchResult`
- `pub struct ExactSearch`
- `impl ExactSearch`
  - `pub fn new(root: impl Into<std::path::PathBuf>) -> Self`
  - `pub fn with_hit_cap(root: impl Into<std::path::PathBuf>, hit_cap: usize) -> Self`
  - `pub fn literal(&self, needle: &str) -> Result<ExactSearchResult>`
  - `pub fn regex(&self, pattern: &str) -> Result<ExactSearchResult>`

### `crates/tm-codeintel/src/history.rs`

- `pub struct CommitSummary`
- `pub struct WhyAnswer`
- `pub struct HistoryHit`
- `pub struct DeletedImplementation`
- `pub struct CoChange`
- `pub struct HistoryIndex`
- `impl HistoryIndex`
  - `pub fn new(store: Arc<Store>, repo_root: impl Into<std::path::PathBuf>) -> Self`
  - `pub fn ingest_incremental(&self, clock: &dyn Clock) -> Result<u64>`
  - `pub fn why(&self, path: &str, line_start: u32, line_end: u32) -> Result<WhyAnswer>`
  - `pub fn search(&self, query: &str) -> Result<Vec<HistoryHit>>`
  - `pub fn deleted(&self, query: &str) -> Result<Vec<DeletedImplementation>>`
  - `pub fn co_change(&self, path: &str) -> Result<Vec<CoChange>>`

### `crates/tm-codeintel/src/hybrid.rs`

- `pub struct Query`
- `pub struct RetrievalContext`
- `pub struct SignalContribution`
- `pub enum Signal`
- `pub struct SignalWeights`
- `pub struct RankedHit`
- `pub type SnippetLookup<'a> = dyn Fn(&str, Option<u32>) -> (String, Option<u32>) + 'a;`
- `pub struct SignalRanking`
- `pub fn hybrid(
    _query: &Query,
    _ctx: &RetrievalContext,
    signals: &[SignalRanking],
    weights: SignalWeights,
    snippet_lookup: &SnippetLookup<'_>,
) -> Vec<RankedHit>`
- `pub fn semantic_ranking(chunks: &[ScoredChunk]) -> SignalRanking`
- `pub fn lexical_ranking(hits: &[Hit]) -> SignalRanking`

### `crates/tm-codeintel/src/lib.rs`

- `pub mod walk;`
- `pub mod store;`
- `pub mod embed;`
- `pub mod chunk;`
- `pub mod symbols;`
- `pub mod exact;`
- `pub mod semantic;`
- `pub mod history;`
- `pub mod hybrid;`
- `pub mod api;`

### `crates/tm-codeintel/src/semantic.rs`

- `pub struct ScoredChunk`
- `pub struct SemanticSearchOptions`
- `pub struct SemanticSearch`
- `impl SemanticSearch`
  - `pub fn new(store: Arc<Store>, embedder: Arc<dyn Embedder>) -> Self`
  - `pub fn search(
        &self,
        query_text: &str,
        options: SemanticSearchOptions,
    ) -> Result<Vec<ScoredChunk>>`
- `pub(crate) fn vector_to_bytes(vector: &[f32]) -> Vec<u8>`

### `crates/tm-codeintel/src/store.rs`

- `pub const SCHEMA_VERSION: i64 = 1;`
- `pub const INDEX_DB_RELATIVE_PATH: &str = ".tm/index.db";`
- `pub struct FileRow`
- `pub struct ChunkRow`
- `pub struct VectorRow`
- `pub struct TokenPosting`
- `pub struct SymbolRow`
- `pub struct RefRow`
- `pub struct CommitRow`
- `pub struct CommitFileRow`
- `pub struct DocMeta`
- `pub struct IndexDelta`
- `pub struct Store`
- `pub(crate) fn language_to_str(lang: Option<Language>) -> Option<String>`
- `impl Store`
  - `pub fn open(project_root: &Path) -> Result<Store>`
  - `pub fn open_at(index_dir: &Path) -> Result<Store>`
  - `pub fn db_path(&self) -> &Path`
  - `pub fn reader(&self) -> Result<Connection>`
  - `pub fn writer(&self) -> Result<Connection>`
  - `pub fn list_files(&self) -> Result<Vec<FileRow>>`
  - `pub fn file_by_path(&self, path: &str) -> Result<Option<FileRow>>`
  - `pub fn get_meta(&self, key: &str) -> Result<Option<String>>`
  - `pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()>`
  - `pub const BUSY_TIMEOUT_MS: u32 = 5_000;`

### `crates/tm-codeintel/src/symbols.rs`

- `pub enum SymbolKind`
- `pub struct Range`
- `pub struct Symbol`
- `pub struct Reference`
- `pub enum ResolutionConfidence`
- `pub struct Resolution`
- `pub struct OutlineEntry`
- `pub struct PatchEdit`
- `pub struct RenamePatch`
- `pub struct SymbolIndex`
- `impl SymbolIndex`
  - `pub fn new() -> Self`
  - `pub fn parse_file(&mut self, path: &str, text: &str, lang: Language) -> Result<()>`
  - `pub fn candidates(&self, name: &str, from_path: &str) -> Vec<&Symbol>`
  - `pub fn resolve(&self, reference: &Reference) -> Resolution`
  - `pub fn definition(&self, name: &str, from_path: &str) -> Option<&Symbol>`
  - `pub fn references(&self, symbol_id: u64) -> Vec<&Reference>`
  - `pub fn implementations(&self, interface_name: &str) -> Vec<&Symbol>`
  - `pub fn callers(&self, symbol_id: u64) -> Vec<&Symbol>`
  - `pub fn callees(&self, symbol_id: u64) -> Vec<&Symbol>`
  - `pub fn type_of(&self, path: &str, byte_offset: usize) -> Option<&Symbol>`
  - `pub fn outline(&self, path: &str) -> Vec<OutlineEntry>`
  - `pub fn rename_preview(&self, symbol_id: u64, new_name: &str) -> Result<RenamePatch>`

### `crates/tm-codeintel/src/walk.rs`

- `pub struct FileRecord`
- `pub enum Language`
- `impl Language`
  - `pub fn from_extension(ext: &str) -> Option<Language>`
  - `pub fn has_grammar(self) -> bool`
- `pub const MAX_INDEXABLE_BYTES: u64 = 2 * 1024 * 1024;`
- `pub const BINARY_SNIFF_BYTES: usize = 8192;`
- `pub struct RepoWalker`
- `impl RepoWalker`
  - `pub fn new(root: impl Into<PathBuf>) -> Self`
  - `pub fn root(&self) -> &Path`
  - `pub fn walk(&self) -> Result<Vec<FileRecord>>`
  - `pub fn diff(current: &[FileRecord], previous: &[FileRecord]) -> ChangeSet`
- `pub struct ChangeSet`
- `impl ChangeSet`
  - `pub fn is_empty(&self) -> bool`
