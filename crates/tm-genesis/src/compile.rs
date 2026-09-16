//! Graph compilation: turning a [`crate::spec::Specification`] into an actual `tm-core` graph.
//!
//! This module is split into three pure-then-I/O phases so each is independently testable:
//!
//! 1. [`propose_graph`] / [`compile_graph`] — ask `planner.frontier` for a [`GraphCompilation`]:
//!    tickets, dependencies, milestones, authority domains, verification policies, resource
//!    declarations, executor roles and budgets, addressed by local [`TicketRef`]s (not real
//!    [`tm_types::TicketId`]s, which don't exist until commit).
//! 2. [`validate_graph`] — pure: projects the proposal onto the existing [`tm_core::ProjectView`]
//!    and runs `tm_core::check_invariants` against the result, *before* anything is committed.
//! 3. [`commit_graph`] — I/O: turns every ref into a real ticket/milestone id and commits the
//!    whole graph as one logical operation, or rejects it wholesale. [`compile_with_retry`]
//!    composes all three: an invalid graph is regenerated with the validation errors fed back,
//!    bounded by [`RetryPolicy::max_attempts`], then escalated as a [`tm_core::Decision`]
//!    requiring human input rather than looping forever.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tm_core::invariants::Violation;
use tm_core::ticket::{
    ContextRef, DependencyKind, ExecutorRequirements, ResourceClaim, RetryPolicy as TicketRetryPolicy,
    TicketKind, VerificationPolicy,
};
use tm_core::{ProjectView, Store};
use tm_types::{
    ArtifactId, Authority, Budget, Clock, IdSource, MilestoneId, ParticipantId, Predicate, Result as TmResult,
    TicketId, TmError,
};

use crate::spec::Specification;

/// A local, unresolved reference to a ticket or milestone within one [`GraphCompilation`]
/// proposal, e.g. `"t-core-store"`. Resolved to a real id only by [`commit_graph`], since real
/// ids don't exist until the ticket/milestone is actually created.
pub type Ref = String;

/// One proposed ticket, addressed by [`Ref`] rather than [`TicketId`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposedTicket {
    /// This ticket's local reference.
    pub ticket_ref: Ref,
    /// What kind of work this is.
    pub kind: TicketKind,
    /// The objective, in prose.
    pub objective: String,
    /// The parent ticket's ref, if this is a child.
    pub parent_ref: Option<Ref>,
    /// The milestone's ref this ticket belongs to, if any.
    pub milestone_ref: Option<Ref>,
    /// Authority granted to this ticket's executor.
    pub authority: Authority,
    /// Resource claims this ticket's lease will hold.
    pub resources: Vec<ResourceClaim>,
    /// Executor requirements.
    pub executor: ExecutorRequirements,
    /// Context references a worker should load first.
    pub context_refs: Vec<ContextRef>,
    /// Success predicates.
    pub success: Vec<Predicate>,
    /// How submissions are verified.
    pub verification: VerificationPolicy,
    /// Budget ceiling.
    pub budget: Budget,
    /// Retry policy on failure.
    pub retry: TicketRetryPolicy,
    /// Scheduling priority.
    pub priority: i32,
}

/// One proposed dependency edge between two [`Ref`]s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedDependency {
    /// The dependent ticket's ref.
    pub from_ref: Ref,
    /// The dependency's ref.
    pub to_ref: Ref,
    /// Hard, soft or loop.
    pub kind: DependencyKind,
}

/// One proposed milestone, as a set of ticket [`Ref`]s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedMilestone {
    /// This milestone's local reference.
    pub milestone_ref: Ref,
    /// Milestone title.
    pub title: String,
    /// Member ticket refs.
    pub ticket_refs: Vec<Ref>,
}

/// A named authority domain applied across a set of tickets, so the proposal can express "these
/// N tickets share this authority" once instead of repeating it per [`ProposedTicket`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorityDomain {
    /// Domain name, e.g. `"core-crates"`.
    pub name: String,
    /// The authority this domain grants.
    pub authority: Authority,
    /// Ticket refs this domain applies to.
    pub applies_to: Vec<Ref>,
}

/// The full compiled-but-uncommitted graph proposal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphCompilation {
    /// The artifact id of the [`Specification`] this graph was compiled from, once persisted.
    pub source_spec: Option<ArtifactId>,
    /// Every proposed ticket.
    pub tickets: Vec<ProposedTicket>,
    /// Every proposed dependency edge.
    pub dependencies: Vec<ProposedDependency>,
    /// Every proposed milestone.
    pub milestones: Vec<ProposedMilestone>,
    /// Named authority domains, applied on top of each ticket's own `authority`.
    pub authority_domains: Vec<AuthorityDomain>,
    /// Which attempt this is, starting at `1`. Fed back into the prompt on retry along with the
    /// prior attempt's [`Violation`]s.
    pub attempt: u32,
}

/// Why compiling or committing a graph proposal failed.
#[derive(Debug, thiserror::Error)]
pub enum CompilationError {
    /// The proposal, projected onto the existing project state, violates one or more `tm-core`
    /// invariants.
    #[error("proposed graph violates {0} tm-core invariant(s)")]
    InvalidGraph(Vec<Violation>),
    /// A [`Ref`] used by a dependency, milestone membership or `parent_ref` does not name any
    /// [`ProposedTicket::ticket_ref`]/[`ProposedMilestone::milestone_ref`] in the same proposal.
    #[error("proposal references unknown ref {0:?}")]
    DanglingRef(Ref),
    /// [`compile_with_retry`] exhausted its bounded attempts without producing a valid graph.
    #[error("graph compilation exhausted {attempts} attempt(s), escalating")]
    Exhausted {
        /// How many attempts were made.
        attempts: u32,
        /// The violations from the final attempt.
        last_violations: Vec<Violation>,
    },
}

/// Ask `planner.frontier` to propose a [`GraphCompilation`] for `spec`. `attempt` and
/// `prior_violations` are empty/`1` on a first try; [`compile_with_retry`] fills them in on
/// retries so the model can see exactly what was wrong last time.
///
// IMPL: single `CompletionRequest` with `role = Role::PlannerFrontier`, prompt embedding
// `spec`'s requirements/architecture/interfaces/milestones/v0/v1 plus, on a retry, a rendering of
// `prior_violations` (invariant name + subject + detail) so the model can address the *specific*
// failure rather than regenerating blind. Parse structured JSON into `GraphCompilation` fields;
// every `Ref` used in `dependencies`/`milestones`/`parent_ref`/`milestone_ref`/
// `authority_domains.applies_to` must resolve to a `ticket_ref`/`milestone_ref` declared in the
// same response — check this here (not deferred to `validate_graph`, which only knows about
// `tm-core` invariants, not ref hygiene) and return `CompilationError::DanglingRef` eagerly.
// `source_spec` is left `None`, filled in by the caller once `spec` is persisted.
// Errors: `TmError::Provider` on completion failure, `TmError::Parse` on malformed output,
// `TmError::Invariant` wrapping `CompilationError::DanglingRef` on a dangling ref.
pub async fn propose_graph(
    spec: &Specification,
    prior_violations: &[Violation],
    attempt: u32,
    provider: &dyn tm_provider::Provider,
) -> TmResult<GraphCompilation> {
    let _ = (spec, prior_violations, attempt, provider);
    todo!("propose_graph: PlannerFrontier round-trip producing a GraphCompilation, feeding prior_violations back on retry, see module IMPL note")
}

/// Project `proposal` onto `existing` (as if every ticket/milestone/dependency/authority grant
/// it describes had already been committed, using placeholder [`TicketId`]/[`MilestoneId`]
/// values derived from each [`Ref`]) and run `tm_core::check_invariants` against the result.
///
// IMPL: pure. Build a scratch `ProjectView` cloned from `existing`; for each `ProposedTicket`
// synthesize a `tm_core::ticket::Ticket` in `TicketState::Draft` keyed by a deterministic
// placeholder `TicketId` (e.g. derived from `ticket_ref` — real ids are assigned later by
// `commit_graph`, but `check_invariants` only needs *some* stable id per proposed ticket, not the
// real one), resolve `parent_ref`/`milestone_ref` to those placeholders, insert
// `ProposedDependency` edges into `DependencyGraph`, and fold `AuthorityDomain` grants into each
// member ticket's `authority` via `Authority::intersect` (a domain narrows, never widens, a
// ticket's own declared authority — mirrors `tm-core`'s child-authority-contained invariant).
// Then call `tm_core::check_invariants(&scratch_view)` and return its `Vec<Violation>` verbatim
// (empty means the proposal is valid). Never mutates `existing`.
pub fn validate_graph(proposal: &GraphCompilation, existing: &ProjectView) -> Vec<Violation> {
    let _ = (proposal, existing);
    todo!("validate_graph: project proposal onto existing view and run tm_core::check_invariants, see module IMPL note")
}

/// What committing a validated [`GraphCompilation`] produced.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    /// Every event emitted while committing.
    pub events: Vec<tm_events::Event>,
    /// Real ticket ids, keyed by the [`Ref`] they were proposed under.
    pub tickets: BTreeMap<Ref, TicketId>,
    /// Real milestone ids, keyed by the [`Ref`] they were proposed under.
    pub milestones: BTreeMap<Ref, MilestoneId>,
}

/// Commit an already-[`validate_graph`]-clean `proposal` to `store` as one logical operation:
/// every milestone, then every ticket (parents before children, dependency order respected), then
/// every dependency edge.
///
// IMPL: I/O. `tm_core::Store`'s individual command methods (`create_ticket`, `create_milestone`,
// `add_dependency`) each commit their own SQLite transaction (see `store.rs` module docs); to
// honor "commit in ONE transaction or reject wholesale", this function must either (a) call an
// extended `Store` API this crate does not own (out of scope — `tm-core` is owned by another
// agent) that accepts a batch of drafts and commits them in a single `tm_events::log::Tx`, or (b)
// accept the current per-call transactions but make the *sequence* atomic from the caller's
// perspective by: creating milestones first, then tickets in topological (parent-before-child)
// order via `create_ticket`, then edges via `add_dependency`, and on any failure calling
// `store.cancel`/best-effort compensating deletes for everything already created in this call,
// then returning the original error. Prefer (a) once available; document the chosen approach at
// the top of the real implementation, since it is a load-bearing deviation from "one transaction"
// either way. `ticket_ref`/`milestone_ref` values become the keys of `CommitOutcome`; every
// `parent_ref`/`milestone_ref`/dependency `Ref` is resolved through the maps built as ids are
// allocated. `actor` is the participant recorded on every emitted event.
// Errors: whatever the underlying `Store` calls return; `TmError::invariant` if `proposal`
// contains a `Ref` unresolved after every ticket/milestone in it has been processed (should not
// happen if `propose_graph`'s dangling-ref check ran, but re-checked here as a last line of
// defense before committing).
pub fn commit_graph(
    store: &Store,
    proposal: &GraphCompilation,
    actor: ParticipantId,
) -> TmResult<CommitOutcome> {
    let _ = (store, proposal, actor);
    todo!("commit_graph: create milestones, then tickets in parent-before-child order, then dependency edges, atomically per module IMPL note")
}

/// Bounds on [`compile_with_retry`]'s regenerate-on-violation loop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    /// Maximum number of `propose_graph` attempts before escalating.
    pub max_attempts: u32,
}

impl RetryPolicy {
    /// A conservative default: try once, retry twice more with feedback, then escalate.
    pub fn default_bounded() -> Self {
        RetryPolicy { max_attempts: 3 }
    }
}

/// Compose [`propose_graph`], [`validate_graph`] and [`commit_graph`]: propose, validate against
/// `store`'s current view, and either commit or retry with the violations fed back, bounded by
/// `policy.max_attempts`. On exhaustion, records a `tm_core::Decision` requiring human input
/// (via `Store::record_decision`) rather than looping forever, and returns
/// `CompilationError::Exhausted`.
///
// IMPL: loop `attempt in 1..=policy.max_attempts`: call `propose_graph(spec, violations,
// attempt, provider)`, then `validate_graph(&proposal, &store.view()?)`; if empty, call
// `commit_graph(store, &proposal, actor.clone())` and return its `Ok`. If nonempty, stash the
// violations as `prior_violations` for the next loop iteration and continue. After the loop
// exits without success, call `store.record_decision` with `subject` = "genesis graph
// compilation exhausted retries", `decision` = a human-readable summary of the last attempt's
// violations, `reason` = "bounded retries exhausted, escalating to a human per SPEC.md §12", no
// evidence/affected_tickets/affected_paths, then return
// `TmError::invariant` wrapping a rendering of `CompilationError::Exhausted`.
#[allow(clippy::too_many_arguments)]
pub async fn compile_with_retry(
    spec: &Specification,
    provider: &dyn tm_provider::Provider,
    store: &Store,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    actor: ParticipantId,
    policy: &RetryPolicy,
) -> TmResult<CommitOutcome> {
    let _ = (spec, provider, store, clock, ids, actor, policy);
    todo!("compile_with_retry: propose/validate/commit loop bounded by policy.max_attempts, escalating via Store::record_decision on exhaustion, see module IMPL note")
}

impl From<CompilationError> for TmError {
    fn from(err: CompilationError) -> Self {
        TmError::invariant(err.to_string())
    }
}
