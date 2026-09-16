//! Hierarchical budgets: project -> milestone -> ticket -> lease (`SPEC.md` §4.7).
//!
//! Recording usage debits every ancestor scope atomically, inside the caller's transaction, and
//! refuses rather than going negative. Pure logic over an injected ancestor chain; `Store` reads
//! the chain from materialized state and commits the debit alongside the `usage.recorded` event.

use tm_types::{Budget, Spend};

/// Which level of the hierarchy a budget belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetScope {
    /// The whole project.
    Project,
    /// A milestone, by id.
    Milestone(tm_types::MilestoneId),
    /// A ticket, by id.
    Ticket(tm_types::TicketId),
    /// A lease, by id.
    Lease(tm_types::LeaseId),
}

/// Which scope in a chain was exhausted, returned by [`BudgetLedger::record_usage`] on failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("budget exhausted at scope {scope:?}")]
pub struct ExhaustedScope {
    /// The scope whose remaining budget could not cover the spend.
    pub scope: BudgetScope,
}

/// One scope's budget, paired with its identity in the hierarchy, for use as an element of the
/// ancestor chain [`BudgetLedger::record_usage`] debits.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedBudget {
    /// Which scope this is.
    pub scope: BudgetScope,
    /// The budget itself (limits plus running spend).
    pub budget: Budget,
}

/// Facade over pure budget operations; `Store` wraps these with SQLite persistence inside the
/// caller's `Tx`.
pub struct BudgetLedger;

impl BudgetLedger {
    /// Debit `amount` from every scope in `chain`, ordered from the narrowest (e.g. lease) to
    /// the widest (project), atomically: either every scope in the chain has enough remaining
    /// budget and all are debited, or none are (this function itself is pure and returns the
    /// updated chain or an error; `Store` is responsible for only committing on `Ok`, which
    /// SQLite transactionality gives it for free since both paths run inside the same `Tx`).
    ///
    /// # Errors
    /// [`ExhaustedScope`] naming the first scope (narrowest to widest) whose
    /// [`Budget::try_spend`] would go negative. No partial debit is applied in that case.
    pub fn record_usage(
        chain: Vec<ScopedBudget>,
        amount: Spend,
    ) -> Result<Vec<ScopedBudget>, ExhaustedScope> {
        // First pass: speculatively check all scopes can afford the spend without mutating.
        // Clone each budget and attempt try_spend to find the first failure (if any).
        for scoped in &chain {
            let mut test_budget = scoped.budget;
            if test_budget.try_spend(amount).is_err() {
                return Err(ExhaustedScope {
                    scope: scoped.scope.clone(),
                });
            }
        }

        // Second pass: actually apply the spend to all budgets in the chain.
        // We already verified every scope can afford this in the first pass.
        let mut result = chain;
        for scoped in &mut result {
            // This is safe because first pass verified it will succeed.
            let _ = scoped.budget.try_spend(amount);
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `Spend` from its three components for test readability.
    fn spend(tokens: u64, dollars_micros: u64, wall_seconds: u64) -> Spend {
        Spend::tokens(tokens)
            .plus(Spend::dollars_micros(dollars_micros))
            .plus(Spend::seconds(wall_seconds))
    }

    /// Creates a ScopedBudget at Project scope with specified limits.
    fn project_budget(tokens: u64, dollars_micros: u64, wall_seconds: u64) -> ScopedBudget {
        ScopedBudget {
            scope: BudgetScope::Project,
            budget: Budget::new(tokens, dollars_micros, wall_seconds),
        }
    }

    /// Creates a ScopedBudget at Milestone scope with specified limits.
    fn milestone_budget(
        mid: tm_types::MilestoneId,
        tokens: u64,
        dollars_micros: u64,
        wall_seconds: u64,
    ) -> ScopedBudget {
        ScopedBudget {
            scope: BudgetScope::Milestone(mid),
            budget: Budget::new(tokens, dollars_micros, wall_seconds),
        }
    }

    /// Creates a ScopedBudget at Ticket scope with specified limits.
    fn ticket_budget(
        tid: tm_types::TicketId,
        tokens: u64,
        dollars_micros: u64,
        wall_seconds: u64,
    ) -> ScopedBudget {
        ScopedBudget {
            scope: BudgetScope::Ticket(tid),
            budget: Budget::new(tokens, dollars_micros, wall_seconds),
        }
    }

    /// Creates a ScopedBudget at Lease scope with specified limits.
    fn lease_budget(
        lid: tm_types::LeaseId,
        tokens: u64,
        dollars_micros: u64,
        wall_seconds: u64,
    ) -> ScopedBudget {
        ScopedBudget {
            scope: BudgetScope::Lease(lid),
            budget: Budget::new(tokens, dollars_micros, wall_seconds),
        }
    }

    #[test]
    fn happy_path_single_scope() {
        let chain = vec![project_budget(1000, 1000000, 3600)];
        let amount = spend(100, 100000, 60);

        let result = BudgetLedger::record_usage(chain, amount);

        assert!(result.is_ok());
        let updated = result.unwrap();
        assert_eq!(updated.len(), 1);
        let remaining = updated[0].budget.remaining();
        assert_eq!(remaining.tokens, 900);
        assert_eq!(remaining.dollars_micros, 900000);
        assert_eq!(remaining.wall_seconds, 3540);
    }

    #[test]
    fn happy_path_multiple_scopes() {
        let mid = tm_types::MilestoneId::new("M-1").unwrap();
        let tid = tm_types::TicketId::new("T-1").unwrap();
        let chain = vec![
            lease_budget(
                tm_types::LeaseId::new("L-abc123def456").unwrap(),
                1000,
                1000000,
                3600,
            ),
            ticket_budget(tid.clone(), 2000, 2000000, 7200),
            milestone_budget(mid.clone(), 5000, 5000000, 18000),
            project_budget(10000, 10000000, 36000),
        ];
        let amount = spend(100, 100000, 60);

        let result = BudgetLedger::record_usage(chain, amount);

        assert!(result.is_ok());
        let updated = result.unwrap();
        assert_eq!(updated.len(), 4);

        // Verify each scope was debited
        for i in 0..4 {
            let remaining = updated[i].budget.remaining();
            assert_eq!(remaining.tokens, [900, 1900, 4900, 9900][i]);
            assert_eq!(
                remaining.dollars_micros,
                [900000, 1900000, 4900000, 9900000][i]
            );
            assert_eq!(remaining.wall_seconds, [3540, 7140, 17940, 35940][i]);
        }
    }

    #[test]
    fn error_exhausted_first_scope() {
        let chain = vec![project_budget(100, 100000, 60)];
        let amount = spend(101, 100000, 60);

        let result = BudgetLedger::record_usage(chain, amount);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.scope, BudgetScope::Project);
    }

    #[test]
    fn error_exhausted_middle_scope() {
        let mid = tm_types::MilestoneId::new("M-1").unwrap();
        let tid = tm_types::TicketId::new("T-1").unwrap();
        let chain = vec![
            lease_budget(
                tm_types::LeaseId::new("L-abc123def456").unwrap(),
                1000,
                1000000,
                3600,
            ),
            ticket_budget(tid.clone(), 50, 2000000, 7200),
            milestone_budget(mid.clone(), 5000, 5000000, 18000),
            project_budget(10000, 10000000, 36000),
        ];
        let amount = spend(100, 100000, 60);

        let result = BudgetLedger::record_usage(chain, amount);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.scope, BudgetScope::Ticket(tid));
    }

    #[test]
    fn error_exhausted_last_scope() {
        let mid = tm_types::MilestoneId::new("M-1").unwrap();
        let tid = tm_types::TicketId::new("T-1").unwrap();
        let chain = vec![
            lease_budget(
                tm_types::LeaseId::new("L-abc123def456").unwrap(),
                1000,
                1000000,
                3600,
            ),
            ticket_budget(tid.clone(), 2000, 2000000, 7200),
            milestone_budget(mid.clone(), 5000, 5000000, 18000),
            project_budget(10000, 50000, 36000),
        ];
        let amount = spend(100, 100000, 60);

        let result = BudgetLedger::record_usage(chain, amount);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.scope, BudgetScope::Project);
    }

    #[test]
    fn zero_spend_always_succeeds() {
        let chain = vec![project_budget(0, 0, 0)];
        let amount = spend(0, 0, 0);

        let result = BudgetLedger::record_usage(chain, amount);

        assert!(result.is_ok());
        let updated = result.unwrap();
        let remaining = updated[0].budget.remaining();
        assert_eq!(remaining.tokens, 0);
        assert_eq!(remaining.dollars_micros, 0);
        assert_eq!(remaining.wall_seconds, 0);
    }

    #[test]
    fn atomicity_no_partial_debit_on_failure() {
        let mid = tm_types::MilestoneId::new("M-1").unwrap();
        let tid = tm_types::TicketId::new("T-1").unwrap();
        let chain = vec![
            lease_budget(
                tm_types::LeaseId::new("L-abc123def456").unwrap(),
                1000,
                1000000,
                3600,
            ),
            ticket_budget(tid.clone(), 2000, 2000000, 7200),
            milestone_budget(mid.clone(), 50, 5000000, 18000),
        ];
        let amount = spend(100, 100000, 60);

        let result = BudgetLedger::record_usage(chain, amount);

        assert!(result.is_err());
        // Even though first scope could afford it, it should not be debited
        // since the error returned is from the milestone (second scope)
        let err = result.unwrap_err();
        assert_eq!(err.scope, BudgetScope::Milestone(mid));
    }

    #[test]
    fn exhausted_at_one_dimension_not_others() {
        let chain = vec![project_budget(1000, 50000, 3600)];
        let amount = spend(100, 100000, 60);

        let result = BudgetLedger::record_usage(chain, amount);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.scope, BudgetScope::Project);
    }

    #[test]
    fn exact_match_boundary() {
        let chain = vec![project_budget(100, 100000, 60)];
        let amount = spend(100, 100000, 60);

        let result = BudgetLedger::record_usage(chain, amount);

        assert!(result.is_ok());
        let updated = result.unwrap();
        let remaining = updated[0].budget.remaining();
        assert_eq!(remaining.tokens, 0);
        assert_eq!(remaining.dollars_micros, 0);
        assert_eq!(remaining.wall_seconds, 0);
    }

    #[test]
    fn multiple_sequential_debits() {
        let chain = vec![project_budget(1000, 1000000, 3600)];
        let amount1 = spend(100, 100000, 60);

        let result1 = BudgetLedger::record_usage(chain, amount1);
        assert!(result1.is_ok());

        let chain2 = result1.unwrap();
        let amount2 = spend(200, 200000, 120);

        let result2 = BudgetLedger::record_usage(chain2, amount2);
        assert!(result2.is_ok());

        let updated = result2.unwrap();
        let remaining = updated[0].budget.remaining();
        assert_eq!(remaining.tokens, 700);
        assert_eq!(remaining.dollars_micros, 700000);
        assert_eq!(remaining.wall_seconds, 3420);
    }
}
