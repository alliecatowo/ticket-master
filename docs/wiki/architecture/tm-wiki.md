+++
[doc]
id = "wiki/architecture/tm-wiki"
mode = "generated"
derived_from = ["crates/tm-wiki/src/**"]
+++

# Architecture: tm-wiki

## Module tree

- `crates/tm-wiki/src/architecture.rs`
- `crates/tm-wiki/src/decisions.rs`
- `crates/tm-wiki/src/generate.rs`
- `crates/tm-wiki/src/glossary.rs`
- `crates/tm-wiki/src/history.rs`
- `crates/tm-wiki/src/lib.rs`
- `crates/tm-wiki/src/page.rs`
- `crates/tm-wiki/src/tickets.rs`

## Public symbols

### `crates/tm-wiki/src/architecture.rs`

- `pub fn pages(project_root: &Path, ci: &CodeIntel) -> Result<Vec<WikiPage>>`

### `crates/tm-wiki/src/decisions.rs`

- `pub fn pages(view: &ProjectView) -> Vec<WikiPage>`

### `crates/tm-wiki/src/generate.rs`

- `pub struct PageOutcome`
- `pub struct GenerationReport`
- `impl GenerationReport`
  - `pub fn written(&self) -> impl Iterator<Item = &PageOutcome>`
  - `pub fn skipped(&self) -> impl Iterator<Item = &PageOutcome>`
- `pub fn run(
    project_root: &Path,
    store: &Store,
    ci: &CodeIntel,
    clock: &dyn Clock,
    actor: ParticipantId,
    history_paths: &[String],
) -> Result<GenerationReport>`
- `pub fn dry_run(
    project_root: &Path,
    store: &Store,
    ci: &CodeIntel,
    history_paths: &[String],
) -> Result<GenerationReport>`
- `pub fn default_history_paths(view: &ProjectView, project_root: &Path) -> Vec<String>`

### `crates/tm-wiki/src/glossary.rs`

- `pub fn page(view: &ProjectView) -> WikiPage`

### `crates/tm-wiki/src/history.rs`

- `pub fn pages(project_root: &Path, ci: &CodeIntel, paths: &[String]) -> Result<Vec<WikiPage>>`

### `crates/tm-wiki/src/lib.rs`

- `pub mod architecture;`
- `pub mod decisions;`
- `pub mod generate;`
- `pub mod glossary;`
- `pub mod history;`
- `pub mod page;`
- `pub mod tickets;`

### `crates/tm-wiki/src/page.rs`

- `pub const WIKI_DIR: &str = "docs/wiki";`
- `pub struct WikiPage`
- `impl WikiPage`
  - `pub fn new(
        id: impl Into<String>,
        rel_path: impl Into<String>,
        body: impl Into<String>,
        derived_from: Vec<String>,
    ) -> Self`
  - `pub fn project_path(&self) -> String`
  - `pub fn render(&self) -> String`
- `pub enum WriteOutcome`
- `pub fn write_page(project_root: &Path, page: &WikiPage, clock: &dyn Clock) -> Result<WriteOutcome>`
- `pub fn preview_write(project_root: &Path, page: &WikiPage) -> Result<WriteOutcome>`

### `crates/tm-wiki/src/tickets.rs`

- `pub fn page(view: &ProjectView) -> WikiPage`
