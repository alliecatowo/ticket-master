//! Ticketmaster Harness: the project's software for operating on itself.
//!
//! `harness.toml` is versioned project state — tool preferences, search routing weights,
//! context compilation policy, role→capability mapping, verification defaults, prompt
//! fragments, command handling and editing behavior ([`config`]) — the same as `SPEC.md` and
//! `harness.toml` are project artifacts, not framework code, and they evolve under the same
//! ticket workflow as everything else the project produces.
//!
//! The safety property that makes self-improvement unmysterious: a session pins the harness
//! epoch it starts with and never observes a later epoch mid-session, even if `harness.toml` is
//! edited and a new epoch is promoted while the session is running ([`epoch`]). Improvement is
//! measured, not asserted — [`metrics`] records what actually happened per ticket and session,
//! [`bench`] replays repository-local tasks deterministically against a seeded mock provider to
//! score a candidate epoch, and promotion can be gated on a benchmark gain. [`efficacy`] bounds
//! how much of the scheduler's capacity harness work itself may consume, so self-improvement
//! stays subordinate to the project it serves.
//!
//! See `SPEC.md` §10.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bench;
pub mod config;
pub mod efficacy;
pub mod epoch;
pub mod metrics;

pub use bench::{
    compare, BenchFixture, BenchRunner, BenchTask, BenchmarkReport, ExpectedOutcome,
    PromotionReport, ScoringSpec, SeededProvider, TaskResult,
};
pub use config::{
    CommandPolicy, ConfigError, ContextPolicy, EditingPolicy, HarnessConfig, PromptFragments,
    RoleMapping, RoutingWeights, ToolPreferences, VerificationDefaults,
};
pub use efficacy::{EfficacyAccount, EfficacyBudget, EfficacyError};
pub use epoch::{
    EpochRegistry, EpochResolutionError, HarnessEpoch, PromotionDecision, PromotionGate,
    PromotionOutcome, SessionPin,
};
pub use metrics::{AggregateMetrics, EpochComparison, SessionMetrics, TicketMetrics};
