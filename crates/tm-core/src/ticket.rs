//! The ticket record and its vocabulary.
//!
//! Owns [`Ticket`] itself plus every type that appears in its fields: [`TicketKind`],
//! [`TicketState`], [`Trigger`] (the machine-input alphabet consumed by [`crate::machine`]),
//! [`DependencyKind`], [`ResourceClaim`], [`ExecutorRequirements`], [`ContextRef`],
//! [`VerificationPolicy`], [`RetryPolicy`], [`CycleBudget`], [`FailureClass`] and
//! [`FailureRecord`]. Everything here is plain data: no I/O, no clock reads, no id allocation.
//! Serde uses external tagging throughout (never internal), matching `tm-types`' convention,
//! because internally-tagged recursive enums blow rustc's recursion limit.

use serde::{Deserialize, Serialize};
use tm_types::{Authority, Budget, MilestoneId, Predicate, TicketId, Timestamp};

/// `"YYYY-MM-DD"`, the one wire/CLI shape [`Ticket::due`] ever appears as -- both
/// `due_date::{serialize,deserialize}` below and [`parse_due_date`]/[`format_due_date`] (used by
/// `tm-cli`'s `--due` flag and `ticket show`/`ticket list`) share this single format constant so
/// the shape can't drift between the two call sites.
pub(crate) const DUE_DATE_FORMAT: &[time::format_description::BorrowedFormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day]");

/// Parse a `--due`-flag-shaped date string, per `SPEC.md`'s "date literal" convention: exactly
/// `YYYY-MM-DD`, no time-of-day, no offset. The error text is meant to reach a person verbatim
/// (a CLI arg-parse error), so it stays plain rather than echoing `time`'s own parser diagnostic.
pub fn parse_due_date(s: &str) -> Result<time::Date, String> {
    time::Date::parse(s, DUE_DATE_FORMAT).map_err(|_| "use YYYY-MM-DD, e.g. 2026-10-01".to_string())
}

/// Render a due date back to its `YYYY-MM-DD` wire/display form.
pub fn format_due_date(date: time::Date) -> String {
    // `DUE_DATE_FORMAT` only ever fails to format on an out-of-range component, which
    // `time::Date` cannot represent in the first place, so this is unreachable in practice; fall
    // back to `time`'s own `Display` rather than panicking if it somehow ever did.
    date.format(DUE_DATE_FORMAT)
        .unwrap_or_else(|_| date.to_string())
}

/// `due`'s on-the-wire shape: a plain `"YYYY-MM-DD"` string (or absent/`null`), never `time`'s
/// own internal representation -- matches [`Timestamp`]'s "stable string form" convention in
/// `tm-types` rather than a machine-specific encoding.
mod due_date {
    use super::DUE_DATE_FORMAT as FORMAT;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use time::Date;

    pub fn serialize<S: Serializer>(date: &Option<Date>, s: S) -> Result<S::Ok, S::Error> {
        match date {
            Some(d) => {
                let formatted = d.format(FORMAT).map_err(serde::ser::Error::custom)?;
                Some(formatted).serialize(s)
            }
            None => None::<String>.serialize(s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Date>, D::Error> {
        let opt: Option<String> = Option::deserialize(d)?;
        match opt {
            Some(s) => Date::parse(&s, FORMAT)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

/// What kind of work a ticket represents, per `SPEC.md` §4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketKind {
    /// Ordinary work: implement, fix, refactor.
    #[serde(alias = "task")]
    Work,
    /// A verification node, checking another ticket's submission against its success predicates.
    Verification,
    /// An audit node, checking a verification pass for structural/process correctness.
    Audit,
    /// Open-ended investigation with no predetermined patch.
    Investigation,
    /// Recovery/remediation work spawned to unblock a failure.
    Recovery,
    /// Work that changes the harness (agent configuration, tooling) rather than the product.
    Harness,
}

impl TicketKind {
    /// True for kinds allowed to close with `VerificationPolicy::None` (`SPEC.md` §4.3
    /// invariant: every `Closed` ticket has verification evidence unless policy is `None` and
    /// its kind permits it).
    pub fn permits_unverified_close(self) -> bool {
        matches!(self, TicketKind::Investigation | TicketKind::Harness)
    }
}

/// A ticket's position in the state machine (`SPEC.md` §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketState {
    /// Just created; dependencies not yet evaluated.
    Draft,
    /// Activated but has at least one unsatisfied dependency.
    Blocked,
    /// Eligible for lease: all dependencies satisfied.
    Ready,
    /// Held by a lease, not yet started.
    Leased,
    /// Actively being worked by the lease holder.
    Running,
    /// Work submitted with evidence, awaiting verification.
    Submitted,
    /// A verification ticket is checking the submission.
    Verifying,
    /// An audit ticket is checking the verification pass.
    Auditing,
    /// Sent back for a bounded fix by the same lineage.
    Rework,
    /// Sent back for children to be regenerated by a planner ticket.
    Replan,
    /// Applying `RetryPolicy` after a failure.
    Recovery,
    /// Needs a human or higher authority to respond before it can resume.
    Escalated,
    /// Terminal: done.
    Closed,
    /// Terminal: abandoned.
    Cancelled,
}

impl TicketState {
    /// Every state, in declaration order, for exhaustive enumeration in tests and diagnostics.
    pub const ALL: &'static [TicketState] = &[
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
}

/// The alphabet of inputs the transition function consumes, per `SPEC.md` §4.3's numbered
/// rules. One variant per named trigger in the spec; `Cancel` and `Reopen` carry no data because
/// authority/evidence checks happen in `store.rs`, not in the pure machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Rule 1: `Draft -> Blocked`, continuing to `Ready` if no unsatisfied dependencies.
    Activate,
    /// Rule 2: `Blocked -> Ready`, computed by the scheduler.
    DependenciesSatisfied,
    /// Rule 2 (reverse): a previously-satisfied dependency became unsatisfied again.
    DependenciesUnsatisfied,
    /// Rule 3: `Ready -> Leased`. Only the scheduler emits this.
    LeaseAcquired,
    /// Rule 4: `Leased -> Running`.
    WorkStarted,
    /// Rule 4: `Leased|Running -> Ready`, attempt already incremented at lease time.
    LeaseExpired,
    /// Rule 4: `Leased|Running -> Ready`.
    LeaseReleased,
    /// `Leased|Running -> Ready`: a worker approaching its budget ceiling finished or rolled
    /// back its effect in flight and handed the ticket back clean, instead of discovering
    /// exhaustion as a failure (`SPEC.md` §31.3 "handoff, not death"). Structurally identical to
    /// [`Trigger::LeaseExpired`]/[`Trigger::LeaseReleased`] (same `Leased|Running -> Ready`
    /// shape, `attempts` untouched — `attempts` only ever increments at
    /// [`Trigger::LeaseAcquired`]) but distinct in meaning: this trigger, unlike
    /// [`Trigger::Failed`], never routes through `Recovery`, so a budget handoff consumes no
    /// retry and is not evidence that anything went wrong
    /// (`docs/audit-2026-09-18-fable.md` B-10).
    BudgetHandoff,
    /// Rule 5: `Running -> Submitted`. Must carry evidence (enforced in `store.rs`).
    Submit,
    /// Rule 6: `Submitted -> Verifying`, automatic.
    VerificationStarted,
    /// Rule 6: `Verifying -> Auditing`.
    VerificationPassed,
    /// Rule 6: `Verifying -> Recovery`.
    VerificationFailed,
    /// Rule 7: `Auditing -> Closed`.
    AuditPassed,
    /// Rule 7: `Auditing -> Rework`.
    AuditRejectedMinor,
    /// Rule 7: `Auditing -> Replan`.
    AuditRejectedStructural,
    /// The executing worker failed: `Leased | Running -> Recovery`.
    ///
    /// SPEC.md §4.3's diagram routes `RUNNING ├── failure ──► RECOVERY`, which is how an
    /// executor crash, a tool failure or an insufficient-authority stop enters the retry
    /// machinery. Without it, failure could only be recorded after verification, so a worker
    /// that died mid-run had no legal way back into the graph.
    Failed,
    /// Rule 8: `Recovery -> Ready`, while `attempts < max_attempts` and budget remains.
    RetryScheduled,
    /// Rule 8: `Recovery -> Escalated`, attempts exhausted, budget exhausted, or a
    /// non-retryable failure class.
    RetryExhausted,
    /// Rule 9: `Rework -> Ready`.
    ReworkRestarted,
    /// Rule 9: `Replan -> Blocked`.
    Replanned,
    /// Rule 10: `Escalated -> Blocked`, once a human/higher authority responds.
    EscalationResolved,
    /// Rule 10: `Escalated -> Cancelled`, on abandon.
    Abandoned,
    /// Rule 11: `Closed -> Blocked`, carrying `authority.project.reopen_milestone`.
    Reopen,
    /// Rule 12: any non-terminal state `-> Cancelled`, with `tickets.cancel` authority.
    Cancel,
}

/// How one ticket depends on another, per `SPEC.md` §4.3's cycle rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    /// Must be `Closed` before the dependent can be `Ready`; participates in the acyclic region.
    Hard,
    /// Advisory ordering; does not gate readiness, still must be acyclic.
    Soft,
    /// May participate in a cycle, but only if every edge in that cycle is `Loop` and the cycle
    /// carries a [`CycleBudget`].
    Loop,
}

/// Exclusive vs. shared access for a [`ResourceClaim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceMode {
    /// No other live lease may hold an overlapping claim, exclusive or shared.
    Exclusive,
    /// May coexist with other `Shared` claims over overlapping paths, but not with `Exclusive`.
    Shared,
}

/// A claim a ticket's lease will hold over some subset of the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceClaim {
    /// Path patterns this claim covers.
    pub paths: tm_types::PatternSet,
    /// Exclusive or shared access.
    pub mode: ResourceMode,
}

/// What kind of executor a ticket needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorRequirements {
    /// The role the executor should be provisioned as.
    pub role: tm_types::Role,
    /// True when only a human may execute this ticket.
    pub human_required: bool,
    /// The minimum tolerance for role/capability mismatch the scheduler may accept.
    pub min_capability: tm_types::Tolerance,
}

impl Default for ExecutorRequirements {
    /// What a ticket created without saying gets, from any surface: a fast coding agent.
    fn default() -> Self {
        ExecutorRequirements {
            role: tm_types::Role::CoderFast,
            human_required: false,
            min_capability: tm_types::Tolerance::default(),
        }
    }
}

/// A reference to context material a worker should load before starting, resolved by
/// `tm-context`; `tm-core` only stores the pointer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRef {
    /// Free-form locator (doc path, decision id, ticket id, artifact id) interpreted by
    /// `tm-context`.
    pub locator: String,
    /// Human-readable note on why this context matters for the ticket.
    pub reason: String,
}

/// How a ticket's submission is checked before it may close. Defaults to
/// [`VerificationPolicy::Single`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPolicy {
    /// No verification evidence is required. Only legal for kinds where
    /// [`TicketKind::permits_unverified_close`] is true.
    None,
    /// A single verification ticket must pass.
    #[default]
    Single,
    /// Every predicate in `success` must be independently checked.
    EveryPredicate,
    /// A verification ticket must pass, and its pass must itself be audited.
    Audited,
}

/// Exponential-backoff retry policy applied by `Recovery` (`SPEC.md` §4.3 rule 8).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of attempts before escalating.
    pub max_attempts: u32,
    /// Base delay before the first retry.
    pub base_delay_seconds: u32,
    /// Multiplier applied to the delay after each attempt.
    pub backoff_multiplier: f64,
    /// Upper bound on the computed delay, regardless of attempt count.
    pub max_delay_seconds: u32,
}

impl Default for RetryPolicy {
    /// Three attempts, five-second base delay, doubling backoff, capped at five minutes.
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 5,
            backoff_multiplier: 2.0,
            max_delay_seconds: 300,
        }
    }
}

impl RetryPolicy {
    /// The delay before the retry following `attempt` (1-based: the delay before the *second*
    /// attempt is `delay_for_attempt(1)`), clamped to `max_delay_seconds`.
    pub fn delay_for_attempt(&self, attempt: u32) -> u32 {
        if attempt == 0 {
            0
        } else {
            let delay =
                self.base_delay_seconds as f64 * self.backoff_multiplier.powi(attempt as i32 - 1);
            (delay as u32).min(self.max_delay_seconds)
        }
    }
}

/// Budget governing how many times a `Loop`-edge cycle may execute before it must terminate,
/// per `SPEC.md` §4.3's cycle-legality rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleBudget {
    /// Maximum number of times the cycle may be traversed.
    pub max_iterations: u32,
    /// Number of traversals so far.
    pub iterations: u32,
}

impl CycleBudget {
    /// True when another traversal is still permitted.
    pub fn has_budget(&self) -> bool {
        self.iterations < self.max_iterations
    }
}

/// The closed set of reasons a ticket can fail, per `SPEC.md` §4.3/§4.7. Determines whether
/// `Recovery` may retry (`RetryPolicy`) or must escalate immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The executor process crashed or its lease expired without a submission.
    ExecutorCrash,
    /// The submitted change failed verification against `success` predicates.
    VerificationFailed,
    /// An audit rejected the verification pass as structurally wrong.
    AuditRejected,
    /// A provider (model/API) was unavailable or errored persistently.
    ProviderUnavailable,
    /// A budget (project, milestone, ticket or lease) was exhausted mid-execution.
    BudgetExhausted,
    /// The ticket's authority was insufficient for an action it needed to take.
    AuthorityDenied,
    /// A resource conflict prevented the lease from being usable.
    ResourceConflict,
    /// Failure for a reason not captured by another class; `detail` on [`FailureRecord`] carries
    /// specifics.
    Other,
}

impl FailureClass {
    /// True when `Recovery` may schedule a retry for this class; false forces immediate
    /// escalation (`SPEC.md` §4.7: `BudgetExhausted` is always non-retryable).
    pub fn is_retryable(self) -> bool {
        !matches!(self, FailureClass::BudgetExhausted)
    }
}

/// One recorded failure on a ticket's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    /// The class of failure.
    pub class: FailureClass,
    /// Free-form detail (error message, rejection reason).
    pub detail: String,
    /// When the failure was recorded.
    pub at: Timestamp,
    /// The attempt number this failure occurred on.
    pub attempt: u32,
}

/// The full ticket record, per `SPEC.md` §4.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    /// Identity.
    pub id: TicketId,
    /// What kind of work this is.
    pub kind: TicketKind,
    /// Natural-language objective; may be fuzzy.
    pub objective: String,
    /// Current machine state.
    pub state: TicketState,
    /// Parent ticket, if this is a child.
    pub parent: Option<TicketId>,
    /// Direct children.
    pub children: Vec<TicketId>,
    /// Dependencies, with their kind tracked separately in `ticket_deps` (materialized) /
    /// [`crate::graph`] (in-memory); this field lists the target ids only.
    pub dependencies: Vec<TicketId>,
    /// The milestone this ticket belongs to, if any.
    pub milestone: Option<MilestoneId>,
    /// When this ticket is due, if a date was set. `tm ticket new/edit --due YYYY-MM-DD` sets
    /// it, `--due none` clears it; a milestone's own due date is the max of its member
    /// tickets' due dates (`tm milestone show`).
    #[serde(default, skip_serializing_if = "Option::is_none", with = "due_date")]
    pub due: Option<time::Date>,
    /// The authority this ticket may lease out to an executor.
    pub authority: Authority,
    /// Resource claims a lease on this ticket will hold.
    pub resources: Vec<ResourceClaim>,
    /// Requirements on who/what may execute this ticket.
    pub executor: ExecutorRequirements,
    /// Context material a worker should load.
    pub context_refs: Vec<ContextRef>,
    /// Success predicates, machine-checkable where possible.
    pub success: Vec<Predicate>,
    /// How submissions are verified before close.
    pub verification: VerificationPolicy,
    /// Budget available to this ticket and its leases.
    pub budget: Budget,
    /// Retry behavior on failure.
    pub retry: RetryPolicy,
    /// Cycle budget, if this ticket participates in a `Loop`-edge cycle.
    pub cycle: Option<CycleBudget>,
    /// Number of lease attempts so far.
    pub attempts: u32,
    /// History of recorded failures.
    pub failures: Vec<FailureRecord>,
    /// Scheduling priority; higher runs first among otherwise-eligible tickets.
    pub priority: i32,
    /// Creation timestamp.
    pub created: Timestamp,
    /// Last-updated timestamp.
    pub updated: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_delay_for_attempt_zero_returns_zero() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 10,
            backoff_multiplier: 2.0,
            max_delay_seconds: 120,
        };
        assert_eq!(policy.delay_for_attempt(0), 0);
    }

    #[test]
    fn retry_policy_delay_for_attempt_one_returns_base_delay() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 10,
            backoff_multiplier: 2.0,
            max_delay_seconds: 120,
        };
        assert_eq!(policy.delay_for_attempt(1), 10);
    }

    #[test]
    fn retry_policy_delay_exponential_backoff() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay_seconds: 10,
            backoff_multiplier: 2.0,
            max_delay_seconds: 1000,
        };
        assert_eq!(policy.delay_for_attempt(1), 10);
        assert_eq!(policy.delay_for_attempt(2), 20);
        assert_eq!(policy.delay_for_attempt(3), 40);
        assert_eq!(policy.delay_for_attempt(4), 80);
        assert_eq!(policy.delay_for_attempt(5), 160);
    }

    #[test]
    fn retry_policy_delay_clamped_at_max() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base_delay_seconds: 10,
            backoff_multiplier: 2.0,
            max_delay_seconds: 100,
        };
        assert_eq!(policy.delay_for_attempt(5), 100);
        assert_eq!(policy.delay_for_attempt(6), 100);
        assert_eq!(policy.delay_for_attempt(10), 100);
    }

    #[test]
    fn retry_policy_delay_fractional_backoff() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay_seconds: 100,
            backoff_multiplier: 1.5,
            max_delay_seconds: 1000,
        };
        assert_eq!(policy.delay_for_attempt(1), 100);
        assert_eq!(policy.delay_for_attempt(2), 150);
        assert_eq!(policy.delay_for_attempt(3), 225);
        assert_eq!(policy.delay_for_attempt(4), 337);
    }

    #[test]
    fn retry_policy_delay_one_multiplier() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base_delay_seconds: 50,
            backoff_multiplier: 1.0,
            max_delay_seconds: 200,
        };
        assert_eq!(policy.delay_for_attempt(1), 50);
        assert_eq!(policy.delay_for_attempt(5), 50);
        assert_eq!(policy.delay_for_attempt(100), 50);
    }

    #[test]
    fn cycle_budget_has_budget_not_exhausted() {
        let budget = CycleBudget {
            max_iterations: 5,
            iterations: 0,
        };
        assert!(budget.has_budget());

        let budget = CycleBudget {
            max_iterations: 5,
            iterations: 4,
        };
        assert!(budget.has_budget());
    }

    #[test]
    fn cycle_budget_has_budget_exhausted() {
        let budget = CycleBudget {
            max_iterations: 5,
            iterations: 5,
        };
        assert!(!budget.has_budget());

        let budget = CycleBudget {
            max_iterations: 1,
            iterations: 1,
        };
        assert!(!budget.has_budget());
    }

    #[test]
    fn failure_class_is_retryable() {
        assert!(FailureClass::ExecutorCrash.is_retryable());
        assert!(FailureClass::VerificationFailed.is_retryable());
        assert!(FailureClass::AuditRejected.is_retryable());
        assert!(FailureClass::ProviderUnavailable.is_retryable());
        assert!(FailureClass::AuthorityDenied.is_retryable());
        assert!(FailureClass::ResourceConflict.is_retryable());
        assert!(FailureClass::Other.is_retryable());
    }

    #[test]
    fn failure_class_not_retryable() {
        assert!(!FailureClass::BudgetExhausted.is_retryable());
    }

    #[test]
    fn ticket_kind_permits_unverified_close() {
        assert!(TicketKind::Investigation.permits_unverified_close());
        assert!(TicketKind::Harness.permits_unverified_close());
        assert!(!TicketKind::Work.permits_unverified_close());
        assert!(!TicketKind::Verification.permits_unverified_close());
        assert!(!TicketKind::Audit.permits_unverified_close());
        assert!(!TicketKind::Recovery.permits_unverified_close());
    }

    #[test]
    fn ticket_state_all_contains_all_variants() {
        assert_eq!(TicketState::ALL.len(), 14);
        assert_eq!(TicketState::ALL[0], TicketState::Draft);
        assert_eq!(TicketState::ALL[1], TicketState::Blocked);
        assert_eq!(TicketState::ALL[2], TicketState::Ready);
        assert_eq!(TicketState::ALL[13], TicketState::Cancelled);
    }

    #[test]
    fn ticket_kind_serialization() {
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
    fn ticket_kind_task_alias_deserializes_to_work() {
        let deserialized: TicketKind =
            serde_json::from_str("\"task\"").expect("deserialize task alias");
        assert_eq!(deserialized, TicketKind::Work);
    }

    #[test]
    fn ticket_state_serialization() {
        for &state in TicketState::ALL {
            let json = serde_json::to_string(&state).expect("serialize");
            let deserialized: TicketState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(state, deserialized);
        }
    }

    #[test]
    fn trigger_serialization() {
        let triggers = vec![
            Trigger::Activate,
            Trigger::DependenciesSatisfied,
            Trigger::DependenciesUnsatisfied,
            Trigger::LeaseAcquired,
            Trigger::WorkStarted,
            Trigger::LeaseExpired,
            Trigger::LeaseReleased,
            Trigger::Submit,
            Trigger::VerificationStarted,
            Trigger::VerificationPassed,
            Trigger::VerificationFailed,
            Trigger::AuditPassed,
            Trigger::AuditRejectedMinor,
            Trigger::AuditRejectedStructural,
            Trigger::RetryScheduled,
            Trigger::RetryExhausted,
            Trigger::ReworkRestarted,
            Trigger::Replanned,
            Trigger::EscalationResolved,
            Trigger::Abandoned,
            Trigger::Reopen,
            Trigger::Cancel,
        ];
        for trigger in triggers {
            let json = serde_json::to_string(&trigger).expect("serialize");
            let deserialized: Trigger = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(trigger, deserialized);
        }
    }

    #[test]
    fn dependency_kind_serialization() {
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
    fn resource_mode_serialization() {
        let modes = vec![ResourceMode::Exclusive, ResourceMode::Shared];
        for mode in modes {
            let json = serde_json::to_string(&mode).expect("serialize");
            let deserialized: ResourceMode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(mode, deserialized);
        }
    }

    #[test]
    fn resource_claim_construction() {
        let patterns = tm_types::PatternSet::empty();
        let claim = ResourceClaim {
            paths: patterns,
            mode: ResourceMode::Exclusive,
        };
        assert_eq!(claim.mode, ResourceMode::Exclusive);
    }

    #[test]
    fn resource_claim_serialization() {
        let patterns = tm_types::PatternSet::all();
        let claim = ResourceClaim {
            paths: patterns,
            mode: ResourceMode::Shared,
        };
        let json = serde_json::to_string(&claim).expect("serialize");
        let deserialized: ResourceClaim = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(claim, deserialized);
    }

    #[test]
    fn executor_requirements_construction() {
        let reqs = ExecutorRequirements {
            role: tm_types::Role::CoderFast,
            human_required: false,
            min_capability: tm_types::Tolerance::Any,
        };
        assert!(!reqs.human_required);
        assert_eq!(reqs.role, tm_types::Role::CoderFast);
    }

    #[test]
    fn executor_requirements_serialization() {
        let reqs = ExecutorRequirements {
            role: tm_types::Role::ReviewerSemantic,
            human_required: true,
            min_capability: tm_types::Tolerance::Strict,
        };
        let json = serde_json::to_string(&reqs).expect("serialize");
        let deserialized: ExecutorRequirements = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(reqs, deserialized);
    }

    #[test]
    fn context_ref_construction() {
        let ctx = ContextRef {
            locator: "doc/path".to_string(),
            reason: "background material".to_string(),
        };
        assert_eq!(ctx.locator, "doc/path");
        assert_eq!(ctx.reason, "background material");
    }

    #[test]
    fn context_ref_serialization() {
        let ctx = ContextRef {
            locator: "M-123".to_string(),
            reason: "related milestone".to_string(),
        };
        let json = serde_json::to_string(&ctx).expect("serialize");
        let deserialized: ContextRef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ctx, deserialized);
    }

    #[test]
    fn verification_policy_serialization() {
        let policies = vec![
            VerificationPolicy::None,
            VerificationPolicy::Single,
            VerificationPolicy::EveryPredicate,
            VerificationPolicy::Audited,
        ];
        for policy in policies {
            let json = serde_json::to_string(&policy).expect("serialize");
            let deserialized: VerificationPolicy =
                serde_json::from_str(&json).expect("deserialize");
            assert_eq!(policy, deserialized);
        }
    }

    #[test]
    fn failure_record_construction() {
        let now = Timestamp::EPOCH;
        let record = FailureRecord {
            class: FailureClass::ExecutorCrash,
            detail: "Process exited with code 1".to_string(),
            at: now,
            attempt: 1,
        };
        assert_eq!(record.class, FailureClass::ExecutorCrash);
        assert_eq!(record.attempt, 1);
    }

    #[test]
    fn failure_record_serialization() {
        let now = Timestamp::EPOCH.plus_seconds(100);
        let record = FailureRecord {
            class: FailureClass::ProviderUnavailable,
            detail: "API timeout".to_string(),
            at: now,
            attempt: 2,
        };
        let json = serde_json::to_string(&record).expect("serialize");
        let deserialized: FailureRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(record, deserialized);
    }

    #[test]
    fn retry_policy_construction() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay_seconds: 10,
            backoff_multiplier: 2.0,
            max_delay_seconds: 300,
        };
        assert_eq!(policy.max_attempts, 5);
        assert_eq!(policy.base_delay_seconds, 10);
    }

    #[test]
    fn retry_policy_serialization() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 5,
            backoff_multiplier: 1.5,
            max_delay_seconds: 60,
        };
        let json = serde_json::to_string(&policy).expect("serialize");
        let deserialized: RetryPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(policy, deserialized);
    }

    #[test]
    fn cycle_budget_construction() {
        let budget = CycleBudget {
            max_iterations: 10,
            iterations: 3,
        };
        assert_eq!(budget.max_iterations, 10);
        assert_eq!(budget.iterations, 3);
    }

    #[test]
    fn cycle_budget_serialization() {
        let budget = CycleBudget {
            max_iterations: 5,
            iterations: 2,
        };
        let json = serde_json::to_string(&budget).expect("serialize");
        let deserialized: CycleBudget = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(budget, deserialized);
    }

    #[test]
    fn ticket_minimal_construction() {
        let now = Timestamp::EPOCH;
        let id = TicketId::new("T-1").expect("valid id");
        let ticket = Ticket {
            id,
            kind: TicketKind::Work,
            objective: "Implement feature".to_string(),
            state: TicketState::Draft,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            authority: Authority::none(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: tm_types::Role::CoderFast,
                human_required: false,
                min_capability: tm_types::Tolerance::Any,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 5,
                backoff_multiplier: 2.0,
                max_delay_seconds: 60,
            },
            cycle: None,
            attempts: 0,
            failures: vec![],
            priority: 0,
            created: now,
            updated: now,
        };
        assert_eq!(ticket.kind, TicketKind::Work);
        assert_eq!(ticket.state, TicketState::Draft);
        assert!(ticket.parent.is_none());
        assert!(ticket.children.is_empty());
    }

    #[test]
    fn ticket_with_cycle_budget() {
        let now = Timestamp::EPOCH;
        let id = TicketId::new("V-1").expect("valid id");
        let cycle_budget = Some(CycleBudget {
            max_iterations: 2,
            iterations: 0,
        });
        let ticket = Ticket {
            id,
            kind: TicketKind::Verification,
            objective: "Verify work".to_string(),
            state: TicketState::Verifying,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            authority: Authority::read_only(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: tm_types::Role::ReviewerSemantic,
                human_required: false,
                min_capability: tm_types::Tolerance::Preferred,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::None,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 1,
                base_delay_seconds: 0,
                backoff_multiplier: 1.0,
                max_delay_seconds: 0,
            },
            cycle: cycle_budget,
            attempts: 0,
            failures: vec![],
            priority: 10,
            created: now,
            updated: now,
        };
        assert!(ticket.cycle.is_some());
        assert_eq!(ticket.cycle.unwrap().max_iterations, 2);
    }

    #[test]
    fn ticket_with_failures() {
        let now = Timestamp::EPOCH;
        let id = TicketId::new("T-2").expect("valid id");
        let failures = vec![
            FailureRecord {
                class: FailureClass::ExecutorCrash,
                detail: "Crashed on attempt 1".to_string(),
                at: now,
                attempt: 1,
            },
            FailureRecord {
                class: FailureClass::ProviderUnavailable,
                detail: "API unavailable on attempt 2".to_string(),
                at: now.plus_seconds(60),
                attempt: 2,
            },
        ];
        let ticket = Ticket {
            id,
            kind: TicketKind::Work,
            objective: "Retry-prone task".to_string(),
            state: TicketState::Recovery,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            authority: Authority::none(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: tm_types::Role::CoderDeep,
                human_required: false,
                min_capability: tm_types::Tolerance::Strict,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 5,
                base_delay_seconds: 10,
                backoff_multiplier: 2.0,
                max_delay_seconds: 300,
            },
            cycle: None,
            attempts: 2,
            failures,
            priority: 5,
            created: now,
            updated: now.plus_seconds(120),
        };
        assert_eq!(ticket.failures.len(), 2);
        assert_eq!(ticket.attempts, 2);
        assert_eq!(ticket.state, TicketState::Recovery);
    }

    #[test]
    fn ticket_serialization() {
        let now = Timestamp::EPOCH;
        let id = TicketId::new("T-3").expect("valid id");
        let ticket = Ticket {
            id,
            kind: TicketKind::Harness,
            objective: "Update tooling".to_string(),
            state: TicketState::Ready,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            authority: Authority::default(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: tm_types::Role::ExplorerCheap,
                human_required: false,
                min_capability: tm_types::Tolerance::Any,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::None,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 1,
                base_delay_seconds: 0,
                backoff_multiplier: 1.0,
                max_delay_seconds: 0,
            },
            cycle: None,
            attempts: 0,
            failures: vec![],
            priority: 1,
            created: now,
            updated: now,
        };
        let json = serde_json::to_string(&ticket).expect("serialize");
        let deserialized: Ticket = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ticket, deserialized);
    }

    #[test]
    fn parse_due_date_accepts_iso_date() {
        let d = parse_due_date("2026-10-01").expect("valid date");
        assert_eq!(format_due_date(d), "2026-10-01");
    }

    #[test]
    fn parse_due_date_rejects_other_shapes_with_a_plain_message() {
        let err = parse_due_date("10/01/2026").expect_err("not YYYY-MM-DD");
        assert_eq!(err, "use YYYY-MM-DD, e.g. 2026-10-01");
    }

    #[test]
    fn due_absent_serializes_without_the_field_and_round_trips_to_none() {
        let ticket = ticket_minimal();
        assert!(ticket.due.is_none());
        let json = serde_json::to_value(&ticket).expect("serialize");
        assert!(
            json.get("due").is_none(),
            "an absent due date should not appear in the JSON at all: {json}"
        );
        let back: Ticket = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.due, None);
    }

    #[test]
    fn due_present_round_trips_as_a_plain_date_string() {
        let mut ticket = ticket_minimal();
        ticket.due = Some(parse_due_date("2026-10-01").expect("valid date"));
        let json = serde_json::to_value(&ticket).expect("serialize");
        assert_eq!(json["due"], "2026-10-01");
        let back: Ticket = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.due, ticket.due);
    }

    /// A minimal but complete [`Ticket`], for tests that only care about `due`.
    fn ticket_minimal() -> Ticket {
        let now = Timestamp::from_unix_seconds(0);
        Ticket {
            id: TicketId::new("T-1").expect("valid id"),
            kind: TicketKind::Work,
            objective: "x".to_string(),
            state: TicketState::Draft,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            authority: Authority::default(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: tm_types::Role::ExplorerCheap,
                human_required: false,
                min_capability: tm_types::Tolerance::Any,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 1,
                base_delay_seconds: 0,
                backoff_multiplier: 1.0,
                max_delay_seconds: 0,
            },
            cycle: None,
            attempts: 0,
            failures: vec![],
            priority: 0,
            created: now,
            updated: now,
        }
    }
}
