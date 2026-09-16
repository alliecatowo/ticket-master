#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Genesis: turn a tiny prompt into a running project, assimilating existing repositories.
//!
//! Genesis orchestrates a sequence of stages that transform a user's natural-language request into
//! a complete, executable project. Each stage builds on prior artifacts and decisions, preserving
//! the chain of reasoning from the original prompt through compilation, ignition, and maturity
//! checks.

pub mod seed;
pub mod vision;
pub mod spec;
pub mod compile;
pub mod ignition;
pub mod maturity;
pub mod stages;
pub mod attach;

pub use seed::{Assumption, Question, Seed};
pub use vision::Vision;
pub use spec::Specification;
pub use compile::GraphCompilation;
pub use ignition::IgnitionPolicy;
pub use maturity::MaturityGateResult;
pub use stages::{Stage, StageEvent, GenesisState, GenesisDriver};
