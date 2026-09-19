//! The epoch registry: append-only history of promoted [`HarnessConfig`]s, session pinning, and
//! promotion.
//!
//! The safety property `SPEC.md` §10 requires — a session pins the harness epoch it starts with
//! and never observes a later one mid-session — lives entirely in this module's shape: promotion
//! ([`EpochRegistry::promote`]) only ever *appends* a new [`HarnessEpoch`] with a higher
//! `number`; it never mutates an existing epoch's `config`. A [`SessionPin`] records a fixed
//! epoch `number`, and [`EpochRegistry::resolve`] looks that number up verbatim, so a pin taken
//! before a promotion resolves to the same config after it, no matter how many later epochs have
//! since been promoted. This is the guarantee the pinning test in this module proves.

use tm_types::{Clock, ParticipantId, Result, SessionId, Timestamp};

use crate::bench::BenchmarkReport;
use crate::config::{ConfigHash, HarnessConfig};

/// One promoted version of the project's harness policy.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessEpoch {
    /// Monotonically increasing epoch number; `0` is the genesis epoch every project starts at.
    pub number: u64,
    /// [`HarnessConfig::config_hash`] of `config`, cached so equality/identity checks don't
    /// re-hash.
    pub config_hash: ConfigHash,
    /// The full harness policy this epoch pins.
    pub config: HarnessConfig,
    /// When this epoch was promoted.
    pub promoted_at: Timestamp,
    /// Who promoted it (a human, or an agent acting under delegated project authority).
    pub promoted_by: ParticipantId,
    /// The benchmark run this epoch was promoted on, if any. `None` only for the genesis epoch,
    /// which has no prior baseline to have been benchmarked against.
    pub benchmark: Option<BenchmarkReport>,
}

/// A session's fixed reference to the epoch it started under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPin {
    /// The session that pinned this epoch.
    pub session: SessionId,
    /// The epoch number pinned; never updated for the lifetime of `session`.
    pub epoch_number: u64,
    /// When the pin was taken.
    pub pinned_at: Timestamp,
}

/// Why resolving or promoting an epoch failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EpochResolutionError {
    /// No epoch with this number exists in the registry.
    #[error("unknown harness epoch {0}")]
    UnknownEpoch(u64),
    /// [`EpochRegistry::promote`] was called with a `number` that does not immediately follow
    /// the current highest epoch (registry must grow by exactly one at a time).
    #[error("cannot promote epoch {attempted}: current epoch is {current}")]
    NonSequentialPromotion {
        /// The epoch number the caller attempted to promote.
        attempted: u64,
        /// The registry's current highest epoch number.
        current: u64,
    },
}

/// The gate a candidate epoch must clear to be promoted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PromotionGate {
    /// When true, a candidate must show a benchmark gain over the current epoch to be promoted.
    pub require_benchmark_gain: bool,
    /// Minimum `aggregate_score` gain required when `require_benchmark_gain` is set.
    pub min_gain: f64,
}

/// The gate's verdict on a candidate promotion.
#[derive(Debug, Clone, PartialEq)]
pub enum PromotionDecision {
    /// The candidate cleared the gate.
    Approved,
    /// The candidate did not clear the gate; the reason is human-readable.
    Rejected(String),
}

impl PromotionDecision {
    /// True for [`PromotionDecision::Approved`].
    pub fn is_approved(&self) -> bool {
        matches!(self, PromotionDecision::Approved)
    }
}

impl PromotionGate {
    /// Evaluate whether `candidate`'s benchmark clears this gate relative to `baseline`.
    ///
    /// If gain checking is disabled, always approves. Otherwise requires a baseline and
    /// compares aggregate scores; approval depends on whether gain meets the minimum.
    pub fn evaluate(
        &self,
        baseline: Option<&BenchmarkReport>,
        candidate: &BenchmarkReport,
    ) -> PromotionDecision {
        if !self.require_benchmark_gain {
            return PromotionDecision::Approved;
        }

        let Some(baseline) = baseline else {
            return PromotionDecision::Rejected("no baseline to compare against".into());
        };

        let gain = candidate.aggregate_score - baseline.aggregate_score;
        if gain >= self.min_gain {
            PromotionDecision::Approved
        } else {
            // Round for display only: raw f64 subtraction can carry binary-representation noise
            // (e.g. 2.2 - 2.0 == 0.20000000000000018) that would otherwise leak into the message.
            let displayed_gain = (gain * 1e9).round() / 1e9;
            PromotionDecision::Rejected(format!(
                "benchmark gain {displayed_gain} below required {}",
                self.min_gain
            ))
        }
    }
}

/// The result of attempting a promotion.
#[derive(Debug, Clone, PartialEq)]
pub enum PromotionOutcome {
    /// The candidate was promoted; carries the new, now-current [`HarnessEpoch`].
    Promoted(Box<HarnessEpoch>),
    /// The candidate was rejected by the [`PromotionGate`]; the registry is unchanged.
    Rejected(PromotionDecision),
}

/// Append-only history of a project's harness epochs.
#[derive(Debug, Clone, PartialEq)]
pub struct EpochRegistry {
    epochs: Vec<HarnessEpoch>,
}

impl EpochRegistry {
    /// Start a registry at its genesis epoch (number `0`).
    pub fn new(genesis: HarnessEpoch) -> EpochRegistry {
        EpochRegistry {
            epochs: vec![genesis],
        }
    }

    /// The current (highest-numbered) epoch.
    ///
    /// # Panics
    /// Never: a registry is constructed with at least a genesis epoch and only ever grows.
    pub fn current(&self) -> &HarnessEpoch {
        self.epochs
            .last()
            .expect("EpochRegistry always holds at least the genesis epoch")
    }

    /// Look up an epoch by number, regardless of whether it is current.
    pub fn get(&self, number: u64) -> Option<&HarnessEpoch> {
        self.epochs.iter().find(|e| e.number == number)
    }

    /// All epochs in the registry, oldest first.
    pub fn epochs(&self) -> &[HarnessEpoch] {
        &self.epochs
    }

    /// Pin `session` to this registry's current epoch as of `now`.
    ///
    /// Called once, at session start. The caller persists the returned [`SessionPin`] and passes
    /// it to [`EpochRegistry::resolve`] for the rest of the session's lifetime; the registry is
    /// never consulted via [`EpochRegistry::current`] again for that session.
    pub fn pin_current(&self, session: SessionId, clock: &dyn Clock) -> SessionPin {
        SessionPin {
            session,
            epoch_number: self.current().number,
            pinned_at: clock.now(),
        }
    }

    /// Resolve the epoch a [`SessionPin`] fixed, independent of any promotion since.
    ///
    /// The pinning guarantee: returns the epoch the pin captured, never the current epoch,
    /// ensuring a session's config never changes mid-execution even if promoted.
    pub fn resolve(&self, pin: &SessionPin) -> Result<&HarnessEpoch, EpochResolutionError> {
        self.get(pin.epoch_number)
            .ok_or(EpochResolutionError::UnknownEpoch(pin.epoch_number))
    }

    /// Attempt to promote `candidate` (already-validated, per [`HarnessConfig::validate`]) as the
    /// next epoch.
    ///
    /// Evaluates the gate; on approval, appends to epochs (append-only); on rejection, leaves
    /// registry unchanged. Never mutates existing epochs. Rejects early if benchmark required
    /// but not supplied.
    #[allow(clippy::too_many_arguments)]
    pub fn promote(
        &mut self,
        candidate: HarnessConfig,
        promoted_at: Timestamp,
        promoted_by: ParticipantId,
        benchmark: Option<BenchmarkReport>,
        gate: &PromotionGate,
    ) -> Result<PromotionOutcome, EpochResolutionError> {
        let next_number = self.current().number + 1;

        if benchmark.is_none() && gate.require_benchmark_gain {
            return Ok(PromotionOutcome::Rejected(PromotionDecision::Rejected(
                "no benchmark supplied".into(),
            )));
        }

        // `benchmark` is `None` here only when `gate.require_benchmark_gain` is false (the
        // `None`+`true` combination already returned above), in which case
        // `PromotionGate::evaluate` returns `Approved` without reading `candidate` at all — so a
        // placeholder report is never actually inspected. Reaching for it via `.expect(..)`
        // instead would panic on exactly that reachable, gate-permitted call shape (a real
        // caller promoting with `require_benchmark_gain: false` and no benchmark to hand), which
        // is what this local `unwrap_or_default` avoids.
        let placeholder_benchmark;
        let candidate_benchmark = match benchmark.as_ref() {
            Some(b) => b,
            None => {
                placeholder_benchmark = BenchmarkReport::default();
                &placeholder_benchmark
            }
        };
        let decision = gate.evaluate(self.current().benchmark.as_ref(), candidate_benchmark);

        match decision {
            PromotionDecision::Approved => {
                let epoch = HarnessEpoch {
                    number: next_number,
                    config_hash: candidate.config_hash(),
                    config: candidate,
                    promoted_at,
                    promoted_by,
                    benchmark,
                };
                self.epochs.push(epoch.clone());
                Ok(PromotionOutcome::Promoted(Box::new(epoch)))
            }
            PromotionDecision::Rejected(_) => Ok(PromotionOutcome::Rejected(decision)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::FixedClock;

    fn test_benchmark_report(aggregate_score: f64) -> BenchmarkReport {
        BenchmarkReport {
            aggregate_score,
            ..Default::default()
        }
    }

    fn test_harness_config() -> HarnessConfig {
        HarnessConfig::default()
    }

    fn test_genesis_epoch(clock: &dyn Clock) -> HarnessEpoch {
        let config = test_harness_config();
        HarnessEpoch {
            number: 0,
            config_hash: config.config_hash(),
            config,
            promoted_at: clock.now(),
            promoted_by: ParticipantId::system(),
            benchmark: None,
        }
    }

    fn test_session_id() -> SessionId {
        SessionId::new("S-1").expect("valid session id")
    }

    #[test]
    fn promotion_gate_no_gain_check_always_approves() {
        let gate = PromotionGate {
            require_benchmark_gain: false,
            min_gain: 0.0,
        };
        let candidate = test_benchmark_report(1.0);
        let baseline = test_benchmark_report(5.0);

        let decision = gate.evaluate(Some(&baseline), &candidate);
        assert!(decision.is_approved());
    }

    #[test]
    fn promotion_gate_with_gain_check_rejects_no_baseline() {
        let gate = PromotionGate {
            require_benchmark_gain: true,
            min_gain: 0.5,
        };
        let candidate = test_benchmark_report(2.0);

        let decision = gate.evaluate(None, &candidate);
        assert!(!decision.is_approved());
        match decision {
            PromotionDecision::Rejected(msg) => {
                assert_eq!(msg, "no baseline to compare against");
            }
            _ => panic!("expected rejection"),
        }
    }

    #[test]
    fn promotion_gate_approves_sufficient_gain() {
        let gate = PromotionGate {
            require_benchmark_gain: true,
            min_gain: 0.5,
        };
        let baseline = test_benchmark_report(1.0);
        let candidate = test_benchmark_report(2.0);

        let decision = gate.evaluate(Some(&baseline), &candidate);
        assert!(decision.is_approved());
    }

    #[test]
    fn promotion_gate_rejects_insufficient_gain() {
        let gate = PromotionGate {
            require_benchmark_gain: true,
            min_gain: 0.5,
        };
        let baseline = test_benchmark_report(2.0);
        let candidate = test_benchmark_report(2.2);

        let decision = gate.evaluate(Some(&baseline), &candidate);
        assert!(!decision.is_approved());
        match decision {
            PromotionDecision::Rejected(msg) => {
                assert!(msg.contains("benchmark gain 0.2 below required 0.5"));
            }
            _ => panic!("expected rejection"),
        }
    }

    #[test]
    fn promotion_gate_accepts_exact_minimum_gain() {
        let gate = PromotionGate {
            require_benchmark_gain: true,
            min_gain: 0.5,
        };
        let baseline = test_benchmark_report(1.0);
        let candidate = test_benchmark_report(1.5);

        let decision = gate.evaluate(Some(&baseline), &candidate);
        assert!(decision.is_approved());
    }

    #[test]
    fn epoch_registry_pin_and_resolve_basic_flow() {
        let clock = FixedClock::epoch();
        let genesis = test_genesis_epoch(&clock);
        let registry = EpochRegistry::new(genesis.clone());

        let session_id = test_session_id();
        let pin = registry.pin_current(session_id.clone(), &clock);

        assert_eq!(pin.epoch_number, 0);
        assert_eq!(pin.session, session_id);

        let resolved = registry
            .resolve(&pin)
            .expect("should resolve genesis epoch");
        assert_eq!(resolved.number, 0);
        assert_eq!(resolved.config_hash, genesis.config_hash);
    }

    #[test]
    fn epoch_registry_resolve_unknown_epoch_fails() {
        let clock = FixedClock::epoch();
        let genesis = test_genesis_epoch(&clock);
        let registry = EpochRegistry::new(genesis);

        let fake_pin = SessionPin {
            session: test_session_id(),
            epoch_number: 99,
            pinned_at: clock.now(),
        };

        let result = registry.resolve(&fake_pin);
        assert!(result.is_err());
        match result.unwrap_err() {
            EpochResolutionError::UnknownEpoch(n) => assert_eq!(n, 99),
            _ => panic!("expected UnknownEpoch error"),
        }
    }

    #[test]
    fn epoch_registry_pinning_guarantee_survives_promotion() {
        let clock = FixedClock::epoch();
        let mut genesis = test_genesis_epoch(&clock);
        genesis.number = 0;
        let mut registry = EpochRegistry::new(genesis);

        let session_id = test_session_id();
        let pin = registry.pin_current(session_id, &clock);

        clock.advance_seconds(60);

        let gate = PromotionGate {
            require_benchmark_gain: false,
            min_gain: 0.0,
        };
        let candidate = test_harness_config();
        let new_benchmark = test_benchmark_report(2.5);

        let outcome = registry
            .promote(
                candidate,
                clock.now(),
                ParticipantId::system(),
                Some(new_benchmark),
                &gate,
            )
            .expect("promotion should succeed");

        match outcome {
            PromotionOutcome::Promoted(new_epoch) => {
                assert_eq!(new_epoch.number, 1);
            }
            _ => panic!("expected promotion"),
        }

        let resolved = registry.resolve(&pin).expect("pin should still resolve");
        assert_eq!(resolved.number, 0);
    }

    #[test]
    fn epoch_registry_promote_with_lenient_gate_and_no_benchmark_does_not_panic() {
        // Regression test: `promote`'s early "no benchmark supplied" rejection only fires when
        // `gate.require_benchmark_gain` is true. A lenient gate (`false`) with `benchmark: None`
        // used to reach an `.expect("benchmark required when gate is consulted")` on the `None`
        // and panic, even though `PromotionGate::evaluate` never reads its `candidate` argument
        // when the gate is lenient. A caller promoting with no benchmark to hand (e.g. `tm
        // harness promote --force` with none found on disk) must not crash.
        let clock = FixedClock::epoch();
        let mut genesis = test_genesis_epoch(&clock);
        genesis.number = 0;
        let mut registry = EpochRegistry::new(genesis);

        let gate = PromotionGate {
            require_benchmark_gain: false,
            min_gain: 0.0,
        };
        let candidate = test_harness_config();

        let outcome = registry
            .promote(candidate, clock.now(), ParticipantId::system(), None, &gate)
            .expect("promotion attempt should return outcome, not panic");

        match outcome {
            PromotionOutcome::Promoted(epoch) => {
                assert_eq!(epoch.number, 1);
                assert!(epoch.benchmark.is_none());
            }
            _ => panic!("expected promotion under a lenient gate"),
        }
    }

    #[test]
    fn epoch_registry_promote_increments_number() {
        let clock = FixedClock::epoch();
        let mut genesis = test_genesis_epoch(&clock);
        genesis.number = 0;
        let mut registry = EpochRegistry::new(genesis);

        assert_eq!(registry.current().number, 0);

        let gate = PromotionGate {
            require_benchmark_gain: false,
            min_gain: 0.0,
        };
        let candidate = test_harness_config();
        let benchmark = test_benchmark_report(1.5);

        let outcome = registry
            .promote(
                candidate,
                clock.now(),
                ParticipantId::system(),
                Some(benchmark),
                &gate,
            )
            .expect("promotion should succeed");

        match outcome {
            PromotionOutcome::Promoted(epoch) => assert_eq!(epoch.number, 1),
            _ => panic!("expected promotion"),
        }

        assert_eq!(registry.current().number, 1);
    }

    #[test]
    fn epoch_registry_promote_rejected_by_gate_leaves_registry_unchanged() {
        let clock = FixedClock::epoch();
        let mut genesis = test_genesis_epoch(&clock);
        genesis.benchmark = Some(test_benchmark_report(5.0));
        let mut registry = EpochRegistry::new(genesis);

        let gate = PromotionGate {
            require_benchmark_gain: true,
            min_gain: 2.0,
        };
        let candidate = test_harness_config();
        let weak_benchmark = test_benchmark_report(5.3);

        let outcome = registry
            .promote(
                candidate,
                clock.now(),
                ParticipantId::system(),
                Some(weak_benchmark),
                &gate,
            )
            .expect("promotion attempt should return outcome");

        match outcome {
            PromotionOutcome::Rejected(_) => {
                assert_eq!(registry.epochs().len(), 1);
                assert_eq!(registry.current().number, 0);
            }
            _ => panic!("expected rejection"),
        }
    }

    #[test]
    fn epoch_registry_promote_rejects_missing_benchmark_when_required() {
        let clock = FixedClock::epoch();
        let mut genesis = test_genesis_epoch(&clock);
        genesis.number = 0;
        let mut registry = EpochRegistry::new(genesis);

        let gate = PromotionGate {
            require_benchmark_gain: true,
            min_gain: 0.5,
        };
        let candidate = test_harness_config();

        let outcome = registry
            .promote(candidate, clock.now(), ParticipantId::system(), None, &gate)
            .expect("promotion attempt should return outcome");

        match outcome {
            PromotionOutcome::Rejected(PromotionDecision::Rejected(msg)) => {
                assert_eq!(msg, "no benchmark supplied");
            }
            _ => panic!("expected rejection with 'no benchmark supplied'"),
        }

        assert_eq!(registry.epochs().len(), 1);
    }

    #[test]
    fn epoch_registry_promoted_epoch_has_correct_fields() {
        let clock = FixedClock::epoch();
        let mut genesis = test_genesis_epoch(&clock);
        genesis.number = 0;
        let mut registry = EpochRegistry::new(genesis);

        let gate = PromotionGate {
            require_benchmark_gain: false,
            min_gain: 0.0,
        };
        let candidate = test_harness_config();
        let benchmark = test_benchmark_report(3.5);
        let promoted_by = ParticipantId::system();
        let promoted_at = clock.now();

        let outcome = registry
            .promote(
                candidate.clone(),
                promoted_at,
                promoted_by.clone(),
                Some(benchmark.clone()),
                &gate,
            )
            .expect("promotion should succeed");

        match outcome {
            PromotionOutcome::Promoted(epoch) => {
                assert_eq!(epoch.number, 1);
                assert_eq!(epoch.promoted_at, promoted_at);
                assert_eq!(epoch.promoted_by, promoted_by);
                assert_eq!(epoch.config_hash, candidate.config_hash());
                assert_eq!(epoch.benchmark.as_ref().unwrap().aggregate_score, 3.5);
            }
            _ => panic!("expected promotion"),
        }
    }

    #[test]
    fn epoch_registry_multiple_promotions_maintain_append_only_property() {
        let clock = FixedClock::epoch();
        let mut genesis = test_genesis_epoch(&clock);
        genesis.number = 0;
        let mut registry = EpochRegistry::new(genesis.clone());

        let gate = PromotionGate {
            require_benchmark_gain: false,
            min_gain: 0.0,
        };

        for i in 1..=3 {
            clock.advance_seconds(10);
            let candidate = test_harness_config();
            let benchmark = test_benchmark_report(1.0 + (i as f64) * 0.5);

            registry
                .promote(
                    candidate,
                    clock.now(),
                    ParticipantId::system(),
                    Some(benchmark),
                    &gate,
                )
                .expect("promotion should succeed");
        }

        assert_eq!(registry.epochs().len(), 4);
        assert_eq!(registry.epochs()[0].number, 0);
        assert_eq!(registry.epochs()[1].number, 1);
        assert_eq!(registry.epochs()[2].number, 2);
        assert_eq!(registry.epochs()[3].number, 3);
    }

    #[test]
    fn epoch_registry_get_retrieves_any_epoch() {
        let clock = FixedClock::epoch();
        let mut genesis = test_genesis_epoch(&clock);
        genesis.number = 0;
        let mut registry = EpochRegistry::new(genesis);

        let gate = PromotionGate {
            require_benchmark_gain: false,
            min_gain: 0.0,
        };

        for _ in 1..=2 {
            registry
                .promote(
                    test_harness_config(),
                    clock.now(),
                    ParticipantId::system(),
                    Some(test_benchmark_report(2.0)),
                    &gate,
                )
                .expect("promotion should succeed");
        }

        assert!(registry.get(0).is_some());
        assert!(registry.get(1).is_some());
        assert!(registry.get(2).is_some());
        assert!(registry.get(3).is_none());
    }

    #[test]
    fn promotion_decision_is_approved_method() {
        let approved = PromotionDecision::Approved;
        assert!(approved.is_approved());

        let rejected = PromotionDecision::Rejected("some reason".to_string());
        assert!(!rejected.is_approved());
    }
}
