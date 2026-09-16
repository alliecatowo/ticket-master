//! The Vision artifact: the product's taste and identity, fixed before any architecture exists.
//!
//! [`compile_vision`] is the only function in this module that touches a provider, and it runs
//! under `Role::VisionFrontier` (`vision.frontier`) deliberately — this is a judgment call about
//! what the project *should feel like*, not a mechanical extraction, and the spec explicitly
//! reserves frontier-tier judgment for it. Everything else here is plain data plus pure
//! accessors, so the shape of a Vision is unit-testable without a provider at all.

use serde::{Deserialize, Serialize};
use tm_types::{ArtifactId, Clock, Result as TmResult, Timestamp};

use crate::seed::Seed;

/// The Vision artifact compiled from a [`Seed`] under `vision.frontier`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vision {
    /// The artifact id of the [`Seed`] this vision was compiled from, once persisted. `None`
    /// until the caller has stored the seed and can fill this in.
    pub source_seed: Option<ArtifactId>,
    /// What the project is *for*, and why it should exist at all.
    pub product_thesis: String,
    /// What using the finished product should feel like.
    pub user_experience: String,
    /// The aesthetic and quality judgments that don't reduce to a requirement: taste.
    pub taste: String,
    /// Constraints the vision itself imposes (distinct from [`Seed::explicit_constraints`] /
    /// `inferred_constraints`, which come from the prompt) — things architecture must respect to
    /// stay true to this vision.
    pub governing_constraints: Vec<String>,
    /// The project's identity: what it is, in one paragraph a stranger could repeat back.
    pub identity: String,
    /// Explicitly out of scope, so later stages don't have to rediscover this by omission.
    pub non_goals: Vec<String>,
    /// The architectural character this vision implies (e.g. "boring and auditable" vs.
    /// "exploratory and fast-moving") — read by [`crate::spec`] when compiling architecture.
    pub architectural_character: String,
    /// Things that would satisfy every explicit requirement while still betraying the vision —
    /// "technically valid but spiritually wrong". Exists so later reviewers have a named list to
    /// check proposals against, not just a feeling.
    pub spiritually_wrong: Vec<String>,
    /// When this vision was compiled.
    pub created: Timestamp,
}

impl Vision {
    /// True once `source_seed` has been filled in after persistence.
    pub fn is_anchored(&self) -> bool {
        self.source_seed.is_some()
    }
}

/// Compile a [`Vision`] from `seed` under `Role::VisionFrontier`.
///
// IMPL: builds a single `CompletionRequest` (see `tm_provider::types`) whose prompt embeds
// `seed.raw_prompt` plus its explicit/inferred constraints and assumptions, and asks the model to
// produce prose for each `Vision` field plus JSON lists for `governing_constraints`, `non_goals`
// and `spiritually_wrong`. Route it through `provider.complete` with `req.role ==
// Role::VisionFrontier` (frontier judgment, per `SPEC.md` §12 — do not substitute a cheaper
// role). Parse the structured portion of the response as JSON; treat any missing required field
// as `TmError::parse`, not a silently empty string, since a Vision with a blank `identity` would
// silently propagate into every later stage. `source_seed` is left `None` here; the caller sets
// it once the returned `Seed` has actually been persisted as an artifact (this function has no
// access to a `Store`, keeping it a pure provider round-trip). `clock` stamps `created`.
// Errors: `TmError::Provider` on completion failure, `TmError::Parse` on malformed/incomplete
// output.
pub async fn compile_vision(
    seed: &Seed,
    provider: &dyn tm_provider::Provider,
    clock: &dyn Clock,
) -> TmResult<Vision> {
    let _ = (seed, provider, clock);
    todo!("compile_vision: VisionFrontier round-trip producing every Vision field, see module IMPL note")
}
