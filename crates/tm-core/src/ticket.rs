//! The ticket record and its vocabulary.
//!
//! Owns [`Ticket`] itself and every type that appears in one of its fields: the kind/state
//! enums, [`Trigger`] (the state machine's input alphabet, consumed by `machine::transition`),
//! dependency and resource claim shapes, executor requirements, context references, retry and
//! verification policy, cycle budgets and failure records. Pure data only — no I/O, no
//! validation logic (that lives in `machine.rs` / `invariants.rs`), just the shapes and their
//! serde mappings so every other module (and every other crate) speaks the same vocabulary.

use serde::{Deserialize, Serialize};
use tm_types::{
    ArtifactId, Authority, Budget, DecisionId, MilestoneId, PathPattern, Predicate, Role, TicketId,
    Timestamp, Tolerance,
};

/// One node in the project's ticket graph: the unit of work, verification, audit, investigation,
/// recovery or harness change. See `SPEC.md` §4.2 for the authoritative field list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    /// Stable identifier; also encodes `kind` for `Work`/`Verification`/`Audit` via its prefix.
    pub id: TicketId,
    /// What kind of node this is in the ticket graph.
    pub kind: TicketKind,
    /// Natural-language statement of intent; may be fuzzy, refined over the ticket's life.
    pub objective: String,
    /// Current state, reachable only via `machine::transition`.
    pub state: TicketState,
    /// The ticket that created/owns this one, if any.
    pub parent: Option<TicketId>,
    /// Tickets this one created/owns.
    pub children: Vec<TicketId>,
    /// Tickets this one depends on (edges materialized in `ticket_deps`).
    pub dependencies: Vec<TicketId>,
    /// The milestone this ticket belongs to, if any.
    pub milestone: Option<MilestoneId>,
    /// The authority this ticket may lease out to an executor; never exceeds the parent's.
    pub authority: Authority,
    /// Filesystem paths (and their access mode) this ticket's work touches.
    pub resources: Vec<ResourceClaim>,
    /// What kind of executor may take this ticket.
    pub executor: ExecutorRequirements,
    /// Context the executor should be given when leasing this ticket.
    pub context_refs: Vec<ContextRef>,
    /// Predicates that define "done"; machine-checkable where possible.
    pub success: Vec<Predicate>,
    /// How completion is verified before a ticket may close.
    pub verification: VerificationPolicy,
    /// This ticket's own budget scope (see `budget.rs`).
    pub budget: Budget,
    /// Retry behavior on recoverable failure.
    pub retry: RetryPolicy,
    /// Present only for tickets that participate in a `Loop`-edge cycle.
    pub cycle: Option<CycleBudget>,
    /// Number of lease/execution attempts made so far.
    pub attempts: u32,
    /// History of recorded failures for this ticket.
    pub failures: Vec<FailureRecord>,
    /// Scheduler priority; higher schedules first, no other semantics here.
    pub priority: i32,
    /// When this ticket was created.
    pub created: Timestamp,
    /// When this ticket was last materially changed.
    pub updated: Timestamp,
}

/// What role a ticket plays in the project graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TicketKind {
    /// Ordinary unit of work.
    Work,
    /// Checks that a `Work` ticket's submission satisfies its success predicates.
    Verification,
    /// Independent review of a verified submission before it may close.
    Audit,
    /// Open-ended exploration with no fixed success predicate; may permit `VerificationPolicy::None`.
    Investigation,
    /// Repairs project state after a failure (e.g. a stuck lease, a rejected audit).
    Recovery,
    /// Changes to the harness/tooling itself; may permit `VerificationPolicy::None`.
    Harness,
}

/// A ticket's position in the state machine. See `SPEC.md` §4.3 and `machine.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TicketState {
    /// Just created; not yet activated into the dependency-tracked graph.
    Draft,
    /// Activated but has at least one unsatisfied dependency.
    Blocked,
    /// No unsatisfied dependency; eligible for the scheduler to lease.
    Ready,
    /// A lease is held; work has not yet started.
    Leased,
    /// The leaseholder is actively executing.
    Running,
    /// Work finished and evidence was submitted; awaiting automatic verification handoff.
    Submitted,
    /// A `Verification` ticket is checking the submission.
    Verifying,
    /// An `Audit` ticket is reviewing a passed verification.
    Auditing,
    /// Audit found a minor issue; sent back for another attempt.
    Rework,
    /// Audit found a structural issue; children must be regenerated.
    Replan,
    /// Verification failed or a lease expired; `RetryPolicy` decides the next state.
    Recovery,
    /// Recovery exhausted retries (or hit a non-retryable failure); needs human/authority input.
    Escalated,
    /// Terminal: done.
    Closed,
    /// Terminal: abandoned.
    Cancelled,
}

/// The input alphabet of the ticket state machine. Every legal `(TicketState, Trigger)` pair is
/// enumerated in `machine::transition`'s table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    /// Moves `Draft` into the dependency-tracked graph.
    Activate,
    /// The scheduler's pure recomputation that all dependencies are `Closed`.
    DependenciesSatisfied,
    /// A dependency that had been satisfied is no longer `Closed` (e.g. reopened).
    DependencyReopened,
    /// The scheduler granted a lease.
    LeaseAcquired,
    /// The leaseholder began executing.
    WorkStarted,
    /// The lease's TTL passed without a heartbeat.
    LeaseExpired,
    /// The leaseholder gave up the lease voluntarily.
    LeaseReleased,
    /// The leaseholder submitted evidence of completion.
    Submit,
    /// Automatic handoff from `Submitted` into verification.
    BeginVerification,
    /// A `Verification` ticket found the submission satisfies its success predicates.
    VerificationPassed,
    /// A `Verification` ticket found the submission does not satisfy its success predicates.
    VerificationFailed,
    /// An `Audit` ticket approved the verified submission.
    AuditPassed,
    /// An `Audit` ticket rejected the submission for a minor, same-ticket-fixable reason.
    AuditRejectedMinor,
    /// An `Audit` ticket rejected the submission for a structural reason requiring replanning.
    AuditRejectedStructural,
    /// `RetryPolicy` allows another attempt.
    RetryPermitted,
    /// `RetryPolicy` is exhausted, or the failure class is non-retryable.
    RetryExhausted,
    /// A human or higher authority responded to an escalation.
    EscalationResolved,
    /// An escalation was abandoned rather than resolved.
    EscalationAbandoned,
    /// `Replan` finished regenerating children.
    ReplanComplete,
    /// Explicit cancellation, from any non-terminal state.
    Cancel,
    /// Explicit reopen of a `Closed` ticket.
    Reopen,
}

/// How one ticket's completion depends on another's, and what that implies for the
/// acyclic-graph invariant (`invariants.rs`, rule: acyclic except `Loop` edges).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DependencyKind {
    /// Must be `Closed` before the dependent can become `Ready`; never part of a legal cycle.
    Hard,
    /// Informational sequencing preference; does not gate readiness.
    Soft,
    /// May participate in a cycle, but only when every edge in that cycle is also `Loop` and
    /// the cycle carries a [`CycleBudget`].
    Loop,
}

/// A filesystem path pattern this ticket's execution touches, and how it touches it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceClaim {
    /// The path pattern claimed.
    pub pattern: PathPattern,
    /// Whether this claim excludes concurrent claims that may overlap it.
    pub mode: ResourceMode,
}

/// Whether a [`ResourceClaim`] permits concurrent overlapping claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceMode {
    /// No other live claim may overlap this one.
    Exclusive,
    /// Other `Shared` claims may overlap this one; an `Exclusive` claim may not.
    Shared,
}

/// What kind of executor is eligible to lease a ticket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutorRequirements {
    /// The model role best suited to this ticket (see `tm_types::Role`).
    pub role: Role,
    /// True when only a human participant may execute this ticket.
    pub human_required: bool,
    /// The minimum acceptable capability tolerance for the assigned role.
    pub min_capability: Tolerance,
}

/// A reference into project context (docs, prior decisions, artifacts) to hand an executor at
/// lease time. Deliberately loose: context compilation (`tm-context`) resolves these against the
/// live project state; `tm-core` only records the pointer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContextRef {
    /// A doc registered in the `docs` table, by its project-relative path.
    Doc(String),
    /// An active decision relevant to this ticket.
    Decision(DecisionId),
    /// A stored artifact relevant to this ticket.
    Artifact(ArtifactId),
    /// A free-form path into the repository (not a registered doc).
    Path(String),
}

/// How a ticket's completion is verified before it may transition to `Closed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VerificationPolicy {
    /// No verification ticket is spawned; closing requires no evidence. Only legal for
    /// `TicketKind::Investigation` and `TicketKind::Harness` (enforced in `invariants.rs`).
    None,
    /// A single `Verification` ticket checks the submission.
    Single,
    /// Every predicate in `Ticket::success` gets its own `Verification` ticket; all must pass.
    PerPredicate,
    /// Verification, then a separate `Audit` ticket reviews the verification itself.
    DoubleBlind,
}

/// Exponential backoff retry behavior applied while a ticket is in `Recovery`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of attempts (including the first) before `Recovery` escalates.
    pub max_attempts: u32,
    /// Delay before the first retry, in seconds.
    pub base_delay_seconds: u64,
    /// Multiplier applied to the delay after each retry.
    pub backoff_multiplier: f64,
    /// Upper bound on the computed delay, in seconds.
    pub max_delay_seconds: u64,
    /// Failure classes that skip retry entirely and escalate immediately.
    pub non_retryable: Vec<FailureClass>,
}

impl RetryPolicy {
    /// The delay before attempt number `attempt` (1-based), clamped to `max_delay_seconds`.
    pub fn delay_for_attempt(&self, attempt: u32) -> u64 {
        let exponent = attempt.saturating_sub(1) as i32;
        let computed_delay =
            self.base_delay_seconds as f64 * self.backoff_multiplier.powi(exponent);
        let clamped = computed_delay.min(self.max_delay_seconds as f64);
        clamped as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_delay_for_attempt_one() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay_seconds: 10,
            backoff_multiplier: 2.0,
            max_delay_seconds: 300,
            non_retryable: vec![],
        };
        assert_eq!(policy.delay_for_attempt(1), 10);
    }

    #[test]
    fn retry_policy_delay_exponential_growth() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay_seconds: 1,
            backoff_multiplier: 2.0,
            max_delay_seconds: 1000,
            non_retryable: vec![],
        };
        assert_eq!(policy.delay_for_attempt(1), 1);
        assert_eq!(policy.delay_for_attempt(2), 2);
        assert_eq!(policy.delay_for_attempt(3), 4);
        assert_eq!(policy.delay_for_attempt(4), 8);
        assert_eq!(policy.delay_for_attempt(5), 16);
    }

    #[test]
    fn retry_policy_delay_clamped_to_max() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base_delay_seconds: 10,
            backoff_multiplier: 2.0,
            max_delay_seconds: 50,
            non_retryable: vec![],
        };
        assert_eq!(policy.delay_for_attempt(1), 10);
        assert_eq!(policy.delay_for_attempt(2), 20);
        assert_eq!(policy.delay_for_attempt(3), 40);
        assert_eq!(policy.delay_for_attempt(4), 50); // clamped
        assert_eq!(policy.delay_for_attempt(5), 50); // clamped
    }

    #[test]
    fn retry_policy_delay_no_backoff() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay_seconds: 10,
            backoff_multiplier: 1.0,
            max_delay_seconds: 100,
            non_retryable: vec![],
        };
        assert_eq!(policy.delay_for_attempt(1), 10);
        assert_eq!(policy.delay_for_attempt(2), 10);
        assert_eq!(policy.delay_for_attempt(3), 10);
        assert_eq!(policy.delay_for_attempt(100), 10);
    }

    #[test]
    fn retry_policy_delay_zero_backoff() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay_seconds: 10,
            backoff_multiplier: 0.0,
            max_delay_seconds: 100,
            non_retryable: vec![],
        };
        assert_eq!(policy.delay_for_attempt(1), 10);
        assert_eq!(policy.delay_for_attempt(2), 0);
        assert_eq!(policy.delay_for_attempt(3), 0);
    }

    #[test]
    fn retry_policy_delay_attempt_zero() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay_seconds: 10,
            backoff_multiplier: 2.0,
            max_delay_seconds: 100,
            non_retryable: vec![],
        };
        // Attempt 0 saturates to 0, so exponent is 0, and 10 * 2^0 = 10
        assert_eq!(policy.delay_for_attempt(0), 10);
    }

    #[test]
    fn retry_policy_delay_max_delay_less_than_base() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay_seconds: 100,
            backoff_multiplier: 2.0,
            max_delay_seconds: 50,
            non_retryable: vec![],
        };
        // First attempt: 100 clamped to 50
        assert_eq!(policy.delay_for_attempt(1), 50);
        assert_eq!(policy.delay_for_attempt(2), 50);
    }

    #[test]
    fn retry_policy_delay_fractional_multiplier() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay_seconds: 100,
            backoff_multiplier: 0.5,
            max_delay_seconds: 1000,
            non_retryable: vec![],
        };
        assert_eq!(policy.delay_for_attempt(1), 100);
        assert_eq!(policy.delay_for_attempt(2), 50);
        assert_eq!(policy.delay_for_attempt(3), 25);
        assert_eq!(policy.delay_for_attempt(4), 12);
    }

    #[test]
    fn retry_policy_delay_large_attempt() {
        let policy = RetryPolicy {
            max_attempts: 100,
            base_delay_seconds: 1,
            backoff_multiplier: 10.0,
            max_delay_seconds: 60,
            non_retryable: vec![],
        };
        // 10^20 would be huge, so clamps to max_delay_seconds
        assert_eq!(policy.delay_for_attempt(100), 60);
    }

    #[test]
    fn cycle_budget_has_capacity() {
        let mut budget = CycleBudget {
            max_iterations: 5,
            iterations: 0,
        };
        assert!(budget.has_capacity());

        budget.iterations = 4;
        assert!(budget.has_capacity());

        budget.iterations = 5;
        assert!(!budget.has_capacity());

        budget.iterations = 100;
        assert!(!budget.has_capacity());
    }

    #[test]
    fn ticket_kind_round_trip_serde() {
        let kinds = vec![
            TicketKind::Work,
            TicketKind::Verification,
            TicketKind::Audit,
            TicketKind::Investigation,
            TicketKind::Recovery,
            TicketKind::Harness,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).expect("serialize");
            let deserialized: TicketKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(kind, deserialized);
        }
    }

    #[test]
    fn ticket_state_round_trip_serde() {
        let states = vec![
            TicketState::Draft,
            TicketState::Blocked,
            TicketState::Ready,
            TicketState::Leased,
            TicketState::Running,
            TicketState::Submitted,
            TicketState::Verifying,
            TicketState::Auditing,
            TicketState::Rework,
            TicketState::Replan,
            TicketState::Recovery,
            TicketState::Escalated,
            TicketState::Closed,
            TicketState::Cancelled,
        ];
        for state in states {
            let json = serde_json::to_string(&state).expect("serialize");
            let deserialized: TicketState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(state, deserialized);
        }
    }

    #[test]
    fn trigger_round_trip_serde() {
        let triggers = vec![
            Trigger::Activate,
            Trigger::DependenciesSatisfied,
            Trigger::DependencyReopened,
            Trigger::LeaseAcquired,
            Trigger::WorkStarted,
            Trigger::LeaseExpired,
            Trigger::LeaseReleased,
            Trigger::Submit,
            Trigger::BeginVerification,
            Trigger::VerificationPassed,
            Trigger::VerificationFailed,
            Trigger::AuditPassed,
            Trigger::AuditRejectedMinor,
            Trigger::AuditRejectedStructural,
            Trigger::RetryPermitted,
            Trigger::RetryExhausted,
            Trigger::EscalationResolved,
            Trigger::EscalationAbandoned,
            Trigger::ReplanComplete,
            Trigger::Cancel,
            Trigger::Reopen,
        ];
        for trigger in triggers {
            let json = serde_json::to_string(&trigger).expect("serialize");
            let deserialized: Trigger = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(trigger, deserialized);
        }
    }

    #[test]
    fn dependency_kind_round_trip_serde() {
        let kinds = vec![
            DependencyKind::Hard,
            DependencyKind::Soft,
            DependencyKind::Loop,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).expect("serialize");
            let deserialized: DependencyKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(kind, deserialized);
        }
    }

    #[test]
    fn resource_claim_round_trip_serde() {
        let pattern = PathPattern::new("src/**/*.rs").expect("valid pattern");
        let claim = ResourceClaim {
            pattern,
            mode: ResourceMode::Exclusive,
        };
        let json = serde_json::to_string(&claim).expect("serialize");
        let deserialized: ResourceClaim = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(claim, deserialized);
    }

    #[test]
    fn resource_mode_round_trip_serde() {
        let modes = vec![ResourceMode::Exclusive, ResourceMode::Shared];
        for mode in modes {
            let json = serde_json::to_string(&mode).expect("serialize");
            let deserialized: ResourceMode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(mode, deserialized);
        }
    }

    #[test]
    fn verification_policy_round_trip_serde() {
        let policies = vec![
            VerificationPolicy::None,
            VerificationPolicy::Single,
            VerificationPolicy::PerPredicate,
            VerificationPolicy::DoubleBlind,
        ];
        for policy in policies {
            let json = serde_json::to_string(&policy).expect("serialize");
            let deserialized: VerificationPolicy =
                serde_json::from_str(&json).expect("deserialize");
            assert_eq!(policy, deserialized);
        }
    }

    #[test]
    fn failure_class_round_trip_serde() {
        let classes = vec![
            FailureClass::ExecutorCrash,
            FailureClass::LeaseTimeout,
            FailureClass::VerificationFailed,
            FailureClass::AuditRejectedMinor,
            FailureClass::AuditRejectedStructural,
            FailureClass::BudgetExhausted,
            FailureClass::AuthorityDenied,
            FailureClass::ProviderError,
            FailureClass::ResourceConflict,
            FailureClass::Other,
        ];
        for class in classes {
            let json = serde_json::to_string(&class).expect("serialize");
            let deserialized: FailureClass = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(class, deserialized);
        }
    }

    #[test]
    fn context_ref_round_trip_serde() {
        let refs = vec![
            ContextRef::Doc("docs/architecture.md".to_string()),
            ContextRef::Decision(tm_types::DecisionId::new("D-1").expect("valid decision id")),
            ContextRef::Artifact(
                tm_types::ArtifactId::new("ART-abc123def456").expect("valid artifact id"),
            ),
            ContextRef::Path("src/main.rs".to_string()),
        ];
        for context_ref in refs {
            let json = serde_json::to_string(&context_ref).expect("serialize");
            let deserialized: ContextRef = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(context_ref, deserialized);
        }
    }

    #[test]
    fn failure_record_round_trip_serde() {
        let record = FailureRecord {
            class: FailureClass::ExecutorCrash,
            reason: "Process exited with code 1".to_string(),
            attempt: 1,
            ts: tm_types::Timestamp::EPOCH,
        };
        let json = serde_json::to_string(&record).expect("serialize");
        let deserialized: FailureRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(record, deserialized);
    }

    #[test]
    fn retry_policy_with_non_retryable() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay_seconds: 10,
            backoff_multiplier: 2.0,
            max_delay_seconds: 300,
            non_retryable: vec![FailureClass::ExecutorCrash, FailureClass::AuthorityDenied],
        };
        assert_eq!(policy.non_retryable.len(), 2);
        assert!(policy.non_retryable.contains(&FailureClass::ExecutorCrash));
        assert!(policy
            .non_retryable
            .contains(&FailureClass::AuthorityDenied));
    }
}

/// Budget for how many times a `Loop`-edge cycle may execute before it must terminate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CycleBudget {
    /// Maximum number of times the cycle may iterate.
    pub max_iterations: u32,
    /// Number of iterations completed so far.
    pub iterations: u32,
}

impl CycleBudget {
    /// True when at least one more iteration is permitted.
    pub fn has_capacity(&self) -> bool {
        self.iterations < self.max_iterations
    }
}

/// The full, closed set of failure classes a [`FailureRecord`] may carry. Determines, together
/// with `RetryPolicy::non_retryable`, whether `Recovery` retries or escalates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum FailureClass {
    /// The executor process died or was killed before completing.
    ExecutorCrash,
    /// The lease expired without a heartbeat.
    LeaseTimeout,
    /// Submitted evidence did not satisfy the success predicates.
    VerificationFailed,
    /// An auditor rejected the submission for a minor, fixable reason.
    AuditRejectedMinor,
    /// An auditor rejected the submission for a structural reason.
    AuditRejectedStructural,
    /// A budget scope was exhausted mid-execution.
    BudgetExhausted,
    /// The ticket's authority was insufficient for an action it needed to take.
    AuthorityDenied,
    /// An underlying provider (model/tool) became unavailable or errored.
    ProviderError,
    /// A resource claim conflicted with another live claim.
    ResourceConflict,
    /// Any failure that does not fit another class; `reason` on the record carries detail.
    Other,
}

/// One recorded failure against a ticket, feeding `RetryPolicy` and the audit trail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailureRecord {
    /// The class of failure, gating retryability.
    pub class: FailureClass,
    /// Free-text detail, always present even when `class` is specific (for humans and audits).
    pub reason: String,
    /// The attempt number this failure occurred on.
    pub attempt: u32,
    /// When this failure was recorded.
    pub ts: Timestamp,
}
