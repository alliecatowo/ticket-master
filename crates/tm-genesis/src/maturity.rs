//! The maturity gate: deterministic predicate plus one frontier judgment, both required.
//!
//! `SPEC.md` §12 requires *both* a machine-checkable predicate over project state
//! ([`MaturityPredicate`]) and a frontier judgment call recorded as a [`tm_core::Decision`]
//! ([`MaturityJudgment`]) to agree before Genesis leaves `Stabilization`. Passing swaps
//! [`crate::ignition::IgnitionPolicy`] for [`crate::ignition::SteadyStatePolicy`] and revokes
//! every Genesis-scoped lease ([`reconverge_authority`]) — authority genuinely narrows, it
//! doesn't just stop being checked.

use serde::{Deserialize, Serialize};
use tm_core::{ProjectView, Store};
use tm_types::{Clock, DecisionId, LeaseId, MilestoneId, ParticipantId, Result as TmResult, Role};

use crate::ignition::{IgnitionPolicy, SteadyStatePolicy};

/// Thresholds the deterministic predicate is evaluated against.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MaturityThresholds {
    /// Minimum acceptable verification pass rate, `0.0..=1.0`, over the trailing window.
    pub verification_pass_rate: f64,
    /// Maximum acceptable spec churn rate (fraction of spec content changed per unit time,
    /// however the caller chooses to normalize it), `0.0..=1.0`.
    pub spec_churn_rate: f64,
    /// How many of the most recent verification outcomes to consider for
    /// `verification_pass_rate`.
    pub verification_window: usize,
}

impl MaturityThresholds {
    /// A conservative default: 90% pass rate over the last 20 verifications, spec churn under
    /// 10%.
    pub fn conservative() -> Self {
        MaturityThresholds {
            verification_pass_rate: 0.9,
            spec_churn_rate: 0.1,
            verification_window: 20,
        }
    }
}

/// The deterministic half of the gate: every field is computed from project state, no judgment
/// involved.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MaturityPredicate {
    /// A working end-to-end artifact exists (e.g. the v0/v1 release definitions' exit criteria
    /// include at least one satisfied artifact-producing predicate).
    pub end_to_end_artifact_exists: bool,
    /// The V1 milestone is closed.
    pub v1_closed: bool,
    /// Verification pass rate over the trailing window, `0.0..=1.0`.
    pub verification_pass_rate: f64,
    /// Spec churn rate since the last evaluation, `0.0..=1.0`.
    pub spec_churn_rate: f64,
    /// Count of open (unresolved) structural audits (`ticket.audit_rejected` with a structural
    /// cause, per `TicketKind::Audit` semantics, not yet followed by a passing re-audit).
    pub open_structural_audits: u32,
    /// The thresholds this predicate was evaluated against.
    pub thresholds: MaturityThresholds,
}

impl MaturityPredicate {
    /// True when every deterministic condition holds: an end-to-end artifact exists, V1 is
    /// closed, the verification pass rate meets `thresholds.verification_pass_rate`, spec churn
    /// is at or below `thresholds.spec_churn_rate`, and no structural audits remain open.
    pub fn is_satisfied(&self) -> bool {
        self.end_to_end_artifact_exists
            && self.v1_closed
            && self.verification_pass_rate >= self.thresholds.verification_pass_rate
            && self.spec_churn_rate <= self.thresholds.spec_churn_rate
            && self.open_structural_audits == 0
    }
}

/// Evaluate [`MaturityPredicate`] over `view`.
///
// IMPL: pure. `v1_closed` = `view.milestones.get(v1).map(|m| m.state ==
// MilestoneState::Closed).unwrap_or(false)`. `verification_pass_rate` = over the last
// `thresholds.verification_window` `TicketKind::Verification` tickets by `updated` timestamp,
// fraction that reached `TicketState::Closed` rather than exhausting retries into `Escalated`
// (an empty window should count as `1.0`, not `0.0` — no verification attempts is not evidence
// of failure, and the caller can still gate on `end_to_end_artifact_exists`/`v1_closed`).
// `open_structural_audits` = count of `TicketKind::Audit` tickets whose latest `FailureRecord` in
// `failures` is structural (mirrors the audit-rejected-structural trigger) and that have not
// since been superseded by a passing audit. `end_to_end_artifact_exists` and `spec_churn_rate`
// need data `ProjectView` alone may not carry in full (artifact *content*, spec revision
// history); where that's the case, compute the best available approximation from `view.artifacts`
// / `view.decisions` and document the approximation inline, since `tm-core` is not this crate's
// to extend.
pub fn evaluate_predicate(
    view: &ProjectView,
    v1: &MilestoneId,
    thresholds: MaturityThresholds,
) -> MaturityPredicate {
    let _ = (view, v1, thresholds);
    todo!("evaluate_predicate: pure deterministic computation over ProjectView, see module IMPL note")
}

/// The frontier half of the gate: one judgment call, recorded as a [`tm_core::Decision`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaturityJudgment {
    /// The decision this judgment was recorded as.
    pub decision: DecisionId,
    /// Whether the judging role considers the project mature enough to leave `Stabilization`.
    pub mature: bool,
    /// The judgment's reasoning, in prose.
    pub rationale: String,
}

/// Ask `role` (expected to be a frontier role, e.g. `Role::ArchitectFrontier`) whether the
/// project is mature enough to leave `Stabilization`, given `predicate`, and record the answer as
/// a [`tm_core::Decision`] via `store.record_decision`.
///
// IMPL: build a `CompletionRequest` under `role` whose prompt embeds every `MaturityPredicate`
// field plus a project summary (open tickets, recent failures, milestone state) so the judgment
// isn't made blind to context the deterministic predicate doesn't capture (code quality signals,
// whether V1's *spirit* was met, not just its exit criteria). Parse `{mature: bool, rationale:
// String}` from the response (`TmError::parse` on malformed output), then call
// `store.record_decision` with `subject` = "genesis maturity gate", `decision` = "mature" or
// "not yet mature", `reason` = the parsed rationale, `affected_tickets` = the V1 milestone's
// member tickets, `actor` = the participant this judgment runs as; use the returned `DecisionId`
// (extracted from the emitted `decision.created` event's subject) to populate
// `MaturityJudgment::decision`. `clock` is threaded through for any caller-side timestamping,
// though `record_decision` stamps its own event time.
// Errors: `TmError::Provider` on completion failure, `TmError::Parse` on malformed output,
// whatever `store.record_decision` returns on failure to persist.
pub async fn judge_maturity(
    view: &ProjectView,
    predicate: &MaturityPredicate,
    role: Role,
    provider: &dyn tm_provider::Provider,
    store: &Store,
    clock: &dyn Clock,
    actor: ParticipantId,
) -> TmResult<MaturityJudgment> {
    let _ = (view, predicate, role, provider, store, clock, actor);
    todo!("judge_maturity: frontier judgment round-trip recorded via Store::record_decision, see module IMPL note")
}

/// The gate's verdict: both halves must agree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaturityGateResult {
    /// The deterministic predicate that was evaluated.
    pub predicate: MaturityPredicate,
    /// The frontier judgment that was recorded.
    pub judgment: MaturityJudgment,
    /// True only when `predicate.is_satisfied()` and `judgment.mature` both hold.
    pub passed: bool,
}

/// Combine `predicate` and `judgment`: the gate passes only when both agree.
pub fn gate(predicate: MaturityPredicate, judgment: MaturityJudgment) -> MaturityGateResult {
    let passed = predicate.is_satisfied() && judgment.mature;
    MaturityGateResult { predicate, judgment, passed }
}

/// What [`reconverge_authority`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconvergenceOutcome {
    /// Every Genesis-scoped lease that was revoked.
    pub revoked_leases: Vec<LeaseId>,
    /// The steady-state policy now in effect, replacing the ignition policy.
    pub steady_state: SteadyStatePolicy,
    /// Every event emitted while reconverging (lease releases, `authority.reverted`,
    /// `genesis.completed`).
    pub events: Vec<tm_events::Event>,
}

/// On a passing [`MaturityGateResult`], revoke every lease in `genesis_leases`, replace `ignition`
/// with a computed [`SteadyStatePolicy`], and emit `authority.reverted` / `genesis.completed`.
///
// IMPL: I/O, only called once `result.passed` (caller's responsibility to check; this function
// does not re-check, since the gate result already carries its own justification). For each id in
// `genesis_leases`, call `store.release(id, actor.clone())` (or `store.expire_leases()` if the
// caller prefers a sweep over an explicit list — an explicit list is preferred here since
// "Genesis-scoped" is a Genesis-side notion `tm-core` doesn't track natively) and collect the
// returned events; then compute `SteadyStatePolicy::narrowed_from(ignition, narrower_authority)`
// where `narrower_authority` is derived by intersecting every released lease's prior authority
// down to whatever the project's own root authority declares as its steady-state ceiling (a
// `tm-core`-side concept this crate reads via `store.view()?.tickets` root authority, not
// invents); finally append a `genesis.completed` event via the log this `Store` wraps (there is
// no dedicated `Store::genesis_completed` method — appending directly through
// `tm_events::EventLog`, obtained the same way `Store::open` does, is the documented exception,
// matching how `store.rs`'s own module docs describe its few hand-rolled event appends).
// Errors: whatever `store.release` returns for an already-expired/missing lease; propagated, not
// swallowed, since a Genesis-scoped lease that can't be revoked means authority did not actually
// reconverge and the caller must know.
pub fn reconverge_authority(
    store: &Store,
    genesis_leases: &[LeaseId],
    ignition: &IgnitionPolicy,
    actor: ParticipantId,
) -> TmResult<ReconvergenceOutcome> {
    let _ = (store, genesis_leases, ignition, actor);
    todo!("reconverge_authority: revoke genesis leases, compute SteadyStatePolicy, emit authority.reverted + genesis.completed, see module IMPL note")
}
