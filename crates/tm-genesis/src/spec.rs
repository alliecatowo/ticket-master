//! The Specification artifact: the Vision made concrete enough to compile into a graph.
//!
//! [`compile_spec`] runs under `Role::ArchitectFrontier` (`architect.frontier`) — turning taste
//! and identity into requirements, architecture, interfaces, a data model, technology choices, a
//! quality bar, a security model, a testing strategy, milestones, and the definitions of v0 and
//! v1 is itself a frontier judgment, not mechanical derivation. [`crate::compile`] is the next
//! stage down, which turns a `Specification` into an actual ticket graph.

use serde::{Deserialize, Serialize};
use tm_types::{ArtifactId, Clock, Predicate, Result as TmResult, Timestamp};

use crate::vision::Vision;

/// One functional or non-functional requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    /// Stable short id, e.g. `"R1"`, unique within the specification.
    pub id: String,
    /// The requirement, in prose.
    pub text: String,
    /// Relative priority; lower sorts first. Ties are legal.
    pub priority: i32,
}

/// One interface the system exposes or consumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceSpec {
    /// Interface name, e.g. `"CLI"`, `"HTTP API"`, `"tm-core::Store"`.
    pub name: String,
    /// What it does and who calls it.
    pub description: String,
}

/// One technology decision the spec pins down, so `GraphCompilation` doesn't have to re-derive
/// it and later reviewers can see why it was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TechnologyChoice {
    /// The area this choice covers, e.g. `"language"`, `"storage"`, `"transport"`.
    pub area: String,
    /// The choice itself.
    pub choice: String,
    /// Why, in prose.
    pub rationale: String,
}

/// One milestone the spec anticipates, coarse enough that `GraphCompilation` can flesh it out
/// into real tickets rather than invent milestone boundaries from scratch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MilestoneOutline {
    /// Milestone title.
    pub title: String,
    /// What closing this milestone means.
    pub objective: String,
    /// A hint at the scope of work it covers, in prose (not a ticket list — `GraphCompilation`
    /// owns turning this into actual tickets).
    pub scope_hint: String,
}

/// What "done" means for one release line (v0 or v1): an objective plus machine- or
/// judgment-checkable exit criteria.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseDefinition {
    /// One-line objective for this release.
    pub objective: String,
    /// Conditions that must hold for this release to be considered reached. Reuses
    /// `tm_types::Predicate` so the same predicate machinery `tm-core` verification already
    /// understands can evaluate these directly.
    pub exit_criteria: Vec<Predicate>,
}

/// The Specification artifact compiled from a [`Vision`] under `architect.frontier`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Specification {
    /// The artifact id of the [`Vision`] this spec was compiled from, once persisted.
    pub source_vision: Option<ArtifactId>,
    /// Every requirement, functional and non-functional.
    pub requirements: Vec<Requirement>,
    /// The architecture, in prose (component breakdown, data/control flow, key decisions).
    pub architecture: String,
    /// Every interface the system exposes or consumes.
    pub interfaces: Vec<InterfaceSpec>,
    /// The data model, in prose (entities, relationships, persistence shape).
    pub data_model: String,
    /// Pinned technology choices.
    pub technology_choices: Vec<TechnologyChoice>,
    /// The quality bar this project holds itself to, in prose.
    pub quality_bar: String,
    /// The security model: trust boundaries, threat model, mitigations.
    pub security_model: String,
    /// How the project verifies its own work, in prose (feeds `VerificationPolicy` choices in
    /// [`crate::compile`]).
    pub testing_strategy: String,
    /// Anticipated milestone breakdown.
    pub milestones: Vec<MilestoneOutline>,
    /// What v0 means and how it's checked.
    pub v0: ReleaseDefinition,
    /// What v1 means and how it's checked.
    pub v1: ReleaseDefinition,
    /// When this specification was compiled.
    pub created: Timestamp,
}

/// Compile a [`Specification`] from `vision` under `Role::ArchitectFrontier`.
///
// IMPL: single `CompletionRequest` with `role = Role::ArchitectFrontier`, prompt embedding every
// `Vision` field (especially `architectural_character`, `governing_constraints` and
// `spiritually_wrong`, since those directly bound acceptable architecture choices) and asking for
// structured JSON matching this module's types, including a nonempty `v0`/`v1` pair whose
// `exit_criteria` are expressed as `tm_types::Predicate` (prefer `Predicate::TestsPass` /
// `Predicate::CommandSucceeds` / `Predicate::FileExists` where the model can name something
// concrete; fall back to `Predicate::Judgment` only when a criterion is inherently a judgment
// call). Reject (return `TmError::parse`) a response whose `v0`/`v1` share no exit criteria at
// all with `requirements`, since that would mean the release definitions are disconnected from
// the requirements they're supposed to gate. `source_vision` is left `None`, filled in by the
// caller once the `Vision` is persisted (mirrors `vision::compile_vision`). `clock` stamps
// `created`.
// Errors: `TmError::Provider` on completion failure, `TmError::Parse` on malformed/incomplete/
// disconnected output.
pub async fn compile_spec(
    vision: &Vision,
    provider: &dyn tm_provider::Provider,
    clock: &dyn Clock,
) -> TmResult<Specification> {
    let _ = (vision, provider, clock);
    todo!("compile_spec: ArchitectFrontier round-trip producing every Specification field including v0/v1, see module IMPL note")
}
