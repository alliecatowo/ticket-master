//! The efficacy budget: how much of the scheduler's capacity harness (self-improvement) work may
//! consume, and the accounting that lets the scheduler enforce it as a hard admission rule.
//!
//! Self-improvement is bounded and subordinate to the project it serves by construction: this
//! module cannot itself run anything, only answer "may a `TicketKind::Harness` ticket be
//! admitted right now" as a pure function of a running total the scheduler feeds it.

use serde::{Deserialize, Serialize};

/// Configuration for how much scheduler capacity harness work may consume.
///
/// This is the `[efficacy]` table of `harness.toml`, versioned inside [`crate::config::HarnessConfig`]
/// would be the natural home, but it is kept as its own top-level concept here because the
/// scheduler consults it independently of the rest of harness policy, on every admission
/// decision, not just at session start.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EfficacyBudget {
    /// Whether harness self-improvement work is permitted at all in this project.
    pub enabled: bool,
    /// Maximum fraction (`[0.0, 1.0]`) of total scheduler capacity that `TicketKind::Harness`
    /// tickets may occupy concurrently.
    pub max_compute_fraction: f64,
    /// Whether a harness epoch promotion requires [`crate::epoch::PromotionGate::require_benchmark_gain`]
    /// to be set (i.e. this flag is the project-level policy; the gate is where it's enforced).
    pub require_benchmark_gain: bool,
}

impl Default for EfficacyBudget {
    /// Conservative default: harness work is allowed but capped small, and promotion must earn
    /// its keep.
    fn default() -> Self {
        EfficacyBudget {
            enabled: true,
            max_compute_fraction: 0.1,
            require_benchmark_gain: true,
        }
    }
}

/// Why an [`EfficacyBudget`] or an admission check was rejected.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum EfficacyError {
    /// `max_compute_fraction` was outside `[0.0, 1.0]`.
    #[error("efficacy.max_compute_fraction must be in [0.0, 1.0], got {0}")]
    InvalidFraction(f64),
    /// Harness work was attempted while [`EfficacyBudget::enabled`] is false.
    #[error("harness self-improvement work is disabled")]
    Disabled,
    /// Admitting the requested unit of harness work would exceed `max_compute_fraction`.
    #[error("admitting harness work would use fraction {requested}, cap is {cap}")]
    FractionExceeded {
        /// The fraction of total capacity harness work would occupy if admitted.
        requested: f64,
        /// The configured cap.
        cap: f64,
    },
}

impl EfficacyBudget {
    /// Check that `max_compute_fraction` is a valid fraction.
    pub fn validate(&self) -> Result<(), EfficacyError> {
        if (0.0..=1.0).contains(&self.max_compute_fraction) {
            Ok(())
        } else {
            Err(EfficacyError::InvalidFraction(self.max_compute_fraction))
        }
    }
}

/// A running account of scheduler capacity consumed by harness vs. non-harness work, used to
/// answer the scheduler's hard admission-rule question.
///
/// Units are whatever the scheduler measures capacity in (worker-slots, concurrent leases, ...);
/// this type is unit-agnostic and only ever compares harness units to total units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EfficacyAccount {
    /// The budget this account is measured against.
    pub budget: EfficacyBudget,
    /// Capacity units currently occupied by `TicketKind::Harness` work.
    pub harness_units: u64,
    /// Capacity units currently occupied by all work (harness and non-harness).
    pub total_units: u64,
}

impl EfficacyAccount {
    /// Start an account at zero occupancy under `budget`.
    pub fn new(budget: EfficacyBudget) -> EfficacyAccount {
        EfficacyAccount {
            budget,
            harness_units: 0,
            total_units: 0,
        }
    }

    /// Current fraction of `total_units` occupied by harness work, or `0.0` when `total_units`
    /// is zero.
    pub fn current_fraction(&self) -> f64 {
        if self.total_units == 0 {
            0.0
        } else {
            self.harness_units as f64 / self.total_units as f64
        }
    }

    /// May one more unit of harness work be admitted right now?
    ///
    /// This is the scheduler's hard admission rule: call before leasing a
    /// `TicketKind::Harness` ticket, not after. Pure, total, no I/O.
    pub fn admit_harness_unit(&self) -> Result<(), EfficacyError> {
        if !self.budget.enabled {
            return Err(EfficacyError::Disabled);
        }

        let requested = (self.harness_units + 1) as f64 / (self.total_units + 1) as f64;
        if requested > self.budget.max_compute_fraction {
            return Err(EfficacyError::FractionExceeded {
                requested,
                cap: self.budget.max_compute_fraction,
            });
        }

        Ok(())
    }

    /// Record that one unit of capacity was occupied, either by harness work or not.
    pub fn record(&mut self, is_harness: bool, units: u64) {
        self.total_units = self.total_units.saturating_add(units);
        if is_harness {
            self.harness_units = self.harness_units.saturating_add(units);
        }
    }

    /// Record that `units` of previously-occupied capacity were released.
    pub fn release(&mut self, is_harness: bool, units: u64) {
        self.total_units = self.total_units.saturating_sub(units);
        if is_harness {
            self.harness_units = self.harness_units.saturating_sub(units);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admit_harness_unit_when_disabled() {
        let budget = EfficacyBudget {
            enabled: false,
            max_compute_fraction: 0.5,
            require_benchmark_gain: true,
        };
        let account = EfficacyAccount::new(budget);

        let result = account.admit_harness_unit();
        assert_eq!(result, Err(EfficacyError::Disabled));
    }

    #[test]
    fn admit_harness_unit_from_empty_state() {
        let budget = EfficacyBudget {
            enabled: true,
            max_compute_fraction: 0.5,
            require_benchmark_gain: true,
        };
        let account = EfficacyAccount::new(budget);

        // With zero total units, admitting one harness and one total gives 1/1 = 1.0
        let result = account.admit_harness_unit();
        assert_eq!(
            result,
            Err(EfficacyError::FractionExceeded {
                requested: 1.0,
                cap: 0.5,
            })
        );
    }

    #[test]
    fn admit_harness_unit_well_below_cap() {
        let budget = EfficacyBudget {
            enabled: true,
            max_compute_fraction: 0.5,
            require_benchmark_gain: true,
        };
        let mut account = EfficacyAccount::new(budget);

        // Add 10 non-harness units: harness=0, total=10
        account.record(false, 10);

        // Try to admit one harness unit: (0+1)/(10+1) = 1/11 ≈ 0.09, which is < 0.5
        let result = account.admit_harness_unit();
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn admit_harness_unit_exceeds_cap() {
        let budget = EfficacyBudget {
            enabled: true,
            max_compute_fraction: 0.1,
            require_benchmark_gain: true,
        };
        let mut account = EfficacyAccount::new(budget);

        // Add 1 harness unit and 8 non-harness: harness=1, total=9
        account.record(true, 1);
        account.record(false, 8);

        // Try to admit one more harness: (1+1)/(9+1) = 2/10 = 0.2, which exceeds 0.1
        let result = account.admit_harness_unit();
        assert_eq!(
            result,
            Err(EfficacyError::FractionExceeded {
                requested: 0.2,
                cap: 0.1,
            })
        );
    }

    #[test]
    fn admit_harness_unit_at_boundary() {
        let budget = EfficacyBudget {
            enabled: true,
            max_compute_fraction: 0.5,
            require_benchmark_gain: true,
        };
        let mut account = EfficacyAccount::new(budget);

        // Add 4 non-harness units: harness=0, total=4
        account.record(false, 4);

        // Try to admit one harness: (0+1)/(4+1) = 1/5 = 0.2, which is < 0.5
        let result = account.admit_harness_unit();
        assert_eq!(result, Ok(()));

        // Now add 3 more harness units: harness=1, total=7 (after previous admission)
        // Actually test the at-boundary case: if we have harness=3, total=6
        let mut account = EfficacyAccount::new(budget);
        account.record(true, 3);
        account.record(false, 3);

        // Try to admit one more: (3+1)/(6+1) = 4/7 ≈ 0.571, which exceeds 0.5
        let result = account.admit_harness_unit();
        assert_eq!(
            result,
            Err(EfficacyError::FractionExceeded {
                requested: 4.0 / 7.0,
                cap: 0.5,
            })
        );
    }

    #[test]
    fn validate_budget_valid_fractions() {
        let test_cases = vec![0.0, 0.1, 0.5, 0.9, 1.0];
        for fraction in test_cases {
            let budget = EfficacyBudget {
                enabled: true,
                max_compute_fraction: fraction,
                require_benchmark_gain: false,
            };
            assert_eq!(budget.validate(), Ok(()));
        }
    }

    #[test]
    fn validate_budget_invalid_fractions() {
        let invalid_cases = vec![-0.1, 1.1, 2.0];
        for fraction in invalid_cases {
            let budget = EfficacyBudget {
                enabled: true,
                max_compute_fraction: fraction,
                require_benchmark_gain: false,
            };
            assert_eq!(
                budget.validate(),
                Err(EfficacyError::InvalidFraction(fraction))
            );
        }
    }

    #[test]
    fn current_fraction_empty_account() {
        let budget = EfficacyBudget::default();
        let account = EfficacyAccount::new(budget);

        assert_eq!(account.current_fraction(), 0.0);
    }

    #[test]
    fn current_fraction_with_data() {
        let budget = EfficacyBudget::default();
        let mut account = EfficacyAccount::new(budget);

        account.record(true, 3);
        account.record(false, 7);

        // harness=3, total=10 => 3/10 = 0.3
        assert_eq!(account.current_fraction(), 0.3);
    }

    #[test]
    fn record_harness_work() {
        let budget = EfficacyBudget::default();
        let mut account = EfficacyAccount::new(budget);

        account.record(true, 5);

        assert_eq!(account.harness_units, 5);
        assert_eq!(account.total_units, 5);
    }

    #[test]
    fn record_non_harness_work() {
        let budget = EfficacyBudget::default();
        let mut account = EfficacyAccount::new(budget);

        account.record(false, 8);

        assert_eq!(account.harness_units, 0);
        assert_eq!(account.total_units, 8);
    }

    #[test]
    fn record_mixed_work() {
        let budget = EfficacyBudget::default();
        let mut account = EfficacyAccount::new(budget);

        account.record(true, 3);
        account.record(false, 7);
        account.record(true, 2);

        assert_eq!(account.harness_units, 5);
        assert_eq!(account.total_units, 12);
    }

    #[test]
    fn release_harness_work() {
        let budget = EfficacyBudget::default();
        let mut account = EfficacyAccount::new(budget);

        account.record(true, 10);
        account.release(true, 3);

        assert_eq!(account.harness_units, 7);
        assert_eq!(account.total_units, 7);
    }

    #[test]
    fn release_non_harness_work() {
        let budget = EfficacyBudget::default();
        let mut account = EfficacyAccount::new(budget);

        account.record(false, 10);
        account.release(false, 4);

        assert_eq!(account.harness_units, 0);
        assert_eq!(account.total_units, 6);
    }

    #[test]
    fn release_capped_at_zero() {
        let budget = EfficacyBudget::default();
        let mut account = EfficacyAccount::new(budget);

        account.record(true, 5);
        account.release(true, 100);

        assert_eq!(account.harness_units, 0);
        assert_eq!(account.total_units, 0);
    }

    #[test]
    fn default_budget() {
        let budget = EfficacyBudget::default();

        assert!(budget.enabled);
        assert_eq!(budget.max_compute_fraction, 0.1);
        assert!(budget.require_benchmark_gain);
    }

    #[test]
    fn efficacy_account_new() {
        let budget = EfficacyBudget {
            enabled: true,
            max_compute_fraction: 0.25,
            require_benchmark_gain: false,
        };
        let account = EfficacyAccount::new(budget);

        assert_eq!(account.budget, budget);
        assert_eq!(account.harness_units, 0);
        assert_eq!(account.total_units, 0);
    }

    #[test]
    fn admit_harness_unit_strict_cap_ten_percent() {
        let budget = EfficacyBudget {
            enabled: true,
            max_compute_fraction: 0.1,
            require_benchmark_gain: true,
        };
        let mut account = EfficacyAccount::new(budget);

        // Add 99 non-harness units
        account.record(false, 99);

        // Try to admit one harness: (0+1)/(99+1) = 1/100 = 0.01, which is <= 0.1
        let result = account.admit_harness_unit();
        assert_eq!(result, Ok(()));

        // Now simulate that one being admitted
        account.record(true, 1);

        // Try to admit one more: (1+1)/(100+1) = 2/101 ≈ 0.0198, which is still <= 0.1
        let result = account.admit_harness_unit();
        assert_eq!(result, Ok(()));

        // Keep adding harness until we hit the limit
        account.record(true, 8); // Now harness=9, total=108

        // Try to admit one more: (9+1)/(108+1) = 10/109 ≈ 0.0917, which is <= 0.1
        let result = account.admit_harness_unit();
        assert_eq!(result, Ok(()));

        account.record(true, 1); // Now harness=10, total=109

        // Try to admit one more: (10+1)/(109+1) = 11/110 = 0.1, which equals 0.1
        // The check is strictly greater than (>), so this should pass
        let result = account.admit_harness_unit();
        assert_eq!(result, Ok(()));

        account.record(true, 1); // Now harness=11, total=110

        // Try to admit one more: (11+1)/(110+1) = 12/111 ≈ 0.1081, which exceeds 0.1
        let result = account.admit_harness_unit();
        assert!(matches!(
            result,
            Err(EfficacyError::FractionExceeded { .. })
        ));
    }
}
