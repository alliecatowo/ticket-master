+++
[doc]
id = "wiki/architecture/tm-types"
mode = "generated"
derived_from = ["crates/tm-types/src/**"]
+++

# Architecture: tm-types

## Module tree

- `crates/tm-types/src/action.rs`
- `crates/tm-types/src/authority.rs`
- `crates/tm-types/src/budget.rs`
- `crates/tm-types/src/capability.rs`
- `crates/tm-types/src/clock.rs`
- `crates/tm-types/src/error.rs`
- `crates/tm-types/src/id.rs`
- `crates/tm-types/src/lib.rs`
- `crates/tm-types/src/pattern.rs`
- `crates/tm-types/src/predicate.rs`
- `crates/tm-types/src/role.rs`
- `crates/tm-types/src/time_.rs`

## Public symbols

### `crates/tm-types/src/action.rs`

- `pub enum Action`
- `pub enum GitOp`
- `pub enum TicketOp`
- `pub enum ProjectOp`
- `impl Action`
  - `pub fn class(&self) -> String`
- `impl GitOp`
  - `pub fn as_str(self) -> &'static str`
- `impl TicketOp`
  - `pub fn as_str(self) -> &'static str`
- `impl ProjectOp`
  - `pub fn as_str(self) -> &'static str`
- `pub enum Decision`
- `impl Decision`
  - `pub fn is_allowed(&self) -> bool`
- `pub struct Oversight`
- `impl Oversight`
  - `pub fn autonomous() -> Self`
  - `pub fn conservative() -> Self`
  - `pub fn review(&self, action: &Action, base: Decision) -> Decision`

### `crates/tm-types/src/authority.rs`

- `pub struct AuthorityDenied`
- `pub struct RepoAuthority`
- `pub struct GitAuthority`
- `pub struct TicketAuthority`
- `pub struct ProjectAuthority`
- `pub struct NetworkAuthority`
- `impl NetworkAuthority`
  - `pub const DOC_HOSTS: [&'static str; 6] = [
        "docs.rs",
        "doc.rust-lang.org",
        "developer.mozilla.org",
        "pkg.go.dev",
        "docs.python.org",
        "crates.io",
    ];`
  - `pub fn permits_url(&self, url: &str) -> bool`
- `pub struct ShellAuthority`
- `impl ShellAuthority`
  - `pub fn command_line(command: &[String]) -> String`
  - `pub fn permits(&self, command: &[String]) -> bool`
  - `pub fn permits_pty_send(&self) -> bool`
- `pub struct ComputerAuthority`
- `pub struct ResourceAuthority`
- `pub struct Authority`
- `impl Authority`
  - `pub fn root() -> Self`
  - `pub fn none() -> Self`
  - `pub fn with_inherited_denials(&self, requested: &Authority) -> Authority`
  - `pub fn read_only() -> Self`
  - `pub fn contains(&self, other: &Authority) -> bool`
  - `pub fn containment_failures(&self, other: &Authority) -> Vec<String>`
  - `pub fn intersect(&self, other: &Authority) -> Authority`
  - `pub fn attenuate(&self, requested: &Authority) -> Result<Authority, AuthorityDenied>`
  - `pub fn attenuate_lossy(&self, requested: &Authority) -> Authority`
  - `pub fn permits(&self, action: &Action) -> Decision`

### `crates/tm-types/src/budget.rs`

- `pub struct Spend`
- `impl Spend`
  - `pub fn tokens(n: u64) -> Self`
  - `pub fn dollars_micros(n: u64) -> Self`
  - `pub fn seconds(n: u64) -> Self`
  - `pub fn plus(self, o: Spend) -> Spend`
  - `pub fn is_zero(self) -> bool`
- `pub enum BudgetError`
- `pub struct Budget`
- `impl Budget`
  - `pub fn unlimited() -> Self`
  - `pub fn none() -> Self`
  - `pub fn new(tokens: u64, dollars_micros: u64, wall_seconds: u64) -> Self`
  - `pub fn remaining(&self) -> Spend`
  - `pub fn is_exhausted(&self) -> bool`
  - `pub fn check(&self, s: Spend) -> Result<(), BudgetError>`
  - `pub fn try_spend(&mut self, s: Spend) -> Result<(), BudgetError>`
  - `pub fn contains(&self, other: &Budget) -> bool`
  - `pub fn intersect(&self, other: &Budget) -> Budget`

### `crates/tm-types/src/capability.rs`

- `pub enum CostClass`
- `pub enum AuthorityRequirement`
- `impl AuthorityRequirement`
  - `pub fn admits(&self, authority: &Authority) -> bool`
- `pub struct ToolSchema`
- `pub struct CallContext<'a>`
- `pub trait CapabilityProvider: Send + Sync`

### `crates/tm-types/src/clock.rs`

- `pub trait Clock: Send + Sync`
- `pub trait IdSource: Send + Sync`
- `pub struct SystemClock;`
- `pub struct FixedClock`
- `impl FixedClock`
  - `pub fn new(t: Timestamp) -> Self`
  - `pub fn epoch() -> Self`
  - `pub fn advance_seconds(&self, secs: i64)`
  - `pub fn advance_millis(&self, millis: i64)`
  - `pub fn set(&self, t: Timestamp)`
- `pub struct CounterIds`
- `pub type TestIds = CounterIds;`
- `impl CounterIds`
  - `pub fn new() -> Self`
  - `pub fn seeded(seed: u64) -> Self`
  - `pub fn with_counters(counters: BTreeMap<String, u64>, seed: u64) -> Self`
  - `pub fn snapshot(&self) -> BTreeMap<String, u64>`
  - `pub fn observe(&self, kind: IdKind, number: u64)`
  - `pub fn next_number(&self, kind: IdKind) -> u64`

### `crates/tm-types/src/error.rs`

- `pub type Result<T, E = TmError> = std::result::Result<T, E>;`
- `pub enum TmError`
- `impl TmError`
  - `pub fn not_found(kind: &'static str, id: impl fmt::Display) -> Self`
  - `pub fn conflict(msg: impl fmt::Display) -> Self`
  - `pub fn storage(msg: impl fmt::Display) -> Self`
  - `pub fn invariant(msg: impl fmt::Display) -> Self`
  - `pub fn parse(msg: impl fmt::Display) -> Self`
  - `pub fn exit_code(&self) -> i32`

### `crates/tm-types/src/id.rs`

- `pub enum IdKind`
- `impl IdKind`
  - `pub fn prefix(self) -> &'static str`
  - `pub fn counter(self) -> &'static str`
  - `pub fn pad(self) -> usize`
- `impl TicketId`
  - `pub fn kind(&self) -> IdKind`
- `impl ParticipantId`
  - `pub fn system() -> Self`
  - `pub fn is_agent(&self) -> bool`
  - `pub fn is_human(&self) -> bool`
- `pub struct Id`
- `impl Id`
  - `pub fn new(s: impl Into<String>) -> Self`
  - `pub fn none() -> Self`
  - `pub fn as_str(&self) -> &str`
  - `pub fn is_empty(&self) -> bool`
  - `pub fn kind(&self) -> Option<IdKind>`

### `crates/tm-types/src/lib.rs`

- `pub mod action;`
- `pub mod authority;`
- `pub mod budget;`
- `pub mod capability;`
- `pub mod clock;`
- `pub mod error;`
- `pub mod id;`
- `pub mod pattern;`
- `pub mod predicate;`
- `pub mod role;`
- `pub mod time_;`

### `crates/tm-types/src/pattern.rs`

- `pub struct PathPattern`
- `impl PathPattern`
  - `pub fn new(s: impl Into<String>) -> Result<Self, crate::error::TmError>`
  - `pub fn as_str(&self) -> &str`
  - `pub fn implied_by(&self, other: &PathPattern) -> bool`
  - `pub fn may_overlap(&self, other: &PathPattern) -> bool`
- `pub struct PatternSet`
- `impl PatternSet`
  - `pub fn empty() -> Self`
  - `pub fn all() -> Self`
  - `pub fn parse<I, S>(items: I) -> Result<Self, crate::error::TmError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,`
  - `pub fn patterns(&self) -> &[PathPattern]`
  - `pub fn is_empty(&self) -> bool`
  - `pub fn matches(&self, path: impl AsRef<str>) -> bool`
  - `pub fn matches_text(&self, text: impl AsRef<str>) -> bool`
  - `pub fn is_subset_of(&self, other: &PatternSet) -> bool`
  - `pub fn union(&self, other: &PatternSet) -> PatternSet`
  - `pub fn intersect(&self, other: &PatternSet) -> PatternSet`
  - `pub fn overlaps(&self, other: &PatternSet) -> bool`

### `crates/tm-types/src/predicate.rs`

- `pub enum Predicate`
- `impl Predicate`
  - `pub fn is_machine_checkable(&self) -> bool`
  - `pub fn referenced_tickets(&self) -> Vec<TicketId>`
  - `pub fn walk(&self, f: &mut impl FnMut(&Predicate))`
  - `pub fn evaluate(
        &self,
        leaf: &mut impl FnMut(&Predicate) -> PredicateOutcome,
    ) -> PredicateOutcome`
- `pub enum PredicateOutcome`
- `impl PredicateOutcome`
  - `pub fn is_satisfied(&self) -> bool`

### `crates/tm-types/src/role.rs`

- `pub enum Role`
- `impl Role`
  - `pub const ALL: [Role; 12] = [
        Role::VisionFrontier,
        Role::PlannerFrontier,
        Role::ArchitectFrontier,
        Role::CoderDeep,
        Role::CoderFast,
        Role::ExplorerCheap,
        Role::ReviewerSemantic,
        Role::AuditorSemantic,
        Role::SynthesizerLongContext,
        Role::SummarizerCheap,
        Role::Embedder,
        Role::ComputerUse,
    ];`
  - `pub fn as_str(self) -> &'static str`
  - `pub fn config_key(self) -> String`
  - `pub fn is_frontier(self) -> bool`
  - `pub fn default_tolerance(self) -> Tolerance`
- `pub enum Tolerance`

### `crates/tm-types/src/time_.rs`

- `pub struct Timestamp`
- `impl Timestamp`
  - `pub const EPOCH: Timestamp = Timestamp(0);`
  - `pub const fn from_unix_nanos(nanos: i128) -> Self`
  - `pub const fn from_unix_seconds(secs: i64) -> Self`
  - `pub const fn unix_nanos(self) -> i128`
  - `pub fn unix_seconds(self) -> i64`
  - `pub const fn plus_seconds(self, secs: i64) -> Self`
  - `pub const fn plus_millis(self, millis: i64) -> Self`
  - `pub fn seconds_since(self, earlier: Timestamp) -> i64`
  - `pub fn millis_since(self, earlier: Timestamp) -> i64`
  - `pub fn to_rfc3339(self) -> String`
  - `pub fn parse_rfc3339(s: &str) -> Result<Self, ParseTimestampError>`
- `pub struct ParseTimestampError`
