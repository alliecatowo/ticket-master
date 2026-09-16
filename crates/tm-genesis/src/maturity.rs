//! The maturity gate: deterministic predicate plus one frontier judgment, both required.
//!
//! `SPEC.md` §12 requires *both* a machine-checkable predicate over project state
//! ([`MaturityPredicate`]) and a frontier judgment call recorded as a [`tm_core::Decision`]
//! ([`MaturityJudgment`]) to agree before Genesis leaves `Stabilization`. Passing swaps
//! [`crate::ignition::IgnitionPolicy`] for [`crate::ignition::SteadyStatePolicy`] and revokes
//! every Genesis-scoped lease ([`reconverge_authority`]) — authority genuinely narrows, it
//! doesn't just stop being checked.

use serde::{Deserialize, Serialize};
use tm_core::{ArtifactKind, FailureClass, ProjectView, Store, TicketKind, TicketState};
use tm_events::EventKind;
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
/// `end_to_end_artifact_exists` and `spec_churn_rate` need data `ProjectView` alone does not
/// carry in full (artifact *content*, spec revision history), since `tm-core` is not this
/// crate's to extend. Both are approximated from what `view` does carry, documented at each
/// site below.
pub fn evaluate_predicate(
    view: &ProjectView,
    v1: &MilestoneId,
    thresholds: MaturityThresholds,
) -> MaturityPredicate {
    let v1_closed = view
        .milestones
        .get(v1)
        .map(|m| m.state == tm_core::MilestoneState::Closed)
        .unwrap_or(false);

    let mut verifications: Vec<_> = view
        .tickets
        .values()
        .filter(|t| t.kind == TicketKind::Verification)
        .collect();
    verifications.sort_by_key(|t| std::cmp::Reverse(t.updated));
    verifications.truncate(thresholds.verification_window);
    let verification_pass_rate = if verifications.is_empty() {
        // No verification attempts is not evidence of failure: the caller can still gate on
        // `end_to_end_artifact_exists`/`v1_closed`.
        1.0
    } else {
        let closed = verifications
            .iter()
            .filter(|t| t.state == TicketState::Closed)
            .count();
        closed as f64 / verifications.len() as f64
    };

    // A structural audit is open when the latest failure recorded against an `Audit`-kind
    // ticket is an audit rejection whose detail names it structural (mirroring
    // `Trigger::AuditRejectedStructural`, which `FailureClass` itself doesn't distinguish from
    // a minor rejection) and the ticket hasn't since closed via a passing re-audit.
    let open_structural_audits = view
        .tickets
        .values()
        .filter(|t| t.kind == TicketKind::Audit && t.state != TicketState::Closed)
        .filter(|t| {
            t.failures.last().is_some_and(|f| {
                f.class == FailureClass::AuditRejected
                    && f.detail.to_lowercase().contains("structural")
            })
        })
        .count() as u32;

    // Approximation: an end-to-end artifact "exists" when some ticket evidence links to a
    // `Patch` artifact on a ticket that actually closed, i.e. a change made it all the way
    // through the graph rather than merely being attempted.
    let end_to_end_artifact_exists = view.evidence.iter().any(|e| {
        view.tickets
            .get(&e.ticket)
            .is_some_and(|t| t.state == TicketState::Closed)
            && view
                .artifacts
                .get(&e.artifact)
                .is_some_and(|a| a.kind == ArtifactKind::Patch)
    });

    // Approximation: `ProjectView` carries no spec revision history, so churn is read off
    // decisions instead — the fraction of the most recent (by `ts`) active decisions, up to
    // `verification_window`, whose `affected_paths` touch the spec. No decisions at all is
    // treated as zero churn (there is nothing to measure, not evidence of instability).
    let mut decisions: Vec<_> = view.decisions.values().filter(|d| d.is_active()).collect();
    decisions.sort_by_key(|d| std::cmp::Reverse(d.ts));
    decisions.truncate(thresholds.verification_window);
    let spec_churn_rate = if decisions.is_empty() {
        0.0
    } else {
        let spec_touching = decisions
            .iter()
            .filter(|d| {
                d.affected_paths
                    .iter()
                    .any(|p| p.to_lowercase().contains("spec"))
            })
            .count();
        spec_touching as f64 / decisions.len() as f64
    };

    MaturityPredicate {
        end_to_end_artifact_exists,
        v1_closed,
        verification_pass_rate,
        spec_churn_rate,
        open_structural_audits,
        thresholds,
    }
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
pub async fn judge_maturity(
    view: &ProjectView,
    predicate: &MaturityPredicate,
    role: Role,
    provider: &dyn tm_provider::Provider,
    store: &Store,
    clock: &dyn Clock,
    actor: ParticipantId,
) -> TmResult<MaturityJudgment> {
    let open_tickets = view
        .tickets
        .values()
        .filter(|t| !matches!(t.state, TicketState::Closed | TicketState::Cancelled))
        .count();
    let recent_failures: Vec<String> = view
        .tickets
        .values()
        .flat_map(|t| t.failures.iter().map(move |f| (t, f)))
        .map(|(t, f)| {
            format!(
                "{} attempt {}: {:?} ({})",
                t.id, f.attempt, f.class, f.detail
            )
        })
        .take(10)
        .collect();
    let milestones_summary: Vec<String> = view
        .milestones
        .values()
        .map(|m| format!("{} \"{}\": {:?}", m.id, m.title, m.state))
        .collect();

    let prompt = format!(
        "You are judging, under role {role:?}, whether a software project is mature enough to \
leave Genesis's Stabilization stage as of {now}.\n\n\
The deterministic maturity predicate evaluated:\n\
- end_to_end_artifact_exists: {artifact}\n\
- v1_closed: {v1}\n\
- verification_pass_rate: {vpr:.3} (threshold {vpr_t:.3})\n\
- spec_churn_rate: {churn:.3} (threshold {churn_t:.3})\n\
- open_structural_audits: {audits}\n\n\
Project summary:\n\
- open (non-terminal) tickets: {open_tickets}\n\
- milestones: {milestones:?}\n\
- recent failures: {failures:?}\n\n\
The deterministic predicate alone can't see code quality or whether V1's *spirit* was met, not \
just its exit criteria — that's what your judgment is for.\n\n\
Return ONLY a JSON object of the form {{\"mature\": bool, \"rationale\": string}}, with no \
markdown formatting or code fences.",
        now = clock.now(),
        artifact = predicate.end_to_end_artifact_exists,
        v1 = predicate.v1_closed,
        vpr = predicate.verification_pass_rate,
        vpr_t = predicate.thresholds.verification_pass_rate,
        churn = predicate.spec_churn_rate,
        churn_t = predicate.thresholds.spec_churn_rate,
        audits = predicate.open_structural_audits,
        milestones = milestones_summary,
        failures = recent_failures,
    );

    let req = tm_provider::CompletionRequest {
        system: None,
        messages: vec![tm_provider::Message {
            role: tm_provider::MessageRole::User,
            content: vec![tm_provider::ContentBlock::Text { text: prompt }],
        }],
        tools: vec![],
        max_tokens: 1024,
        temperature: None,
        stop_sequences: vec![],
        stream: false,
        n: 1,
    };

    let completion = provider
        .complete(req)
        .await
        .map_err(|e| tm_types::TmError::Provider(format!("maturity judgment failed: {e}")))?;

    let response_text = completion
        .candidates
        .first()
        .map(|c| {
            c.content
                .iter()
                .filter_map(|b| match b {
                    tm_provider::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
        })
        .ok_or_else(|| tm_types::TmError::parse("maturity judgment returned no candidates"))?;

    let parsed: serde_json::Value = serde_json::from_str(&response_text)
        .map_err(|e| tm_types::TmError::parse(format!("malformed maturity judgment JSON: {e}")))?;

    let mature = parsed
        .get("mature")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| tm_types::TmError::parse("maturity judgment missing boolean \"mature\""))?;
    let rationale = parsed
        .get("rationale")
        .and_then(|v| v.as_str())
        .ok_or_else(|| tm_types::TmError::parse("maturity judgment missing string \"rationale\""))?
        .to_string();

    let affected_tickets = v1_member_tickets(view);

    let events = store.record_decision(
        "genesis maturity gate".to_string(),
        if mature {
            "mature".to_string()
        } else {
            "not yet mature".to_string()
        },
        rationale.clone(),
        Vec::new(),
        affected_tickets,
        Vec::new(),
        actor,
    )?;

    let decision = events
        .iter()
        .find(|e| e.kind == EventKind::DecisionCreated)
        .and_then(|e| e.payload.as_decision_created())
        .map(|p| p.decision.clone())
        .ok_or_else(|| {
            tm_types::TmError::invariant("record_decision did not emit a decision.created event")
        })?;

    Ok(MaturityJudgment {
        decision,
        mature,
        rationale,
    })
}

/// Best-effort membership for "the V1 milestone's member tickets": `judge_maturity` isn't given
/// an explicit V1 milestone id, so a milestone titled `v1` (case-insensitively) is treated as
/// V1; absent one, no tickets are attributed rather than guessing at the wrong milestone.
fn v1_member_tickets(view: &ProjectView) -> Vec<tm_types::TicketId> {
    view.milestones
        .values()
        .find(|m| m.title.to_lowercase().contains("v1"))
        .map(|m| m.tickets.clone())
        .unwrap_or_default()
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
    MaturityGateResult {
        predicate,
        judgment,
        passed,
    }
}

/// What [`reconverge_authority`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconvergenceOutcome {
    /// Every Genesis-scoped lease that was revoked.
    pub revoked_leases: Vec<LeaseId>,
    /// The steady-state policy now in effect, replacing the ignition policy.
    pub steady_state: SteadyStatePolicy,
    /// Every event emitted while reconverging (lease releases, plus a decision recording the
    /// reconvergence's completion — see the function docs for why that stands in for a
    /// dedicated `authority.reverted` / `genesis.completed` append).
    pub events: Vec<tm_events::Event>,
}

/// On a passing [`MaturityGateResult`], revoke every lease in `genesis_leases`, replace `ignition`
/// with a computed [`SteadyStatePolicy`], and record the reconvergence's completion.
///
/// Only called once `result.passed` (caller's responsibility to check; this function does not
/// re-check, since the gate result already carries its own justification).
///
/// `tm_core::Store` deliberately keeps its underlying event log and project root private (see
/// that crate's own module docs listing its few hand-rolled event appends as a closed set), and
/// this function's signature — fixed, per this crate's contract — carries neither. So rather
/// than fabricating a second `tm_events::EventLog` handle this crate has no legitimate path to
/// open, reconvergence's completion is recorded as a `tm_core::Decision` via
/// `store.record_decision`, the nearest public write path that can durably hold free-form
/// completion text; its emitted events are folded into [`ReconvergenceOutcome::events`] alongside
/// the real lease-release events.
///
/// # Errors
/// Whatever `store.release` returns for an already-expired/missing lease, or `store.record_decision`
/// returns on failure to persist; propagated, not swallowed, since a Genesis-scoped lease that
/// can't be revoked means authority did not actually reconverge and the caller must know.
pub fn reconverge_authority(
    store: &Store,
    genesis_leases: &[LeaseId],
    ignition: &IgnitionPolicy,
    actor: ParticipantId,
) -> TmResult<ReconvergenceOutcome> {
    let view = store.view()?;

    // "Genesis-scoped" is a Genesis-side notion `tm-core` doesn't track natively, so the ceiling
    // for the narrowed authority comes from the project's root ticket (the one with no parent)
    // rather than from any dedicated `tm-core` concept; absent a root ticket, the ignition
    // policy's own default is the best available fallback.
    let root_ceiling = view
        .tickets
        .values()
        .find(|t| t.parent.is_none())
        .map(|t| t.authority.clone())
        .unwrap_or_else(|| ignition.default_authority.clone());

    let narrower_authority =
        genesis_leases
            .iter()
            .fold(root_ceiling, |authority, id| match view.leases.get(id) {
                Some(lease) => authority.intersect(&lease.authority),
                None => authority,
            });

    let mut events = Vec::new();
    for id in genesis_leases {
        events.extend(store.release(id, actor.clone())?);
    }

    let steady_state = SteadyStatePolicy::narrowed_from(ignition, narrower_authority);

    let completion_events = store.record_decision(
        "genesis authority reconvergence".to_string(),
        "steady state".to_string(),
        format!(
            "revoked {} genesis-scoped lease(s); ignition policy replaced by steady-state policy",
            genesis_leases.len()
        ),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        actor,
    )?;
    events.extend(completion_events);

    Ok(ReconvergenceOutcome {
        revoked_leases: genesis_leases.to_vec(),
        steady_state,
        events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tm_core::{ExecutorRequirements, FailureRecord, RetryPolicy, VerificationPolicy};
    use tm_provider::{
        Candidate, Completion, ContentBlock, ModelId, ProviderError, StopReason, Usage,
    };
    use tm_types::{
        Authority, Budget, CounterIds, FixedClock, IdSource, MilestoneId, Predicate, TicketId,
        Timestamp, Tolerance,
    };

    fn executor() -> ExecutorRequirements {
        ExecutorRequirements {
            role: tm_types::Role::CoderFast,
            human_required: false,
            min_capability: Tolerance::Any,
        }
    }

    fn retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 1,
            backoff_multiplier: 2.0,
            max_delay_seconds: 60,
        }
    }

    fn open_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Store::open_with(dir.path(), clock, ids).expect("open store");
        (dir, store)
    }

    fn actor() -> ParticipantId {
        ParticipantId::system()
    }

    // --- MaturityPredicate ---

    fn base_predicate() -> MaturityPredicate {
        MaturityPredicate {
            end_to_end_artifact_exists: true,
            v1_closed: true,
            verification_pass_rate: 1.0,
            spec_churn_rate: 0.0,
            open_structural_audits: 0,
            thresholds: MaturityThresholds::conservative(),
        }
    }

    #[test]
    fn predicate_satisfied_when_every_condition_holds() {
        assert!(base_predicate().is_satisfied());
    }

    #[test]
    fn predicate_unsatisfied_without_end_to_end_artifact() {
        let mut p = base_predicate();
        p.end_to_end_artifact_exists = false;
        assert!(!p.is_satisfied());
    }

    #[test]
    fn predicate_unsatisfied_when_v1_not_closed() {
        let mut p = base_predicate();
        p.v1_closed = false;
        assert!(!p.is_satisfied());
    }

    #[test]
    fn predicate_unsatisfied_below_verification_threshold() {
        let mut p = base_predicate();
        p.verification_pass_rate = 0.5;
        assert!(!p.is_satisfied());
    }

    #[test]
    fn predicate_unsatisfied_above_churn_threshold() {
        let mut p = base_predicate();
        p.spec_churn_rate = 0.5;
        assert!(!p.is_satisfied());
    }

    #[test]
    fn predicate_unsatisfied_with_open_structural_audit() {
        let mut p = base_predicate();
        p.open_structural_audits = 1;
        assert!(!p.is_satisfied());
    }

    #[test]
    fn evaluate_predicate_on_empty_view_has_no_verifications_but_full_pass_rate() {
        let view = ProjectView::empty();
        let v1 = MilestoneId::new("M-1").unwrap();
        let predicate = evaluate_predicate(&view, &v1, MaturityThresholds::conservative());
        assert!(!predicate.v1_closed);
        assert_eq!(predicate.verification_pass_rate, 1.0);
        assert_eq!(predicate.open_structural_audits, 0);
        assert!(!predicate.end_to_end_artifact_exists);
        assert_eq!(predicate.spec_churn_rate, 0.0);
    }

    #[test]
    fn evaluate_predicate_sees_v1_closed() {
        let (_dir, store) = open_store();
        let events = store
            .create_milestone("V1".to_string(), vec![], vec![], actor())
            .unwrap();
        let milestone_id = events[0].subject.clone();
        let v1 = MilestoneId::new(milestone_id.as_str()).unwrap();
        store.close_milestone(&v1, actor()).unwrap();
        let view = store.view().unwrap();
        let predicate = evaluate_predicate(&view, &v1, MaturityThresholds::conservative());
        assert!(predicate.v1_closed);
    }

    #[test]
    fn evaluate_predicate_counts_open_structural_audit() {
        let mut view = ProjectView::empty();
        let ticket_id = TicketId::new("T-1").unwrap();
        let mut ticket = test_ticket(ticket_id.clone(), TicketKind::Audit, TicketState::Replan);
        ticket.failures.push(FailureRecord {
            class: FailureClass::AuditRejected,
            detail: "structural: children need to be regenerated".to_string(),
            at: Timestamp::EPOCH,
            attempt: 1,
        });
        view.tickets.insert(ticket_id, ticket);
        let v1 = MilestoneId::new("M-1").unwrap();
        let predicate = evaluate_predicate(&view, &v1, MaturityThresholds::conservative());
        assert_eq!(predicate.open_structural_audits, 1);
    }

    #[test]
    fn evaluate_predicate_ignores_structural_audit_once_ticket_closed() {
        let mut view = ProjectView::empty();
        let ticket_id = TicketId::new("T-1").unwrap();
        let mut ticket = test_ticket(ticket_id.clone(), TicketKind::Audit, TicketState::Closed);
        ticket.failures.push(FailureRecord {
            class: FailureClass::AuditRejected,
            detail: "structural issue".to_string(),
            at: Timestamp::EPOCH,
            attempt: 1,
        });
        view.tickets.insert(ticket_id, ticket);
        let v1 = MilestoneId::new("M-1").unwrap();
        let predicate = evaluate_predicate(&view, &v1, MaturityThresholds::conservative());
        assert_eq!(predicate.open_structural_audits, 0);
    }

    #[test]
    fn evaluate_predicate_verification_pass_rate_counts_closed_over_window() {
        let mut view = ProjectView::empty();
        for (i, state) in [
            TicketState::Closed,
            TicketState::Closed,
            TicketState::Escalated,
        ]
        .into_iter()
        .enumerate()
        {
            let id = TicketId::new(format!("T-{}", i + 1)).unwrap();
            let mut t = test_ticket(id.clone(), TicketKind::Verification, state);
            t.updated = Timestamp::from_unix_nanos((i as i128) * 1_000_000_000);
            view.tickets.insert(id, t);
        }
        let v1 = MilestoneId::new("M-1").unwrap();
        let thresholds = MaturityThresholds {
            verification_window: 3,
            ..MaturityThresholds::conservative()
        };
        let predicate = evaluate_predicate(&view, &v1, thresholds);
        assert!((predicate.verification_pass_rate - (2.0 / 3.0)).abs() < 1e-9);
    }

    fn test_ticket(id: TicketId, kind: TicketKind, state: TicketState) -> tm_core::Ticket {
        tm_core::Ticket {
            id,
            kind,
            objective: "x".to_string(),
            state,
            parent: None,
            children: Vec::new(),
            dependencies: Vec::new(),
            milestone: None,
            authority: Authority::none(),
            resources: Vec::new(),
            executor: executor(),
            context_refs: Vec::new(),
            success: Vec::<Predicate>::new(),
            verification: VerificationPolicy::None,
            budget: Budget::none(),
            retry: retry(),
            cycle: None,
            attempts: 0,
            failures: Vec::new(),
            priority: 0,
            created: Timestamp::EPOCH,
            updated: Timestamp::EPOCH,
        }
    }

    // --- judge_maturity ---

    struct ScriptedProvider {
        response: String,
    }

    #[async_trait::async_trait]
    impl tm_provider::fabric::Provider for ScriptedProvider {
        fn id(&self) -> &str {
            "test"
        }

        async fn complete(
            &self,
            _req: tm_provider::CompletionRequest,
        ) -> Result<Completion, ProviderError> {
            Ok(Completion {
                model: ModelId::new("test", "test-model"),
                candidates: vec![Candidate {
                    content: vec![ContentBlock::Text {
                        text: self.response.clone(),
                    }],
                    stop_reason: StopReason::EndTurn,
                }],
                usage: Usage::default(),
                latency: std::time::Duration::from_millis(1),
                received_at: Timestamp::EPOCH,
            })
        }

        async fn embed(
            &self,
            _req: tm_provider::EmbedRequest,
        ) -> Result<tm_provider::Embeddings, ProviderError> {
            Err(ProviderError::Unscripted("embed not implemented".into()))
        }
    }

    #[tokio::test]
    async fn judge_maturity_records_a_decision_on_a_mature_verdict() {
        let (_dir, store) = open_store();
        let view = store.view().unwrap();
        let predicate = base_predicate();
        let provider = ScriptedProvider {
            response: r#"{"mature": true, "rationale": "V1's spirit was met"}"#.to_string(),
        };
        let clock = FixedClock::epoch();

        let judgment = judge_maturity(
            &view,
            &predicate,
            Role::ArchitectFrontier,
            &provider,
            &store,
            &clock,
            actor(),
        )
        .await
        .unwrap();

        assert!(judgment.mature);
        assert_eq!(judgment.rationale, "V1's spirit was met");
        let view_after = store.view().unwrap();
        assert!(view_after.decisions.contains_key(&judgment.decision));
    }

    #[tokio::test]
    async fn judge_maturity_rejects_malformed_json() {
        let (_dir, store) = open_store();
        let view = store.view().unwrap();
        let predicate = base_predicate();
        let provider = ScriptedProvider {
            response: "not json".to_string(),
        };
        let clock = FixedClock::epoch();

        let result = judge_maturity(
            &view,
            &predicate,
            Role::ArchitectFrontier,
            &provider,
            &store,
            &clock,
            actor(),
        )
        .await;

        assert!(matches!(result, Err(tm_types::TmError::Parse(_))));
    }

    #[tokio::test]
    async fn judge_maturity_rejects_missing_mature_field() {
        let (_dir, store) = open_store();
        let view = store.view().unwrap();
        let predicate = base_predicate();
        let provider = ScriptedProvider {
            response: r#"{"rationale": "no verdict"}"#.to_string(),
        };
        let clock = FixedClock::epoch();

        let result = judge_maturity(
            &view,
            &predicate,
            Role::ArchitectFrontier,
            &provider,
            &store,
            &clock,
            actor(),
        )
        .await;

        assert!(matches!(result, Err(tm_types::TmError::Parse(_))));
    }

    // --- gate ---

    fn judgment(decision: &str, mature: bool) -> MaturityJudgment {
        MaturityJudgment {
            decision: DecisionId::new(decision).unwrap(),
            mature,
            rationale: "because".to_string(),
        }
    }

    #[test]
    fn gate_passes_only_when_predicate_and_judgment_agree() {
        assert!(gate(base_predicate(), judgment("D-1", true)).passed);
    }

    #[test]
    fn gate_fails_when_judgment_disagrees() {
        assert!(!gate(base_predicate(), judgment("D-1", false)).passed);
    }

    #[test]
    fn gate_fails_when_predicate_disagrees() {
        let mut p = base_predicate();
        p.v1_closed = false;
        assert!(!gate(p, judgment("D-1", true)).passed);
    }

    // --- reconverge_authority ---

    fn ignition_policy() -> IgnitionPolicy {
        IgnitionPolicy::for_v0(MilestoneId::new("M-1").unwrap())
    }

    #[test]
    fn reconverge_authority_revokes_every_genesis_lease() {
        let (_dir, store) = open_store();
        let events = store
            .create_ticket(
                TicketKind::Work,
                "root".to_string(),
                None,
                None,
                Authority::root(),
                Vec::new(),
                executor(),
                Vec::new(),
                Vec::new(),
                VerificationPolicy::None,
                Budget::unlimited(),
                retry(),
                0,
                actor(),
            )
            .unwrap();
        let ticket_id = TicketId::new(events[0].subject.as_str()).unwrap();
        store.activate(&ticket_id, actor()).unwrap();
        let lease_events = store
            .acquire_lease(
                &ticket_id,
                actor(),
                Authority::root(),
                Vec::new(),
                3600,
                actor(),
            )
            .unwrap();
        let lease_id_str = lease_events
            .iter()
            .find_map(|e| e.payload.as_ticket_leased())
            .expect("ticket.leased event")
            .lease
            .as_str()
            .to_string();
        let lease_id = LeaseId::new(lease_id_str).unwrap();

        let outcome =
            reconverge_authority(&store, &[lease_id.clone()], &ignition_policy(), actor()).unwrap();

        assert_eq!(outcome.revoked_leases, vec![lease_id]);
        assert!(!outcome.events.is_empty());
        let view = store.view().unwrap();
        assert!(view.leases.is_empty());
    }

    #[test]
    fn reconverge_authority_narrows_fan_out_from_ignition() {
        let (_dir, store) = open_store();
        let ignition = ignition_policy();
        let outcome = reconverge_authority(&store, &[], &ignition, actor()).unwrap();
        assert!(outcome.steady_state.fan_out_width <= ignition.fan_out_width);
        assert_eq!(
            outcome.steady_state.exploratory_code_tolerance,
            Tolerance::Strict
        );
    }

    #[test]
    fn reconverge_authority_errors_on_unknown_lease() {
        let (_dir, store) = open_store();
        let bogus = LeaseId::new("L-000000000000").unwrap();
        let result = reconverge_authority(&store, &[bogus], &ignition_policy(), actor());
        assert!(result.is_err());
    }
}
