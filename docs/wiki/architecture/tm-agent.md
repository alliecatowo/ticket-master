+++
[doc]
id = "wiki/architecture/tm-agent"
mode = "generated"
derived_from = ["crates/tm-agent/src/**"]
+++

# Architecture: tm-agent

## Module tree

- `crates/tm-agent/src/agent_loop.rs`
- `crates/tm-agent/src/executor.rs`
- `crates/tm-agent/src/lib.rs`
- `crates/tm-agent/src/outcome.rs`
- `crates/tm-agent/src/patch.rs`
- `crates/tm-agent/src/prompt.rs`
- `crates/tm-agent/src/pruning.rs`
- `crates/tm-agent/src/session.rs`
- `crates/tm-agent/src/tools.rs`

## Public symbols

### `crates/tm-agent/src/agent_loop.rs`

- `pub const DEFAULT_MAX_STEPS: u32 = 64;`
- `pub const DEFAULT_MAX_EVENTS_PER_TICKET: u32 = 5000;`
- `pub struct PromptCacheState`
- `impl PromptCacheState`
  - `pub fn new() -> Self`
- `pub struct AgentLoop`
- `impl AgentLoop`
  - `pub fn new(
        fabric: Arc<Fabric>,
        tools: ToolRegistry,
        authority: Authority,
        budget: Budget,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
        role: Role,
        actor: ParticipantId,
        store: Arc<Store>,
    ) -> Self`
  - `pub fn with_max_steps(mut self, max_steps: u32) -> Self`
  - `pub fn with_max_events_per_ticket(mut self, max_events_per_ticket: u32) -> Self`
  - `pub fn authority(&self) -> &Authority`
  - `pub fn budget(&self) -> &Budget`
  - `pub fn tools(&self) -> &ToolRegistry`
  - `pub async fn run(&mut self, task: AgentTask) -> Result<AgentOutcome>`
  - `pub async fn resume(
        &mut self,
        task: AgentTask,
        steps_so_far: Vec<crate::outcome::StepRecord>,
        pending: crate::outcome::PendingApproval,
        approved: bool,
    ) -> Result<AgentOutcome>`
  - `pub fn cache_state(&self) -> &PromptCacheState`
- `pub(crate) fn rebuild_messages(task_prompt: &str, steps: &[StepRecord]) -> Vec<Message>`
- `pub(crate) fn tool_result_text(resolution: &ToolOutcome) -> (String, bool)`
- `pub fn first_exhausted_dimension(budget: &Budget) -> Option<BudgetDimension>`
- `pub fn first_unaffordable_dimension(budget: &Budget, estimated: Spend) -> Option<BudgetDimension>`

### `crates/tm-agent/src/executor.rs`

- `pub struct StoreArtifactSink`
- `impl StoreArtifactSink`
  - `pub fn new(store: Arc<Store>) -> Self`
- `pub struct BrowserWiring`
- `pub struct ComputerWiring`
- `pub struct BuiltinExecutor`
- `impl BuiltinExecutor`
  - `pub fn new(
        id: impl Into<String>,
        fabric: Arc<Fabric>,
        ci: Arc<CodeIntel>,
        store: Arc<Store>,
        command_cache: Arc<dyn CommandCache + Send + Sync>,
        command_executor: Arc<dyn CommandExecutor + Send + Sync>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
        browser: Option<BrowserWiring>,
        computer: ComputerWiring,
    ) -> Self`
- `pub struct HumanDecision`
- `pub trait HumanApprovalSink: Send + Sync`
- `pub struct HumanExecutor`
- `impl HumanExecutor`
  - `pub fn new(id: impl Into<String>, sink: Arc<dyn HumanApprovalSink>) -> Self`

### `crates/tm-agent/src/lib.rs`

- `pub mod agent_loop;`
- `pub mod executor;`
- `pub mod outcome;`
- `pub mod patch;`
- `pub mod prompt;`
- `pub mod session;`
- `pub mod tools;`

### `crates/tm-agent/src/outcome.rs`

- `pub struct AgentTask`
- `pub enum AgentOutcome`
- `impl AgentOutcome`
  - `pub fn is_terminal(&self) -> bool`
  - `pub fn steps(&self) -> &[StepRecord]`
  - `pub fn bytes_pruned(&self) -> u64`
  - `pub fn tokens_pruned(&self) -> u64`
- `pub enum BudgetDimension`
- `pub struct PendingApproval`
- `pub struct StepRecord`
- `pub struct ToolCallRecord`
- `pub enum ToolCallResolution`
- `pub struct EvidenceBundle`

### `crates/tm-agent/src/patch.rs`

- `pub enum Edit`
- `impl Edit`
  - `pub fn path(&self) -> &str`
- `pub struct DiffSummary`
- `pub struct Patch`
- `pub enum PatchError`
- `pub type PatchOutcome = std::result::Result<Patch, PatchError>;`
- `pub struct PatchEngine`
- `impl PatchEngine`
  - `pub fn new(root: PathBuf, authority: Authority) -> Self`
  - `pub fn root(&self) -> &Path`
  - `pub fn apply(&self, edit: &Edit) -> PatchOutcome`
  - `pub fn in_write_scope(&self, path: &str) -> Result<bool>`

### `crates/tm-agent/src/prompt.rs`

- `pub struct RenderedPrompt`
- `pub fn render_system_prompt(fragments: &PromptFragments) -> String`
- `pub fn render_task_prompt(ticket: &TicketId, pack: &ContextPack) -> String`
- `pub fn render(
    ticket: &TicketId,
    pack: &ContextPack,
    fragments: &PromptFragments,
) -> RenderedPrompt`

### `crates/tm-agent/src/pruning.rs`

- `pub(crate) enum ToolCallState`
- `pub(crate) struct StepRef<'a>`
- `pub(crate) struct WorkingSet<'a>`
- `pub(crate) fn working_set(steps: &[StepRecord]) -> WorkingSet<'_>`

### `crates/tm-agent/src/session.rs`

- `pub struct Session`
- `impl Session`
  - `pub fn new(id: SessionId, ticket: TicketId, harness_epoch: u64, started_at: Timestamp) -> Self`
  - `pub fn record_step(&mut self, step: StepRecord)`
  - `pub fn last_step(&self) -> Option<&StepRecord>`
  - `pub fn promote(&self) -> DurablePromotion`
- `pub struct DurablePromotion`
- `impl DurablePromotion`
  - `pub fn is_empty(&self) -> bool`

### `crates/tm-agent/src/tools.rs`

- `pub const MAX_INLINE_RESULT_BYTES: usize = 8 * 1024;`
- `pub(crate) enum ToolName`
- `impl ToolName`
  - `pub(crate) const ALL: &'static [ToolName] = &[
        ToolName::SearchSemantic,
        ToolName::SearchExact,
        ToolName::SearchRegex,
        ToolName::SearchHybrid,
        ToolName::SymbolDefinition,
        ToolName::SymbolReferences,
        ToolName::SymbolCallers,
        ToolName::SymbolCallees,
        ToolName::SymbolOutline,
        ToolName::SymbolRenamePreview,
        ToolName::HistoryWhy,
        ToolName::HistorySearch,
        ToolName::HistoryDeleted,
        ToolName::FsRead,
        ToolName::FsReadRange,
        ToolName::FsList,
        ToolName::FsStat,
        ToolName::EditApplyPatch,
        ToolName::EditWriteFile,
        ToolName::EditCreateFile,
        ToolName::EditDeleteFile,
        ToolName::ShellRun,
        ToolName::ShellQueryOutput,
        ToolName::GitStatus,
        ToolName::GitDiff,
        ToolName::GitLog,
        ToolName::GitCommit,
        ToolName::GitBranch,
        ToolName::GitWorktree,
        ToolName::TestRun,
        ToolName::BuildRun,
        ToolName::TicketCreateChild,
        ToolName::TicketDelegate,
        ToolName::TicketSubmit,
        ToolName::TicketComment,
        ToolName::DecisionRecord,
        ToolName::ArtifactStore,
        ToolName::EvidenceAttach,
        ToolName::AskHuman,
    ];`
  - `pub(crate) fn as_str(self) -> &'static str`
  - `pub(crate) fn parse(name: &str) -> Option<ToolName>`
- `pub struct BuiltinCapability`
- `impl BuiltinCapability`
  - `pub fn new(
        ci: Arc<CodeIntel>,
        store: Arc<Store>,
        command_cache: Arc<dyn CommandCache + Send + Sync>,
        command_executor: Arc<dyn CommandExecutor + Send + Sync>,
    ) -> Self`
- `pub struct ToolRegistry`
- `impl ToolRegistry`
  - `pub fn new(providers: Vec<Arc<dyn CapabilityProvider>>, store: Arc<Store>) -> Self`
  - `pub fn standard(
        ci: Arc<CodeIntel>,
        store: Arc<Store>,
        command_cache: Arc<dyn CommandCache + Send + Sync>,
        command_executor: Arc<dyn CommandExecutor + Send + Sync>,
    ) -> Self`
  - `pub fn with_capabilities(
        ci: Arc<CodeIntel>,
        store: Arc<Store>,
        command_cache: Arc<dyn CommandCache + Send + Sync>,
        command_executor: Arc<dyn CommandExecutor + Send + Sync>,
        extra: Vec<Arc<dyn CapabilityProvider>>,
    ) -> Self`
  - `pub fn get(&self, name: &str) -> Option<&ToolSchema>`
  - `pub fn specs(&self) -> impl Iterator<Item = &ToolSchema>`
  - `pub fn tool_defs(&self) -> Vec<tm_provider::ToolDef>`
  - `pub fn tool_defs_for(&self, authority: &Authority) -> Vec<tm_provider::ToolDef>`
  - `pub fn tool_surface_cost_for(&self, authority: &Authority) -> Vec<tm_context::ToolSurfaceCost>`
  - `pub fn to_action(&self, name: &str, input: &Value) -> Result<Action>`
  - `pub async fn dispatch(&self, call: &ToolCall, ctx: &CallContext<'_>) -> ToolOutcome`
- `pub struct ToolCall`
- `pub type ToolOutcome = crate::outcome::ToolCallResolution;`
