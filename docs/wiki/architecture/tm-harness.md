+++
[doc]
id = "wiki/architecture/tm-harness"
mode = "generated"
derived_from = ["crates/tm-harness/src/**"]
+++

# Architecture: tm-harness

## Module tree

- `crates/tm-harness/src/bench.rs`
- `crates/tm-harness/src/config.rs`
- `crates/tm-harness/src/efficacy.rs`
- `crates/tm-harness/src/epoch.rs`
- `crates/tm-harness/src/lib.rs`
- `crates/tm-harness/src/metrics.rs`

## Public symbols

### `crates/tm-harness/src/bench.rs`

- `pub struct BenchFixture`
- `pub struct ScoringSpec`
- `pub struct ExpectedOutcome`
- `pub struct BenchTask`
- `impl BenchTask`
  - `pub fn parse(source: &str) -> Result<BenchTask>`
- `pub trait SeededProvider`
- `pub struct BenchRunner<'a>`
- `pub struct TaskResult`
- `impl<'a> BenchRunner<'a>`
  - `pub fn run(&self, task: &BenchTask, provider: &dyn SeededProvider) -> Result<TaskResult>`
  - `pub fn run_all(
        &self,
        tasks: &[BenchTask],
        provider: &dyn SeededProvider,
        epoch: u64,
    ) -> Result<BenchmarkReport>`
- `pub struct BenchmarkReport`
- `pub struct PromotionReport`
- `pub fn compare(baseline: &BenchmarkReport, candidate: &BenchmarkReport) -> PromotionReport`

### `crates/tm-harness/src/config.rs`

- `pub struct ConfigHash`
- `impl ConfigHash`
  - `pub fn to_hex(&self) -> String`
- `pub enum ConfigError`
- `pub struct ToolPreferences`
- `pub enum DiffStyle`
- `pub struct RoutingWeights`
- `pub struct ContextPolicy`
- `pub struct RoleBinding`
- `pub struct RoleMapping`
- `impl RoleMapping`
  - `pub fn get(&self, role: Role) -> Option<&RoleBinding>`
- `pub struct VerificationDefaults`
- `pub struct PromptFragments`
- `pub struct CommandPolicy`
- `pub struct EditingPolicy`
- `pub struct HarnessConfig`
- `pub const CURRENT_SCHEMA_VERSION: u32 = 1;`
- `impl HarnessConfig`
  - `pub fn parse(source: &str) -> Result<HarnessConfig, ConfigError>`
  - `pub fn validate(&self) -> Result<(), ConfigError>`
  - `pub fn config_hash(&self) -> ConfigHash`

### `crates/tm-harness/src/efficacy.rs`

- `pub struct EfficacyBudget`
- `pub enum EfficacyError`
- `impl EfficacyBudget`
  - `pub fn validate(&self) -> Result<(), EfficacyError>`
- `pub struct EfficacyAccount`
- `impl EfficacyAccount`
  - `pub fn new(budget: EfficacyBudget) -> EfficacyAccount`
  - `pub fn current_fraction(&self) -> f64`
  - `pub fn admit_harness_unit(&self) -> Result<(), EfficacyError>`
  - `pub fn record(&mut self, is_harness: bool, units: u64)`
  - `pub fn release(&mut self, is_harness: bool, units: u64)`

### `crates/tm-harness/src/epoch.rs`

- `pub struct HarnessEpoch`
- `pub struct SessionPin`
- `pub enum EpochResolutionError`
- `pub struct PromotionGate`
- `pub enum PromotionDecision`
- `impl PromotionDecision`
  - `pub fn is_approved(&self) -> bool`
- `impl PromotionGate`
  - `pub fn evaluate(
        &self,
        baseline: Option<&BenchmarkReport>,
        candidate: &BenchmarkReport,
    ) -> PromotionDecision`
- `pub enum PromotionOutcome`
- `pub struct EpochRegistry`
- `impl EpochRegistry`
  - `pub fn new(genesis: HarnessEpoch) -> EpochRegistry`
  - `pub fn current(&self) -> &HarnessEpoch`
  - `pub fn get(&self, number: u64) -> Option<&HarnessEpoch>`
  - `pub fn epochs(&self) -> &[HarnessEpoch]`
  - `pub fn pin_current(&self, session: SessionId, clock: &dyn Clock) -> SessionPin`
  - `pub fn resolve(&self, pin: &SessionPin) -> Result<&HarnessEpoch, EpochResolutionError>`
  - `pub fn promote(
        &mut self,
        candidate: HarnessConfig,
        promoted_at: Timestamp,
        promoted_by: ParticipantId,
        benchmark: Option<BenchmarkReport>,
        gate: &PromotionGate,
    ) -> Result<PromotionOutcome, EpochResolutionError>`

### `crates/tm-harness/src/lib.rs`

- `pub mod bench;`
- `pub mod config;`
- `pub mod efficacy;`
- `pub mod epoch;`
- `pub mod metrics;`

### `crates/tm-harness/src/metrics.rs`

- `pub struct TicketMetrics`
- `pub struct SessionMetrics`
- `pub struct AggregateMetrics`
- `impl AggregateMetrics`
  - `pub fn zero() -> AggregateMetrics`
  - `pub fn accumulate(&mut self, metrics: &TicketMetrics)`
  - `pub fn from_tickets(tickets: &[TicketMetrics]) -> AggregateMetrics`
  - `pub fn mean_searches_before_first_relevant_hit(&self) -> f64`
  - `pub fn mean_dollars_micros(&self) -> f64`
- `pub struct EpochComparison`
- `impl EpochComparison`
  - `pub fn new(
        baseline_epoch: u64,
        baseline: AggregateMetrics,
        candidate_epoch: u64,
        candidate: AggregateMetrics,
    ) -> EpochComparison`
  - `pub fn mean_dollars_delta(&self) -> f64`
  - `pub fn mean_searches_delta(&self) -> f64`
