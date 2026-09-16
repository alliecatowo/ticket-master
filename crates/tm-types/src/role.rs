//! Capability roles. Tickets request roles; the provider fabric maps roles to models.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// A capability a unit of work needs, independent of any particular model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Genesis vision: what is this thing.
    VisionFrontier,
    /// Decomposition and graph compilation.
    PlannerFrontier,
    /// Architecture and specification.
    ArchitectFrontier,
    /// Implementation where judgment matters.
    CoderDeep,
    /// Implementation of well-specified work.
    CoderFast,
    /// Cheap repository exploration.
    ExplorerCheap,
    /// Semantic review of a change.
    ReviewerSemantic,
    /// Does the change satisfy the intent.
    AuditorSemantic,
    /// Long-context synthesis.
    SynthesizerLongContext,
    /// Cheap summarization.
    SummarizerCheap,
    /// Text embedding.
    Embedder,
    /// Driving a computer or browser.
    ComputerUse,
}

impl Role {
    /// Every role, in a stable order.
    pub const ALL: [Role; 12] = [
        Role::VisionFrontier,
        Role::PlannerFrontier,
        Role::ArchitectFrontier,
        Role::CoderDeep,
        Role::CoderFast,
        Role::ExplorerCheap,
        Role::ReviewerSemantic,
        Role::AuditorSemantic,
        Role::SynthesizerLongContext,
        Role::SummarizerCheap,
        Role::Embedder,
        Role::ComputerUse,
    ];

    /// The dotted name used in configuration and events, e.g. `coder.fast`.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::VisionFrontier => "vision.frontier",
            Role::PlannerFrontier => "planner.frontier",
            Role::ArchitectFrontier => "architect.frontier",
            Role::CoderDeep => "coder.deep",
            Role::CoderFast => "coder.fast",
            Role::ExplorerCheap => "explorer.cheap",
            Role::ReviewerSemantic => "reviewer.semantic",
            Role::AuditorSemantic => "auditor.semantic",
            Role::SynthesizerLongContext => "synthesizer.long_context",
            Role::SummarizerCheap => "summarizer.cheap",
            Role::Embedder => "embedder",
            Role::ComputerUse => "computer_use",
        }
    }

    /// The configuration key form, e.g. `coder_fast`.
    pub fn config_key(self) -> String {
        self.as_str().replace('.', "_")
    }

    /// True for roles where frontier judgment is the point and degrading is a mistake.
    pub fn is_frontier(self) -> bool {
        matches!(
            self,
            Role::VisionFrontier
                | Role::PlannerFrontier
                | Role::ArchitectFrontier
                | Role::AuditorSemantic
        )
    }

    /// The default fallback tolerance for this role.
    pub fn default_tolerance(self) -> Tolerance {
        match self {
            r if r.is_frontier() => Tolerance::Strict,
            Role::CoderDeep | Role::ReviewerSemantic | Role::Embedder => Tolerance::Preferred,
            _ => Tolerance::Any,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = crate::error::TmError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let norm = s
            .replace('_', ".")
            .replace("long.context", "long_context")
            .replace("computer.use", "computer_use");
        Role::ALL
            .iter()
            .copied()
            .find(|r| r.as_str() == norm || r.as_str() == s || r.config_key() == s)
            .ok_or_else(|| crate::error::TmError::parse(format!("unknown role: {s}")))
    }
}

/// How willing a request is to be served by something other than its first choice.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Tolerance {
    /// Wait for the right capability rather than degrade.
    Strict,
    /// Degrade at most one tier.
    #[default]
    Preferred,
    /// Take whatever is available.
    Any,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_names_round_trip() {
        for r in Role::ALL {
            assert_eq!(Role::from_str(r.as_str()).unwrap(), r);
            assert_eq!(Role::from_str(&r.config_key()).unwrap(), r);
        }
        assert!(Role::from_str("nonsense").is_err());
    }

    #[test]
    fn frontier_roles_do_not_degrade_by_default() {
        assert_eq!(Role::VisionFrontier.default_tolerance(), Tolerance::Strict);
        assert_eq!(Role::AuditorSemantic.default_tolerance(), Tolerance::Strict);
        assert_eq!(Role::SummarizerCheap.default_tolerance(), Tolerance::Any);
        assert_eq!(Role::CoderDeep.default_tolerance(), Tolerance::Preferred);
    }

    #[test]
    fn config_keys_are_underscored() {
        assert_eq!(Role::CoderFast.config_key(), "coder_fast");
        assert_eq!(
            Role::SynthesizerLongContext.config_key(),
            "synthesizer_long_context"
        );
    }
}
