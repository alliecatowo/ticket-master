//! The pure ticket state machine.
//!
//! Owns [`transition`], the single pure function that decides ticket-state legality, plus the
//! `is_terminal`/`is_live`/`can_lease` helpers callers use instead of matching on `TicketState`
//! directly. Every numbered rule in `SPEC.md` §4.3 has a home here. No I/O, no clock, no ids:
//! given a state and a trigger, the answer is always the same.
//!
//! `transition` is total over `TicketState × Trigger`: every pair is covered by the match in its
//! implementation (most as `Err(InvalidTransition)`), which is exactly what the exhaustive
//! cross-product test in this module's `#[cfg(test)]` asserts — no pair is silently unhandled.

use thiserror::Error;

use crate::ticket::{TicketState, Trigger};

/// The `(from, trigger)` pair was not a legal transition.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("no transition from {from:?} on {trigger:?}")]
pub struct InvalidTransition {
    /// The state the ticket was in.
    pub from: TicketState,
    /// The trigger that was rejected.
    pub trigger: Trigger,
}

/// The pure transition function: `SPEC.md` §4.3 rules 1–12, exhaustively.
///
/// Note that rules 1 and 2 each describe two hops (`Draft -> Blocked [-> Ready]`); this function
/// only ever returns the *first* hop for a given trigger. The immediate `Blocked -> Ready`
/// continuation described in rule 1 ("if no unsatisfied dependencies it continues to `Ready`")
/// is the caller's (`store.rs::activate`) responsibility: it calls `transition` for `Activate`,
/// and then, if the freshly materialized ticket has no unsatisfied dependencies, calls
/// `transition` again for `DependenciesSatisfied` in the same command — both hops still go
/// through this one function, just twice.
pub fn transition(from: TicketState, trigger: Trigger) -> Result<TicketState, InvalidTransition> {
    use TicketState::*;
    use Trigger::*;

    // Rule 12 is a guard over `from` rather than a per-state arm: legal from any non-terminal
    // state, regardless of which state that is.
    if trigger == Cancel && !is_terminal(from) {
        return Ok(Cancelled);
    }

    match (from, trigger) {
        // Rule 1: Draft -> Blocked on Activate. The immediate Blocked -> Ready continuation (when
        // there are no unsatisfied dependencies) is the caller's responsibility; see module docs.
        (Draft, Activate) => Ok(Blocked),

        // Rule 2: Blocked -> Ready only via the scheduler's DependenciesSatisfied recomputation.
        (Blocked, DependenciesSatisfied) => Ok(Ready),
        // A previously-satisfied dependency regressing sends a Ready ticket back to Blocked.
        (Ready, DependencyReopened) => Ok(Blocked),

        // Rule 3: Ready -> Leased, scheduler-only.
        (Ready, LeaseAcquired) => Ok(Leased),

        // Rule 4: Leased -> Running on work starting; both Leased and Running fall back to Ready
        // on lease expiry or voluntary release.
        (Leased, WorkStarted) => Ok(Running),
        (Leased, LeaseExpired) => Ok(Ready),
        (Leased, LeaseReleased) => Ok(Ready),
        (Running, LeaseExpired) => Ok(Ready),
        (Running, LeaseReleased) => Ok(Ready),

        // Rule 5: Running -> Submitted, evidence-carrying submission.
        (Running, Submit) => Ok(Submitted),

        // Rule 6: Submitted -> Verifying automatically; Verifying resolves to Auditing or Recovery.
        (Submitted, BeginVerification) => Ok(Verifying),
        (Verifying, VerificationPassed) => Ok(Auditing),
        (Verifying, VerificationFailed) => Ok(Recovery),

        // Rule 7: Auditing resolves to Closed, Rework, or Replan.
        (Auditing, AuditPassed) => Ok(Closed),
        (Auditing, AuditRejectedMinor) => Ok(Rework),
        (Auditing, AuditRejectedStructural) => Ok(Replan),

        // Rule 8: Recovery applies RetryPolicy: another attempt goes back to Ready, exhaustion
        // (or a non-retryable failure class) goes to Escalated.
        (Recovery, RetryPermitted) => Ok(Ready),
        (Recovery, RetryExhausted) => Ok(Escalated),

        // Rule 9: Rework retries directly to Ready; Replan returns to Blocked once the planner
        // ticket has regenerated children.
        (Rework, RetryPermitted) => Ok(Ready),
        (Replan, ReplanComplete) => Ok(Blocked),

        // Rule 10: Escalation resolves back into the graph, or is abandoned (terminal).
        (Escalated, EscalationResolved) => Ok(Blocked),
        (Escalated, EscalationAbandoned) => Ok(Cancelled),

        // Rule 11: reopening a Closed ticket requires milestone-scoped authority, which this
        // pure function does not check; the caller (`store.rs`) gates the call on it.
        (Closed, Reopen) => Ok(Blocked),

        _ => Err(InvalidTransition { from, trigger }),
    }
}

/// True for `Closed` and `Cancelled`: no trigger (other than `Reopen`, for `Closed`) leads
/// anywhere from here without an explicit authority-gated exception.
pub fn is_terminal(state: TicketState) -> bool {
    matches!(state, TicketState::Closed | TicketState::Cancelled)
}

/// True for every state where the ticket is an active participant in scheduling/execution
/// (i.e. not `Draft`, not terminal). Used by invariant checks that only apply to live tickets.
pub fn is_live(state: TicketState) -> bool {
    !matches!(state, TicketState::Draft) && !is_terminal(state)
}

/// True only for `Ready`: the sole state from which `lease.rs::acquire` may succeed.
pub fn can_lease(state: TicketState) -> bool {
    matches!(state, TicketState::Ready)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: &[TicketState] = &[
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

    fn all_triggers() -> Vec<Trigger> {
        vec![
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
        ]
    }

    /// Enumerates the full `TicketState x Trigger` cross product and asserts `transition` never
    /// panics for any pair, i.e. the match in `transition` is total.
    #[test]
    fn transition_is_total_over_the_full_cross_product() {
        for &from in ALL_STATES {
            for trigger in all_triggers() {
                let _ = transition(from, trigger);
            }
        }
    }

    /// Every non-terminal state accepts `Cancel` and lands in `Cancelled` (rule 12).
    #[test]
    fn cancel_is_legal_from_every_non_terminal_state() {
        for &from in ALL_STATES {
            let result = transition(from, Trigger::Cancel);
            if is_terminal(from) {
                assert_eq!(
                    result,
                    Err(InvalidTransition {
                        from,
                        trigger: Trigger::Cancel
                    })
                );
            } else {
                assert_eq!(result, Ok(TicketState::Cancelled));
            }
        }
    }

    #[test]
    fn draft_activates_into_blocked() {
        assert_eq!(
            transition(TicketState::Draft, Trigger::Activate),
            Ok(TicketState::Blocked)
        );
    }

    #[test]
    fn blocked_becomes_ready_only_via_dependencies_satisfied() {
        assert_eq!(
            transition(TicketState::Blocked, Trigger::DependenciesSatisfied),
            Ok(TicketState::Ready)
        );
        assert!(transition(TicketState::Blocked, Trigger::LeaseAcquired).is_err());
    }

    #[test]
    fn ready_reverts_to_blocked_when_a_dependency_reopens() {
        assert_eq!(
            transition(TicketState::Ready, Trigger::DependencyReopened),
            Ok(TicketState::Blocked)
        );
    }

    #[test]
    fn ready_leases_and_leased_starts_running() {
        assert_eq!(
            transition(TicketState::Ready, Trigger::LeaseAcquired),
            Ok(TicketState::Leased)
        );
        assert_eq!(
            transition(TicketState::Leased, Trigger::WorkStarted),
            Ok(TicketState::Running)
        );
    }

    #[test]
    fn leased_and_running_fall_back_to_ready_on_expiry_or_release() {
        for state in [TicketState::Leased, TicketState::Running] {
            assert_eq!(
                transition(state, Trigger::LeaseExpired),
                Ok(TicketState::Ready)
            );
            assert_eq!(
                transition(state, Trigger::LeaseReleased),
                Ok(TicketState::Ready)
            );
        }
    }

    #[test]
    fn running_submits_into_submitted() {
        assert_eq!(
            transition(TicketState::Running, Trigger::Submit),
            Ok(TicketState::Submitted)
        );
    }

    #[test]
    fn submitted_moves_through_verifying_to_auditing_or_recovery() {
        assert_eq!(
            transition(TicketState::Submitted, Trigger::BeginVerification),
            Ok(TicketState::Verifying)
        );
        assert_eq!(
            transition(TicketState::Verifying, Trigger::VerificationPassed),
            Ok(TicketState::Auditing)
        );
        assert_eq!(
            transition(TicketState::Verifying, Trigger::VerificationFailed),
            Ok(TicketState::Recovery)
        );
    }

    #[test]
    fn auditing_resolves_to_closed_rework_or_replan() {
        assert_eq!(
            transition(TicketState::Auditing, Trigger::AuditPassed),
            Ok(TicketState::Closed)
        );
        assert_eq!(
            transition(TicketState::Auditing, Trigger::AuditRejectedMinor),
            Ok(TicketState::Rework)
        );
        assert_eq!(
            transition(TicketState::Auditing, Trigger::AuditRejectedStructural),
            Ok(TicketState::Replan)
        );
    }

    #[test]
    fn recovery_retries_to_ready_or_escalates_when_exhausted() {
        assert_eq!(
            transition(TicketState::Recovery, Trigger::RetryPermitted),
            Ok(TicketState::Ready)
        );
        assert_eq!(
            transition(TicketState::Recovery, Trigger::RetryExhausted),
            Ok(TicketState::Escalated)
        );
    }

    #[test]
    fn rework_retries_to_ready_and_replan_returns_to_blocked() {
        assert_eq!(
            transition(TicketState::Rework, Trigger::RetryPermitted),
            Ok(TicketState::Ready)
        );
        assert_eq!(
            transition(TicketState::Replan, Trigger::ReplanComplete),
            Ok(TicketState::Blocked)
        );
    }

    #[test]
    fn escalation_resolves_to_blocked_or_is_abandoned_into_cancelled() {
        assert_eq!(
            transition(TicketState::Escalated, Trigger::EscalationResolved),
            Ok(TicketState::Blocked)
        );
        assert_eq!(
            transition(TicketState::Escalated, Trigger::EscalationAbandoned),
            Ok(TicketState::Cancelled)
        );
    }

    #[test]
    fn closed_only_reopens_via_reopen_trigger() {
        assert_eq!(
            transition(TicketState::Closed, Trigger::Reopen),
            Ok(TicketState::Blocked)
        );
        assert_eq!(
            transition(TicketState::Closed, Trigger::Activate),
            Err(InvalidTransition {
                from: TicketState::Closed,
                trigger: Trigger::Activate
            })
        );
    }

    #[test]
    fn cancelled_is_a_dead_end() {
        for trigger in all_triggers() {
            assert!(transition(TicketState::Cancelled, trigger).is_err());
        }
    }

    #[test]
    fn unrelated_pairs_are_rejected_with_the_originating_state_and_trigger() {
        let err = transition(TicketState::Draft, Trigger::Submit).unwrap_err();
        assert_eq!(
            err,
            InvalidTransition {
                from: TicketState::Draft,
                trigger: Trigger::Submit
            }
        );
    }

    #[test]
    fn is_terminal_covers_closed_and_cancelled_only() {
        assert!(is_terminal(TicketState::Closed));
        assert!(is_terminal(TicketState::Cancelled));
        assert!(!is_terminal(TicketState::Ready));
    }

    #[test]
    fn is_live_excludes_draft_and_terminal_states() {
        assert!(!is_live(TicketState::Draft));
        assert!(!is_live(TicketState::Closed));
        assert!(!is_live(TicketState::Cancelled));
        assert!(is_live(TicketState::Ready));
        assert!(is_live(TicketState::Blocked));
        assert!(is_live(TicketState::Escalated));
    }

    #[test]
    fn can_lease_is_true_only_for_ready() {
        for &state in ALL_STATES {
            assert_eq!(can_lease(state), state == TicketState::Ready);
        }
    }
}
