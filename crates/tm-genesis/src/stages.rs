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
//!
//! A note on persistence: the driver below persists every stage artifact, and a full
//! [`GenesisState`] snapshot after every [`GenesisDriver::advance`] call, via
//! `tm_core::Store::store_artifact`. `tm_core::Store` (finished, not this crate's to extend)
//! exposes no accessor for its project root or its underlying `tm_events::EventLog`, so this
//! module cannot append bespoke `genesis.*` events the way the module-level design note
//! originally sketched; instead each snapshot *is* the resumability contract; [`GenesisDriver::resume`]
//! finds the most recently updated one by scanning `Store::view`'s artifacts, which is the
//! read path `tm_core` does expose.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tm_core::{ArtifactKind, ArtifactStorage};
use tm_types::{
    ArtifactId, Clock, IdSource, LeaseId, MilestoneId, ParticipantId, Result as TmResult, Role,
    TicketId, Timestamp, TmError,
};

use crate::compile::{CommitOutcome, Ref, RetryPolicy as GraphRetryPolicy};
use crate::ignition::IgnitionPolicy;
use crate::maturity::{MaturityGateResult, MaturityThresholds, ReconvergenceOutcome};
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

    /// This stage's stable, lowercase-with-underscores name, matching its serde form (used for
    /// display/logging, since a crash-recovery driver needs a human-legible stage label
    /// somewhere and this is the one the enum already commits to via `#[serde(rename_all)]`).
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Seed => "seed",
            Stage::Vision => "vision",
            Stage::Spec => "spec",
            Stage::GraphCompilation => "graph_compilation",
            Stage::Ignition => "ignition",
            Stage::V0 => "v0",
            Stage::Evaluation => "evaluation",
            Stage::V1 => "v1",
            Stage::Stabilization => "stabilization",
            Stage::MaturityGate => "maturity_gate",
            Stage::AuthorityReconvergence => "authority_reconvergence",
            Stage::SteadyState => "steady_state",
        }
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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
#[error("can't leave the {from} stage with this event")]
pub struct IllegalTransition {
    /// The stage the machine was in.
    pub from: Stage,
}

/// The pure transition function: given the stage `from` and a completed `event`, return the
/// next legal [`Stage`], or [`IllegalTransition`] if `event` does not belong to `from`.
pub fn transition(from: Stage, event: &StageEvent) -> Result<Stage, IllegalTransition> {
    match (from, event) {
        (Stage::Seed, StageEvent::SeedCreated(_)) => Ok(Stage::Vision),
        (Stage::Vision, StageEvent::VisionCompiled(_)) => Ok(Stage::Spec),
        (Stage::Spec, StageEvent::SpecCompiled(_)) => Ok(Stage::GraphCompilation),
        (Stage::GraphCompilation, StageEvent::GraphCommitted(_)) => Ok(Stage::Ignition),
        (Stage::Ignition, StageEvent::IgnitionStarted(_)) => Ok(Stage::V0),
        (Stage::V0, StageEvent::V0Reached) => Ok(Stage::Evaluation),
        (Stage::Evaluation, StageEvent::EvaluationRecorded(_)) => Ok(Stage::V1),
        (Stage::V1, StageEvent::V1Reached) => Ok(Stage::Stabilization),
        (Stage::Stabilization, StageEvent::StabilizationEntered) => Ok(Stage::MaturityGate),
        (Stage::MaturityGate, StageEvent::MaturityEvaluated(result)) => {
            if result.passed {
                Ok(Stage::AuthorityReconvergence)
            } else {
                Ok(Stage::Stabilization)
            }
        }
        (Stage::AuthorityReconvergence, StageEvent::AuthorityReconverged(_)) => {
            Ok(Stage::SteadyState)
        }
        _ => Err(IllegalTransition { from }),
    }
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

/// A local, JSON-serializable projection of [`CommitOutcome`], used only to persist a
/// `Stage::GraphCompilation` artifact. `CommitOutcome` itself carries raw `tm_events::Event`s,
/// which are not `Serialize`; the events aren't needed to resume, only the resolved ids are.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GraphSummary {
    tickets: BTreeMap<Ref, TicketId>,
    milestones: BTreeMap<Ref, MilestoneId>,
    /// The [`crate::compile::CommitOutcome::selected_template`] this graph was compiled with, if
    /// any. `#[serde(default)]` so a `GraphSummary` persisted before this field existed still
    /// deserializes (as `None`).
    #[serde(default)]
    selected_template: Option<String>,
}

const META_FIELD_KEY: &str = "genesis_field";
const META_SNAPSHOT_KEY: &str = "genesis_snapshot";

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
        GenesisDriver {
            store,
            provider,
            clock,
            ids,
        }
    }

    /// Persist `value` as a `Report` artifact tagged with `field` (e.g. `"seed"`), returning the
    /// resulting [`ArtifactId`].
    fn persist_field<T: Serialize>(
        &self,
        value: &T,
        field: &'static str,
        actor: ParticipantId,
    ) -> TmResult<ArtifactId> {
        let bytes = serde_json::to_vec(value)?;
        let meta = serde_json::json!({ META_FIELD_KEY: field });
        let events = self.store.store_artifact(
            ArtifactKind::Report,
            "application/json".to_string(),
            bytes,
            meta,
            None,
            actor,
        )?;
        events
            .iter()
            .find_map(|e| e.payload.as_artifact_created().map(|p| p.artifact.clone()))
            .ok_or_else(|| TmError::invariant("saving that didn't produce a stored artifact"))
    }

    /// Persist a full `state` snapshot, the mechanism [`GenesisDriver::resume`] reads back.
    fn persist_snapshot(&self, state: &GenesisState, actor: ParticipantId) -> TmResult<()> {
        let bytes = serde_json::to_vec(state)?;
        let meta = serde_json::json!({ META_SNAPSHOT_KEY: true });
        self.store.store_artifact(
            ArtifactKind::Report,
            "application/json".to_string(),
            bytes,
            meta,
            None,
            actor,
        )?;
        Ok(())
    }

    /// Load and deserialize a previously-[`GenesisDriver::persist_field`]ed artifact.
    fn load_field<T: serde::de::DeserializeOwned>(&self, id: &ArtifactId) -> TmResult<T> {
        let view = self.store.view()?;
        let artifact = view
            .artifacts
            .get(id)
            .ok_or_else(|| TmError::not_found("artifact", id.as_str()))?;
        match &artifact.storage {
            ArtifactStorage::Inline(bytes) => Ok(serde_json::from_slice(bytes)?),
            ArtifactStorage::OnDisk(_) => Err(TmError::storage(
                "can't read this artifact yet: it was stored on disk, and this step only reads artifacts stored inline",
            )),
        }
    }

    /// Advance `state` by exactly one stage: run the current stage's work, persist its artifact
    /// (via `tm_core::Store::store_artifact`, `ArtifactKind::Report`), compute the next stage via
    /// [`transition`], persist the updated snapshot, and return it. Idempotent to call again
    /// after a crash: [`GenesisDriver::resume`] reconstructs `state` from the last successfully
    /// persisted snapshot, so at most one stage's provider call is ever redone.
    ///
    /// # Errors
    /// Whatever the dispatched stage function returns; `TmError::invariant` if the computed
    /// [`StageEvent`] does not belong to `state.stage` (a driver bug, not a caller error).
    pub async fn advance(
        &self,
        state: &GenesisState,
        actor: ParticipantId,
    ) -> TmResult<GenesisState> {
        let mut next_state = state.clone();
        let illegal = |e: IllegalTransition| TmError::invariant(e.to_string());

        let next_stage = match state.stage {
            Stage::Seed => {
                let seed =
                    crate::seed::analyze_prompt(state.project.clone(), self.provider, self.clock)
                        .await?;
                let id = self.persist_field(&seed, "seed", actor.clone())?;
                next_state.seed = Some(id);
                transition(state.stage, &StageEvent::SeedCreated(seed)).map_err(illegal)?
            }
            Stage::Vision => {
                let seed_id = state.seed.clone().ok_or_else(|| {
                    TmError::invariant(
                        "reached the Vision stage without a saved Seed to build from",
                    )
                })?;
                let seed: Seed = self.load_field(&seed_id)?;
                let mut vision =
                    crate::vision::compile_vision(&seed, self.provider, self.clock).await?;
                vision.source_seed = Some(seed_id);
                let id = self.persist_field(&vision, "vision", actor.clone())?;
                next_state.vision = Some(id);
                transition(state.stage, &StageEvent::VisionCompiled(vision)).map_err(illegal)?
            }
            Stage::Spec => {
                let vision_id = state.vision.clone().ok_or_else(|| {
                    TmError::invariant(
                        "reached the Spec stage without a saved Vision to build from",
                    )
                })?;
                let vision: Vision = self.load_field(&vision_id)?;
                let mut spec =
                    crate::spec::compile_spec(&vision, self.provider, self.clock).await?;
                spec.source_vision = Some(vision_id);
                let id = self.persist_field(&spec, "spec", actor.clone())?;
                next_state.spec = Some(id);
                transition(state.stage, &StageEvent::SpecCompiled(spec)).map_err(illegal)?
            }
            Stage::GraphCompilation => {
                let spec_id = state.spec.clone().ok_or_else(|| {
                    TmError::invariant(
                        "reached graph compilation without a saved Specification to compile from",
                    )
                })?;
                let spec: Specification = self.load_field(&spec_id)?;
                let outcome = crate::compile::compile_with_retry(
                    &spec,
                    self.provider,
                    self.store,
                    self.clock,
                    self.ids,
                    actor.clone(),
                    // No template catalog wired into `GenesisDriver` yet — an empty slice makes
                    // `compile::select_template` a no-op, so this stage's behavior is unchanged
                    // from before `compile_with_retry` grew this parameter (`SPEC.md` §27.1's
                    // template-selection path is additive; wiring an actual catalog in here is
                    // follow-up work, not part of this change).
                    &[],
                    &GraphRetryPolicy::default_bounded(),
                )
                .await?;
                let summary = GraphSummary {
                    tickets: outcome.tickets.clone(),
                    milestones: outcome.milestones.clone(),
                    selected_template: outcome.selected_template.clone(),
                };
                let id = self.persist_field(&summary, "graph", actor.clone())?;
                next_state.graph = Some(id);
                transition(state.stage, &StageEvent::GraphCommitted(outcome)).map_err(illegal)?
            }
            Stage::Ignition => {
                let graph_id = state.graph.clone().ok_or_else(|| {
                    TmError::invariant(
                        "reached the Ignition stage without a committed ticket graph",
                    )
                })?;
                let summary: GraphSummary = self.load_field(&graph_id)?;
                // Approximation: the compiled graph doesn't name "the" V0 milestone distinctly
                // from any other, so the first (by `Ref`) committed milestone stands in for it.
                let v0 = summary.milestones.values().next().cloned().ok_or_else(|| {
                    TmError::invariant("graph compilation didn't produce a milestone to start from")
                })?;
                let policy = IgnitionPolicy::for_v0(v0);
                next_state.ignition = Some(policy.clone());
                transition(state.stage, &StageEvent::IgnitionStarted(policy)).map_err(illegal)?
            }
            Stage::V0 => transition(state.stage, &StageEvent::V0Reached).map_err(illegal)?,
            Stage::Evaluation => {
                let findings = format!(
                    "Reached the V0 milestone for project {}. Recording a checkpoint before moving on toward V1.",
                    state.project
                );
                transition(state.stage, &StageEvent::EvaluationRecorded(findings))
                    .map_err(illegal)?
            }
            Stage::V1 => transition(state.stage, &StageEvent::V1Reached).map_err(illegal)?,
            Stage::Stabilization => {
                transition(state.stage, &StageEvent::StabilizationEntered).map_err(illegal)?
            }
            Stage::MaturityGate => {
                let view = self.store.view()?;
                let v0 = state
                    .ignition
                    .as_ref()
                    .map(|p| p.v0_objective_milestone.clone());
                // Approximation: `GenesisState` carries no dedicated "V1 milestone" field, so the
                // first milestone distinct from the ignition's V0 objective stands in for V1 (or
                // the same milestone, if there is only one).
                let v1 = view
                    .milestones
                    .keys()
                    .find(|id| Some((*id).clone()) != v0)
                    .or_else(|| view.milestones.keys().next())
                    .cloned()
                    .ok_or_else(|| {
                        TmError::invariant(
                            "no milestone exists yet for the maturity gate to evaluate",
                        )
                    })?;
                let thresholds = MaturityThresholds::conservative();
                let predicate = crate::maturity::evaluate_predicate(&view, &v1, thresholds);
                let judgment = crate::maturity::judge_maturity(
                    &view,
                    &predicate,
                    Role::ArchitectFrontier,
                    self.provider,
                    self.store,
                    self.clock,
                    actor.clone(),
                )
                .await?;
                let result = crate::maturity::gate(predicate, judgment);
                transition(state.stage, &StageEvent::MaturityEvaluated(result)).map_err(illegal)?
            }
            Stage::AuthorityReconvergence => {
                let ignition = state.ignition.clone().ok_or_else(|| {
                    TmError::invariant(
                        "reached authority reconvergence without an active ignition policy",
                    )
                })?;
                let outcome = crate::maturity::reconverge_authority(
                    self.store,
                    &state.genesis_leases,
                    &ignition,
                    actor.clone(),
                )?;
                next_state.ignition = None;
                next_state.genesis_leases.clear();
                transition(state.stage, &StageEvent::AuthorityReconverged(outcome))
                    .map_err(illegal)?
            }
            Stage::SteadyState => {
                transition(state.stage, &StageEvent::SteadyStateEntered).map_err(illegal)?
            }
        };

        next_state.stage = next_stage;
        next_state.updated = self.clock.now();
        self.persist_snapshot(&next_state, actor)?;
        Ok(next_state)
    }

    /// Rebuild a [`GenesisState`] from `store`'s persisted artifacts after a crash or restart, by
    /// scanning for the most recently updated snapshot [`GenesisDriver::persist_snapshot`] wrote.
    ///
    /// # Errors
    /// `TmError::not_found` if no snapshot artifact exists (nothing to resume). Whatever
    /// `Store::view` returns on I/O failure, or a JSON error if a snapshot is corrupt.
    pub fn resume(store: &'a tm_core::Store) -> TmResult<GenesisState> {
        let view = store.view()?;
        let mut latest: Option<GenesisState> = None;
        for artifact in view.artifacts.values() {
            if artifact
                .meta
                .get(META_SNAPSHOT_KEY)
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            {
                continue;
            }
            let bytes = match &artifact.storage {
                ArtifactStorage::Inline(bytes) => bytes,
                // Skip: unreadable without a project-root accessor (see the module-level note);
                // an in-range snapshot is inline anyway, since `GenesisState` is small JSON.
                ArtifactStorage::OnDisk(_) => continue,
            };
            let candidate: GenesisState = serde_json::from_slice(bytes)?;
            let replace = match &latest {
                Some(current) => candidate.updated > current.updated,
                None => true,
            };
            if replace {
                latest = Some(candidate);
            }
        }
        latest.ok_or_else(|| {
            TmError::not_found("genesis_state", "this project hasn't started Genesis yet")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ignition::SteadyStatePolicy;
    use crate::maturity::{MaturityJudgment, MaturityPredicate};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tm_types::{CounterIds, DecisionId, FixedClock, Tolerance};

    fn actor() -> ParticipantId {
        ParticipantId::new("agent:test/genesis").expect("valid participant id")
    }

    fn sample_seed(clock: &dyn Clock) -> Seed {
        Seed::new("build me a thing".to_string(), clock)
    }

    fn sample_ignition() -> IgnitionPolicy {
        IgnitionPolicy::for_v0(MilestoneId::new("M-000000000001").expect("valid milestone id"))
    }

    fn sample_gate_result(passed: bool) -> MaturityGateResult {
        let predicate = MaturityPredicate {
            end_to_end_artifact_exists: passed,
            v1_closed: passed,
            verification_pass_rate: 1.0,
            spec_churn_rate: 0.0,
            open_structural_audits: 0,
            thresholds: MaturityThresholds::conservative(),
        };
        let judgment = MaturityJudgment {
            decision: DecisionId::new("D-000000000001").expect("valid decision id"),
            mature: passed,
            rationale: "test judgment".to_string(),
        };
        crate::maturity::gate(predicate, judgment)
    }

    fn sample_reconvergence() -> ReconvergenceOutcome {
        ReconvergenceOutcome {
            revoked_leases: Vec::new(),
            steady_state: SteadyStatePolicy {
                default_authority: tm_types::Authority::default(),
                exploratory_code_tolerance: Tolerance::Strict,
                fan_out_width: 1,
            },
            events: Vec::new(),
        }
    }

    fn open_store() -> (TempDir, tm_core::Store) {
        let dir = TempDir::new().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = tm_core::Store::open_with(dir.path(), clock, ids).expect("open store");
        (dir, store)
    }

    #[test]
    fn legal_next_lists_every_stage_once_except_steady_state() {
        for stage in Stage::ALL {
            if *stage == Stage::SteadyState {
                assert!(stage.legal_next().is_empty());
            } else {
                assert!(!stage.legal_next().is_empty());
            }
        }
    }

    #[test]
    fn transition_walks_the_full_happy_path() {
        let clock = FixedClock::epoch();
        let seed = sample_seed(&clock);
        assert_eq!(
            transition(Stage::Seed, &StageEvent::SeedCreated(seed)),
            Ok(Stage::Vision)
        );

        let vision = Vision {
            source_seed: None,
            product_thesis: String::new(),
            user_experience: String::new(),
            taste: String::new(),
            governing_constraints: Vec::new(),
            identity: String::new(),
            non_goals: Vec::new(),
            architectural_character: String::new(),
            spiritually_wrong: Vec::new(),
            created: Timestamp::EPOCH,
        };
        assert_eq!(
            transition(Stage::Vision, &StageEvent::VisionCompiled(vision)),
            Ok(Stage::Spec)
        );

        assert_eq!(
            transition(Stage::V0, &StageEvent::V0Reached),
            Ok(Stage::Evaluation)
        );
        assert_eq!(
            transition(
                Stage::Evaluation,
                &StageEvent::EvaluationRecorded("looks fine".into())
            ),
            Ok(Stage::V1)
        );
        assert_eq!(
            transition(Stage::V1, &StageEvent::V1Reached),
            Ok(Stage::Stabilization)
        );
        assert_eq!(
            transition(Stage::Stabilization, &StageEvent::StabilizationEntered),
            Ok(Stage::MaturityGate)
        );

        let policy = sample_ignition();
        assert_eq!(
            transition(Stage::Ignition, &StageEvent::IgnitionStarted(policy)),
            Ok(Stage::V0)
        );

        let outcome = sample_reconvergence();
        assert_eq!(
            transition(
                Stage::AuthorityReconvergence,
                &StageEvent::AuthorityReconverged(outcome)
            ),
            Ok(Stage::SteadyState)
        );
    }

    #[test]
    fn maturity_gate_passing_advances_to_authority_reconvergence() {
        let result = sample_gate_result(true);
        assert_eq!(
            transition(Stage::MaturityGate, &StageEvent::MaturityEvaluated(result)),
            Ok(Stage::AuthorityReconvergence)
        );
    }

    #[test]
    fn maturity_gate_failing_stays_in_stabilization() {
        let result = sample_gate_result(false);
        assert_eq!(
            transition(Stage::MaturityGate, &StageEvent::MaturityEvaluated(result)),
            Ok(Stage::Stabilization)
        );
    }

    #[test]
    fn transition_rejects_an_event_that_does_not_belong_to_the_stage() {
        let err = transition(Stage::Seed, &StageEvent::V0Reached).unwrap_err();
        assert_eq!(err, IllegalTransition { from: Stage::Seed });
    }

    #[test]
    fn transition_rejects_any_event_from_steady_state() {
        let err = transition(Stage::SteadyState, &StageEvent::SteadyStateEntered).unwrap_err();
        assert_eq!(err.from, Stage::SteadyState);
    }

    #[test]
    fn stage_as_str_matches_its_serde_rename() {
        let json = serde_json::to_string(&Stage::GraphCompilation).unwrap();
        assert_eq!(json, format!("\"{}\"", Stage::GraphCompilation.as_str()));
    }

    #[test]
    fn genesis_state_new_starts_at_seed_with_nothing_populated() {
        let clock = FixedClock::epoch();
        let state = GenesisState::new("demo".to_string(), &clock);
        assert_eq!(state.stage, Stage::Seed);
        assert!(state.seed.is_none());
        assert!(state.vision.is_none());
        assert!(state.spec.is_none());
        assert!(state.graph.is_none());
        assert!(state.ignition.is_none());
        assert!(state.genesis_leases.is_empty());
    }

    #[test]
    fn resume_fails_not_found_when_no_snapshot_exists() {
        let (_dir, store) = open_store();
        let err = GenesisDriver::resume(&store).unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }

    #[test]
    fn resume_picks_the_most_recently_updated_snapshot() {
        let (_dir, store) = open_store();

        let older = GenesisState {
            project: "demo".to_string(),
            stage: Stage::Vision,
            seed: None,
            vision: None,
            spec: None,
            graph: None,
            ignition: None,
            genesis_leases: Vec::new(),
            updated: Timestamp::EPOCH,
        };
        let mut newer = older.clone();
        newer.stage = Stage::Spec;
        newer.updated = Timestamp::from_unix_nanos(1_000_000_000);

        for state in [&older, &newer] {
            let bytes = serde_json::to_vec(state).unwrap();
            store
                .store_artifact(
                    ArtifactKind::Report,
                    "application/json".to_string(),
                    bytes,
                    serde_json::json!({ META_SNAPSHOT_KEY: true }),
                    None,
                    actor(),
                )
                .expect("store snapshot");
        }
        // A non-snapshot artifact must not confuse resume().
        store
            .store_artifact(
                ArtifactKind::Report,
                "application/json".to_string(),
                b"{}".to_vec(),
                serde_json::json!({}),
                None,
                actor(),
            )
            .expect("store unrelated artifact");

        let resumed = GenesisDriver::resume(&store).expect("resume");
        assert_eq!(resumed.stage, Stage::Spec);
        assert_eq!(resumed.updated, newer.updated);
    }

    #[tokio::test]
    async fn advance_persists_a_seed_and_moves_to_vision() {
        use tm_provider::mock::MockProvider;
        use tm_provider::types::{
            Candidate, Completion, CompletionRequest, ContentBlock, Message, MessageRole, ModelId,
            StopReason, Usage,
        };

        let (_dir, store) = open_store();
        let clock = FixedClock::epoch();
        let ids = CounterIds::new();
        let clock_arc: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let provider = MockProvider::new("test", ModelId::new("test", "model"), clock_arc);

        let raw_prompt = "build a tiny todo app".to_string();
        let system = Some(
            "You are an expert project analyst. Extract and infer structured metadata from a user's prompt. \
             Return a JSON object with this exact structure (all fields required, arrays may be empty):\n\
             {\n  \"explicit_constraints\": [{\"text\": \"...\", \"rationale\": \"\"}],\n  \
             \"inferred_constraints\": [{\"text\": \"...\", \"rationale\": \"...\"}],\n  \
             \"assumptions\": [{\"text\": \"...\", \"rationale\": \"...\", \"confidence\": 0.8}],\n  \
             \"unresolved_questions\": [{\"text\": \"...\", \"blocking\": true, \"rationale\": \"...\", \"resolution\": null}]\n\
             }\n\n\
             Explicit constraints are stated directly in the prompt. \
             Inferred constraints come from domain conventions or implied expectations. \
             Assumptions are reasonable defaults you adopt instead of asking. \
             Questions with blocking=true should be rare—only when no reasonable assumption exists. \
             Questions with blocking=false should instead become assumptions for bias to action."
                .into(),
        );
        let req = CompletionRequest {
            system,
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: format!(
                        "Analyze this project prompt and extract all constraints, assumptions, and questions:\n\n{}",
                        raw_prompt
                    ),
                }],
            }],
            tools: vec![],
            max_tokens: 2048,
            temperature: Some(0.2),
            stop_sequences: vec![],
            stream: false,
            n: 1,
            model: None,
        };
        let completion = Completion {
            model: ModelId::new("test", "model"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: serde_json::json!({
                        "explicit_constraints": [],
                        "inferred_constraints": [],
                        "assumptions": [],
                        "unresolved_questions": [],
                    })
                    .to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: std::time::Duration::from_millis(1),
            received_at: Timestamp::EPOCH,
        };
        provider.script_response(&req, completion);

        let driver = GenesisDriver::new(&store, &provider, &clock, &ids);
        let state = GenesisState::new(raw_prompt, &clock);

        let advanced = driver
            .advance(&state, actor())
            .await
            .expect("advance past seed");
        assert_eq!(advanced.stage, Stage::Vision);
        assert!(advanced.seed.is_some());

        let resumed = GenesisDriver::resume(&store).expect("resume after advance");
        assert_eq!(resumed, advanced);
    }
}
