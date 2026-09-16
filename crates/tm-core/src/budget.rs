//! Hierarchical budgets: project -> milestone -> ticket -> lease.
//!
//! Recording usage debits every ancestor scope atomically, inside the caller's transaction;
//! going negative is refused, never clamped. `SPEC.md` §4.7. This module owns the pure debit
//! logic over an in-memory snapshot of the scope chain; `store.rs::record_usage` resolves the
//! actual ancestor chain from `view.rs` and persists the result through `tx`.

use thiserror::Error;
use tm_types::{Budget, LeaseId, MilestoneId, Spend, TicketId};

/// One scope in the budget hierarchy, from narrowest to widest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum BudgetScope {
    /// A single lease's budget.
    Lease(LeaseId),
    /// A single ticket's budget.
    Ticket(TicketId),
    /// A milestone's budget.
    Milestone(MilestoneId),
    /// The project's overall budget.
    Project,
}

/// Which scope in the ancestor chain was exhausted, refusing a `record_usage` call.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("budget exhausted at scope {scope:?}")]
pub struct ExhaustedScope {
    /// The scope that could not absorb the spend.
    pub scope: BudgetScope,
}

/// Debit `amount` from every budget in `chain`, in order (narrowest first), refusing rather than
/// letting any scope go negative. `SPEC.md` §4.7: "`Budget::try_spend` is atomic in the same
/// transaction as the usage event and returns `BudgetExhausted` rather than going negative."
///
/// Returns the updated budgets (same order as `chain`) on success. On failure, returns which
/// scope was exhausted; the caller must not apply *any* of the debits (all-or-nothing), which is
/// why this function computes the whole chain before returning rather than mutating in place.
pub fn record_usage(
    chain: &[(BudgetScope, Budget)],
    amount: Spend,
) -> Result<Vec<Budget>, ExhaustedScope> {
    // Create mutable copies of all budgets to test the spend atomically
    let mut test_budgets = chain.iter().map(|(_, b)| *b).collect::<Vec<_>>();

    // Try to spend against each budget in order (narrowest first)
    for (i, (scope, _)) in chain.iter().enumerate() {
        if test_budgets[i].try_spend(amount).is_err() {
            return Err(ExhaustedScope {
                scope: scope.clone(),
            });
        }
    }

    // All budgets accepted the spend
    Ok(test_budgets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_chain_succeeds() {
        let chain = vec![];
        let amount = Spend::tokens(100);
        let result = record_usage(&chain, amount);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), vec![]);
    }

    #[test]
    fn single_budget_with_enough_tokens() {
        let budget = Budget {
            tokens: 1000,
            dollars_micros: 1_000_000,
            wall_seconds: 3600,
            spent: Spend::tokens(0),
        };
        let chain = vec![(BudgetScope::Project, budget)];
        let amount = Spend::tokens(100);
        let result = record_usage(&chain, amount);

        assert!(result.is_ok());
        let updated = result.unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].spent.tokens, 100);
        assert_eq!(updated[0].tokens, 1000);
    }

    #[test]
    fn multiple_budgets_all_accept_spend() {
        let ticket_id = TicketId::new("T-1").unwrap();
        let milestone_id = MilestoneId::new("M-1").unwrap();
        let lease_id = LeaseId::new("L-000000000001").unwrap();

        let lease_budget = Budget {
            tokens: 500,
            dollars_micros: 500_000,
            wall_seconds: 1800,
            spent: Spend::tokens(0),
        };
        let ticket_budget = Budget {
            tokens: 5000,
            dollars_micros: 5_000_000,
            wall_seconds: 18000,
            spent: Spend::tokens(0),
        };
        let milestone_budget = Budget {
            tokens: 50000,
            dollars_micros: 50_000_000,
            wall_seconds: 180000,
            spent: Spend::tokens(0),
        };
        let project_budget = Budget {
            tokens: 500000,
            dollars_micros: 500_000_000,
            wall_seconds: 1800000,
            spent: Spend::tokens(0),
        };

        let chain = vec![
            (BudgetScope::Lease(lease_id), lease_budget),
            (BudgetScope::Ticket(ticket_id), ticket_budget),
            (BudgetScope::Milestone(milestone_id), milestone_budget),
            (BudgetScope::Project, project_budget),
        ];

        let amount = Spend::tokens(100);
        let result = record_usage(&chain, amount);

        assert!(result.is_ok());
        let updated = result.unwrap();
        assert_eq!(updated.len(), 4);
        for budget in &updated {
            assert_eq!(budget.spent.tokens, 100);
        }
    }

    #[test]
    fn exhausted_at_lease_scope() {
        let ticket_id = TicketId::new("T-1").unwrap();
        let lease_id = LeaseId::new("L-000000000001").unwrap();

        let lease_budget = Budget {
            tokens: 50,
            dollars_micros: 500_000,
            wall_seconds: 1800,
            spent: Spend::tokens(0),
        };
        let ticket_budget = Budget {
            tokens: 5000,
            dollars_micros: 5_000_000,
            wall_seconds: 18000,
            spent: Spend::tokens(0),
        };

        let chain = vec![
            (BudgetScope::Lease(lease_id.clone()), lease_budget),
            (BudgetScope::Ticket(ticket_id), ticket_budget),
        ];

        let amount = Spend::tokens(100);
        let result = record_usage(&chain, amount);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.scope, BudgetScope::Lease(lease_id));
    }

    #[test]
    fn exhausted_at_ticket_scope() {
        let ticket_id = TicketId::new("T-2").unwrap();
        let milestone_id = MilestoneId::new("M-1").unwrap();

        let ticket_budget = Budget {
            tokens: 50,
            dollars_micros: 500_000,
            wall_seconds: 1800,
            spent: Spend::tokens(0),
        };
        let milestone_budget = Budget {
            tokens: 5000,
            dollars_micros: 5_000_000,
            wall_seconds: 18000,
            spent: Spend::tokens(0),
        };

        let chain = vec![
            (BudgetScope::Ticket(ticket_id.clone()), ticket_budget),
            (BudgetScope::Milestone(milestone_id), milestone_budget),
        ];

        let amount = Spend::tokens(100);
        let result = record_usage(&chain, amount);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.scope, BudgetScope::Ticket(ticket_id));
    }

    #[test]
    fn exhausted_at_milestone_scope() {
        let milestone_id = MilestoneId::new("M-1").unwrap();

        let milestone_budget = Budget {
            tokens: 50,
            dollars_micros: 500_000,
            wall_seconds: 1800,
            spent: Spend::tokens(0),
        };
        let project_budget = Budget {
            tokens: 5000,
            dollars_micros: 5_000_000,
            wall_seconds: 18000,
            spent: Spend::tokens(0),
        };

        let chain = vec![
            (
                BudgetScope::Milestone(milestone_id.clone()),
                milestone_budget,
            ),
            (BudgetScope::Project, project_budget),
        ];

        let amount = Spend::tokens(100);
        let result = record_usage(&chain, amount);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.scope, BudgetScope::Milestone(milestone_id));
    }

    #[test]
    fn exhausted_at_project_scope() {
        let project_budget = Budget {
            tokens: 50,
            dollars_micros: 500_000,
            wall_seconds: 1800,
            spent: Spend::tokens(0),
        };

        let chain = vec![(BudgetScope::Project, project_budget)];

        let amount = Spend::tokens(100);
        let result = record_usage(&chain, amount);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.scope, BudgetScope::Project);
    }

    #[test]
    fn zero_spend_always_succeeds() {
        let budget = Budget {
            tokens: 0,
            dollars_micros: 0,
            wall_seconds: 0,
            spent: Spend::tokens(0),
        };
        let chain = vec![(BudgetScope::Project, budget)];
        let amount = Spend::tokens(0);
        let result = record_usage(&chain, amount);

        assert!(result.is_ok());
        let updated = result.unwrap();
        assert_eq!(updated[0].spent.tokens, 0);
    }

    #[test]
    fn dollars_dimension_exhaustion() {
        let budget = Budget {
            tokens: 10000,
            dollars_micros: 100,
            wall_seconds: 3600,
            spent: Spend::tokens(0),
        };
        let chain = vec![(BudgetScope::Project, budget)];
        let amount = Spend {
            tokens: 10,
            dollars_micros: 200,
            wall_seconds: 0,
        };
        let result = record_usage(&chain, amount);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.scope, BudgetScope::Project);
    }

    #[test]
    fn wall_seconds_dimension_exhaustion() {
        let budget = Budget {
            tokens: 10000,
            dollars_micros: 1_000_000,
            wall_seconds: 100,
            spent: Spend::tokens(0),
        };
        let chain = vec![(BudgetScope::Project, budget)];
        let amount = Spend {
            tokens: 10,
            dollars_micros: 1000,
            wall_seconds: 200,
        };
        let result = record_usage(&chain, amount);

        assert!(result.is_err());
    }

    #[test]
    fn caller_chain_unchanged_on_failure() {
        let ticket_id = TicketId::new("T-3").unwrap();
        let lease_id = LeaseId::new("L-000000000002").unwrap();

        let lease_budget = Budget {
            tokens: 50,
            dollars_micros: 500_000,
            wall_seconds: 1800,
            spent: Spend::tokens(0),
        };
        let ticket_budget = Budget {
            tokens: 5000,
            dollars_micros: 5_000_000,
            wall_seconds: 18000,
            spent: Spend::tokens(0),
        };

        let mut chain = vec![
            (BudgetScope::Lease(lease_id), lease_budget),
            (BudgetScope::Ticket(ticket_id), ticket_budget),
        ];

        let original_lease_spent = chain[0].1.spent.tokens;
        let original_ticket_spent = chain[1].1.spent.tokens;

        let amount = Spend::tokens(100);
        let _result = record_usage(&chain, amount);

        // Verify the original chain is unchanged
        assert_eq!(chain[0].1.spent.tokens, original_lease_spent);
        assert_eq!(chain[1].1.spent.tokens, original_ticket_spent);
    }

    #[test]
    fn multiple_scopes_with_partial_spend() {
        let ticket_id = TicketId::new("T-4").unwrap();
        let milestone_id = MilestoneId::new("M-2").unwrap();

        let ticket_budget = Budget {
            tokens: 500,
            dollars_micros: 5_000_000,
            wall_seconds: 18000,
            spent: Spend::tokens(200),
        };
        let milestone_budget = Budget {
            tokens: 5000,
            dollars_micros: 50_000_000,
            wall_seconds: 180000,
            spent: Spend::tokens(2000),
        };

        let chain = vec![
            (BudgetScope::Ticket(ticket_id), ticket_budget),
            (BudgetScope::Milestone(milestone_id), milestone_budget),
        ];

        let amount = Spend::tokens(150);
        let result = record_usage(&chain, amount);

        assert!(result.is_ok());
        let updated = result.unwrap();
        assert_eq!(updated[0].spent.tokens, 350); // 200 + 150
        assert_eq!(updated[1].spent.tokens, 2150); // 2000 + 150
    }

    #[test]
    fn exact_remaining_budget_spend() {
        let budget = Budget {
            tokens: 500,
            dollars_micros: 5_000_000,
            wall_seconds: 18000,
            spent: Spend::tokens(400),
        };
        let chain = vec![(BudgetScope::Ticket(TicketId::new("T-5").unwrap()), budget)];

        let amount = Spend::tokens(100);
        let result = record_usage(&chain, amount);

        assert!(result.is_ok());
        let updated = result.unwrap();
        assert_eq!(updated[0].spent.tokens, 500); // Exactly at limit
    }

    #[test]
    fn unlimited_budget_never_exhausted() {
        let budget = Budget {
            tokens: u64::MAX,
            dollars_micros: u64::MAX,
            wall_seconds: u64::MAX,
            spent: Spend {
                tokens: 0,
                dollars_micros: 0,
                wall_seconds: 0,
            },
        };
        let chain = vec![(BudgetScope::Project, budget)];

        let amount = Spend {
            tokens: u64::MAX - 1,
            dollars_micros: u64::MAX - 1,
            wall_seconds: u64::MAX - 1,
        };
        let result = record_usage(&chain, amount);

        assert!(result.is_ok());
    }
}
