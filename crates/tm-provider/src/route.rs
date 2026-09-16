//! The pure router.
//!
//! `route()` decides, given only [`crate::role_config::RoleTable`], [`crate::state::FabricState`]
//! and a request, which candidate (if any) should serve a role right now. It performs no I/O and
//! reads no clock — `now` is a parameter — so it is exhaustively unit-testable. Quota exhaustion,
//! an open breaker, or a request that must wait are all ordinary outputs ([`RouteDecision`]), not
//! errors.

use std::time::Duration;
use tm_types::{Role, Timestamp, Tolerance};

use crate::role_config::{RoleCandidate, RoleTable};
use crate::state::{BreakerState, CandidateState, FabricState};
use crate::types::ModelId;

/// What a request needs beyond its role: an estimate of load, for admission checks that depend
/// on size (tpm headroom, price ceilings).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Need {
    /// How willing this request is to be served by a degraded candidate rather than wait.
    pub tolerance: Tolerance,
    /// Estimated total tokens (input + output) this request will consume, for tpm admission.
    pub estimated_tokens: u32,
    /// A ceiling on cost in micro-dollars this request will accept, if any.
    pub max_cost_micros: Option<u64>,
}

/// The outcome of routing a role at a point in time.
#[derive(Debug, Clone, PartialEq)]
pub enum RouteDecision {
    /// Serve with this candidate at its normal (non-degraded) tier.
    Use(ModelId),
    /// Serve with this candidate, but it is a lower tier than the role's primary; `reason`
    /// explains why (e.g. `"primary at capacity"`, `"primary breaker open"`).
    Degrade(ModelId, String),
    /// No candidate is admissible right now, but at least one will free up by `until`; the
    /// caller should retry then. Chosen instead of `Degrade` when `tolerance` is `Strict`.
    Wait(Timestamp),
    /// No candidate can serve this role at all (role has no candidates, or every candidate is
    /// permanently over a hard cap such as the monthly ceiling with no reset before `Wait` would
    /// help).
    Exhausted,
}

/// Route `role` to a candidate given the current `table` and `state`, as of `now`.
pub fn route(
    table: &RoleTable,
    state: &FabricState,
    role: Role,
    need: &Need,
    now: Timestamp,
) -> RouteDecision {
    let candidates = table.candidates_for(role);
    if candidates.is_empty() {
        return RouteDecision::Exhausted;
    }

    let admissions: Vec<Admission> = candidates
        .iter()
        .map(|c| {
            let key = ModelId::new(c.provider.clone(), c.model.clone());
            admit(c, state.candidate(&key), need, now)
        })
        .collect();

    if matches!(admissions[0], Admission::Admit) {
        let primary = &candidates[0];
        return RouteDecision::Use(ModelId::new(
            primary.provider.clone(),
            primary.model.clone(),
        ));
    }

    if need.tolerance != Tolerance::Strict {
        for (candidate, admission) in candidates.iter().zip(admissions.iter()).skip(1) {
            if matches!(admission, Admission::Admit) {
                let reason = match &admissions[0] {
                    Admission::Blocked(_, reason) => (*reason).to_string(),
                    Admission::PermanentlyBlocked(reason) => (*reason).to_string(),
                    Admission::Admit => unreachable!("primary Admit already returned above"),
                };
                return RouteDecision::Degrade(
                    ModelId::new(candidate.provider.clone(), candidate.model.clone()),
                    reason,
                );
            }
        }
    }

    if admissions
        .iter()
        .all(|a| matches!(a, Admission::PermanentlyBlocked(_)))
    {
        return RouteDecision::Exhausted;
    }

    let earliest_retry = admissions
        .iter()
        .filter_map(|a| match a {
            Admission::Blocked(retry_at, _) => Some(*retry_at),
            _ => None,
        })
        .min();

    match earliest_retry {
        Some(t) => RouteDecision::Wait(t),
        None => RouteDecision::Exhausted,
    }
}

/// One candidate's admissibility at `now`.
#[derive(Debug, Clone, PartialEq)]
enum Admission {
    /// May be used right now.
    Admit,
    /// Not usable right now, but will be by `Timestamp` (a window reset or breaker cooldown).
    Blocked(Timestamp, &'static str),
    /// Not usable and no future time in this state will fix it without new state (e.g. the
    /// monthly cap is already spent and cannot reset within this routing horizon).
    PermanentlyBlocked(&'static str),
}

/// Check one candidate against health, quota, concurrency and price.
fn admit(
    candidate: &RoleCandidate,
    state: Option<&CandidateState>,
    need: &Need,
    now: Timestamp,
) -> Admission {
    let state = match state {
        Some(s) => s,
        None => return Admission::PermanentlyBlocked("not registered"),
    };

    if let BreakerState::Open { retry_at } = state.breaker.state {
        return Admission::Blocked(retry_at, "breaker open");
    }

    if state.live_concurrency >= candidate.max_concurrency {
        return Admission::Blocked(
            now.plus_seconds(CONCURRENCY_RETRY_HINT.as_secs() as i64),
            "concurrency limit reached",
        );
    }

    if let Some(cap) = candidate.limits.requests_per_minute {
        if state.rpm.used >= cap as u64 {
            return Admission::Blocked(state.rpm.window_start.plus_seconds(60), "rpm exhausted");
        }
    }

    if let Some(cap) = candidate.limits.tokens_per_minute {
        if state.tpm.used + need.estimated_tokens as u64 > cap as u64 {
            return Admission::Blocked(state.tpm.window_start.plus_seconds(60), "tpm exhausted");
        }
    }

    if let Some(cap) = candidate.limits.requests_per_day {
        if state.daily.used >= cap as u64 {
            return Admission::Blocked(
                state.daily.window_start.plus_seconds(24 * 60 * 60),
                "daily cap reached",
            );
        }
    }

    if let Some(cap) = candidate.limits.requests_per_month {
        if state.monthly.used >= cap as u64 {
            return Admission::Blocked(
                state.monthly.window_start.plus_seconds(30 * 24 * 60 * 60),
                "monthly cap reached",
            );
        }
    }

    if let (Some(ceiling), Some(price)) = (need.max_cost_micros, candidate.price) {
        let estimated_cost = need.estimated_tokens as u64 * price.output_micros_per_token;
        if estimated_cost > ceiling {
            return Admission::PermanentlyBlocked("over price ceiling");
        }
    }

    Admission::Admit
}

/// Nominal poll-again horizon used when a resource (concurrency slot) has no principled reset
/// time.
const CONCURRENCY_RETRY_HINT: Duration = Duration::from_secs(1);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FabricEvent;
    use std::time::Duration as StdDuration;
    use tm_types::Role;

    fn need(tolerance: Tolerance) -> Need {
        Need {
            tolerance,
            estimated_tokens: 100,
            max_cost_micros: None,
        }
    }

    fn rpm_limited_table() -> RoleTable {
        RoleTable::parse(
            r#"
            [coder_fast]
            candidates = [
              { provider = "a", model = "strong", max_concurrency = 5, limits = { requests_per_minute = 1 } },
              { provider = "a", model = "cheap", max_concurrency = 10, degraded_ok = true },
            ]
            "#,
        )
        .expect("valid providers.toml fixture")
    }

    #[test]
    fn exhausted_primary_falls_back_to_degraded_candidate_under_any_tolerance() {
        let table = rpm_limited_table();
        let now = Timestamp::from_unix_seconds(1_000);
        let mut state = FabricState::new();
        let strong = ModelId::new("a", "strong");
        let cheap = ModelId::new("a", "cheap");
        state.register(
            strong.clone(),
            now,
            3,
            StdDuration::from_secs(60),
            StdDuration::from_secs(30),
            0.5,
        );
        state.register(
            cheap.clone(),
            now,
            3,
            StdDuration::from_secs(60),
            StdDuration::from_secs(30),
            0.5,
        );
        state.apply(
            FabricEvent::RequestStarted {
                candidate: strong.clone(),
            },
            now,
        );

        let decision = route(&table, &state, Role::CoderFast, &need(Tolerance::Any), now);
        match decision {
            RouteDecision::Degrade(model, reason) => {
                assert_eq!(model, cheap);
                assert_eq!(reason, "rpm exhausted");
            }
            other => panic!("expected Degrade, got {other:?}"),
        }
    }

    #[test]
    fn strict_tolerance_waits_instead_of_degrading() {
        let table = rpm_limited_table();
        let now = Timestamp::from_unix_seconds(1_000);
        let mut state = FabricState::new();
        let strong = ModelId::new("a", "strong");
        let cheap = ModelId::new("a", "cheap");
        state.register(
            strong.clone(),
            now,
            3,
            StdDuration::from_secs(60),
            StdDuration::from_secs(30),
            0.5,
        );
        state.register(
            cheap.clone(),
            now,
            3,
            StdDuration::from_secs(60),
            StdDuration::from_secs(30),
            0.5,
        );
        state.apply(FabricEvent::RequestStarted { candidate: strong }, now);

        let decision = route(
            &table,
            &state,
            Role::CoderFast,
            &need(Tolerance::Strict),
            now,
        );
        assert_eq!(decision, RouteDecision::Wait(now.plus_seconds(60)));
    }

    #[test]
    fn open_breaker_is_skipped_in_favor_of_next_candidate() {
        let table = RoleTable::parse(
            r#"
            [coder_fast]
            candidates = [
              { provider = "a", model = "strong", max_concurrency = 5 },
              { provider = "a", model = "cheap", max_concurrency = 10, degraded_ok = true },
            ]
            "#,
        )
        .expect("valid providers.toml fixture");
        let now = Timestamp::from_unix_seconds(1_000);
        let mut state = FabricState::new();
        let strong = ModelId::new("a", "strong");
        let cheap = ModelId::new("a", "cheap");
        state.register(
            strong.clone(),
            now,
            1,
            StdDuration::from_secs(60),
            StdDuration::from_secs(30),
            0.5,
        );
        state.register(
            cheap.clone(),
            now,
            1,
            StdDuration::from_secs(60),
            StdDuration::from_secs(30),
            0.5,
        );
        state.apply(
            FabricEvent::RequestFailed {
                candidate: strong,
                counts_against_breaker: true,
            },
            now,
        );

        let decision = route(&table, &state, Role::CoderFast, &need(Tolerance::Any), now);
        match decision {
            RouteDecision::Degrade(model, reason) => {
                assert_eq!(model, cheap);
                assert_eq!(reason, "breaker open");
            }
            other => panic!("expected Degrade, got {other:?}"),
        }
    }

    #[test]
    fn all_candidates_unregistered_returns_exhausted() {
        let table = rpm_limited_table();
        let now = Timestamp::from_unix_seconds(1_000);
        let state = FabricState::new();

        let decision = route(&table, &state, Role::CoderFast, &need(Tolerance::Any), now);
        assert_eq!(decision, RouteDecision::Exhausted);
    }

    #[test]
    fn every_candidate_blocked_returns_wait_at_earliest_retry() {
        let table = RoleTable::parse(
            r#"
            [coder_fast]
            candidates = [
              { provider = "a", model = "strong", max_concurrency = 5, limits = { requests_per_minute = 1 } },
              { provider = "a", model = "cheap", max_concurrency = 10, limits = { requests_per_minute = 1 } },
            ]
            "#,
        )
        .expect("valid providers.toml fixture");
        let now = Timestamp::from_unix_seconds(1_000);
        let strong = ModelId::new("a", "strong");
        let cheap = ModelId::new("a", "cheap");
        let mut state = FabricState::new();
        // `cheap` registers earlier, so its rpm window (and thus its reset time) is earlier too.
        state.register(
            strong.clone(),
            now,
            3,
            StdDuration::from_secs(60),
            StdDuration::from_secs(30),
            0.5,
        );
        state.register(
            cheap.clone(),
            now.plus_seconds(-10),
            3,
            StdDuration::from_secs(60),
            StdDuration::from_secs(30),
            0.5,
        );
        state.apply(FabricEvent::RequestStarted { candidate: strong }, now);
        state.apply(FabricEvent::RequestStarted { candidate: cheap }, now);

        let decision = route(&table, &state, Role::CoderFast, &need(Tolerance::Any), now);
        assert_eq!(decision, RouteDecision::Wait(now.plus_seconds(50)));
    }

    #[test]
    fn role_absent_from_table_returns_exhausted() {
        let table = RoleTable::parse("").expect("empty providers.toml is valid");
        let now = Timestamp::from_unix_seconds(1_000);
        let state = FabricState::new();

        let decision = route(&table, &state, Role::CoderFast, &need(Tolerance::Any), now);
        assert_eq!(decision, RouteDecision::Exhausted);
    }
}
