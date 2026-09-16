//! Mutable-but-pure fabric state: per-candidate counters, health and the accounting ledger.
//!
//! Everything here is updated by pure functions of `(state, event, now)` — no direct clock reads,
//! no I/O. [`crate::fabric::Fabric`] is the only place that actually calls the injected
//! [`tm_types::Clock`] and feeds the result in. Keeping updates pure is what makes
//! [`crate::route::route`] unit-testable without wall-clock flakiness.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tm_types::Timestamp;

use crate::types::ModelId;

// Tracing is available via workspace dependencies for warnings.
use tracing::warn;

/// A candidate is identified by its `(provider, model)` pair; this is the map key into
/// [`FabricState`]'s per-candidate tables.
pub type CandidateKey = ModelId;

/// A sliding window counter: how many units were consumed inside the current window, and when
/// the window resets.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Units consumed so far in the current window (requests or tokens, depending on the field).
    pub used: u64,
    /// The instant the window began.
    pub window_start: Timestamp,
}

/// The circuit breaker's state machine: `Closed -> Open` after `N` failures inside a window,
/// `Open -> HalfOpen` after a cooldown elapses, then back to `Closed` on a probe success or
/// `Open` on a probe failure.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BreakerState {
    /// Requests flow normally.
    Closed,
    /// Requests are refused outright until `retry_at`.
    Open {
        /// When the breaker may transition to `HalfOpen`.
        retry_at: Timestamp,
    },
    /// A single probe request is allowed through to test recovery.
    HalfOpen,
}

/// A candidate's circuit breaker: state plus the failure bookkeeping that drives transitions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Breaker {
    /// Current state.
    pub state: BreakerState,
    /// Consecutive-failure count observed inside the current failure window.
    pub failure_count: u32,
    /// When the current failure-counting window began.
    pub window_start: Timestamp,
    /// Failures inside `window` before tripping to `Open`.
    pub failure_threshold: u32,
    /// Width of the failure-counting window.
    pub window: std::time::Duration,
    /// How long `Open` lasts before allowing a `HalfOpen` probe.
    pub cooldown: std::time::Duration,
}

impl Breaker {
    /// A freshly closed breaker with the given thresholds.
    pub fn new(
        failure_threshold: u32,
        window: std::time::Duration,
        cooldown: std::time::Duration,
        now: Timestamp,
    ) -> Self {
        Breaker {
            state: BreakerState::Closed,
            failure_count: 0,
            window_start: now,
            failure_threshold,
            window,
            cooldown,
        }
    }
}

/// Everything the fabric tracks for one `(provider, model)` candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateState {
    /// Requests-per-minute window.
    pub rpm: Window,
    /// Tokens-per-minute window.
    pub tpm: Window,
    /// Requests-per-day window.
    pub daily: Window,
    /// Requests-per-month window.
    pub monthly: Window,
    /// Requests currently in flight.
    pub live_concurrency: u32,
    /// Circuit breaker.
    pub breaker: Breaker,
    /// Exponentially weighted moving average latency, in milliseconds.
    pub ewma_latency_ms: f64,
    /// Smoothing factor for the EWMA update, in `(0, 1]`.
    pub ewma_alpha: f64,
}

/// One posted entry in the cumulative accounting ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// The candidate this spend was against.
    pub candidate: CandidateKey,
    /// When the spend was recorded.
    pub at: Timestamp,
    /// Input tokens billed.
    pub input_tokens: u32,
    /// Output tokens billed.
    pub output_tokens: u32,
    /// Cost in micro-dollars, matching [`tm_types::Budget`]'s unit.
    pub cost_micros: u64,
}

/// An event that mutates [`FabricState`]. Constructed by [`crate::fabric::Fabric`] after each
/// provider call (or scheduling decision) and folded in via [`FabricState::apply`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FabricEvent {
    /// A request started against `candidate`: bump `live_concurrency` and the rpm window.
    RequestStarted {
        /// The candidate the request was sent to.
        candidate: CandidateKey,
    },
    /// A request finished successfully.
    RequestSucceeded {
        /// The candidate the request was sent to.
        candidate: CandidateKey,
        /// Wall-clock latency observed.
        latency: std::time::Duration,
        /// Tokens consumed, for tpm accounting and the ledger.
        usage: crate::types::Usage,
        /// Cost in micro-dollars for this call, if pricing is known.
        cost_micros: Option<u64>,
    },
    /// A request failed.
    RequestFailed {
        /// The candidate the request was sent to.
        candidate: CandidateKey,
        /// Whether this failure should count against the circuit breaker (transient
        /// provider/network failures do; a caller-caused `InvalidRequest` should not).
        counts_against_breaker: bool,
    },
    /// A half-open probe succeeded; close the breaker.
    ProbeSucceeded {
        /// The candidate whose breaker is recovering.
        candidate: CandidateKey,
    },
}

/// The fabric's complete mutable state: every candidate's counters and health, plus the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FabricState {
    candidates: BTreeMap<CandidateKey, CandidateState>,
    ledger: Vec<LedgerEntry>,
}

impl FabricState {
    /// An empty state with no candidates registered yet.
    pub fn new() -> Self {
        FabricState::default()
    }

    /// Register a candidate with fresh counters, if not already present. Idempotent.
    /// No-op if `key` is already in `candidates` (registering must not reset a live
    /// candidate's counters, e.g. on config reload). Otherwise insert a `CandidateState` with all
    /// windows starting at `now`, zero `live_concurrency`, a `Closed` breaker built via
    /// `Breaker::new` with the given thresholds, and `ewma_latency_ms = 0.0`.
    pub fn register(
        &mut self,
        key: CandidateKey,
        now: Timestamp,
        failure_threshold: u32,
        breaker_window: std::time::Duration,
        breaker_cooldown: std::time::Duration,
        ewma_alpha: f64,
    ) {
        self.candidates
            .entry(key)
            .or_insert_with(|| CandidateState {
                rpm: Window {
                    used: 0,
                    window_start: now,
                },
                tpm: Window {
                    used: 0,
                    window_start: now,
                },
                daily: Window {
                    used: 0,
                    window_start: now,
                },
                monthly: Window {
                    used: 0,
                    window_start: now,
                },
                live_concurrency: 0,
                breaker: Breaker::new(failure_threshold, breaker_window, breaker_cooldown, now),
                ewma_latency_ms: 0.0,
                ewma_alpha,
            });
    }

    /// Read a candidate's current state, if registered.
    pub fn candidate(&self, key: &CandidateKey) -> Option<&CandidateState> {
        self.candidates.get(key)
    }

    /// All registered candidates, in key order.
    pub fn candidates(&self) -> impl Iterator<Item = (&CandidateKey, &CandidateState)> {
        self.candidates.iter()
    }

    /// The full ledger, in the order entries were posted.
    pub fn ledger(&self) -> &[LedgerEntry] {
        &self.ledger
    }

    /// Fold one event into the state as of `now`. Rolls windows forward before applying the
    /// event so stale usage never leaks into a new window.
    ///
    /// Looks up `event`'s candidate (no-op with a `tracing::warn!` if unregistered — this
    /// indicates a caller bug but must never panic). Calls `roll_windows` first. Then:
    /// `RequestStarted` increments `live_concurrency` and `rpm.used`; `RequestSucceeded`
    /// decrements `live_concurrency` (saturating), adds `usage.input_tokens + usage.output_tokens`
    /// to `tpm.used`, `daily.used` and `monthly.used`, updates `ewma_latency_ms` via
    /// `ewma_alpha * latency_ms + (1 - ewma_alpha) * old`, appends a `LedgerEntry` when
    /// `cost_micros.is_some()`, resets `breaker.failure_count` to 0, and if `breaker.state` was
    /// `HalfOpen` transitions it to `Closed`; `RequestFailed` decrements `live_concurrency`
    /// (saturating) and, when `counts_against_breaker`, increments `failure_count` and — once it
    /// reaches `failure_threshold` — transitions to `Open { retry_at: now + cooldown }`;
    /// `ProbeSucceeded` transitions `Open`/`HalfOpen` to `Closed` and resets `failure_count`.
    pub fn apply(&mut self, event: FabricEvent, now: Timestamp) {
        let candidate_key = match &event {
            FabricEvent::RequestStarted { candidate } => candidate,
            FabricEvent::RequestSucceeded { candidate, .. } => candidate,
            FabricEvent::RequestFailed { candidate, .. } => candidate,
            FabricEvent::ProbeSucceeded { candidate } => candidate,
        };

        if !self.candidates.contains_key(candidate_key) {
            warn!(
                "apply event for unregistered candidate: {:?}",
                candidate_key
            );
            return;
        }

        // Roll windows before applying the event to ensure stale usage doesn't leak.
        self.roll_windows(now);

        let candidate = self
            .candidates
            .get_mut(candidate_key)
            .expect("checked above: candidate_key is present in self.candidates");

        match event {
            FabricEvent::RequestStarted { .. } => {
                candidate.live_concurrency = candidate.live_concurrency.saturating_add(1);
                candidate.rpm.used = candidate.rpm.used.saturating_add(1);
            }
            FabricEvent::RequestSucceeded {
                latency,
                usage,
                cost_micros,
                ..
            } => {
                candidate.live_concurrency = candidate.live_concurrency.saturating_sub(1);

                let tokens_used = (usage.input_tokens as u64) + (usage.output_tokens as u64);
                candidate.tpm.used = candidate.tpm.used.saturating_add(tokens_used);
                candidate.daily.used = candidate.daily.used.saturating_add(tokens_used);
                candidate.monthly.used = candidate.monthly.used.saturating_add(tokens_used);

                // Update EWMA latency: ewma_alpha * latency_ms + (1 - ewma_alpha) * old
                let latency_ms = latency.as_secs_f64() * 1000.0;
                candidate.ewma_latency_ms = candidate.ewma_alpha * latency_ms
                    + (1.0 - candidate.ewma_alpha) * candidate.ewma_latency_ms;

                // Append ledger entry if cost is known.
                if let Some(cost_micros) = cost_micros {
                    self.ledger.push(LedgerEntry {
                        candidate: candidate_key.clone(),
                        at: now,
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        cost_micros,
                    });
                }

                // Reset failure count and close breaker if it was half-open.
                candidate.breaker.failure_count = 0;
                if candidate.breaker.state == BreakerState::HalfOpen {
                    candidate.breaker.state = BreakerState::Closed;
                }
            }
            FabricEvent::RequestFailed {
                counts_against_breaker,
                ..
            } => {
                candidate.live_concurrency = candidate.live_concurrency.saturating_sub(1);

                if counts_against_breaker {
                    candidate.breaker.failure_count =
                        candidate.breaker.failure_count.saturating_add(1);

                    if candidate.breaker.failure_count >= candidate.breaker.failure_threshold {
                        let retry_at =
                            now.plus_millis(candidate.breaker.cooldown.as_millis() as i64);
                        candidate.breaker.state = BreakerState::Open { retry_at };
                    }
                }
            }
            FabricEvent::ProbeSucceeded { .. } => {
                candidate.breaker.state = BreakerState::Closed;
                candidate.breaker.failure_count = 0;
            }
        }
    }

    /// Roll every window whose period has elapsed back to zero, and flip `Open` breakers whose
    /// cooldown has elapsed to `HalfOpen`. Called at the start of `apply` and by [`crate::route`]
    /// before reading state, so routing decisions never see stale counters.
    ///
    /// RPM/TPM windows reset every 60s, daily every 24h, monthly every 30 * 24h (calendar
    /// months are out of scope — a fixed 30-day period is the documented approximation). A window
    /// resets when `now.seconds_since(window.window_start) >= period`; on reset set
    /// `used = 0, window_start = now`. Separately, for any candidate whose breaker is
    /// `Open { retry_at }` with `now >= retry_at`, transition to `HalfOpen` (this does not reset
    /// `failure_count`, which only clears on `ProbeSucceeded` or a fresh `RequestSucceeded`).
    pub fn roll_windows(&mut self, now: Timestamp) {
        const RPM_PERIOD_SECS: i64 = 60;
        const TPM_PERIOD_SECS: i64 = 60;
        const DAILY_PERIOD_SECS: i64 = 24 * 60 * 60;
        const MONTHLY_PERIOD_SECS: i64 = 30 * 24 * 60 * 60;

        for candidate in self.candidates.values_mut() {
            // Roll rpm window
            if now.seconds_since(candidate.rpm.window_start) >= RPM_PERIOD_SECS {
                candidate.rpm = Window {
                    used: 0,
                    window_start: now,
                };
            }

            // Roll tpm window
            if now.seconds_since(candidate.tpm.window_start) >= TPM_PERIOD_SECS {
                candidate.tpm = Window {
                    used: 0,
                    window_start: now,
                };
            }

            // Roll daily window
            if now.seconds_since(candidate.daily.window_start) >= DAILY_PERIOD_SECS {
                candidate.daily = Window {
                    used: 0,
                    window_start: now,
                };
            }

            // Roll monthly window
            if now.seconds_since(candidate.monthly.window_start) >= MONTHLY_PERIOD_SECS {
                candidate.monthly = Window {
                    used: 0,
                    window_start: now,
                };
            }

            // Flip Open breakers to HalfOpen if cooldown has elapsed
            if let BreakerState::Open { retry_at } = candidate.breaker.state {
                if now >= retry_at {
                    candidate.breaker.state = BreakerState::HalfOpen;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tm_types::Timestamp;

    fn new_state() -> FabricState {
        FabricState::new()
    }

    fn test_candidate() -> CandidateKey {
        ModelId::new("test-provider", "test-model")
    }

    fn alternative_candidate() -> CandidateKey {
        ModelId::new("alt-provider", "alt-model")
    }

    #[test]
    fn register_creates_fresh_candidate_state() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        let cstate = state.candidate(&candidate).unwrap();
        assert_eq!(cstate.rpm.used, 0);
        assert_eq!(cstate.rpm.window_start, now);
        assert_eq!(cstate.tpm.used, 0);
        assert_eq!(cstate.daily.used, 0);
        assert_eq!(cstate.monthly.used, 0);
        assert_eq!(cstate.live_concurrency, 0);
        assert_eq!(cstate.breaker.state, BreakerState::Closed);
        assert_eq!(cstate.breaker.failure_count, 0);
        assert_eq!(cstate.ewma_latency_ms, 0.0);
        assert_eq!(cstate.ewma_alpha, 0.1);
    }

    #[test]
    fn register_is_idempotent() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );
        let first = state.candidate(&candidate).unwrap().clone();

        // Register again at a later time with different parameters.
        let later = now.plus_seconds(100);
        state.register(
            candidate.clone(),
            later,
            5,
            Duration::from_secs(20),
            Duration::from_secs(60),
            0.2,
        );
        let second = state.candidate(&candidate).unwrap().clone();

        // State must not change (idempotent).
        assert_eq!(first, second);
    }

    #[test]
    fn request_started_increments_live_concurrency_and_rpm() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate.clone(),
            },
            now,
        );

        let cstate = state.candidate(&candidate).unwrap();
        assert_eq!(cstate.live_concurrency, 1);
        assert_eq!(cstate.rpm.used, 1);
    }

    #[test]
    fn request_succeeded_decrements_live_concurrency_and_updates_windows() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        // Start a request to bump live_concurrency.
        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate.clone(),
            },
            now,
        );

        let usage = crate::types::Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };

        // Complete the request successfully.
        state.apply(
            FabricEvent::RequestSucceeded {
                candidate: candidate.clone(),
                latency: Duration::from_millis(150),
                usage,
                cost_micros: Some(1000),
            },
            now,
        );

        let cstate = state.candidate(&candidate).unwrap();
        assert_eq!(cstate.live_concurrency, 0);
        assert_eq!(cstate.tpm.used, 150);
        assert_eq!(cstate.daily.used, 150);
        assert_eq!(cstate.monthly.used, 150);
        assert_eq!(cstate.breaker.failure_count, 0);
        assert_eq!(cstate.breaker.state, BreakerState::Closed);

        // Check EWMA update: 0.1 * 150.0 + 0.9 * 0.0 = 15.0.
        assert!((cstate.ewma_latency_ms - 15.0).abs() < 0.01);

        // Check ledger entry was appended.
        assert_eq!(state.ledger().len(), 1);
        assert_eq!(state.ledger()[0].input_tokens, 100);
        assert_eq!(state.ledger()[0].output_tokens, 50);
        assert_eq!(state.ledger()[0].cost_micros, 1000);
    }

    #[test]
    fn request_succeeded_without_cost_does_not_append_ledger() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate.clone(),
            },
            now,
        );

        let usage = crate::types::Usage {
            input_tokens: 50,
            output_tokens: 25,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };

        state.apply(
            FabricEvent::RequestSucceeded {
                candidate: candidate.clone(),
                latency: Duration::from_millis(100),
                usage,
                cost_micros: None,
            },
            now,
        );

        assert_eq!(state.ledger().len(), 0);
    }

    #[test]
    fn request_succeeded_updates_ewma_latency() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.2,
        );

        let usage = crate::types::Usage::default();

        // First request: 100ms latency.
        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate.clone(),
            },
            now,
        );
        state.apply(
            FabricEvent::RequestSucceeded {
                candidate: candidate.clone(),
                latency: Duration::from_millis(100),
                usage,
                cost_micros: None,
            },
            now,
        );

        let ewma1 = state.candidate(&candidate).unwrap().ewma_latency_ms;
        assert!((ewma1 - 20.0).abs() < 0.01); // 0.2 * 100 + 0.8 * 0 = 20

        // Second request: 200ms latency.
        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate.clone(),
            },
            now,
        );
        state.apply(
            FabricEvent::RequestSucceeded {
                candidate: candidate.clone(),
                latency: Duration::from_millis(200),
                usage,
                cost_micros: None,
            },
            now,
        );

        let ewma2 = state.candidate(&candidate).unwrap().ewma_latency_ms;
        // 0.2 * 200 + 0.8 * 20 = 40 + 16 = 56
        assert!((ewma2 - 56.0).abs() < 0.01);
    }

    #[test]
    fn request_failed_with_breaker_increments_failure_count() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate.clone(),
            },
            now,
        );

        state.apply(
            FabricEvent::RequestFailed {
                candidate: candidate.clone(),
                counts_against_breaker: true,
            },
            now,
        );

        let cstate = state.candidate(&candidate).unwrap();
        assert_eq!(cstate.live_concurrency, 0);
        assert_eq!(cstate.breaker.failure_count, 1);
        assert_eq!(cstate.breaker.state, BreakerState::Closed);
    }

    #[test]
    fn request_failed_without_breaker_does_not_increment_failure_count() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate.clone(),
            },
            now,
        );

        state.apply(
            FabricEvent::RequestFailed {
                candidate: candidate.clone(),
                counts_against_breaker: false,
            },
            now,
        );

        let cstate = state.candidate(&candidate).unwrap();
        assert_eq!(cstate.breaker.failure_count, 0);
    }

    #[test]
    fn circuit_breaker_opens_after_threshold() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            2,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        // First failure.
        state.apply(
            FabricEvent::RequestFailed {
                candidate: candidate.clone(),
                counts_against_breaker: true,
            },
            now,
        );
        assert_eq!(
            state.candidate(&candidate).unwrap().breaker.state,
            BreakerState::Closed
        );

        // Second failure opens the breaker.
        state.apply(
            FabricEvent::RequestFailed {
                candidate: candidate.clone(),
                counts_against_breaker: true,
            },
            now,
        );
        let cstate = state.candidate(&candidate).unwrap();
        assert_eq!(cstate.breaker.failure_count, 2);
        match cstate.breaker.state {
            BreakerState::Open { retry_at } => {
                assert_eq!(retry_at, now.plus_millis(30_000));
            }
            _ => panic!("Expected Open state"),
        }
    }

    #[test]
    fn probe_succeeded_closes_breaker_and_resets_failure_count() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            2,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        // Trip the breaker.
        state.apply(
            FabricEvent::RequestFailed {
                candidate: candidate.clone(),
                counts_against_breaker: true,
            },
            now,
        );
        state.apply(
            FabricEvent::RequestFailed {
                candidate: candidate.clone(),
                counts_against_breaker: true,
            },
            now,
        );

        assert!(matches!(
            state.candidate(&candidate).unwrap().breaker.state,
            BreakerState::Open { .. }
        ));

        // Manually transition to HalfOpen (would normally happen via roll_windows).
        {
            let cstate = &mut state.candidates.get_mut(&candidate).unwrap();
            cstate.breaker.state = BreakerState::HalfOpen;
        }

        // ProbeSucceeded closes the breaker.
        state.apply(
            FabricEvent::ProbeSucceeded {
                candidate: candidate.clone(),
            },
            now,
        );

        let cstate = state.candidate(&candidate).unwrap();
        assert_eq!(cstate.breaker.state, BreakerState::Closed);
        assert_eq!(cstate.breaker.failure_count, 0);
    }

    #[test]
    fn roll_windows_resets_elapsed_rpm_window() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        // Bump rpm.
        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate.clone(),
            },
            now,
        );

        assert_eq!(state.candidate(&candidate).unwrap().rpm.used, 1);

        // Advance time past the rpm window (60 seconds).
        let later = now.plus_seconds(61);
        state.roll_windows(later);

        let cstate = state.candidate(&candidate).unwrap();
        assert_eq!(cstate.rpm.used, 0);
        assert_eq!(cstate.rpm.window_start, later);
    }

    #[test]
    fn roll_windows_resets_elapsed_tpm_window() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        // Bump tpm.
        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate.clone(),
            },
            now,
        );
        state.apply(
            FabricEvent::RequestSucceeded {
                candidate: candidate.clone(),
                latency: Duration::from_millis(100),
                usage: crate::types::Usage {
                    input_tokens: 50,
                    output_tokens: 25,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                cost_micros: None,
            },
            now,
        );

        assert_eq!(state.candidate(&candidate).unwrap().tpm.used, 75);

        // Advance time past the tpm window (60 seconds).
        let later = now.plus_seconds(61);
        state.roll_windows(later);

        let cstate = state.candidate(&candidate).unwrap();
        assert_eq!(cstate.tpm.used, 0);
        assert_eq!(cstate.tpm.window_start, later);
    }

    #[test]
    fn roll_windows_resets_elapsed_daily_window() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        // Bump daily.
        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate.clone(),
            },
            now,
        );
        state.apply(
            FabricEvent::RequestSucceeded {
                candidate: candidate.clone(),
                latency: Duration::from_millis(100),
                usage: crate::types::Usage {
                    input_tokens: 1000,
                    output_tokens: 500,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                cost_micros: None,
            },
            now,
        );

        assert_eq!(state.candidate(&candidate).unwrap().daily.used, 1500);

        // Advance time past the daily window (86400 seconds = 24 hours).
        let later = now.plus_seconds(86401);
        state.roll_windows(later);

        let cstate = state.candidate(&candidate).unwrap();
        assert_eq!(cstate.daily.used, 0);
        assert_eq!(cstate.daily.window_start, later);
    }

    #[test]
    fn roll_windows_resets_elapsed_monthly_window() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        // Bump monthly.
        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate.clone(),
            },
            now,
        );
        state.apply(
            FabricEvent::RequestSucceeded {
                candidate: candidate.clone(),
                latency: Duration::from_millis(100),
                usage: crate::types::Usage {
                    input_tokens: 10000,
                    output_tokens: 5000,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                cost_micros: None,
            },
            now,
        );

        assert_eq!(state.candidate(&candidate).unwrap().monthly.used, 15000);

        // Advance time past the monthly window (30 * 24 * 60 * 60 = 2592000 seconds).
        let later = now.plus_seconds(2592001);
        state.roll_windows(later);

        let cstate = state.candidate(&candidate).unwrap();
        assert_eq!(cstate.monthly.used, 0);
        assert_eq!(cstate.monthly.window_start, later);
    }

    #[test]
    fn roll_windows_flips_open_breaker_to_half_open() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            1,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        // Trip the breaker.
        state.apply(
            FabricEvent::RequestFailed {
                candidate: candidate.clone(),
                counts_against_breaker: true,
            },
            now,
        );

        assert!(matches!(
            state.candidate(&candidate).unwrap().breaker.state,
            BreakerState::Open { .. }
        ));

        // Advance time past the cooldown (30 seconds).
        let later = now.plus_seconds(31);
        state.roll_windows(later);

        assert_eq!(
            state.candidate(&candidate).unwrap().breaker.state,
            BreakerState::HalfOpen
        );
    }

    #[test]
    fn roll_windows_does_not_reset_recent_windows() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate.clone(),
            },
            now,
        );

        // Advance time by a small amount (less than 60 seconds).
        let later = now.plus_seconds(30);
        state.roll_windows(later);

        // rpm window should not reset.
        assert_eq!(state.candidate(&candidate).unwrap().rpm.used, 1);
        assert_eq!(state.candidate(&candidate).unwrap().rpm.window_start, now);
    }

    #[test]
    fn apply_event_for_unregistered_candidate_no_ops_with_warning() {
        let mut state = new_state();
        let unregistered = test_candidate();
        let now = Timestamp::EPOCH;

        // This should not panic and should log a warning, but the test doesn't verify logging.
        state.apply(
            FabricEvent::RequestStarted {
                candidate: unregistered.clone(),
            },
            now,
        );

        // State should be unchanged.
        assert!(state.candidate(&unregistered).is_none());
        assert_eq!(state.ledger().len(), 0);
    }

    #[test]
    fn multiple_candidates_are_independent() {
        let mut state = new_state();
        let candidate1 = test_candidate();
        let candidate2 = alternative_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate1.clone(),
            now,
            2,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );
        state.register(
            candidate2.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.15,
        );

        // Bump candidate1.
        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate1.clone(),
            },
            now,
        );
        state.apply(
            FabricEvent::RequestFailed {
                candidate: candidate1.clone(),
                counts_against_breaker: true,
            },
            now,
        );

        // Bump candidate2.
        state.apply(
            FabricEvent::RequestStarted {
                candidate: candidate2.clone(),
            },
            now,
        );

        let cs1 = state.candidate(&candidate1).unwrap();
        let cs2 = state.candidate(&candidate2).unwrap();

        assert_eq!(cs1.breaker.failure_count, 1);
        assert_eq!(cs2.breaker.failure_count, 0);
        assert_eq!(cs1.live_concurrency, 0);
        assert_eq!(cs2.live_concurrency, 1);
    }

    #[test]
    fn saturating_arithmetic_on_live_concurrency() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            3,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        // Try to decrement live_concurrency below zero.
        state.apply(
            FabricEvent::RequestFailed {
                candidate: candidate.clone(),
                counts_against_breaker: false,
            },
            now,
        );

        // Should saturate at zero, not panic or wrap.
        assert_eq!(state.candidate(&candidate).unwrap().live_concurrency, 0);
    }

    #[test]
    fn successful_half_open_probe_closes_breaker() {
        let mut state = new_state();
        let candidate = test_candidate();
        let now = Timestamp::EPOCH;

        state.register(
            candidate.clone(),
            now,
            1,
            Duration::from_secs(10),
            Duration::from_secs(30),
            0.1,
        );

        // Trip the breaker with a failure.
        state.apply(
            FabricEvent::RequestFailed {
                candidate: candidate.clone(),
                counts_against_breaker: true,
            },
            now,
        );

        // Manually set to HalfOpen (would be done by roll_windows in reality).
        {
            let cs = &mut state.candidates.get_mut(&candidate).unwrap();
            cs.breaker.state = BreakerState::HalfOpen;
        }

        // Probe succeeds.
        state.apply(
            FabricEvent::RequestSucceeded {
                candidate: candidate.clone(),
                latency: Duration::from_millis(50),
                usage: crate::types::Usage::default(),
                cost_micros: None,
            },
            now,
        );

        let cstate = state.candidate(&candidate).unwrap();
        assert_eq!(cstate.breaker.state, BreakerState::Closed);
        assert_eq!(cstate.breaker.failure_count, 0);
    }
}
