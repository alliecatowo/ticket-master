+++
[doc]
id = "wiki/architecture/tm-cli"
mode = "generated"
derived_from = ["crates/tm-cli/src/**"]
+++

# Architecture: tm-cli

## Module tree

- `crates/tm-cli/src/agent.rs`
- `crates/tm-cli/src/args.rs`
- `crates/tm-cli/src/auth.rs`
- `crates/tm-cli/src/dispatch.rs`
- `crates/tm-cli/src/drive.rs`
- `crates/tm-cli/src/lib.rs`
- `crates/tm-cli/src/main.rs`
- `crates/tm-cli/src/ops.rs`
- `crates/tm-cli/src/project.rs`
- `crates/tm-cli/src/render.rs`
- `crates/tm-cli/src/sched.rs`
- `crates/tm-cli/src/search.rs`
- `crates/tm-cli/src/serve.rs`
- `crates/tm-cli/src/tickets.rs`
- `crates/tm-cli/src/tui.rs`
- `crates/tm-cli/src/wiki.rs`
- `crates/tm-cli/src/workflow.rs`

## Public symbols

### `crates/tm-cli/src/agent.rs`

- `pub(crate) const AGENT_MODEL: &str = "claude-sonnet-5";`
- `pub(crate) enum TurnEvent`
- `pub struct AgentSession`
- `impl AgentSession`
  - `pub fn new(project: Arc<Project>, renderer: Renderer) -> Self`
  - `pub fn attach_ticket(&mut self, ticket: tm_types::TicketId) -> tm_types::Result<()>`
  - `pub fn session_id(&self) -> &SessionId`
  - `pub async fn run_interactive(&mut self) -> tm_types::Result<()>`
  - `pub async fn run_prompt(&mut self, prompt: &str) -> tm_types::Result<()>`
  - `pub(crate) async fn run_turn_streaming(
        &mut self,
        prompt: &str,
        mut on_event: impl FnMut(TurnEvent),
        mut approve: impl FnMut(&PendingApproval) -> tm_types::Result<bool>,
    ) -> tm_types::Result<AgentOutcome>`
- `pub(crate) fn build_fabric(clock: Arc<dyn Clock>) -> tm_types::Result<Arc<Fabric>>`
- `pub(crate) fn format_steps(steps: &[StepRecord]) -> String`
- `pub(crate) fn format_step(step: &StepRecord) -> String`
- `pub(crate) fn format_tool_call(call: &ToolCallRecord) -> String`
- `pub(crate) fn format_outcome_summary(outcome: &AgentOutcome) -> String`
- `pub(crate) fn format_pending_approval(pending: &PendingApproval) -> String`
- `pub(crate) fn format_budget_dimension(dim: BudgetDimension) -> &'static str`
- `pub(crate) struct MemoryCommandCache`
- `impl MemoryCommandCache`
  - `pub(crate) fn new(ids: Arc<dyn IdSource>) -> Self`
- `pub(crate) struct ProcessCommandExecutor;`

### `crates/tm-cli/src/args.rs`

- `pub struct Cli`
- `pub struct GlobalOpts`
- `pub enum Command`
- `pub enum WikiCommand`
- `pub struct WikiGenerateArgs`
- `pub enum ProjectCommand`
- `pub struct InitArgs`
- `pub struct AttachArgs`
- `pub struct GenesisArgs`
- `pub struct StatusArgs`
- `pub struct DoctorArgs`
- `pub enum TicketCommand`
- `pub struct TicketRefArgs`
- `pub struct TicketListArgs`
- `pub enum TicketStateArg`
- `pub struct TicketNewArgs`
- `pub struct TicketEditArgs`
- `pub struct TicketCancelArgs`
- `pub struct TicketDelegateArgs`
- `pub struct TicketSubmitArgs`
- `pub enum DepCommand`
- `pub struct DepEdgeArgs`
- `pub struct DepGraphArgs`
- `pub enum MilestoneCommand`
- `pub struct MilestoneRefArgs`
- `pub enum DecisionCommand`
- `pub struct DecisionRefArgs`
- `pub struct DecisionNewArgs`
- `pub struct DecisionSupersedeArgs`
- `pub enum SchedCommand`
- `pub struct SchedRunArgs`
- `pub enum LeaseCommand`
- `pub struct LeaseListArgs`
- `pub struct LeaseAcquireArgs`
- `pub struct LeaseRefArgs`
- `pub struct RunArgs`
- `pub enum SearchMode`
- `pub struct SearchArgs`
- `pub enum SymbolCommand`
- `pub struct SymbolQueryArgs`
- `pub struct SymbolOutlineArgs`
- `pub enum HistoryCommand`
- `pub struct HistoryWhyArgs`
- `pub struct HistorySearchArgs`
- `pub enum DocsCommand`
- `pub enum TemplatesCommand`
- `pub struct TemplatesShowArgs`
- `pub enum ProviderCommand`
- `pub struct ProviderTestArgs`
- `pub struct AuthArgs`
- `pub enum HarnessCommand`
- `pub struct HarnessSetArgs`
- `pub struct HarnessPromoteArgs`
- `pub enum BenchCommand`
- `pub struct BenchRunArgs`
- `pub struct BenchCompareArgs`
- `pub enum WorkflowCommand`
- `pub struct WorkflowShowArgs`
- `pub struct WorkflowRunArgs`
- `pub enum MirrorCommand`
- `pub struct MirrorLinkArgs`
- `pub struct ServeArgs`
- `pub enum EventsCommand`
- `pub struct EventsTailArgs`
- `pub struct EventsShowArgs`
- `pub struct EventsReplayArgs`
- `pub enum BrowserCommand`
- `pub struct BrowserOpenArgs`
- `pub struct BrowserRefArgs`
- `pub struct BrowserTypeArgs`
- `pub struct BrowserScreenshotArgs`
- `pub enum ComputerCommand`
- `pub struct ComputerSnapshotArgs`
- `pub struct ComputerClickArgs`
- `pub struct ComputerTypeArgs`
- `pub struct ComputerKeyArgs`

### `crates/tm-cli/src/auth.rs`

- `pub async fn auth(args: &AuthArgs, renderer: &Renderer) -> tm_types::Result<()>`

### `crates/tm-cli/src/dispatch.rs`

- `pub fn build_dispatcher(
    project: &Project,
    handle: tokio::runtime::Handle,
) -> tm_types::Result<Arc<ExecutorDispatcher>>`

### `crates/tm-cli/src/drive.rs`

- `pub async fn dispatch_browser(
    cmd: &BrowserCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn optional_browser_wiring(
    project: &Project,
) -> tm_types::Result<Option<tm_agent::BrowserWiring>>`
- `pub async fn browser_open(
    args: &BrowserOpenArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub async fn browser_snapshot(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub async fn browser_click(
    args: &BrowserRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub async fn browser_type(
    args: &BrowserTypeArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub async fn browser_screenshot(
    args: &BrowserScreenshotArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub async fn dispatch_computer(
    cmd: &ComputerCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub async fn computer_snapshot(
    args: &ComputerSnapshotArgs,
    project: &Project,
    renderer: &Renderer,
    session: &ComputerSession,
) -> tm_types::Result<()>`
- `pub async fn computer_click(
    args: &ComputerClickArgs,
    project: &Project,
    renderer: &Renderer,
    session: &mut ComputerSession,
) -> tm_types::Result<()>`
- `pub async fn computer_type(
    args: &ComputerTypeArgs,
    project: &Project,
    renderer: &Renderer,
    session: &mut ComputerSession,
) -> tm_types::Result<()>`
- `pub async fn computer_key(
    args: &ComputerKeyArgs,
    project: &Project,
    renderer: &Renderer,
    session: &mut ComputerSession,
) -> tm_types::Result<()>`

### `crates/tm-cli/src/lib.rs`

- `pub mod agent;`
- `pub mod args;`
- `pub mod auth;`
- `pub mod dispatch;`
- `pub mod drive;`
- `pub mod ops;`
- `pub mod project;`
- `pub mod render;`
- `pub mod sched;`
- `pub mod search;`
- `pub mod serve;`
- `pub mod tickets;`
- `pub mod tui;`
- `pub mod wiki;`
- `pub mod workflow;`

### `crates/tm-cli/src/ops.rs`

- `pub fn dispatch_docs(
    cmd: &DocsCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn docs_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn docs_check(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn docs_reconcile(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn dispatch_templates(
    cmd: &TemplatesCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn templates_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn templates_show(
    args: &TemplatesShowArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub async fn dispatch_provider(
    cmd: &ProviderCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub async fn provider_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub async fn provider_detect(renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn provider_status(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub async fn provider_test(
    args: &ProviderTestArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn dispatch_harness(
    cmd: &HarnessCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn harness_show(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn harness_set(
    args: &HarnessSetArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn harness_epochs(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn harness_promote(
    args: &HarnessPromoteArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub async fn dispatch_bench(
    cmd: &BenchCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn bench_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub async fn bench_run(
    args: &BenchRunArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn bench_compare(args: &BenchCompareArgs, renderer: &Renderer) -> tm_types::Result<()>`
- `pub async fn dispatch_mirror(
    cmd: &MirrorCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn mirror_link(
    args: &MirrorLinkArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub async fn mirror_push(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub async fn mirror_pull(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn mirror_status(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub async fn dispatch_events(
    cmd: &EventsCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub async fn events_tail(
    args: &EventsTailArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn events_show(
    args: &EventsShowArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn events_replay(
    args: &EventsReplayArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn events_verify(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`

### `crates/tm-cli/src/project.rs`

- `pub enum Scope`
- `pub struct Project`
- `impl Project`
  - `pub fn code_intel(&self) -> tm_types::Result<tm_codeintel::CodeIntel>`
  - `pub fn scope_line(&self) -> String`
- `pub fn locate(start: &Path) -> tm_types::Result<PathBuf>`
- `pub fn open_at(root: &Path, state_dir: &Path, scope: Scope) -> tm_types::Result<Project>`
- `pub fn open(root: &Path) -> tm_types::Result<Project>`
- `pub fn tm_home() -> tm_types::Result<PathBuf>`
- `pub fn workspace_root_for(cwd: &Path) -> tm_types::Result<PathBuf>`
- `pub fn global_project_key(root: &Path) -> String`
- `pub fn global_project_dir(root: &Path) -> tm_types::Result<PathBuf>`
- `pub struct Resolved`
- `pub fn resolve_scope(explicit: Option<&Path>, cwd: &Path) -> tm_types::Result<Resolved>`
- `pub fn open_for_command(explicit: Option<&Path>) -> tm_types::Result<Project>`
- `pub fn open_bare(explicit: Option<&Path>, _renderer: &Renderer) -> tm_types::Result<Project>`
- `pub fn dispatch_project(
    cmd: &ProjectCommand,
    explicit: Option<&Path>,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn init(args: &InitArgs, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn attach(args: &AttachArgs, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn genesis(args: &GenesisArgs, renderer: &Renderer) -> tm_types::Result<()>`
- `pub struct ScopeInfo`
- `pub struct StatusReport`
- `pub fn status(project: &Project, args: &StatusArgs, renderer: &Renderer) -> tm_types::Result<()>`
- `pub struct DoctorCheck`
- `pub struct DoctorReport`
- `impl DoctorReport`
  - `pub fn all_ok(&self) -> bool`
- `pub fn doctor(
    project: &Project,
    args: &DoctorArgs,
    renderer: &Renderer,
) -> tm_types::Result<DoctorReport>`
- `impl Project`
  - `pub(crate) fn for_test(
        root: &Path,
        store: Arc<tm_core::Store>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
    ) -> Project`

### `crates/tm-cli/src/render.rs`

- `pub struct Renderer`
- `impl Renderer`
  - `pub fn new(json: bool, quiet: bool, no_color: bool, stdout_is_terminal: bool) -> Self`
  - `pub fn from_flags(json: bool, quiet: bool, no_color: bool) -> Self`
  - `pub fn is_json(&self) -> bool`
  - `pub fn is_quiet(&self) -> bool`
  - `pub fn color_enabled(&self) -> bool`
  - `pub fn emit<T: Serialize>(&self, payload: &T, human: &str) -> tm_types::Result<()>`
  - `pub fn note(&self, text: &str)`
  - `pub fn error(&self, err: &TmError)`
  - `pub fn apply_color(&self, color: Color, text: &str) -> String`
- `pub enum Color`
- `impl Color`
  - `pub fn code(self) -> u8`
- `pub struct Table`
- `impl Table`
  - `pub fn new(headers: Vec<String>, rows: Vec<Vec<String>>) -> Self`
  - `pub fn render(&self) -> String`
- `pub struct Tree`
- `impl Tree`
  - `pub fn leaf(label: impl Into<String>) -> Self`
  - `pub fn render(&self) -> String`
- `pub fn exit_code(err: &TmError) -> i32`

### `crates/tm-cli/src/sched.rs`

- `pub fn dispatch_sched(
    cmd: &SchedCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn sched_plan(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn sched_tick(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub async fn sched_run(
    args: &SchedRunArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn sched_pause(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn sched_resume(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn dispatch_lease(
    cmd: &LeaseCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn lease_list(
    args: &LeaseListArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn lease_acquire(
    args: &LeaseAcquireArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn lease_release(
    args: &LeaseRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn lease_expire(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub async fn run_ticket(
    args: &RunArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`

### `crates/tm-cli/src/search.rs`

- `pub fn search(args: &SearchArgs, project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub type Mode = SearchMode;`
- `pub fn dispatch_symbol(
    cmd: &SymbolCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn symbol_def(
    args: &SymbolQueryArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn symbol_refs(
    args: &SymbolQueryArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn symbol_callers(
    args: &SymbolQueryArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn symbol_callees(
    args: &SymbolQueryArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn symbol_outline(
    args: &SymbolOutlineArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn dispatch_history(
    cmd: &HistoryCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn history_why(
    args: &HistoryWhyArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn history_search(
    args: &HistorySearchArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn history_deleted(
    args: &HistorySearchArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`

### `crates/tm-cli/src/serve.rs`

- `pub async fn serve(
    args: &ServeArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`

### `crates/tm-cli/src/tickets.rs`

- `pub(crate) fn event_ticket_id(subject: &tm_types::Id) -> Option<TicketId>`
- `pub(crate) fn default_executor_requirements() -> ExecutorRequirements`
- `pub(crate) fn default_retry_policy() -> RetryPolicy`
- `pub fn dispatch_ticket(
    cmd: &TicketCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn ticket_list(
    args: &TicketListArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn ticket_show(
    args: &TicketRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn ticket_new(
    args: &TicketNewArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn ticket_edit(
    args: &TicketEditArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn ticket_close(
    args: &TicketRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn ticket_cancel(
    args: &TicketCancelArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn ticket_reopen(
    args: &TicketRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn ticket_tree(
    args: &TicketRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn ticket_delegate(
    args: &TicketDelegateArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn ticket_submit(
    args: &TicketSubmitArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn dispatch_dep(
    cmd: &DepCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn dep_add(args: &DepEdgeArgs, project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn dep_rm(args: &DepEdgeArgs, project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn dep_graph(
    args: &DepGraphArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn dispatch_milestone(
    cmd: &MilestoneCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn milestone_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn milestone_close(
    args: &MilestoneRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn milestone_reopen(
    args: &MilestoneRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn dispatch_decision(
    cmd: &DecisionCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn decision_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn decision_show(
    args: &DecisionRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn decision_new(
    args: &DecisionNewArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn decision_supersede(
    args: &DecisionSupersedeArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`

### `crates/tm-cli/src/tui.rs`

- `pub fn should_launch(global: &GlobalOpts) -> bool`
- `pub async fn run(project: Arc<Project>) -> tm_types::Result<()>`

### `crates/tm-cli/src/wiki.rs`

- `pub fn dispatch_wiki(
    cmd: &WikiCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn wiki_generate(
    args: &WikiGenerateArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`

### `crates/tm-cli/src/workflow.rs`

- `pub fn dispatch_workflow(
    cmd: &WorkflowCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn workflow_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()>`
- `pub fn workflow_show(
    args: &WorkflowShowArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn workflow_run(
    args: &WorkflowRunArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()>`
- `pub fn one_by_one_workflow_names(project: &Project) -> Vec<String>`
- `pub fn has_any_workflow(project: &Project) -> bool`
