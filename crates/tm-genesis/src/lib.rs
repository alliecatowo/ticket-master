#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Genesis: turn a tiny prompt into a running project, assimilating existing repositories.
//!
//! Genesis orchestrates a sequence of stages that transform a user's natural-language request into
//! a complete, executable project. Each stage builds on prior artifacts and decisions, preserving
//! the chain of reasoning from the original prompt through compilation, ignition, and maturity
//! checks.

pub mod attach;
pub mod compile;
pub mod fixtures;
pub mod ignition;
pub mod maturity;
pub mod seed;
pub mod spec;
pub mod stages;
pub mod vision;

pub use compile::GraphCompilation;
pub use fixtures::{graph_compilation, maturity, offline_sequence, seed, spec, vision};
pub use ignition::IgnitionPolicy;
pub use maturity::MaturityGateResult;
pub use seed::{Assumption, Question, Seed};
pub use spec::Specification;
pub use stages::{GenesisDriver, GenesisState, Stage, StageEvent};
pub use vision::Vision;
