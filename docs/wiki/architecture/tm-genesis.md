+++
[doc]
id = "wiki/architecture/tm-genesis"
mode = "generated"
derived_from = ["crates/tm-genesis/src/**"]
+++

# Architecture: tm-genesis

## Module tree

- `crates/tm-genesis/src/attach.rs`
- `crates/tm-genesis/src/compile.rs`
- `crates/tm-genesis/src/ignition.rs`
- `crates/tm-genesis/src/lib.rs`
- `crates/tm-genesis/src/maturity.rs`
- `crates/tm-genesis/src/seed.rs`
- `crates/tm-genesis/src/spec.rs`
- `crates/tm-genesis/src/stages.rs`
- `crates/tm-genesis/src/vision.rs`

## Public symbols

### `crates/tm-genesis/src/attach.rs`

- `pub enum DocKind`
- `pub struct DiscoveredDoc`
- `pub struct BuildSystem`
- `pub struct ExternalTracker`
- `pub struct Convention`
- `pub struct AttachReport`
- `impl AttachReport`
  - `pub fn blocking_open_questions(&self) -> Vec<&Question>`
- `pub fn attach_repository(
    project_root: &Path,
    store: &Store,
    clock: &dyn Clock,
    actor: ParticipantId,
) -> TmResult<AttachReport>`

### `crates/tm-genesis/src/compile.rs`

- `pub type Ref = String;`
- `pub struct ProposedTicket`
- `pub struct ProposedDependency`
- `pub struct ProposedMilestone`
- `pub struct AuthorityDomain`
- `pub struct GraphCompilation`
- `pub enum CompilationError`
- `pub async fn propose_graph(
    spec: &Specification,
    prior_violations: &[Violation],
    attempt: u32,
    provider: &dyn tm_provider::Provider,
) -> TmResult<GraphCompilation>`
- `pub fn select_template<'a>(
    spec: &Specification,
    templates: &'a [TemplateManifest],
) -> Option<&'a TemplateManifest>`
- `pub fn validate_graph(proposal: &GraphCompilation, existing: &ProjectView) -> Vec<Violation>`
- `pub struct CommitOutcome`
- `pub fn commit_graph(
    store: &Store,
    proposal: &GraphCompilation,
    actor: ParticipantId,
) -> TmResult<CommitOutcome>`
- `pub struct RetryPolicy`
- `impl RetryPolicy`
  - `pub fn default_bounded() -> Self`
- `pub async fn compile_with_retry(
    spec: &Specification,
    provider: &dyn tm_provider::Provider,
    store: &Store,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    actor: ParticipantId,
    templates: &[TemplateManifest],
    policy: &RetryPolicy,
) -> TmResult<CommitOutcome>`

### `crates/tm-genesis/src/ignition.rs`

- `pub struct IgnitionPolicy`
- `impl IgnitionPolicy`
  - `pub fn for_v0(v0_objective_milestone: MilestoneId) -> Self`
- `pub struct SteadyStatePolicy`
- `impl SteadyStatePolicy`
  - `pub fn narrowed_from(ignition: &IgnitionPolicy, narrower_authority: Authority) -> Self`

### `crates/tm-genesis/src/lib.rs`

- `pub mod attach;`
- `pub mod compile;`
- `pub mod ignition;`
- `pub mod maturity;`
- `pub mod seed;`
- `pub mod spec;`
- `pub mod stages;`
- `pub mod vision;`

### `crates/tm-genesis/src/maturity.rs`

- `pub struct MaturityThresholds`
- `impl MaturityThresholds`
  - `pub fn conservative() -> Self`
- `pub struct MaturityPredicate`
- `impl MaturityPredicate`
  - `pub fn is_satisfied(&self) -> bool`
- `pub fn evaluate_predicate(
    view: &ProjectView,
    v1: &MilestoneId,
    thresholds: MaturityThresholds,
) -> MaturityPredicate`
- `pub struct MaturityJudgment`
- `pub async fn judge_maturity(
    view: &ProjectView,
    predicate: &MaturityPredicate,
    role: Role,
    provider: &dyn tm_provider::Provider,
    store: &Store,
    clock: &dyn Clock,
    actor: ParticipantId,
) -> TmResult<MaturityJudgment>`
- `pub struct MaturityGateResult`
- `pub fn gate(predicate: MaturityPredicate, judgment: MaturityJudgment) -> MaturityGateResult`
- `pub struct ReconvergenceOutcome`
- `pub fn reconverge_authority(
    store: &Store,
    genesis_leases: &[LeaseId],
    ignition: &IgnitionPolicy,
    actor: ParticipantId,
) -> TmResult<ReconvergenceOutcome>`

### `crates/tm-genesis/src/seed.rs`

- `pub struct Constraint`
- `impl Constraint`
  - `pub fn new(text: impl Into<String>, rationale: impl Into<String>) -> Self`
- `pub struct Assumption`
- `impl Assumption`
  - `pub fn new(text: impl Into<String>, rationale: impl Into<String>, confidence: f64) -> Self`
- `pub struct Question`
- `impl Question`
  - `pub fn new(text: impl Into<String>, blocking: bool, rationale: impl Into<String>) -> Self`
  - `pub fn is_open(&self) -> bool`
- `pub struct Seed`
- `impl Seed`
  - `pub fn new(raw_prompt: String, clock: &dyn Clock) -> Self`
  - `pub fn blocking_open_questions(&self) -> Vec<&Question>`
  - `pub fn is_unblocked(&self) -> bool`
- `pub async fn analyze_prompt(
    raw_prompt: String,
    provider: &dyn tm_provider::Provider,
    clock: &dyn Clock,
) -> TmResult<Seed>`

### `crates/tm-genesis/src/spec.rs`

- `pub struct Requirement`
- `pub struct InterfaceSpec`
- `pub struct TechnologyChoice`
- `pub struct MilestoneOutline`
- `pub struct ReleaseDefinition`
- `pub struct Specification`
- `pub fn build_spec_request(vision: &Vision) -> tm_provider::types::CompletionRequest`
- `pub async fn compile_spec(
    vision: &Vision,
    provider: &dyn tm_provider::Provider,
    clock: &dyn Clock,
) -> TmResult<Specification>`

### `crates/tm-genesis/src/stages.rs`

- `pub enum Stage`
- `impl Stage`
  - `pub const ALL: &'static [Stage] = &[
        Stage::Seed,
        Stage::Vision,
        Stage::Spec,
        Stage::GraphCompilation,
        Stage::Ignition,
        Stage::V0,
        Stage::Evaluation,
        Stage::V1,
        Stage::Stabilization,
        Stage::MaturityGate,
        Stage::AuthorityReconvergence,
        Stage::SteadyState,
    ];`
  - `pub fn legal_next(self) -> &'static [Stage]`
  - `pub fn as_str(self) -> &'static str`
- `pub enum StageEvent`
- `pub struct IllegalTransition`
- `pub fn transition(from: Stage, event: &StageEvent) -> Result<Stage, IllegalTransition>`
- `pub struct GenesisState`
- `impl GenesisState`
  - `pub fn new(project: String, clock: &dyn Clock) -> Self`
- `pub struct GenesisDriver<'a>`
- `impl<'a> GenesisDriver<'a>`
  - `pub fn new(
        store: &'a tm_core::Store,
        provider: &'a dyn tm_provider::Provider,
        clock: &'a dyn Clock,
        ids: &'a dyn IdSource,
    ) -> Self`
  - `pub async fn advance(
        &self,
        state: &GenesisState,
        actor: ParticipantId,
    ) -> TmResult<GenesisState>`
  - `pub fn resume(store: &'a tm_core::Store) -> TmResult<GenesisState>`

### `crates/tm-genesis/src/vision.rs`

- `pub struct Vision`
- `impl Vision`
  - `pub fn is_anchored(&self) -> bool`
- `pub async fn compile_vision(
    seed: &Seed,
    provider: &dyn tm_provider::Provider,
    clock: &dyn Clock,
) -> TmResult<Vision>`
