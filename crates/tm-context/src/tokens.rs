//! Token budgeting: a cheap deterministic token estimator for source and prose text, the
//! [`TokenBudget`] that divides a pack's total budget into per-section shares, and the
//! [`BudgetLedger`] accounting [`crate::pack::compile`] uses to decide what fits.
//!
//! No tokenizer dependency is pulled in: estimation is a fast, deterministic, model-agnostic
//! approximation (character-count based), good enough for admission control, not for exact
//! provider billing.

use std::collections::BTreeMap;

/// The ten sections a context pack is built from, in priority order (index 0 highest). This
/// order is the drop order in [`crate::pack::compile`]: on overflow, the section with the
/// largest index (lowest priority) is dropped first.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SectionKind {
    /// Ticket objective and success predicates.
    Objective,
    /// Remaining budget, burn rate, and the tier-cost menu (`SPEC.md` §31.1,
    /// `docs/audit-2026-09-18-fable.md` B-10): what this ticket can still afford, expressed so a
    /// worker can act on it — not just a number.
    Budget,
    /// Active decisions affecting the ticket's claimed paths.
    Decisions,
    /// Parent/dependency outputs and evidence.
    Dependencies,
    /// Hybrid retrieval results for the objective.
    Retrieval,
    /// Wiki pages (`SPEC.md` §26) matching the objective, ranked alongside code in the same
    /// hybrid retrieval fusion `Retrieval` draws from (`SPEC.md` §26.4) — a compiled explanation
    /// a worker can cite as source material instead of only raw code/history.
    Wiki,
    /// Symbol outlines for claimed paths.
    SymbolOutlines,
    /// Relevant git history.
    GitHistory,
    /// Prior failures for this ticket.
    PriorFailures,
    /// Harness/project conventions.
    Conventions,
}

impl SectionKind {
    /// Every section kind, in priority order (highest priority first, i.e. dropped last).
    pub const PRIORITY_ORDER: &'static [SectionKind] = &[
        SectionKind::Objective,
        SectionKind::Budget,
        SectionKind::Decisions,
        SectionKind::Dependencies,
        SectionKind::Retrieval,
        SectionKind::Wiki,
        SectionKind::SymbolOutlines,
        SectionKind::GitHistory,
        SectionKind::PriorFailures,
        SectionKind::Conventions,
    ];

    /// This kind's position in [`SectionKind::PRIORITY_ORDER`]; lower is higher priority.
    pub fn rank(self) -> u8 {
        Self::PRIORITY_ORDER
            .iter()
            .position(|&k| k == self)
            .expect("PRIORITY_ORDER lists every SectionKind variant") as u8
    }
}

/// Estimate the token count of source code text.
///
/// IMPL: deterministic, no tokenizer dependency. Use a fixed characters-per-token divisor
/// calibrated for code (denser than prose due to punctuation/identifiers; a common rule of
/// thumb is ~3.5 chars/token for source vs ~4 for prose). Round up (`div_ceil`) so estimates
/// never under-count what a section will cost. Empty input returns 0. No error cases: any
/// `&str` is valid input.
pub fn estimate_tokens_source(text: &str) -> usize {
    text.len().div_ceil(3)
}

/// Estimate the token count of prose/natural-language text.
///
/// IMPL: same shape as [`estimate_tokens_source`] but with a prose-calibrated divisor
/// (~4 chars/token). Round up. Empty input returns 0.
pub fn estimate_tokens_prose(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// How a pack's total token budget is divided across sections.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenBudget {
    /// Total tokens available to the whole pack.
    pub total: usize,
    /// Each section's share of `total`, as a fraction in `[0.0, 1.0]`. Sections omitted from
    /// this map get no dedicated share (see [`TokenBudget::share_tokens`]).
    pub shares: BTreeMap<SectionKind, f32>,
}

impl TokenBudget {
    /// A budget with `total` tokens split evenly across every [`SectionKind`].
    pub fn even(total: usize) -> Self {
        let share = 1.0 / SectionKind::PRIORITY_ORDER.len() as f32;
        let shares = SectionKind::PRIORITY_ORDER
            .iter()
            .map(|&k| (k, share))
            .collect();
        TokenBudget { total, shares }
    }

    /// The token allotment for `kind`: `total * shares[kind]`, floored, or 0 if `kind` has no
    /// configured share.
    ///
    /// IMPL: `(self.total as f32 * self.shares.get(&kind).copied().unwrap_or(0.0)).floor() as
    /// usize`. No error cases.
    pub fn share_tokens(&self, kind: SectionKind) -> usize {
        (self.total as f32 * self.shares.get(&kind).copied().unwrap_or(0.0)).floor() as usize
    }
}

/// One section's running token account within a [`BudgetLedger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionAccount {
    /// Which section this account is for.
    pub kind: SectionKind,
    /// Tokens allotted to this section by the governing [`TokenBudget`].
    pub allotted: usize,
    /// Tokens actually spent so far.
    pub used: usize,
}

impl SectionAccount {
    /// Tokens still available in this section's allotment (0 if already over).
    pub fn remaining(&self) -> usize {
        self.allotted.saturating_sub(self.used)
    }

    /// Attempt to record `tokens` of spend against this account. Returns `true` and updates
    /// `used` if `tokens <= remaining()`; otherwise leaves the account untouched and returns
    /// `false`.
    pub fn try_spend(&mut self, tokens: usize) -> bool {
        if tokens > self.remaining() {
            return false;
        }
        self.used += tokens;
        true
    }
}

/// The full per-pack accounting: one [`SectionAccount`] per section, in [`SectionKind`]
/// priority order, used by [`crate::pack::compile`] to decide what fits and what to drop.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetLedger {
    /// The governing budget's total.
    pub total: usize,
    /// Per-section accounts, in [`SectionKind::PRIORITY_ORDER`].
    pub accounts: Vec<SectionAccount>,
}

impl BudgetLedger {
    /// Build a fresh ledger from `budget`, with one zero-`used` [`SectionAccount`] per
    /// [`SectionKind`] in priority order.
    pub fn new(budget: &TokenBudget) -> Self {
        let accounts = SectionKind::PRIORITY_ORDER
            .iter()
            .map(|&kind| SectionAccount {
                kind,
                allotted: budget.share_tokens(kind),
                used: 0,
            })
            .collect();
        BudgetLedger {
            total: budget.total,
            accounts,
        }
    }

    /// Record `tokens` of spend against `kind`'s account. Returns `true` on success (see
    /// [`SectionAccount::try_spend`]); `false`, with no mutation, if `kind` has no account or
    /// insufficient remaining allotment.
    ///
    /// IMPL: find the account for `kind` in `self.accounts` (linear scan; the vec is small and
    /// fixed-size) and delegate to `SectionAccount::try_spend`. `false` if no matching account.
    pub fn spend(&mut self, kind: SectionKind, tokens: usize) -> bool {
        if let Some(account) = self.accounts.iter_mut().find(|a| a.kind == kind) {
            account.try_spend(tokens)
        } else {
            false
        }
    }

    /// Sum of `used` across every account.
    pub fn total_used(&self) -> usize {
        self.accounts.iter().map(|a| a.used).sum()
    }

    /// Sum of `remaining()` across every account.
    pub fn total_remaining(&self) -> usize {
        self.accounts.iter().map(|a| a.remaining()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_order_covers_every_kind() {
        assert_eq!(SectionKind::PRIORITY_ORDER.len(), 9);
    }

    #[test]
    fn estimate_tokens_source_empty_returns_zero() {
        assert_eq!(estimate_tokens_source(""), 0);
    }

    #[test]
    fn estimate_tokens_source_rounds_up() {
        assert_eq!(estimate_tokens_source("a"), 1);
        assert_eq!(estimate_tokens_source("ab"), 1);
        assert_eq!(estimate_tokens_source("abc"), 1);
        assert_eq!(estimate_tokens_source("abcd"), 2);
        assert_eq!(estimate_tokens_source("abcde"), 2);
        assert_eq!(estimate_tokens_source("abcdef"), 2);
        assert_eq!(estimate_tokens_source("abcdefg"), 3);
    }

    #[test]
    fn estimate_tokens_source_divides_by_three() {
        assert_eq!(estimate_tokens_source("123456789"), 3); // 9 chars / 3 = 3
        assert_eq!(estimate_tokens_source("1234567890"), 4); // 10 chars / 3 round up = 4
    }

    #[test]
    fn estimate_tokens_prose_empty_returns_zero() {
        assert_eq!(estimate_tokens_prose(""), 0);
    }

    #[test]
    fn estimate_tokens_prose_rounds_up() {
        assert_eq!(estimate_tokens_prose("a"), 1);
        assert_eq!(estimate_tokens_prose("abcd"), 1);
        assert_eq!(estimate_tokens_prose("abcde"), 2);
        assert_eq!(estimate_tokens_prose("abcdefgh"), 2);
        assert_eq!(estimate_tokens_prose("abcdefghi"), 3);
    }

    #[test]
    fn estimate_tokens_prose_divides_by_four() {
        assert_eq!(estimate_tokens_prose("12345678"), 2); // 8 chars / 4 = 2
        assert_eq!(estimate_tokens_prose("123456789"), 3); // 9 chars / 4 round up = 3
    }

    #[test]
    fn section_account_remaining_when_under_budget() {
        let account = SectionAccount {
            kind: SectionKind::Objective,
            allotted: 100,
            used: 30,
        };
        assert_eq!(account.remaining(), 70);
    }

    #[test]
    fn section_account_remaining_when_exactly_spent() {
        let account = SectionAccount {
            kind: SectionKind::Objective,
            allotted: 100,
            used: 100,
        };
        assert_eq!(account.remaining(), 0);
    }

    #[test]
    fn section_account_remaining_saturates_when_over() {
        let account = SectionAccount {
            kind: SectionKind::Objective,
            allotted: 100,
            used: 150,
        };
        assert_eq!(account.remaining(), 0);
    }

    #[test]
    fn section_account_try_spend_succeeds_within_budget() {
        let mut account = SectionAccount {
            kind: SectionKind::Objective,
            allotted: 100,
            used: 30,
        };
        assert!(account.try_spend(50));
        assert_eq!(account.used, 80);
        assert_eq!(account.remaining(), 20);
    }

    #[test]
    fn section_account_try_spend_fails_when_insufficient() {
        let mut account = SectionAccount {
            kind: SectionKind::Objective,
            allotted: 100,
            used: 80,
        };
        assert!(!account.try_spend(50));
        assert_eq!(account.used, 80); // unchanged
    }

    #[test]
    fn section_account_try_spend_succeeds_at_boundary() {
        let mut account = SectionAccount {
            kind: SectionKind::Objective,
            allotted: 100,
            used: 75,
        };
        assert!(account.try_spend(25));
        assert_eq!(account.used, 100);
    }

    #[test]
    fn section_account_try_spend_fails_at_boundary() {
        let mut account = SectionAccount {
            kind: SectionKind::Objective,
            allotted: 100,
            used: 75,
        };
        assert!(!account.try_spend(26));
        assert_eq!(account.used, 75); // unchanged
    }

    #[test]
    fn token_budget_even_creates_equal_shares() {
        let budget = TokenBudget::even(10000);
        assert_eq!(budget.total, 10000);
        assert_eq!(budget.shares.len(), 10);
        for &kind in SectionKind::PRIORITY_ORDER {
            assert_eq!(budget.shares[&kind], 1.0 / 10.0);
        }
    }

    #[test]
    fn token_budget_share_tokens_with_shares() {
        let budget = TokenBudget::even(10000);
        for &kind in SectionKind::PRIORITY_ORDER {
            assert_eq!(budget.share_tokens(kind), 1000); // 10000 / 10 = 1000
        }
    }

    #[test]
    fn token_budget_share_tokens_absent_kind() {
        let budget = TokenBudget {
            total: 1000,
            shares: BTreeMap::new(),
        };
        assert_eq!(budget.share_tokens(SectionKind::Objective), 0);
        assert_eq!(budget.share_tokens(SectionKind::Decisions), 0);
    }

    #[test]
    fn token_budget_share_tokens_floors_fractional() {
        let mut shares = BTreeMap::new();
        shares.insert(SectionKind::Objective, 0.333); // 333 tokens
        shares.insert(SectionKind::Decisions, 0.667); // 667 tokens
        let budget = TokenBudget {
            total: 1000,
            shares,
        };
        assert_eq!(budget.share_tokens(SectionKind::Objective), 333);
        assert_eq!(budget.share_tokens(SectionKind::Decisions), 667);
    }

    #[test]
    fn token_budget_share_tokens_partial_coverage() {
        let mut shares = BTreeMap::new();
        shares.insert(SectionKind::Objective, 0.5);
        shares.insert(SectionKind::Decisions, 0.3);
        let budget = TokenBudget {
            total: 1000,
            shares,
        };
        assert_eq!(budget.share_tokens(SectionKind::Objective), 500);
        assert_eq!(budget.share_tokens(SectionKind::Decisions), 300);
        assert_eq!(budget.share_tokens(SectionKind::Dependencies), 0); // not in map
    }

    #[test]
    fn budget_ledger_new_initializes_all_sections() {
        let budget = TokenBudget::even(10000);
        let ledger = BudgetLedger::new(&budget);

        assert_eq!(ledger.total, 10000);
        assert_eq!(ledger.accounts.len(), 10);

        for (i, account) in ledger.accounts.iter().enumerate() {
            assert_eq!(account.kind, SectionKind::PRIORITY_ORDER[i]);
            assert_eq!(account.allotted, 1000); // 10000 / 10
            assert_eq!(account.used, 0);
        }
    }

    #[test]
    fn budget_ledger_new_respects_share_distribution() {
        let mut shares = BTreeMap::new();
        shares.insert(SectionKind::Objective, 0.5);
        shares.insert(SectionKind::Decisions, 0.3);
        shares.insert(SectionKind::Dependencies, 0.2);
        let budget = TokenBudget {
            total: 1000,
            shares,
        };
        let ledger = BudgetLedger::new(&budget);

        let obj_acct = ledger
            .accounts
            .iter()
            .find(|a| a.kind == SectionKind::Objective)
            .unwrap();
        let dec_acct = ledger
            .accounts
            .iter()
            .find(|a| a.kind == SectionKind::Decisions)
            .unwrap();
        let dep_acct = ledger
            .accounts
            .iter()
            .find(|a| a.kind == SectionKind::Dependencies)
            .unwrap();
        let other_acct = ledger
            .accounts
            .iter()
            .find(|a| a.kind == SectionKind::Retrieval)
            .unwrap();

        assert_eq!(obj_acct.allotted, 500);
        assert_eq!(dec_acct.allotted, 300);
        assert_eq!(dep_acct.allotted, 200);
        assert_eq!(other_acct.allotted, 0);
    }

    #[test]
    fn budget_ledger_spend_successful() {
        let budget = TokenBudget::even(9000);
        let mut ledger = BudgetLedger::new(&budget);

        assert!(ledger.spend(SectionKind::Objective, 500));
        let obj_acct = ledger
            .accounts
            .iter()
            .find(|a| a.kind == SectionKind::Objective)
            .unwrap();
        assert_eq!(obj_acct.used, 500);
    }

    #[test]
    fn budget_ledger_spend_insufficient_tokens() {
        let budget = TokenBudget::even(9000);
        let mut ledger = BudgetLedger::new(&budget);

        assert!(!ledger.spend(SectionKind::Objective, 1500)); // 1500 > 1000 allotted
        let obj_acct = ledger
            .accounts
            .iter()
            .find(|a| a.kind == SectionKind::Objective)
            .unwrap();
        assert_eq!(obj_acct.used, 0); // unchanged
    }

    #[test]
    fn budget_ledger_spend_nonexistent_kind() {
        let mut budget = TokenBudget::even(9000);
        budget.shares.remove(&SectionKind::Objective); // artificially remove one

        // Even though Objective is in PRIORITY_ORDER, we can't easily remove it from the ledger
        // after creation. Instead, test with a kind that has no allotment.
        let budget2 = TokenBudget {
            total: 1000,
            shares: BTreeMap::new(),
        };
        let mut ledger2 = BudgetLedger::new(&budget2);
        // All accounts exist but have 0 allotment
        assert!(!ledger2.spend(SectionKind::Objective, 1)); // 1 > 0 remaining
    }

    #[test]
    fn budget_ledger_total_used() {
        let budget = TokenBudget::even(9000);
        let mut ledger = BudgetLedger::new(&budget);

        ledger.spend(SectionKind::Objective, 300);
        ledger.spend(SectionKind::Decisions, 200);
        ledger.spend(SectionKind::Dependencies, 400);

        assert_eq!(ledger.total_used(), 900);
    }

    #[test]
    fn budget_ledger_total_remaining() {
        let budget = TokenBudget::even(10000);
        let mut ledger = BudgetLedger::new(&budget);

        ledger.spend(SectionKind::Objective, 300);
        ledger.spend(SectionKind::Decisions, 200);
        ledger.spend(SectionKind::Dependencies, 400);

        assert_eq!(ledger.total_remaining(), 10000 - 900);
    }

    #[test]
    fn budget_ledger_spend_multiple_sections() {
        let budget = TokenBudget::even(10000);
        let mut ledger = BudgetLedger::new(&budget);

        for &kind in &SectionKind::PRIORITY_ORDER[0..4] {
            assert!(ledger.spend(kind, 500));
        }

        assert_eq!(ledger.total_used(), 2000);
        assert_eq!(ledger.total_remaining(), 10000 - 2000);
    }

    #[test]
    fn budget_ledger_spend_exhausts_section() {
        let budget = TokenBudget::even(1000);
        let mut ledger = BudgetLedger::new(&budget);

        assert!(ledger.spend(SectionKind::Objective, 100)); // 1000 / 10
        let obj_acct = ledger
            .accounts
            .iter()
            .find(|a| a.kind == SectionKind::Objective)
            .unwrap();
        assert_eq!(obj_acct.remaining(), 0);
    }

    #[test]
    fn section_kind_rank_is_correct() {
        assert_eq!(SectionKind::Objective.rank(), 0);
        assert_eq!(SectionKind::Budget.rank(), 1);
        assert_eq!(SectionKind::Decisions.rank(), 2);
        assert_eq!(SectionKind::Dependencies.rank(), 3);
        assert_eq!(SectionKind::Retrieval.rank(), 4);
        assert_eq!(SectionKind::Wiki.rank(), 5);
        assert_eq!(SectionKind::SymbolOutlines.rank(), 6);
        assert_eq!(SectionKind::GitHistory.rank(), 7);
        assert_eq!(SectionKind::PriorFailures.rank(), 8);
        assert_eq!(SectionKind::Conventions.rank(), 9);
    }

    #[test]
    fn budget_ledger_spend_order_preserves_priority() {
        let budget = TokenBudget::even(900);
        let ledger = BudgetLedger::new(&budget);

        // Each section gets floor(800 / 9) = 88 tokens.
        for (i, &kind) in SectionKind::PRIORITY_ORDER.iter().enumerate() {
            let acct = &ledger.accounts[i];
            assert_eq!(acct.kind, kind);
            assert_eq!(acct.allotted, 88);
        }
    }
}
