+++
[doc]
id = "wiki/architecture/tm-docs"
mode = "generated"
derived_from = ["crates/tm-docs/src/**"]
+++

# Architecture: tm-docs

## Module tree

- `crates/tm-docs/src/assess.rs`
- `crates/tm-docs/src/lib.rs`
- `crates/tm-docs/src/provenance.rs`
- `crates/tm-docs/src/reconcile.rs`
- `crates/tm-docs/src/registry.rs`

## Public symbols

### `crates/tm-docs/src/assess.rs`

- `pub struct Dismissal`
- `pub struct DocAssessment`
- `pub struct Assessor`
- `impl Assessor`
  - `pub fn new(registry: DocRegistry, provenance: ProvenanceIndex) -> Self`
  - `pub fn registry(&self) -> &DocRegistry`
  - `pub fn registry_mut(&mut self) -> &mut DocRegistry`
  - `pub fn dismiss(&mut self, dismissal: Dismissal)`
  - `pub fn clear_dismissal(&mut self, doc_id: &str) -> Option<Dismissal>`
  - `pub fn assess(&mut self, changes: &ChangeSet) -> Vec<DocAssessment>`
  - `pub fn invalidation_events(
        assessments: &[DocAssessment],
        registry: &DocRegistry,
    ) -> Vec<tm_events::payload::DocInvalidatedPayload>`
  - `pub fn check(&self) -> tm_types::Result<()>`

### `crates/tm-docs/src/lib.rs`

- `pub mod registry;`
- `pub mod provenance;`
- `pub mod assess;`
- `pub mod reconcile;`

### `crates/tm-docs/src/provenance.rs`

- `pub struct CompiledProvenance`
- `impl CompiledProvenance`
  - `pub fn is_touched_by(&self, changed_paths: &[String], superseded: &[DecisionId]) -> bool`
- `pub fn compile(doc: &DocRecord) -> tm_types::Result<CompiledProvenance>`
- `pub struct ChangeSet`
- `impl ChangeSet`
  - `pub fn empty() -> Self`
  - `pub fn from_file_records(records: &[tm_codeintel::FileRecord]) -> Self`
- `pub struct ProvenanceIndex`
- `impl ProvenanceIndex`
  - `pub fn build(docs: &[DocRecord]) -> tm_types::Result<Self>`
  - `pub fn docs_touched_by_paths(&self, changed_paths: &[String]) -> BTreeSet<String>`
  - `pub fn docs_touched_by_decisions(&self, superseded: &[DecisionId]) -> BTreeSet<String>`
  - `pub fn docs_touched(&self, changes: &ChangeSet) -> BTreeSet<String>`

### `crates/tm-docs/src/reconcile.rs`

- `pub enum ReconciliationKind`
- `impl ReconciliationKind`
  - `pub fn for_mode(mode: DocMode) -> Self`
- `pub struct ReconciliationTicket`
- `pub struct Attestation`
- `pub enum ReconcileError`
- `pub fn open_reconciliation(
    doc: &mut DocRecord,
    ids: &dyn IdSource,
    clock: &dyn Clock,
) -> tm_types::Result<ReconciliationTicket>`
- `pub fn accept_attestation(doc: &mut DocRecord, attestation: &Attestation) -> tm_types::Result<()>`
- `pub fn apply_regeneration(
    doc: &mut DocRecord,
    content_hash: &str,
    ts: Timestamp,
) -> tm_types::Result<()>`

### `crates/tm-docs/src/registry.rs`

- `pub const FRONT_MATTER_DELIMITERS: [&str; 2] = ["+++", "---"];`
- `pub const TMDOCS_TOML: &str = ".tmdocs.toml";`
- `pub enum DocMode`
- `pub enum DocState`
- `pub struct DocFrontMatter`
- `pub struct DocTomlEntry`
- `pub struct TmDocsToml`
- `pub fn parse_front_matter(markdown: &str) -> tm_types::Result<Option<DocFrontMatter>>`
- `pub fn parse_tmdocs_toml(contents: &str) -> tm_types::Result<TmDocsToml>`
- `pub fn resolve_front_matter(
    path: &str,
    inline: Option<DocFrontMatter>,
    fallback: Option<&DocTomlEntry>,
) -> tm_types::Result<DocFrontMatter>`
- `pub struct DocRecord`
- `impl DocRecord`
  - `pub fn new(id: String, path: String, mode: DocMode, derived_from: Vec<String>) -> Self`
  - `pub fn is_human(&self) -> bool`
  - `pub fn is_generated(&self) -> bool`
- `pub struct DocRegistry`
- `impl DocRegistry`
  - `pub fn new() -> Self`
  - `pub fn insert(&mut self, record: DocRecord) -> Option<DocRecord>`
  - `pub fn get(&self, id: &str) -> Option<&DocRecord>`
  - `pub fn get_mut(&mut self, id: &str) -> Option<&mut DocRecord>`
  - `pub fn list(&self) -> Vec<&DocRecord>`
  - `pub fn len(&self) -> usize`
  - `pub fn is_empty(&self) -> bool`
- `pub fn registration_events(
    existing: &DocRegistry,
    discovered: &[DocRecord],
) -> Vec<tm_events::payload::DocRegisteredPayload>`
