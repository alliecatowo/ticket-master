//! The pure ticket state machine: `SPEC.md` §4.3's numbered rules as one total function.
//!
//! [`transition`] is the single source of truth for legality; `Store` calls it before ever
//! writing a state change. No side effects, no clock, no authority checks (those live in
//! `store.rs`, which layers evidence/authority preconditions on top of what `transition` allows
//! structurally). [`TransitionTable`] exists so the exhaustive `TicketState x Trigger` unit test
//! (and any diagnostic tooling) can enumerate the whole table without duplicating the match.

use thiserror::Error;

use crate::ticket::{TicketState, Trigger};

/// Returned when `(from, trigger)` is not a legal transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("no transition from {from:?} on {trigger:?}")]
pub struct InvalidTransition {
    /// The state the transition was attempted from.
    pub from: TicketState,
    /// The trigger that was rejected.
    pub trigger: Trigger,
}

/// The pure transition function: `SPEC.md` §4.3 rules 1-12, exhaustively.
///
/// Note on rule 1: `Draft -> Blocked` on `Activate` "continues to Ready if no unsatisfied
/// dependencies" is a two-step effect (the caller checks dependency satisfaction and may follow
/// up with a `DependenciesSatisfied` trigger in the same logical operation); this function
/// itself only performs the single `(from, trigger) -> to` step named by rule 1's first half.
// IMPL: one big `match (from, trigger)`. Every arm either returns `Ok(to)` per the rule table
// below or falls through to `Err(InvalidTransition { from, trigger })`. Do not use a wildcard
// catch-all for the `Ok` arms — list every legal pair explicitly so the exhaustive cross-product
// test in `#[cfg(test)]` actually exercises this function's real branches, not a default.
//
// Rule table (state -> trigger -> state):
//   Draft            Activate                    -> Blocked
//   Draft            Cancel                      -> Cancelled
//   Blocked          DependenciesSatisfied        -> Ready
//   Blocked          Cancel                       -> Cancelled
//   Ready            LeaseAcquired                -> Leased
//   Ready            DependenciesUnsatisfied       -> Blocked
//   Ready            Cancel                       -> Cancelled
//   Leased           WorkStarted                  -> Running
//   Leased           LeaseExpired                 -> Ready
//   Leased           LeaseReleased                -> Ready
//   Leased           Cancel                       -> Cancelled
//   Running          Submit                       -> Submitted
//   Running          LeaseExpired                 -> Ready
//   Running          LeaseReleased                -> Ready
//   Running          Cancel                       -> Cancelled
//   Submitted        VerificationStarted          -> Verifying
//   Submitted        Cancel                       -> Cancelled
//   Verifying        VerificationPassed           -> Auditing
//   Leased           Failed                       -> Recovery
//   Running          Failed                       -> Recovery
//   Verifying        VerificationFailed           -> Recovery
//   Verifying        Cancel                       -> Cancelled
//   Auditing         AuditPassed                  -> Closed
//   Auditing         AuditRejectedMinor            -> Rework
//   Auditing         AuditRejectedStructural       -> Replan
//   Auditing         Cancel                       -> Cancelled
//   Recovery         RetryScheduled               -> Ready
//   Recovery         RetryExhausted               -> Escalated
//   Recovery         Cancel                       -> Cancelled
//   Rework           ReworkRestarted              -> Ready
//   Rework           Cancel                       -> Cancelled
//   Replan           Replanned                    -> Blocked
//   Replan           Cancel                       -> Cancelled
//   Escalated        EscalationResolved           -> Blocked
//   Escalated        Abandoned                    -> Cancelled
//   Escalated        Cancel                       -> Cancelled
//   Closed           Reopen                       -> Blocked
//   (every non-terminal state) Cancel             -> Cancelled  (already listed per-state above)
//   (Cancelled, Closed) accept no triggers except what is listed (Closed accepts only Reopen).
pub fn transition(from: TicketState, trigger: Trigger) -> Result<TicketState, InvalidTransition> {
    use TicketState::*;
    use Trigger::*;

    match (from, trigger) {
        (Draft, Activate) => Ok(Blocked),
        (Draft, Cancel) => Ok(Cancelled),
        (Blocked, DependenciesSatisfied) => Ok(Ready),
        (Blocked, Cancel) => Ok(Cancelled),
        (Ready, LeaseAcquired) => Ok(Leased),
        (Ready, DependenciesUnsatisfied) => Ok(Blocked),
        (Ready, Cancel) => Ok(Cancelled),
        (Leased, WorkStarted) => Ok(Running),
        (Leased, Failed) => Ok(Recovery),
        (Leased, LeaseExpired) => Ok(Ready),
        (Leased, LeaseReleased) => Ok(Ready),
        (Leased, Cancel) => Ok(Cancelled),
        (Running, Submit) => Ok(Submitted),
        (Running, Failed) => Ok(Recovery),
        (Running, LeaseExpired) => Ok(Ready),
        (Running, LeaseReleased) => Ok(Ready),
        (Running, Cancel) => Ok(Cancelled),
        (Submitted, VerificationStarted) => Ok(Verifying),
        (Submitted, Cancel) => Ok(Cancelled),
        (Verifying, VerificationPassed) => Ok(Auditing),
        (Verifying, VerificationFailed) => Ok(Recovery),
        (Verifying, Cancel) => Ok(Cancelled),
        (Auditing, AuditPassed) => Ok(Closed),
        (Auditing, AuditRejectedMinor) => Ok(Rework),
        (Auditing, AuditRejectedStructural) => Ok(Replan),
        (Auditing, Cancel) => Ok(Cancelled),
        (Recovery, RetryScheduled) => Ok(Ready),
        (Recovery, RetryExhausted) => Ok(Escalated),
        (Recovery, Cancel) => Ok(Cancelled),
        (Rework, ReworkRestarted) => Ok(Ready),
        (Rework, Cancel) => Ok(Cancelled),
        (Replan, Replanned) => Ok(Blocked),
        (Replan, Cancel) => Ok(Cancelled),
        (Escalated, EscalationResolved) => Ok(Blocked),
        (Escalated, Abandoned) => Ok(Cancelled),
        (Escalated, Cancel) => Ok(Cancelled),
        (Closed, Reopen) => Ok(Blocked),
        _ => Err(InvalidTransition { from, trigger }),
    }
}

/// True for [`TicketState::Closed`] and [`TicketState::Cancelled`]: no trigger moves a ticket
/// out of these except `Reopen` from `Closed`.
pub fn is_terminal(state: TicketState) -> bool {
    matches!(state, TicketState::Closed | TicketState::Cancelled)
}

/// True for every state that is not terminal: the ticket still has work outstanding.
pub fn is_live(state: TicketState) -> bool {
    !is_terminal(state)
}

/// True only for [`TicketState::Ready`]: the sole state `LeaseAcquired` may transition from.
pub fn can_lease(state: TicketState) -> bool {
    matches!(state, TicketState::Ready)
}

/// A queryable view of every legal `(state, trigger)` pair, built by exhaustively probing
/// [`transition`]. Used by the cross-product unit test and available to other crates (e.g. a
/// `tm doctor` style diagnostic) that want to render the whole table without re-deriving it.
#[derive(Debug, Clone)]
pub struct TransitionTable {
    entries: Vec<(TicketState, Trigger, TicketState)>,
}

impl TransitionTable {
    /// The alphabet of triggers, in declaration order, used to build the full cross product.
    const TRIGGERS: &'static [Trigger] = &[
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
        Trigger::Failed,
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

    /// Build the table by calling [`transition`] for every `(state, trigger)` pair in
    /// [`TicketState::ALL`] x [`TransitionTable::TRIGGERS`], keeping only the legal ones.
    pub fn build() -> Self {
        let mut entries = Vec::new();
        for &from in TicketState::ALL {
            for &trigger in Self::TRIGGERS {
                if let Ok(to) = transition(from, trigger) {
                    entries.push((from, trigger, to));
                }
            }
        }
        TransitionTable { entries }
    }

    /// Every legal `(from, trigger, to)` triple.
    pub fn entries(&self) -> &[(TicketState, Trigger, TicketState)] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule table from `transition`'s doc comment, reproduced here so the cross-product test
    /// can assert equality against it rather than against `transition` itself.
    fn expected() -> Vec<(TicketState, Trigger, TicketState)> {
        use TicketState::*;
        use Trigger::*;
        [
            (Draft, Activate, Blocked),
            (Draft, Cancel, Cancelled),
            (Blocked, DependenciesSatisfied, Ready),
            (Blocked, Cancel, Cancelled),
            (Ready, LeaseAcquired, Leased),
            (Ready, DependenciesUnsatisfied, Blocked),
            (Ready, Cancel, Cancelled),
            (Leased, WorkStarted, Running),
            (Leased, Failed, Recovery),
            (Leased, LeaseExpired, Ready),
            (Leased, LeaseReleased, Ready),
            (Leased, Cancel, Cancelled),
            (Running, Submit, Submitted),
            (Running, Failed, Recovery),
            (Running, LeaseExpired, Ready),
            (Running, LeaseReleased, Ready),
            (Running, Cancel, Cancelled),
            (Submitted, VerificationStarted, Verifying),
            (Submitted, Cancel, Cancelled),
            (Verifying, VerificationPassed, Auditing),
            (Verifying, VerificationFailed, Recovery),
            (Verifying, Cancel, Cancelled),
            (Auditing, AuditPassed, Closed),
            (Auditing, AuditRejectedMinor, Rework),
            (Auditing, AuditRejectedStructural, Replan),
            (Auditing, Cancel, Cancelled),
            (Recovery, RetryScheduled, Ready),
            (Recovery, RetryExhausted, Escalated),
            (Recovery, Cancel, Cancelled),
            (Rework, ReworkRestarted, Ready),
            (Rework, Cancel, Cancelled),
            (Replan, Replanned, Blocked),
            (Replan, Cancel, Cancelled),
            (Escalated, EscalationResolved, Blocked),
            (Escalated, Abandoned, Cancelled),
            (Escalated, Cancel, Cancelled),
            (Closed, Reopen, Blocked),
        ]
        .into_iter()
        .collect()
    }

    fn actual_pairs() -> Vec<(TicketState, Trigger, TicketState)> {
        TransitionTable::build().entries().to_vec()
    }

    /// Order-independent equality between two small triple lists.
    fn same_elements(
        a: &[(TicketState, Trigger, TicketState)],
        b: &[(TicketState, Trigger, TicketState)],
    ) -> bool {
        a.len() == b.len() && a.iter().all(|x| b.contains(x))
    }

    #[test]
    fn cross_product_matches_rule_table_exactly() {
        assert!(same_elements(&actual_pairs(), &expected()));
    }

    #[test]
    fn cross_product_covers_every_state_and_every_trigger() {
        let table = TransitionTable::build();
        for &state in TicketState::ALL {
            // Cancelled accepts no triggers at all, so it legitimately has zero entries.
            if state != TicketState::Cancelled {
                assert!(
                    table.entries().iter().any(|(from, _, _)| *from == state),
                    "no legal transition out of {state:?}"
                );
            }
        }
        for &trigger in TransitionTable::TRIGGERS {
            assert!(
                table.entries().iter().any(|(_, t, _)| *t == trigger),
                "no legal transition for {trigger:?}"
            );
        }
    }

    // Renamed from `draft_only_accepts_activate`: `Draft` also accepts `Cancel` per `SPEC.md`
    // §4.3 rule 12 ("-> Cancelled from any non-terminal state"), covered by
    // `every_non_terminal_state_accepts_cancel` below; this test previously asserted the
    // opposite, which contradicted the spec.
    #[test]
    fn draft_accepts_activate() {
        assert_eq!(
            transition(TicketState::Draft, Trigger::Activate),
            Ok(TicketState::Blocked)
        );
    }

    #[test]
    fn ready_leases_into_leased() {
        assert_eq!(
            transition(TicketState::Ready, Trigger::LeaseAcquired),
            Ok(TicketState::Leased)
        );
    }

    #[test]
    fn leased_and_running_both_return_to_ready_on_lease_loss() {
        assert_eq!(
            transition(TicketState::Leased, Trigger::LeaseExpired),
            Ok(TicketState::Ready)
        );
        assert_eq!(
            transition(TicketState::Leased, Trigger::LeaseReleased),
            Ok(TicketState::Ready)
        );
        assert_eq!(
            transition(TicketState::Running, Trigger::LeaseExpired),
            Ok(TicketState::Ready)
        );
        assert_eq!(
            transition(TicketState::Running, Trigger::LeaseReleased),
            Ok(TicketState::Ready)
        );
    }

    #[test]
    fn auditing_branches_on_verdict() {
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
    fn recovery_branches_on_retry_exhaustion() {
        assert_eq!(
            transition(TicketState::Recovery, Trigger::RetryScheduled),
            Ok(TicketState::Ready)
        );
        assert_eq!(
            transition(TicketState::Recovery, Trigger::RetryExhausted),
            Ok(TicketState::Escalated)
        );
    }

    #[test]
    fn closed_only_accepts_reopen() {
        assert_eq!(
            transition(TicketState::Closed, Trigger::Reopen),
            Ok(TicketState::Blocked)
        );
        assert!(transition(TicketState::Closed, Trigger::Cancel).is_err());
        assert!(transition(TicketState::Closed, Trigger::Activate).is_err());
    }

    #[test]
    fn cancelled_accepts_nothing() {
        for &trigger in TransitionTable::TRIGGERS {
            assert!(
                transition(TicketState::Cancelled, trigger).is_err(),
                "Cancelled must reject {trigger:?}"
            );
        }
    }

    #[test]
    fn every_non_terminal_state_accepts_cancel() {
        for &state in TicketState::ALL {
            if !is_terminal(state) {
                assert_eq!(
                    transition(state, Trigger::Cancel),
                    Ok(TicketState::Cancelled),
                    "{state:?} must accept Cancel"
                );
            }
        }
    }

    #[test]
    fn invalid_pair_reports_from_and_trigger() {
        let err = transition(TicketState::Draft, Trigger::Reopen).unwrap_err();
        assert_eq!(err.from, TicketState::Draft);
        assert_eq!(err.trigger, Trigger::Reopen);
    }

    #[test]
    fn terminal_predicate_matches_closed_and_cancelled_only() {
        for &state in TicketState::ALL {
            let expected_terminal = matches!(state, TicketState::Closed | TicketState::Cancelled);
            assert_eq!(is_terminal(state), expected_terminal);
            assert_eq!(is_live(state), !expected_terminal);
        }
    }

    #[test]
    fn can_lease_is_true_only_for_ready() {
        for &state in TicketState::ALL {
            assert_eq!(can_lease(state), state == TicketState::Ready);
        }
    }
}
