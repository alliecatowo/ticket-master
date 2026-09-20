+++
[doc]
id = "wiki/architecture/tm-scheduler"
mode = "generated"
derived_from = ["crates/tm-scheduler/src/**"]
+++

# Architecture: tm-scheduler

## Module tree

- `crates/tm-scheduler/src/admission.rs`
- `crates/tm-scheduler/src/dispatch.rs`
- `crates/tm-scheduler/src/driver.rs`
- `crates/tm-scheduler/src/lib.rs`
- `crates/tm-scheduler/src/plan.rs`
- `crates/tm-scheduler/src/policy.rs`
- `crates/tm-scheduler/src/retry.rs`
- `crates/tm-scheduler/src/select.rs`
- `crates/tm-scheduler/src/snapshot.rs`

## Public symbols

### `crates/tm-scheduler/src/admission.rs`

- `pub enum AdmissionDecision`
- `pub enum AdmissionRefusal`
- `pub struct AdmissionGate`
- `impl AdmissionGate`
  - `pub fn from_view(view: &SchedulerView, commands_in_flight: u32) -> Self`
  - `pub fn check(
        &self,
        ticket: &Ticket,
        policy: &SchedulingPolicy,
        availability: &dyn ExecutorAvailability,
    ) -> AdmissionDecision`
  - `pub fn record_admission(&mut self, ticket: &Ticket)`

### `crates/tm-scheduler/src/dispatch.rs`

- `pub trait ContextPackSource: Send + Sync`
- `pub struct ExecutorRegistry`
- `impl ExecutorRegistry`
  - `pub fn new(human: Arc<dyn Executor>) -> Self`
  - `pub fn register(&mut self, role: Role, executor: Arc<dyn Executor>)`
  - `pub fn for_role(&self, role: Role) -> Option<Arc<dyn Executor>>`
  - `pub fn human(&self) -> Arc<dyn Executor>`
- `pub enum DispatchError`
- `pub struct ExecutorDispatcher`
- `impl ExecutorDispatcher`
  - `pub fn new(
        store: Arc<Store>,
        handle: tokio::runtime::Handle,
        context: Arc<dyn ContextPackSource>,
        registry: ExecutorRegistry,
        repo_root: Option<PathBuf>,
    ) -> Self`
  - `pub fn dispatch(
        &self,
        ticket: &TicketId,
        t: &Ticket,
        ttl_seconds: u32,
        actor: ParticipantId,
    ) -> tm_types::Result<Vec<Event>>`
  - `pub fn dispatch_with_completion(
        &self,
        ticket: &TicketId,
        t: &Ticket,
        ttl_seconds: u32,
        actor: ParticipantId,
    ) -> tm_types::Result<(Vec<Event>, tokio::sync::oneshot::Receiver<()>)>`

### `crates/tm-scheduler/src/driver.rs`

- `pub enum SchedulerLoopEvent`
- `pub struct SchedulerLoop<'a>`
- `impl<'a> SchedulerLoop<'a>`
  - `pub fn new(store: &'a Store, clock: Arc<dyn Clock>, policy: SchedulingPolicy) -> Self`
  - `pub fn with_dispatcher(mut self, dispatcher: Arc<ExecutorDispatcher>) -> Self`
  - `pub fn policy(&self) -> &SchedulingPolicy`
  - `pub fn set_policy(&mut self, policy: SchedulingPolicy)`
  - `pub fn tick(&self, actor: ParticipantId) -> tm_types::Result<Vec<SchedulerLoopEvent>>`

### `crates/tm-scheduler/src/lib.rs`

- `pub mod admission;`
- `pub mod dispatch;`
- `pub mod driver;`
- `pub mod plan;`
- `pub mod policy;`
- `pub mod retry;`
- `pub mod select;`
- `pub mod snapshot;`

### `crates/tm-scheduler/src/plan.rs`

- `pub enum SchedulerAction`
- `pub fn plan(
    view: &SchedulerView,
    now: Timestamp,
    policy: &SchedulingPolicy,
) -> Vec<SchedulerAction>`

### `crates/tm-scheduler/src/policy.rs`

- `pub enum SchedulingMode`
- `pub struct IgnitionRelaxations`
- `impl IgnitionRelaxations`
  - `pub fn none() -> Self`
- `pub struct OrderingWeights`
- `impl OrderingWeights`
  - `pub fn spec_default() -> Self`
- `pub struct SchedulingPolicy`
- `impl SchedulingPolicy`
  - `pub fn conservative_default() -> Self`
  - `pub fn effective_max_in_flight_per_project(&self) -> u32`

### `crates/tm-scheduler/src/retry.rs`

- `pub enum RetryOutcome`
- `pub enum EscalationReason`
- `pub struct RetryDecision`
- `pub fn decide_retry(ticket: &Ticket, failure: FailureClass, now: Timestamp) -> RetryDecision`
- `pub fn traverse_cycle_edge(budget: CycleBudget) -> CycleBudget`

### `crates/tm-scheduler/src/select.rs`

- `pub struct ExecutorMatch`
- `pub enum SelectionError`
- `pub trait ExecutorAvailability`
- `pub enum CapabilityMismatch`
- `pub fn capabilities_satisfy(
    requirements: &ExecutorRequirements,
    capabilities: &ExecutorCapabilities,
) -> Result<(), CapabilityMismatch>`
- `pub fn compare_tickets(
    a: &Ticket,
    b: &Ticket,
    graph: &DependencyGraph,
    milestone_deadlines: &BTreeMap<MilestoneId, Timestamp>,
) -> Ordering`
- `pub fn ready_tickets(view: &SchedulerView) -> Vec<&Ticket>`
- `pub fn rank_ready(view: &SchedulerView, policy: &SchedulingPolicy) -> Vec<TicketId>`
- `pub fn executor_matches(
    requirements: &ExecutorRequirements,
    availability: &dyn ExecutorAvailability,
) -> bool`
- `pub fn select_next(
    ranked: &[TicketId],
    view: &SchedulerView,
    availability: &dyn ExecutorAvailability,
) -> Result<ExecutorMatch, SelectionError>`

### `crates/tm-scheduler/src/snapshot.rs`

- `pub struct WorkspaceSnapshot`
- `pub fn capture_workspace_snapshot(repo_root: &Path) -> Option<WorkspaceSnapshot>`
