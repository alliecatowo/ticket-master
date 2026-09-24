//! Retry and recovery policy: backoff, retryability, cycle-budget accounting, escalation.
//!
//! Pure functions over `tm-core`'s already-materialized [`tm_core::ticket::Ticket`] fields
//! (`attempts`, `retry`, `budget`, `cycle`) and the failure class being recovered from. No I/O,
//! no clock reads beyond the `now` parameter. Backoff is exponential and **not jittered by
//! default**: at this scale, a scheduler tick that produces a different action list for
//! identical inputs is a worse failure mode than the thundering-herd jitter exists to prevent.
//! When [`crate::policy::SchedulingPolicy`] opts into jitter, it is applied by
//! [`crate::driver::SchedulerLoop`] (which holds an injected `IdSource`) as a post-processing
//! step on the delay [`decide_retry`] computes, never inside `plan()` itself — that keeps `plan`
//! pure regardless of the jitter setting.

use tm_core::ticket::{CycleBudget, FailureClass, Ticket};
use tm_types::Timestamp;

/// What [`decide_retry`] concluded for one failed ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    /// Schedule a retry: the ticket should return to `Ready` no earlier than `after`.
    Retry {
        /// Earliest time the ticket may be retried, per exponential backoff.
        after: Timestamp,
    },
    /// Stop retrying and escalate instead.
    Escalate(EscalationReason),
}

/// Why [`decide_retry`] chose to escalate rather than retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationReason {
    /// `ticket.attempts >= ticket.retry.max_attempts`.
    AttemptsExhausted,
    /// `ticket.budget`'s token allotment is exhausted.
    TokensExhausted,
    /// `ticket.budget`'s dollar allotment is exhausted.
    DollarsExhausted,
    /// `ticket.budget`'s wall-clock allotment is exhausted.
    WallSecondsExhausted,
    /// `ticket.cycle` is `Some` and its `CycleBudget::has_budget()` is false.
    CycleIterationsExhausted,
    /// `failure_class.is_retryable()` was false: this class never retries, regardless of
    /// attempts or budget remaining.
    NonRetryable(FailureClass),
}

/// The full recommendation for one ticket, pairing the ticket's id with [`RetryOutcome`] so
/// callers accumulating results across many tickets (`plan.rs`) don't need a side map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryDecision {
    /// The outcome for this ticket.
    pub outcome: RetryOutcome,
}

/// Decide whether `ticket` (which just failed with `failure`) should retry or escalate, as of
/// `now`. Checked in this order, first match wins:
/// 1. `!failure.is_retryable()` -> [`EscalationReason::NonRetryable`].
/// 2. Attempts exhausted -> [`EscalationReason::AttemptsExhausted`].
/// 3. Budget exhausted (tokens, then dollars, then wall seconds — first exhausted dimension
///    wins) -> the matching `EscalationReason`.
/// 4. `ticket.cycle` present and exhausted -> [`EscalationReason::CycleIterationsExhausted`].
/// 5. Otherwise -> `Retry { after: now + ticket.retry.delay_for_attempt(ticket.attempts + 1) }`.
pub fn decide_retry(ticket: &Ticket, failure: FailureClass, now: Timestamp) -> RetryDecision {
    // Step 1: Check if failure class is retryable.
    if !failure.is_retryable() {
        return RetryDecision {
            outcome: RetryOutcome::Escalate(EscalationReason::NonRetryable(failure)),
        };
    }

    // Step 2: Check if attempts are exhausted.
    if ticket.attempts >= ticket.retry.max_attempts {
        return RetryDecision {
            outcome: RetryOutcome::Escalate(EscalationReason::AttemptsExhausted),
        };
    }

    // Step 3: Check budget exhausted (tokens, then dollars, then wall_seconds in priority order).
    let remaining = ticket.budget.remaining();
    if remaining.tokens == 0 {
        return RetryDecision {
            outcome: RetryOutcome::Escalate(EscalationReason::TokensExhausted),
        };
    }
    if remaining.dollars_micros == 0 {
        return RetryDecision {
            outcome: RetryOutcome::Escalate(EscalationReason::DollarsExhausted),
        };
    }
    if remaining.wall_seconds == 0 {
        return RetryDecision {
            outcome: RetryOutcome::Escalate(EscalationReason::WallSecondsExhausted),
        };
    }

    // Step 4: Check if cycle budget is present and exhausted.
    if let Some(cycle_budget) = ticket.cycle {
        if !cycle_budget.has_budget() {
            return RetryDecision {
                outcome: RetryOutcome::Escalate(EscalationReason::CycleIterationsExhausted),
            };
        }
    }

    // Step 5: Schedule a retry with exponential backoff.
    let delay_seconds = ticket.retry.delay_for_attempt(ticket.attempts + 1);
    let after = now.plus_seconds(delay_seconds.into());
    RetryDecision {
        outcome: RetryOutcome::Retry { after },
    }
}

/// Record one traversal of a `Loop` edge: increments `budget.iterations` by one. Pure,
/// non-panicking even at `u32::MAX` (saturates) since a runaway cycle must escalate via
/// [`decide_retry`]'s cycle check, not panic the scheduler.
pub fn traverse_cycle_edge(budget: CycleBudget) -> CycleBudget {
    CycleBudget {
        max_iterations: budget.max_iterations,
        iterations: budget.iterations.saturating_add(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{Budget, TicketId};

    /// Helper to build a minimal valid ticket for testing.
    fn make_ticket(attempts: u32, max_attempts: u32) -> Ticket {
        Ticket {
            id: TicketId::new("T-1").expect("valid id"),
            kind: tm_core::ticket::TicketKind::Work,
            objective: "Test ticket".to_string(),
            state: tm_core::ticket::TicketState::Recovery,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            authority: tm_types::Authority::none(),
            resources: vec![],
            executor: tm_core::ticket::ExecutorRequirements {
                role: tm_types::Role::CoderFast,
                human_required: false,
                min_capability: tm_types::Tolerance::Any,
            },
            context_refs: vec![],
            success: vec![],
            verification: tm_core::ticket::VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: tm_core::ticket::RetryPolicy {
                max_attempts,
                base_delay_seconds: 10,
                backoff_multiplier: 2.0,
                max_delay_seconds: 300,
            },
            cycle: None,
            attempts,
            failures: vec![],
            priority: 0,
            created: Timestamp::EPOCH,
            updated: Timestamp::EPOCH,
        }
    }

    #[test]
    fn traverse_cycle_edge_increments_iterations() {
        let budget = CycleBudget {
            max_iterations: 5,
            iterations: 2,
        };
        let after = traverse_cycle_edge(budget);
        assert_eq!(after.iterations, 3);
        assert_eq!(after.max_iterations, 5);
    }

    #[test]
    fn decide_retry_happy_path_schedules_retry() {
        let ticket = make_ticket(0, 3);
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Retry { after } => {
                // First retry delay should be base_delay_seconds (10 seconds)
                assert_eq!(after, now.plus_seconds(10));
            }
            _ => panic!("Expected Retry, got Escalate"),
        }
    }

    #[test]
    fn decide_retry_second_attempt_exponential_backoff() {
        let ticket = make_ticket(1, 3);
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Retry { after } => {
                // Second retry delay should be base_delay * multiplier = 10 * 2 = 20 seconds
                assert_eq!(after, now.plus_seconds(20));
            }
            _ => panic!("Expected Retry, got Escalate"),
        }
    }

    #[test]
    fn decide_retry_non_retryable_escalates_immediately() {
        let ticket = make_ticket(0, 3);
        let now = Timestamp::EPOCH;
        let failure = FailureClass::BudgetExhausted;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Escalate(EscalationReason::NonRetryable(class)) => {
                assert_eq!(class, FailureClass::BudgetExhausted);
            }
            _ => panic!("Expected NonRetryable escalation"),
        }
    }

    #[test]
    fn decide_retry_attempts_exhausted() {
        let ticket = make_ticket(3, 3);
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Escalate(EscalationReason::AttemptsExhausted) => {}
            _ => panic!("Expected AttemptsExhausted escalation"),
        }
    }

    #[test]
    fn decide_retry_attempts_at_boundary_escalates() {
        let ticket = make_ticket(2, 3);
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        // With 2 attempts and max_attempts=3, there's still budget for retry
        let decision = decide_retry(&ticket, failure, now);
        match decision.outcome {
            RetryOutcome::Retry { .. } => {}
            _ => panic!("Expected Retry at boundary"),
        }

        // But with 3 attempts and max_attempts=3, it should escalate
        let ticket2 = make_ticket(3, 3);
        let decision2 = decide_retry(&ticket2, failure, now);
        match decision2.outcome {
            RetryOutcome::Escalate(EscalationReason::AttemptsExhausted) => {}
            _ => panic!("Expected AttemptsExhausted at exact boundary"),
        }
    }

    #[test]
    fn decide_retry_tokens_exhausted() {
        let mut ticket = make_ticket(0, 3);
        ticket.budget = Budget::new(0, 1000000, 3600);
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Escalate(EscalationReason::TokensExhausted) => {}
            _ => panic!("Expected TokensExhausted escalation"),
        }
    }

    #[test]
    fn decide_retry_dollars_exhausted() {
        let mut ticket = make_ticket(0, 3);
        ticket.budget = Budget::new(1000, 0, 3600);
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Escalate(EscalationReason::DollarsExhausted) => {}
            _ => panic!("Expected DollarsExhausted escalation"),
        }
    }

    #[test]
    fn decide_retry_wall_seconds_exhausted() {
        let mut ticket = make_ticket(0, 3);
        ticket.budget = Budget::new(1000, 1000000, 0);
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Escalate(EscalationReason::WallSecondsExhausted) => {}
            _ => panic!("Expected WallSecondsExhausted escalation"),
        }
    }

    #[test]
    fn decide_retry_cycle_iterations_exhausted() {
        let mut ticket = make_ticket(0, 3);
        ticket.cycle = Some(CycleBudget {
            max_iterations: 5,
            iterations: 5,
        });
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Escalate(EscalationReason::CycleIterationsExhausted) => {}
            _ => panic!("Expected CycleIterationsExhausted escalation"),
        }
    }

    #[test]
    fn decide_retry_cycle_with_budget_allows_retry() {
        let mut ticket = make_ticket(0, 3);
        ticket.cycle = Some(CycleBudget {
            max_iterations: 5,
            iterations: 3,
        });
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Retry { .. } => {}
            _ => panic!("Expected Retry with cycle budget available"),
        }
    }

    #[test]
    fn decide_retry_priority_non_retryable_before_attempts() {
        let ticket = make_ticket(5, 3);
        let now = Timestamp::EPOCH;
        let failure = FailureClass::BudgetExhausted;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Escalate(EscalationReason::NonRetryable(_)) => {}
            _ => panic!("Expected NonRetryable to be checked before attempts"),
        }
    }

    #[test]
    fn decide_retry_priority_tokens_before_dollars() {
        let mut ticket = make_ticket(0, 3);
        ticket.budget = Budget::new(0, 0, 3600);
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Escalate(EscalationReason::TokensExhausted) => {}
            _ => panic!("Expected TokensExhausted (tokens checked before dollars)"),
        }
    }

    #[test]
    fn decide_retry_priority_dollars_before_wall_seconds() {
        let mut ticket = make_ticket(0, 3);
        ticket.budget = Budget::new(1000, 0, 0);
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Escalate(EscalationReason::DollarsExhausted) => {}
            _ => panic!("Expected DollarsExhausted (dollars checked before wall_seconds)"),
        }
    }

    #[test]
    fn decide_retry_different_failure_classes_retryable() {
        let now = Timestamp::EPOCH;
        let retryable_classes = vec![
            FailureClass::ExecutorCrash,
            FailureClass::VerificationFailed,
            FailureClass::AuditRejected,
            FailureClass::ProviderUnavailable,
            FailureClass::AuthorityDenied,
            FailureClass::ResourceConflict,
            FailureClass::Other,
        ];

        for class in retryable_classes {
            let ticket = make_ticket(0, 3);
            let decision = decide_retry(&ticket, class, now);

            match decision.outcome {
                RetryOutcome::Retry { .. } => {}
                _ => panic!("Expected Retry for retryable class {:?}", class),
            }
        }
    }

    #[test]
    fn decide_retry_max_delay_clamped() {
        let mut ticket = make_ticket(5, 10);
        ticket.retry = tm_core::ticket::RetryPolicy {
            max_attempts: 10,
            base_delay_seconds: 10,
            backoff_multiplier: 2.0,
            max_delay_seconds: 100,
        };
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        let decision = decide_retry(&ticket, failure, now);

        match decision.outcome {
            RetryOutcome::Retry { after } => {
                // With 5 attempts, unclamped would be 10 * 2^4 = 160, but clamped to 100
                assert_eq!(after, now.plus_seconds(100));
            }
            _ => panic!("Expected Retry with clamped delay"),
        }
    }

    #[test]
    fn decide_retry_cycle_at_boundary() {
        let mut ticket = make_ticket(0, 3);
        ticket.cycle = Some(CycleBudget {
            max_iterations: 5,
            iterations: 4,
        });
        let now = Timestamp::EPOCH;
        let failure = FailureClass::ExecutorCrash;

        // With 4 iterations and max 5, has_budget() should be true
        let decision = decide_retry(&ticket, failure, now);
        match decision.outcome {
            RetryOutcome::Retry { .. } => {}
            _ => panic!("Expected Retry with cycle budget at boundary"),
        }

        // With 5 iterations and max 5, has_budget() should be false
        let mut ticket2 = make_ticket(0, 3);
        ticket2.cycle = Some(CycleBudget {
            max_iterations: 5,
            iterations: 5,
        });
        let decision2 = decide_retry(&ticket2, failure, now);
        match decision2.outcome {
            RetryOutcome::Escalate(EscalationReason::CycleIterationsExhausted) => {}
            _ => panic!("Expected CycleIterationsExhausted at exact boundary"),
        }
    }
}
