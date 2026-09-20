+++
[doc]
id = "wiki/architecture/tm-context"
mode = "generated"
derived_from = ["crates/tm-context/src/**"]
+++

# Architecture: tm-context

## Module tree

- `crates/tm-context/src/command.rs`
- `crates/tm-context/src/fingerprint.rs`
- `crates/tm-context/src/lib.rs`
- `crates/tm-context/src/pack.rs`
- `crates/tm-context/src/sections.rs`
- `crates/tm-context/src/tokens.rs`

## Public symbols

### `crates/tm-context/src/command.rs`

- `pub struct CommandSpec`
- `pub struct CommandResult`
- `pub enum ArtifactStream`
- `pub enum Query`
- `pub enum QueryAnswer`
- `pub trait CommandCache`
- `pub trait CommandExecutor`
- `pub struct ExecutionOutcome`
- `pub fn run(
    cmd: &CommandSpec,
    key: &str,
    cache: &dyn CommandCache,
    auth: &Authority,
    executor: &dyn CommandExecutor,
    clock: &dyn Clock,
    actor: &ParticipantId,
) -> Result<(CommandResult, Vec<EventDraft>)>`
- `impl CommandResult`
  - `pub fn query(
        &self,
        stream: ArtifactStream,
        cache: &dyn CommandCache,
        query: Query,
    ) -> Result<QueryAnswer>`

### `crates/tm-context/src/fingerprint.rs`

- `pub struct RepoFingerprint`
- `impl RepoFingerprint`
  - `pub fn digest(&self) -> String`
- `pub struct EnvAllowlist`
- `impl EnvAllowlist`
  - `pub fn new(names: impl IntoIterator<Item = String>) -> Self`
  - `pub fn snapshot(&self, lookup: &dyn Fn(&str) -> Option<String>) -> BTreeMap<String, String>`
- `pub struct DeclaredInput`
- `pub struct CacheKeyInputs`
- `pub fn cache_key(inputs: &CacheKeyInputs) -> String`
- `pub trait GitInspector`
- `pub fn repo_fingerprint(
    project_root: &Path,
    git: &dyn GitInspector,
    hash_file: &dyn Fn(&Path, &str) -> Result<String>,
) -> Result<RepoFingerprint>`

### `crates/tm-context/src/lib.rs`

- `pub mod command;`
- `pub mod fingerprint;`
- `pub mod pack;`
- `pub mod sections;`
- `pub mod tokens;`

### `crates/tm-context/src/pack.rs`

- `pub struct ProvenanceRef`
- `pub struct Section`
- `pub struct DroppedSection`
- `pub struct ContextPack`
- `pub struct ToolSurfaceCost`
- `impl ToolSurfaceCost`
  - `pub fn compute(name: &str, description: &str, input_schema: &serde_json::Value) -> Self`
- `impl ContextPack`
  - `pub fn rent_report(&self, tool_surface: &[ToolSurfaceCost]) -> String`
- `pub fn compile(
    ticket: &Ticket,
    view: &ProjectView,
    ci: &CodeIntel,
    budget: TokenBudget,
    weights: SignalWeights,
    conventions: &[String],
    roles: &RoleTable,
) -> Result<ContextPack>`

### `crates/tm-context/src/sections.rs`

- `pub struct RawSection`
- `pub fn claimed_paths(ticket: &Ticket) -> Vec<String>`
- `pub fn build_objective(ticket: &Ticket) -> RawSection`
- `pub fn build_budget(ticket: &Ticket, roles: &RoleTable) -> RawSection`
- `pub fn build_decisions(ticket: &Ticket, view: &ProjectView) -> RawSection`
- `pub fn build_dependencies(ticket: &Ticket, view: &ProjectView) -> RawSection`
- `pub fn is_wiki_path(path: &str) -> bool`
- `pub fn build_retrieval(
    ticket: &Ticket,
    ci: &CodeIntel,
    weights: SignalWeights,
) -> Result<RawSection>`
- `pub fn build_wiki(ticket: &Ticket, ci: &CodeIntel, weights: SignalWeights) -> Result<RawSection>`
- `pub fn build_symbol_outlines(ticket: &Ticket, ci: &CodeIntel) -> Result<RawSection>`
- `pub fn build_git_history(ticket: &Ticket, ci: &CodeIntel) -> Result<RawSection>`
- `pub fn build_prior_failures(ticket: &Ticket) -> RawSection`
- `pub fn build_conventions(conventions: &[String]) -> RawSection`

### `crates/tm-context/src/tokens.rs`

- `pub enum SectionKind`
- `impl SectionKind`
  - `pub const PRIORITY_ORDER: &'static [SectionKind] = &[
        SectionKind::Objective,
        SectionKind::Budget,
        SectionKind::Decisions,
        SectionKind::Dependencies,
        SectionKind::Retrieval,
        SectionKind::Wiki,
        SectionKind::SymbolOutlines,
        SectionKind::GitHistory,
        SectionKind::PriorFailures,
        SectionKind::Conventions,
    ];`
  - `pub fn rank(self) -> u8`
- `pub fn estimate_tokens_source(text: &str) -> usize`
- `pub fn estimate_tokens_prose(text: &str) -> usize`
- `pub struct TokenBudget`
- `impl TokenBudget`
  - `pub fn even(total: usize) -> Self`
  - `pub fn share_tokens(&self, kind: SectionKind) -> usize`
- `pub struct SectionAccount`
- `impl SectionAccount`
  - `pub fn remaining(&self) -> usize`
  - `pub fn try_spend(&mut self, tokens: usize) -> bool`
- `pub struct BudgetLedger`
- `impl BudgetLedger`
  - `pub fn new(budget: &TokenBudget) -> Self`
  - `pub fn spend(&mut self, kind: SectionKind, tokens: usize) -> bool`
  - `pub fn total_used(&self) -> usize`
  - `pub fn total_remaining(&self) -> usize`
