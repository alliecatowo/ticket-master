//! Hierarchical budgets.
//!
//! A budget carries both its limit and what has been spent against it. Spending is checked
//! before it is recorded and can never drive a budget negative (`SPEC.md` §4.7).

use serde::{Deserialize, Serialize};

/// An amount of resource consumption.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Serialize, Deserialize)]
pub struct Spend {
    /// Model tokens, input plus output.
    #[serde(default)]
    pub tokens: u64,
    /// Money in millionths of a dollar.
    #[serde(default)]
    pub dollars_micros: u64,
    /// Wall-clock seconds.
    #[serde(default)]
    pub wall_seconds: u64,
}

impl Spend {
    /// Tokens only.
    pub fn tokens(n: u64) -> Self {
        Spend { tokens: n, ..Default::default() }
    }

    /// Money only, in millionths of a dollar.
    pub fn dollars_micros(n: u64) -> Self {
        Spend { dollars_micros: n, ..Default::default() }
    }

    /// Wall-clock seconds only.
    pub fn seconds(n: u64) -> Self {
        Spend { wall_seconds: n, ..Default::default() }
    }

    /// Componentwise sum, saturating.
    pub fn plus(self, o: Spend) -> Spend {
        Spend {
            tokens: self.tokens.saturating_add(o.tokens),
            dollars_micros: self.dollars_micros.saturating_add(o.dollars_micros),
            wall_seconds: self.wall_seconds.saturating_add(o.wall_seconds),
        }
    }

    /// True when every component is zero.
    pub fn is_zero(self) -> bool {
        self == Spend::default()
    }
}

/// Why a spend was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    /// The token allowance would be exceeded.
    #[error("token budget exhausted: {spent} spent of {limit}, requested {requested}")]
    Tokens {
        /// Already spent.
        spent: u64,
        /// The limit.
        limit: u64,
        /// The refused request.
        requested: u64,
    },
    /// The money allowance would be exceeded.
    #[error("dollar budget exhausted: {spent} spent of {limit} micros, requested {requested}")]
    Dollars {
        /// Already spent, in micros.
        spent: u64,
        /// The limit, in micros.
        limit: u64,
        /// The refused request, in micros.
        requested: u64,
    },
    /// The wall-clock allowance would be exceeded.
    #[error("time budget exhausted: {spent}s spent of {limit}s, requested {requested}s")]
    Time {
        /// Already spent.
        spent: u64,
        /// The limit.
        limit: u64,
        /// The refused request.
        requested: u64,
    },
}

/// A limit plus its consumption.
///
/// `u64::MAX` in any component means "unlimited" for that component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Token allowance.
    pub tokens: u64,
    /// Money allowance in millionths of a dollar.
    pub dollars_micros: u64,
    /// Wall-clock allowance in seconds.
    pub wall_seconds: u64,
    /// Consumption recorded so far.
    #[serde(default)]
    pub spent: Spend,
}

impl Default for Budget {
    /// A budget that permits nothing.
    ///
    /// Defaulting to "unlimited" would mean an authority deserialized from a config that simply
    /// omits `budget` could spend without bound. Absence means zero; unlimited must be asked for.
    fn default() -> Self {
        Budget::none()
    }
}

impl Budget {
    /// A budget with no limits.
    pub fn unlimited() -> Self {
        Budget {
            tokens: u64::MAX,
            dollars_micros: u64::MAX,
            wall_seconds: u64::MAX,
            spent: Spend::default(),
        }
    }

    /// A budget that permits nothing.
    pub fn none() -> Self {
        Budget { tokens: 0, dollars_micros: 0, wall_seconds: 0, spent: Spend::default() }
    }

    /// A budget with explicit limits.
    pub fn new(tokens: u64, dollars_micros: u64, wall_seconds: u64) -> Self {
        Budget { tokens, dollars_micros, wall_seconds, spent: Spend::default() }
    }

    /// What is left in each component, saturating at zero.
    pub fn remaining(&self) -> Spend {
        Spend {
            tokens: self.tokens.saturating_sub(self.spent.tokens),
            dollars_micros: self.dollars_micros.saturating_sub(self.spent.dollars_micros),
            wall_seconds: self.wall_seconds.saturating_sub(self.spent.wall_seconds),
        }
    }

    /// True when any component has nothing left.
    pub fn is_exhausted(&self) -> bool {
        let r = self.remaining();
        (self.tokens != u64::MAX && r.tokens == 0)
            || (self.dollars_micros != u64::MAX && r.dollars_micros == 0)
            || (self.wall_seconds != u64::MAX && r.wall_seconds == 0)
    }

    /// Check a spend without recording it.
    pub fn check(&self, s: Spend) -> Result<(), BudgetError> {
        let r = self.remaining();
        if s.tokens > r.tokens {
            return Err(BudgetError::Tokens {
                spent: self.spent.tokens,
                limit: self.tokens,
                requested: s.tokens,
            });
        }
        if s.dollars_micros > r.dollars_micros {
            return Err(BudgetError::Dollars {
                spent: self.spent.dollars_micros,
                limit: self.dollars_micros,
                requested: s.dollars_micros,
            });
        }
        if s.wall_seconds > r.wall_seconds {
            return Err(BudgetError::Time {
                spent: self.spent.wall_seconds,
                limit: self.wall_seconds,
                requested: s.wall_seconds,
            });
        }
        Ok(())
    }

    /// Record a spend, refusing it entirely if it would exceed any component.
    pub fn try_spend(&mut self, s: Spend) -> Result<(), BudgetError> {
        self.check(s)?;
        self.spent = self.spent.plus(s);
        Ok(())
    }

    /// True when `other`'s limits fit inside what is left of `self`.
    ///
    /// This is the delegation test: a child may never be handed more than its parent still has.
    pub fn contains(&self, other: &Budget) -> bool {
        fn fits(limit: u64, remaining: u64, want: u64) -> bool {
            limit == u64::MAX || want <= remaining
        }
        let r = self.remaining();
        fits(self.tokens, r.tokens, other.tokens)
            && fits(self.dollars_micros, r.dollars_micros, other.dollars_micros)
            && fits(self.wall_seconds, r.wall_seconds, other.wall_seconds)
    }

    /// The componentwise minimum of two budgets' limits, with spend reset.
    pub fn intersect(&self, other: &Budget) -> Budget {
        Budget {
            tokens: self.remaining().tokens.min(other.remaining().tokens),
            dollars_micros: self.remaining().dollars_micros.min(other.remaining().dollars_micros),
            wall_seconds: self.remaining().wall_seconds.min(other.remaining().wall_seconds),
            spent: Spend::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spending_stops_at_the_limit() {
        let mut b = Budget::new(100, 1_000, 60);
        assert!(b.try_spend(Spend::tokens(60)).is_ok());
        assert_eq!(b.remaining().tokens, 40);
        let err = b.try_spend(Spend::tokens(41)).unwrap_err();
        assert!(matches!(err, BudgetError::Tokens { .. }));
        // The refused spend was not partially applied.
        assert_eq!(b.spent.tokens, 60);
    }

    #[test]
    fn a_refused_spend_applies_no_component() {
        let mut b = Budget::new(100, 10, 100);
        let over = Spend { tokens: 1, dollars_micros: 50, wall_seconds: 1 };
        assert!(b.try_spend(over).is_err());
        assert_eq!(b.spent, Spend::default());
    }

    #[test]
    fn exhaustion_is_per_component() {
        let mut b = Budget::new(10, u64::MAX, u64::MAX);
        assert!(!b.is_exhausted());
        b.try_spend(Spend::tokens(10)).unwrap();
        assert!(b.is_exhausted());
        assert!(!Budget::unlimited().is_exhausted());
        assert!(Budget::none().is_exhausted());
    }

    #[test]
    fn the_default_budget_grants_nothing() {
        assert_eq!(Budget::default(), Budget::none());
        assert!(Budget::default().is_exhausted());
        assert!(Budget::default().check(Spend::tokens(1)).is_err());
    }

    #[test]
    fn a_child_cannot_be_handed_more_than_remains() {
        let mut parent = Budget::new(100, 100, 100);
        parent.try_spend(Spend { tokens: 70, dollars_micros: 0, wall_seconds: 0 }).unwrap();
        assert!(parent.contains(&Budget::new(30, 100, 100)));
        assert!(!parent.contains(&Budget::new(31, 100, 100)));
        assert!(Budget::unlimited().contains(&Budget::new(u64::MAX, 5, 5)));
    }

    #[test]
    fn intersect_takes_the_tighter_remaining_limit() {
        let mut a = Budget::new(100, 100, 100);
        a.try_spend(Spend::tokens(90)).unwrap();
        let i = a.intersect(&Budget::new(50, 20, 200));
        assert_eq!((i.tokens, i.dollars_micros, i.wall_seconds), (10, 20, 100));
        assert!(i.spent.is_zero());
    }
}
