//! The Genesis stage machine, and the driver that advances it.
//!
//! `Seed -> Vision -> Spec -> GraphCompilation -> Ignition -> V0 -> Evaluation -> V1 ->
//! Stabilization -> MaturityGate -> (pass) AuthorityReconvergence -> SteadyState`, per
//! `SPEC.md` §12. [`transition`] is the pure legality check; [`GenesisState`] is the
//! resumable snapshot persisted as project state (an [`tm_types::ArtifactId`] pointer per
//! artifact-bearing stage, so a crash mid-stage loses at most the in-flight provider call, never
//! prior stages); [`GenesisDriver`] is the thin I/O layer that actually calls into
//! [`crate::seed`], [`crate::vision`], [`crate::spec`], [`crate::compile`] and
//! [`crate::maturity`] to advance one stage at a time.

use serde::{Deserialize, Serialize};
use tm_types::{ArtifactId, Clock, IdSource, LeaseId, ParticipantId, Result as TmResult, Timestamp};

use crate::compile::CommitOutcome;
use crate::ignition::IgnitionPolicy;
use crate::maturity::{MaturityGateResult, ReconvergenceOutcome};
use crate::seed::Seed;
use crate::spec::Specification;
use crate::vision::Vision;

/// One position in the Genesis stage machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// The raw prompt has been analyzed into a [`Seed`].
    Seed,
    /// A [`Vision`] has been compiled from the seed.
    Vision,
    /// A [`Specification`] has been compiled from the vision.
    Spec,
    /// A ticket graph has been compiled and committed from the spec.
    GraphCompilation,
    /// The project is running under an [`IgnitionPolicy`].
    Ignition,
    /// The V0 milestone has been reached.
    V0,
    /// V0 is being evaluated before proceeding toward V1.
    Evaluation,
    /// The V1 milestone has been reached.
    V1,
    /// The project is stabilizing (churn intentionally winding down) ahead of the maturity gate.
    Stabilization,
    /// The maturity gate is being (or has been) evaluated.
    MaturityGate,
    /// Genesis-scoped leases are being revoked and the ignition policy is being replaced.
    AuthorityReconvergence,
    /// Genesis is complete; the project runs under [`crate::ignition::SteadyStatePolicy`].
    SteadyState,
}

impl Stage {
    /// Every stage, in the order `SPEC.md` §12 lists them.
    pub const ALL: &'static [Stage] = &[
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
    ];

    /// The stages `self` may legally transition to, given a qualifying [`StageEvent`]. A
    /// `MaturityGate` failure legally stays in `Stabilization` rather than advancing (see
    /// [`transition`]'s handling of `StageEvent::MaturityEvaluated` with `passed == false`), so
    /// `Stabilization` appears in its own `legal_next` for that case.
    pub fn legal_next(self) -> &'static [Stage] {
        match self {
            Stage::Seed => &[Stage::Vision],
            Stage::Vision => &[Stage::Spec],
            Stage::Spec => &[Stage::GraphCompilation],
            Stage::GraphCompilation => &[Stage::Ignition],
            Stage::Ignition => &[Stage::V0],
            Stage::V0 => &[Stage::Evaluation],
            Stage::Evaluation => &[Stage::V1],
            Stage::V1 => &[Stage::Stabilization],
            Stage::Stabilization => &[Stage::MaturityGate],
            Stage::MaturityGate => &[Stage::AuthorityReconvergence, Stage::Stabilization],
            Stage::AuthorityReconvergence => &[Stage::SteadyState],
            Stage::SteadyState => &[],
        }
    }
}

/// The event produced by completing one stage, carrying just enough of that stage's artifact to
/// drive the transition and update [`GenesisState`]. Full artifacts are persisted separately
/// (via `tm_core::Store::store_artifact`); this enum is the pure transition input, not the
/// storage format.
#[derive(Debug, Clone, PartialEq)]
pub enum StageEvent {
    /// `Seed` completed.
    SeedCreated(Seed),
    /// `Vision` completed.
    VisionCompiled(Vision),
    /// `Spec` completed.
    SpecCompiled(Specification),
    /// `GraphCompilation` completed: the graph validated and committed.
    GraphCommitted(CommitOutcome),
    /// `Ignition` started under this policy.
    IgnitionStarted(IgnitionPolicy),
    /// `V0` reached: the V0 milestone closed.
    V0Reached,
    /// `Evaluation` recorded its findings, in prose.
    EvaluationRecorded(String),
    /// `V1` reached: the V1 milestone closed.
    V1Reached,
    /// `Stabilization` entered.
    StabilizationEntered,
    /// `MaturityGate` evaluated, with the verdict.
    MaturityEvaluated(MaturityGateResult),
    /// `AuthorityReconvergence` completed.
    AuthorityReconverged(ReconvergenceOutcome),
    /// `SteadyState` entered.
    SteadyStateEntered,
}

/// [`transition`] refused to move `from` given the given event.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("illegal Genesis transition from {from:?} via this event")]
pub struct IllegalTransition {
    /// The stage the machine was in.
    pub from: Stage,
}

/// The pure transition function: given the stage `from` and a completed `event`, return the
/// next legal [`Stage`], or [`IllegalTransition`] if `event` does not belong to `from`.
///
// IMPL: match `(from, event)` exhaustively against `Stage::legal_next(from)`. Every arm besides
// `MaturityGate` maps its single expected `StageEvent` variant to its single `legal_next` entry
// and rejects every other variant. `(Stage::MaturityGate, StageEvent::MaturityEvaluated(result))`
// is the one branch: `result.passed` selects `Stage::AuthorityReconvergence`, `!result.passed`
// selects `Stage::Stabilization` (gate failure keeps the project in `Stabilization`, recording
// why via the caller's own logging/decision, not via this pure function). Any `(from, event)`
// pair not matching `from`'s expected event kind returns `Err(IllegalTransition { from })`.
pub fn transition(from: Stage, event: &StageEvent) -> Result<Stage, IllegalTransition> {
    let _ = (from, event);
    todo!("transition: match (from, event) against Stage::legal_next, MaturityGate branches on result.passed, see module IMPL note")
}

/// The resumable snapshot of where a project's Genesis run currently stands. Persisted as
/// project state (via genesis.* events referencing stored artifacts); rebuildable by
/// [`GenesisDriver::resume`] after a crash without redoing completed stages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenesisState {
    /// The project's name/slug, for `genesis.started`'s `project` field.
    pub project: String,
    /// The current stage.
    pub stage: Stage,
    /// The persisted [`Seed`] artifact, once `Stage::Seed` has completed.
    pub seed: Option<ArtifactId>,
    /// The persisted [`Vision`] artifact, once `Stage::Vision` has completed.
    pub vision: Option<ArtifactId>,
    /// The persisted [`Specification`] artifact, once `Stage::Spec` has completed.
    pub spec: Option<ArtifactId>,
    /// The persisted [`crate::compile::GraphCompilation`] artifact, once `Stage::GraphCompilation`
    /// has committed.
    pub graph: Option<ArtifactId>,
    /// The active ignition policy, once `Stage::Ignition` has started; replaced by a
    /// [`crate::ignition::SteadyStatePolicy`] (tracked outside `GenesisState` once Genesis
    /// completes, since `Stage::SteadyState` has no further Genesis-owned state to resume into).
    pub ignition: Option<IgnitionPolicy>,
    /// Leases known to be Genesis-scoped, accumulated as `Stage::Ignition` runs, consumed by
    /// [`crate::maturity::reconverge_authority`].
    pub genesis_leases: Vec<LeaseId>,
    /// When this snapshot was last updated.
    pub updated: Timestamp,
}

impl GenesisState {
    /// A fresh `GenesisState` at `Stage::Seed`, with nothing else populated yet.
    pub fn new(project: String, clock: &dyn Clock) -> Self {
        GenesisState {
            project,
            stage: Stage::Seed,
            seed: None,
            vision: None,
            spec: None,
            graph: None,
            ignition: None,
            genesis_leases: Vec::new(),
            updated: clock.now(),
        }
    }
}

/// The thin driver that advances a [`GenesisState`] one stage at a time by calling into
/// [`crate::seed`], [`crate::vision`], [`crate::spec`], [`crate::compile`] and
/// [`crate::maturity`], persisting each stage's artifact before advancing.
pub struct GenesisDriver<'a> {
    store: &'a tm_core::Store,
    provider: &'a dyn tm_provider::Provider,
    clock: &'a dyn Clock,
    ids: &'a dyn IdSource,
}

impl<'a> GenesisDriver<'a> {
    /// Build a driver over the given collaborators. Borrows for the driver's lifetime; callers
    /// construct a fresh one per Genesis run (or per resumed session).
    pub fn new(
        store: &'a tm_core::Store,
        provider: &'a dyn tm_provider::Provider,
        clock: &'a dyn Clock,
        ids: &'a dyn IdSource,
    ) -> Self {
        GenesisDriver { store, provider, clock, ids }
    }

    /// Advance `state` by exactly one stage: run the current stage's work, persist its artifact
    /// (via `tm_core::Store::store_artifact`, `ArtifactKind::Report`), emit
    /// `genesis.stage_completed`, compute the next stage via [`transition`], and return the
    /// updated snapshot. Idempotent to call again after a crash: [`GenesisDriver::resume`]
    /// reconstructs `state` from the last successfully persisted artifact, so at most one
    /// stage's provider call is ever redone.
    ///
    // IMPL: match on `state.stage`; dispatch to the matching stage function
    // (`seed::analyze_prompt`, `vision::compile_vision`, `spec::compile_spec`,
    // `compile::compile_with_retry`, maturity's `evaluate_predicate` + `judge_maturity` + `gate`
    // for `MaturityGate`, `maturity::reconverge_authority` for `AuthorityReconvergence`); for
    // artifact-bearing stages, serialize the result to JSON (`serde_json::to_vec`) and call
    // `store.store_artifact(ArtifactKind::Report, "application/json", bytes, meta, None, actor)`,
    // recording the returned `ArtifactId` on `state`; build the corresponding `StageEvent` and
    // call `transition(state.stage, &event)` to get the next stage; emit
    // `genesis.stage_completed` (`stage: state.stage.as_str()`ish — `Stage` needs a
    // `Display`/string form for this, add one if missing) via `store`'s underlying
    // `tm_events::EventLog`; return the updated `GenesisState` with `updated` restamped from
    // `clock`. `V0`/`Evaluation`/`V1`/`Stabilization`/`SteadyState` have no dedicated artifact
    // (they're checkpoints over already-committed ticket/milestone state), so those arms only
    // emit `genesis.stage_entered`/`genesis.stage_completed` and transition. `ids` provisions any
    // ids this driver itself needs to allocate outside what `Store` already allocates (e.g. a
    // correlation id grouping one stage's events).
    // Errors: whatever the dispatched stage function returns; `TmError::invariant` if
    // `transition` refuses the computed `StageEvent` (a driver bug, not a caller error, but still
    // surfaced rather than panicking).
    pub async fn advance(&self, state: &GenesisState, actor: ParticipantId) -> TmResult<GenesisState> {
        let _ = (state, actor);
        todo!("advance: dispatch state.stage to the matching stage function, persist its artifact, transition, return updated GenesisState, see module IMPL note")
    }

    /// Rebuild a [`GenesisState`] from `store`'s event log after a crash or restart, by scanning
    /// for the most recent `genesis.stage_completed` event and the artifact ids it (and prior
    /// stages) reference.
    ///
    // IMPL: read the project's event log (`store` exposes `view()`/the underlying log via
    // whatever accessor `tm-core::Store` provides for replaying arbitrary event kinds — if none
    // exists publicly, open the same `tm_events::EventLog` `Store::open` would, read-only, at
    // `store`'s known path) filtering for `EventKind::Genesis*`; fold them in `seq` order into a
    // `GenesisState`, starting from `GenesisState::new` and applying each `genesis.stage_entered`/
    // `stage_completed`/`assumption_recorded`/`maturity_evaluated`/`completed` event to update
    // `stage`/`seed`/`vision`/`spec`/`graph`/`ignition`/`genesis_leases` fields as they would have
    // been set by `advance`. `project` comes from the first `genesis.started` event.
    // Errors: `TmError::not_found` if no `genesis.started` event exists for this project (nothing
    // to resume); whatever the log read returns on I/O failure.
    pub fn resume(store: &'a tm_core::Store) -> TmResult<GenesisState> {
        let _ = store;
        todo!("resume: replay genesis.* events into a GenesisState, see module IMPL note")
    }
}
