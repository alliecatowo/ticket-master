//! Operational metrics recorded per ticket and per session, plus aggregation and cross-epoch
//! comparison.
//!
//! Improvement is measured, not asserted: these are the numbers [`crate::bench`] benchmarks
//! against and [`crate::efficacy`] accounting draws on. Recording is the caller's job (`tm-agent`
//! observes a live session); this module owns only the shape of a record and the pure arithmetic
//! over it.

use serde::{Deserialize, Serialize};
use tm_types::{SessionId, TicketId, Timestamp};

/// One ticket's worth of operational metrics, recorded once the ticket's session-of-work ends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TicketMetrics {
    /// The ticket these metrics describe.
    pub ticket: TicketId,
    /// The session the work happened under.
    pub session: SessionId,
    /// The harness epoch [`crate::epoch::SessionPin`] fixed for that session.
    pub harness_epoch: u64,
    /// Total wall-clock time spent on the ticket.
    pub wall_seconds: u64,
    /// Tokens consumed as input across all model calls for this ticket.
    pub tokens_in: u64,
    /// Tokens produced as output across all model calls for this ticket.
    pub tokens_out: u64,
    /// Total spend, in micro-dollars.
    pub dollars_micros: u64,
    /// Total tool calls issued.
    pub tool_calls: u32,
    /// Number of search calls issued before the first one that surfaced a relevant hit (0 if the
    /// first search was relevant, or if no search was needed).
    pub searches_before_first_relevant_hit: u32,
    /// Number of verification predicate failures encountered.
    pub verification_failures: u32,
    /// Number of retries (re-leases, re-attempts) the ticket went through.
    pub retries: u32,
    /// Total bytes of context compiled for this ticket across all worker turns.
    pub context_bytes: u64,
    /// Number of shell commands that had to be re-run (failed then repeated verbatim or near-
    /// verbatim).
    pub commands_rerun: u32,
    /// Number of times a human had to intervene (approve, redirect, or manually fix).
    pub human_interventions: u32,
    /// When this record was recorded.
    pub recorded_at: Timestamp,
}

/// One session's metrics: identity plus the running totals over the tickets it touched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMetrics {
    /// The session these metrics describe.
    pub session: SessionId,
    /// The harness epoch this session pinned.
    pub harness_epoch: u64,
    /// When the session started.
    pub started_at: Timestamp,
    /// When the session ended, if it has.
    pub ended_at: Option<Timestamp>,
    /// Tickets this session touched, in the order their [`TicketMetrics`] were recorded.
    pub tickets: Vec<TicketId>,
    /// Running totals over `tickets`' [`TicketMetrics`].
    pub totals: AggregateMetrics,
}

/// Totals and derived rates over a set of [`TicketMetrics`], for one session or one epoch.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AggregateMetrics {
    /// Number of [`TicketMetrics`] folded into this aggregate.
    pub count: u32,
    /// Sum of `wall_seconds`.
    pub wall_seconds: u64,
    /// Sum of `tokens_in`.
    pub tokens_in: u64,
    /// Sum of `tokens_out`.
    pub tokens_out: u64,
    /// Sum of `dollars_micros`.
    pub dollars_micros: u64,
    /// Sum of `tool_calls`.
    pub tool_calls: u64,
    /// Sum of `searches_before_first_relevant_hit`.
    pub searches_before_first_relevant_hit: u64,
    /// Sum of `verification_failures`.
    pub verification_failures: u64,
    /// Sum of `retries`.
    pub retries: u64,
    /// Sum of `context_bytes`.
    pub context_bytes: u64,
    /// Sum of `commands_rerun`.
    pub commands_rerun: u64,
    /// Sum of `human_interventions`.
    pub human_interventions: u64,
}

impl AggregateMetrics {
    /// The all-zero aggregate, the identity element for [`AggregateMetrics::accumulate`].
    pub fn zero() -> AggregateMetrics {
        AggregateMetrics {
            count: 0,
            wall_seconds: 0,
            tokens_in: 0,
            tokens_out: 0,
            dollars_micros: 0,
            tool_calls: 0,
            searches_before_first_relevant_hit: 0,
            verification_failures: 0,
            retries: 0,
            context_bytes: 0,
            commands_rerun: 0,
            human_interventions: 0,
        }
    }

    /// Fold one [`TicketMetrics`] record into this aggregate in place.
    pub fn accumulate(&mut self, metrics: &TicketMetrics) {
        self.count = self.count.saturating_add(1);
        self.wall_seconds = self.wall_seconds.saturating_add(metrics.wall_seconds);
        self.tokens_in = self.tokens_in.saturating_add(metrics.tokens_in);
        self.tokens_out = self.tokens_out.saturating_add(metrics.tokens_out);
        self.dollars_micros = self.dollars_micros.saturating_add(metrics.dollars_micros);
        self.tool_calls = self.tool_calls.saturating_add(metrics.tool_calls as u64);
        self.searches_before_first_relevant_hit = self
            .searches_before_first_relevant_hit
            .saturating_add(metrics.searches_before_first_relevant_hit as u64);
        self.verification_failures = self
            .verification_failures
            .saturating_add(metrics.verification_failures as u64);
        self.retries = self.retries.saturating_add(metrics.retries as u64);
        self.context_bytes = self.context_bytes.saturating_add(metrics.context_bytes);
        self.commands_rerun = self
            .commands_rerun
            .saturating_add(metrics.commands_rerun as u64);
        self.human_interventions = self
            .human_interventions
            .saturating_add(metrics.human_interventions as u64);
    }

    /// Build an aggregate from a slice of [`TicketMetrics`] in one pass.
    pub fn from_tickets(tickets: &[TicketMetrics]) -> AggregateMetrics {
        let mut agg = AggregateMetrics::zero();
        for m in tickets {
            agg.accumulate(m);
        }
        agg
    }

    /// Mean searches-before-first-relevant-hit across the folded tickets, or `0.0` if `count` is
    /// zero.
    pub fn mean_searches_before_first_relevant_hit(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.searches_before_first_relevant_hit as f64 / self.count as f64
        }
    }

    /// Mean dollars spent per ticket, or `0.0` if `count` is zero.
    pub fn mean_dollars_micros(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.dollars_micros as f64 / self.count as f64
        }
    }
}

/// A comparison of two harness epochs' aggregate operational metrics, e.g. before/after a
/// promotion, independent of [`crate::bench::PromotionReport`]'s benchmark-task scoring.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EpochComparison {
    /// The baseline epoch's number.
    pub baseline_epoch: u64,
    /// The candidate epoch's number.
    pub candidate_epoch: u64,
    /// The baseline epoch's aggregate metrics.
    pub baseline: AggregateMetrics,
    /// The candidate epoch's aggregate metrics.
    pub candidate: AggregateMetrics,
}

impl EpochComparison {
    /// Build a comparison from two epochs' aggregates.
    pub fn new(
        baseline_epoch: u64,
        baseline: AggregateMetrics,
        candidate_epoch: u64,
        candidate: AggregateMetrics,
    ) -> EpochComparison {
        EpochComparison {
            baseline_epoch,
            candidate_epoch,
            baseline,
            candidate,
        }
    }

    /// `candidate.mean_dollars_micros() - baseline.mean_dollars_micros()`; negative is cheaper.
    pub fn mean_dollars_delta(&self) -> f64 {
        self.candidate.mean_dollars_micros() - self.baseline.mean_dollars_micros()
    }

    /// `candidate.mean_searches_before_first_relevant_hit() -
    /// baseline.mean_searches_before_first_relevant_hit()`; negative is more precise retrieval.
    pub fn mean_searches_delta(&self) -> f64 {
        self.candidate.mean_searches_before_first_relevant_hit()
            - self.baseline.mean_searches_before_first_relevant_hit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn sample_ticket_metrics(
        ticket: &str,
        session: &str,
        wall_seconds: u64,
        tokens_in: u64,
        tokens_out: u64,
        dollars_micros: u64,
        tool_calls: u32,
        searches_before_first_relevant_hit: u32,
        verification_failures: u32,
        retries: u32,
        context_bytes: u64,
        commands_rerun: u32,
        human_interventions: u32,
    ) -> TicketMetrics {
        TicketMetrics {
            ticket: TicketId::new(ticket).unwrap(),
            session: SessionId::new(session).unwrap(),
            harness_epoch: 1,
            wall_seconds,
            tokens_in,
            tokens_out,
            dollars_micros,
            tool_calls,
            searches_before_first_relevant_hit,
            verification_failures,
            retries,
            context_bytes,
            commands_rerun,
            human_interventions,
            recorded_at: Timestamp::EPOCH,
        }
    }

    #[test]
    fn accumulate_single_metric() {
        let mut agg = AggregateMetrics::zero();
        let metric =
            sample_ticket_metrics("T-1", "S-1", 100, 1000, 500, 5000, 10, 2, 1, 0, 2000, 3, 0);

        agg.accumulate(&metric);

        assert_eq!(agg.count, 1);
        assert_eq!(agg.wall_seconds, 100);
        assert_eq!(agg.tokens_in, 1000);
        assert_eq!(agg.tokens_out, 500);
        assert_eq!(agg.dollars_micros, 5000);
        assert_eq!(agg.tool_calls, 10);
        assert_eq!(agg.searches_before_first_relevant_hit, 2);
        assert_eq!(agg.verification_failures, 1);
        assert_eq!(agg.retries, 0);
        assert_eq!(agg.context_bytes, 2000);
        assert_eq!(agg.commands_rerun, 3);
        assert_eq!(agg.human_interventions, 0);
    }

    #[test]
    fn accumulate_multiple_metrics() {
        let mut agg = AggregateMetrics::zero();
        let metric1 =
            sample_ticket_metrics("T-1", "S-1", 100, 1000, 500, 5000, 10, 2, 1, 0, 2000, 3, 0);
        let metric2 =
            sample_ticket_metrics("T-2", "S-1", 50, 500, 250, 2500, 5, 1, 0, 1, 1000, 1, 2);

        agg.accumulate(&metric1);
        agg.accumulate(&metric2);

        assert_eq!(agg.count, 2);
        assert_eq!(agg.wall_seconds, 150);
        assert_eq!(agg.tokens_in, 1500);
        assert_eq!(agg.tokens_out, 750);
        assert_eq!(agg.dollars_micros, 7500);
        assert_eq!(agg.tool_calls, 15);
        assert_eq!(agg.searches_before_first_relevant_hit, 3);
        assert_eq!(agg.verification_failures, 1);
        assert_eq!(agg.retries, 1);
        assert_eq!(agg.context_bytes, 3000);
        assert_eq!(agg.commands_rerun, 4);
        assert_eq!(agg.human_interventions, 2);
    }

    #[test]
    fn accumulate_zero_metrics() {
        let mut agg = AggregateMetrics::zero();
        let metric = sample_ticket_metrics("T-1", "S-1", 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);

        agg.accumulate(&metric);

        assert_eq!(agg.count, 1);
        assert_eq!(agg.wall_seconds, 0);
        assert_eq!(agg.tokens_in, 0);
        assert_eq!(agg.tokens_out, 0);
        assert_eq!(agg.dollars_micros, 0);
        assert_eq!(agg.tool_calls, 0);
    }

    #[test]
    fn accumulate_saturation() {
        let mut agg = AggregateMetrics {
            count: u32::MAX,
            wall_seconds: u64::MAX,
            tokens_in: u64::MAX,
            tokens_out: u64::MAX,
            dollars_micros: u64::MAX,
            tool_calls: u64::MAX,
            searches_before_first_relevant_hit: u64::MAX,
            verification_failures: u64::MAX,
            retries: u64::MAX,
            context_bytes: u64::MAX,
            commands_rerun: u64::MAX,
            human_interventions: u64::MAX,
        };
        let metric =
            sample_ticket_metrics("T-1", "S-1", 100, 100, 100, 100, 10, 5, 3, 2, 100, 5, 3);

        agg.accumulate(&metric);

        // All fields should remain at max due to saturating_add
        assert_eq!(agg.count, u32::MAX);
        assert_eq!(agg.wall_seconds, u64::MAX);
        assert_eq!(agg.tokens_in, u64::MAX);
        assert_eq!(agg.tokens_out, u64::MAX);
        assert_eq!(agg.dollars_micros, u64::MAX);
        assert_eq!(agg.tool_calls, u64::MAX);
    }

    #[test]
    fn from_tickets_empty_slice() {
        let agg = AggregateMetrics::from_tickets(&[]);

        assert_eq!(agg, AggregateMetrics::zero());
    }

    #[test]
    fn from_tickets_single_metric() {
        let metric =
            sample_ticket_metrics("T-1", "S-1", 100, 1000, 500, 5000, 10, 2, 1, 0, 2000, 3, 0);
        let agg = AggregateMetrics::from_tickets(std::slice::from_ref(&metric));

        assert_eq!(agg.count, 1);
        assert_eq!(agg.wall_seconds, 100);
        assert_eq!(agg.tokens_in, 1000);
        assert_eq!(agg.tool_calls, 10);
    }

    #[test]
    fn from_tickets_multiple_metrics() {
        let metric1 =
            sample_ticket_metrics("T-1", "S-1", 100, 1000, 500, 5000, 10, 2, 1, 0, 2000, 3, 0);
        let metric2 =
            sample_ticket_metrics("T-2", "S-1", 50, 500, 250, 2500, 5, 1, 0, 1, 1000, 1, 2);
        let metric3 =
            sample_ticket_metrics("T-3", "S-1", 75, 750, 375, 3750, 8, 0, 2, 1, 1500, 2, 1);
        let agg = AggregateMetrics::from_tickets(&[metric1, metric2, metric3]);

        assert_eq!(agg.count, 3);
        assert_eq!(agg.wall_seconds, 225);
        assert_eq!(agg.tokens_in, 2250);
        assert_eq!(agg.tokens_out, 1125);
        assert_eq!(agg.dollars_micros, 11250);
        assert_eq!(agg.tool_calls, 23);
        assert_eq!(agg.searches_before_first_relevant_hit, 3);
        assert_eq!(agg.verification_failures, 3);
        assert_eq!(agg.retries, 2);
        assert_eq!(agg.context_bytes, 4500);
        assert_eq!(agg.commands_rerun, 6);
        assert_eq!(agg.human_interventions, 3);
    }

    #[test]
    fn mean_dollars_micros_zero_count() {
        let agg = AggregateMetrics::zero();

        let mean = agg.mean_dollars_micros();

        assert_eq!(mean, 0.0);
    }

    #[test]
    fn mean_dollars_micros_single_ticket() {
        let metric =
            sample_ticket_metrics("T-1", "S-1", 100, 1000, 500, 10000, 10, 2, 1, 0, 2000, 3, 0);
        let agg = AggregateMetrics::from_tickets(&[metric]);

        let mean = agg.mean_dollars_micros();

        assert_eq!(mean, 10000.0);
    }

    #[test]
    fn mean_dollars_micros_multiple_tickets() {
        let metric1 =
            sample_ticket_metrics("T-1", "S-1", 100, 1000, 500, 10000, 10, 2, 1, 0, 2000, 3, 0);
        let metric2 =
            sample_ticket_metrics("T-2", "S-1", 50, 500, 250, 20000, 5, 1, 0, 1, 1000, 1, 2);
        let agg = AggregateMetrics::from_tickets(&[metric1, metric2]);

        let mean = agg.mean_dollars_micros();

        assert_eq!(mean, 15000.0);
    }

    #[test]
    fn mean_searches_before_first_relevant_hit_zero_count() {
        let agg = AggregateMetrics::zero();

        let mean = agg.mean_searches_before_first_relevant_hit();

        assert_eq!(mean, 0.0);
    }

    #[test]
    fn mean_searches_before_first_relevant_hit_single_ticket() {
        let metric =
            sample_ticket_metrics("T-1", "S-1", 100, 1000, 500, 5000, 10, 5, 1, 0, 2000, 3, 0);
        let agg = AggregateMetrics::from_tickets(&[metric]);

        let mean = agg.mean_searches_before_first_relevant_hit();

        assert_eq!(mean, 5.0);
    }

    #[test]
    fn mean_searches_before_first_relevant_hit_multiple_tickets() {
        let metric1 =
            sample_ticket_metrics("T-1", "S-1", 100, 1000, 500, 5000, 10, 2, 1, 0, 2000, 3, 0);
        let metric2 =
            sample_ticket_metrics("T-2", "S-1", 50, 500, 250, 2500, 5, 8, 0, 1, 1000, 1, 2);
        let agg = AggregateMetrics::from_tickets(&[metric1, metric2]);

        let mean = agg.mean_searches_before_first_relevant_hit();

        assert_eq!(mean, 5.0);
    }

    #[test]
    fn epoch_comparison_mean_dollars_delta_improvement() {
        let baseline = AggregateMetrics {
            count: 2,
            dollars_micros: 20000,
            ..AggregateMetrics::zero()
        };
        let candidate = AggregateMetrics {
            count: 2,
            dollars_micros: 18000,
            ..AggregateMetrics::zero()
        };
        let comparison = EpochComparison::new(1, baseline, 2, candidate);

        let delta = comparison.mean_dollars_delta();

        assert_eq!(delta, -1000.0); // cheaper by 1000 microdollars per ticket
    }

    #[test]
    fn epoch_comparison_mean_dollars_delta_regression() {
        let baseline = AggregateMetrics {
            count: 2,
            dollars_micros: 20000,
            ..AggregateMetrics::zero()
        };
        let candidate = AggregateMetrics {
            count: 2,
            dollars_micros: 24000,
            ..AggregateMetrics::zero()
        };
        let comparison = EpochComparison::new(1, baseline, 2, candidate);

        let delta = comparison.mean_dollars_delta();

        assert_eq!(delta, 2000.0); // more expensive by 2000 microdollars per ticket
    }

    #[test]
    fn epoch_comparison_mean_searches_delta_improvement() {
        let baseline = AggregateMetrics {
            count: 3,
            searches_before_first_relevant_hit: 15,
            ..AggregateMetrics::zero()
        };
        let candidate = AggregateMetrics {
            count: 3,
            searches_before_first_relevant_hit: 12,
            ..AggregateMetrics::zero()
        };
        let comparison = EpochComparison::new(1, baseline, 2, candidate);

        let delta = comparison.mean_searches_delta();

        assert_eq!(delta, -1.0); // one fewer search per ticket
    }

    #[test]
    fn epoch_comparison_mean_searches_delta_regression() {
        let baseline = AggregateMetrics {
            count: 3,
            searches_before_first_relevant_hit: 12,
            ..AggregateMetrics::zero()
        };
        let candidate = AggregateMetrics {
            count: 3,
            searches_before_first_relevant_hit: 18,
            ..AggregateMetrics::zero()
        };
        let comparison = EpochComparison::new(1, baseline, 2, candidate);

        let delta = comparison.mean_searches_delta();

        assert_eq!(delta, 2.0); // two more searches per ticket
    }

    #[test]
    fn epoch_comparison_neutral_change() {
        let baseline = AggregateMetrics {
            count: 2,
            dollars_micros: 20000,
            searches_before_first_relevant_hit: 10,
            ..AggregateMetrics::zero()
        };
        let candidate = baseline;
        let comparison = EpochComparison::new(1, baseline, 2, candidate);

        assert_eq!(comparison.mean_dollars_delta(), 0.0);
        assert_eq!(comparison.mean_searches_delta(), 0.0);
    }

    #[test]
    fn aggregate_metrics_zero_is_identity() {
        let metric =
            sample_ticket_metrics("T-1", "S-1", 100, 1000, 500, 5000, 10, 2, 1, 0, 2000, 3, 0);
        let mut agg = AggregateMetrics::zero();
        agg.accumulate(&metric);

        let from_slice = AggregateMetrics::from_tickets(&[metric]);

        assert_eq!(agg, from_slice);
    }

    #[test]
    fn ticket_metrics_serialization() {
        let metric =
            sample_ticket_metrics("T-1", "S-1", 100, 1000, 500, 5000, 10, 2, 1, 0, 2000, 3, 0);

        let json = serde_json::to_string(&metric).unwrap();
        let deserialized: TicketMetrics = serde_json::from_str(&json).unwrap();

        assert_eq!(metric, deserialized);
    }

    #[test]
    fn aggregate_metrics_serialization() {
        let agg = AggregateMetrics {
            count: 3,
            wall_seconds: 225,
            tokens_in: 2250,
            tokens_out: 1125,
            dollars_micros: 11250,
            tool_calls: 23,
            searches_before_first_relevant_hit: 3,
            verification_failures: 3,
            retries: 2,
            context_bytes: 4500,
            commands_rerun: 6,
            human_interventions: 3,
        };

        let json = serde_json::to_string(&agg).unwrap();
        let deserialized: AggregateMetrics = serde_json::from_str(&json).unwrap();

        assert_eq!(agg, deserialized);
    }
}
