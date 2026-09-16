//! The Seed: the raw human prompt, preserved verbatim forever, plus everything Genesis read
//! into or on top of it.
//!
//! `raw_prompt` is written once and never rewritten — every later Genesis artifact traces back
//! to it, so an auditor (human or agent) can always ask "why does this exist?" and land on the
//! original words. Bias to action: [`analyze_prompt`] turns the prompt into `explicit_`/
//! `inferred_constraints` and [`Assumption`]s wherever a reasonable default exists, and reserves
//! [`Question`] (with `blocking == true`) for the rare case where no reasonable assumption
//! exists at all. Non-blocking questions are not surfaced to a human; they are recorded as
//! assumptions instead, cheap to supersede later via `tm-core`'s [`tm_core::Decision`]
//! machinery.
//!
//! A `Seed` is persisted as project state (via `tm-core::Store::store_artifact`, referenced by
//! a `genesis.stage_completed` event) and never mutated in place; superseding an assumption
//! records a new [`Assumption`]/[`tm_core::Decision`], it does not edit history.

use serde::{Deserialize, Serialize};
use tm_types::{Clock, Result as TmResult, Timestamp};

/// One constraint pulled out of (or inferred from) the raw prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    /// The constraint, in prose, e.g. "must run fully offline".
    pub text: String,
    /// For an inferred constraint, why it was inferred (empty for explicit constraints, whose
    /// evidence is the raw prompt itself).
    pub rationale: String,
}

impl Constraint {
    /// Build a constraint. `rationale` may be empty for an explicit constraint quoted directly
    /// from the prompt.
    pub fn new(text: impl Into<String>, rationale: impl Into<String>) -> Self {
        Constraint { text: text.into(), rationale: rationale.into() }
    }
}

/// A reasonable default Genesis adopted instead of asking a human, so it can proceed. Recorded
/// as a `genesis.assumption_recorded` event; cheap to supersede later by recording a new
/// [`Assumption`] (or a full [`tm_core::Decision`]) rather than editing this one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assumption {
    /// The assumption, in prose, e.g. "target audience is developers, not end users".
    pub text: String,
    /// Why this was the reasonable default given the prompt and inferred constraints.
    pub rationale: String,
    /// How confident the analysis step was in this assumption, `0.0..=1.0`.
    pub confidence: f64,
}

impl Assumption {
    /// Build an assumption record.
    pub fn new(text: impl Into<String>, rationale: impl Into<String>, confidence: f64) -> Self {
        Assumption { text: text.into(), rationale: rationale.into(), confidence }
    }
}

/// Something Genesis could not resolve on its own. Only `blocking == true` questions are ever
/// surfaced to a human (`SPEC.md` §12: "a question is surfaced to the human only when no
/// reasonable assumption exists"); everything else should have become an [`Assumption`] instead
/// of a `Question` in the first place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// The question, in prose.
    pub text: String,
    /// True when no reasonable default exists and a human must answer before Genesis can
    /// proceed past the stage that raised it.
    pub blocking: bool,
    /// Why no reasonable assumption could be made (only meaningful when `blocking`; kept for
    /// non-blocking questions too, as a record of why they were flagged at all before being
    /// downgraded to an assumption elsewhere).
    pub rationale: String,
    /// The human's answer, once given. `None` while still open.
    pub resolution: Option<String>,
}

impl Question {
    /// Build an open (unresolved) question.
    pub fn new(text: impl Into<String>, blocking: bool, rationale: impl Into<String>) -> Self {
        Question { text: text.into(), blocking, rationale: rationale.into(), resolution: None }
    }

    /// True while `resolution` is unset.
    pub fn is_open(&self) -> bool {
        self.resolution.is_none()
    }
}

/// The Seed itself: `raw_prompt` verbatim, plus everything analysis produced from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Seed {
    /// The human's original prompt, byte-for-byte. Never rewritten after construction.
    pub raw_prompt: String,
    /// Constraints stated directly in the prompt.
    pub explicit_constraints: Vec<Constraint>,
    /// Constraints inferred from the prompt (domain conventions, typical expectations for the
    /// stated kind of project, etc.).
    pub inferred_constraints: Vec<Constraint>,
    /// Reasonable defaults adopted in place of asking a human.
    pub assumptions: Vec<Assumption>,
    /// Questions raised during analysis, blocking and non-blocking, open and resolved.
    pub unresolved_questions: Vec<Question>,
    /// When this Seed was created.
    pub created: Timestamp,
}

impl Seed {
    /// An empty Seed over `raw_prompt`, timestamped by `clock`. Analysis (see
    /// [`analyze_prompt`]) populates the rest.
    pub fn new(raw_prompt: String, clock: &dyn Clock) -> Self {
        Seed {
            raw_prompt,
            explicit_constraints: Vec::new(),
            inferred_constraints: Vec::new(),
            assumptions: Vec::new(),
            unresolved_questions: Vec::new(),
            created: clock.now(),
        }
    }

    /// Every question with `blocking == true` that is still open, in encounter order. Callers
    /// (the stage driver, `tm attach`) surface exactly these to a human and nothing else.
    pub fn blocking_open_questions(&self) -> Vec<&Question> {
        self.unresolved_questions.iter().filter(|q| q.blocking && q.is_open()).collect()
    }

    /// True once no blocking question remains open, i.e. Genesis may advance past `Seed`.
    pub fn is_unblocked(&self) -> bool {
        self.blocking_open_questions().is_empty()
    }
}

/// Analyze a raw prompt into a fully populated [`Seed`]: extract explicit constraints, infer
/// reasonable additional constraints and assumptions, and raise a [`Question`] (flagged
/// `blocking`) only where no reasonable default exists.
///
// IMPL: calls `provider.complete` once under `Role::SummarizerCheap` (this is cheap structural
// extraction, not a creative judgment, so it does not need a frontier role) with a prompt asking
// for a JSON object of `{explicit_constraints, inferred_constraints, assumptions,
// unresolved_questions}` matching this module's shapes; parse the completion's text content as
// JSON (`serde_json::from_str`), mapping a parse failure to `TmError::parse`. A completion that
// omits the JSON block entirely, or whose `blocking` questions read as trivially answerable
// (e.g. duplicate an already-inferred constraint), should bias toward folding them into
// `assumptions` rather than passing them through as `blocking` — "bias to action" is enforced
// here, not just documented. `clock` stamps `Seed::created`; never read the wall clock directly.
// Errors: `TmError::Provider` on completion failure, `TmError::Parse` on malformed JSON output.
pub async fn analyze_prompt(
    raw_prompt: String,
    provider: &dyn tm_provider::Provider,
    clock: &dyn Clock,
) -> TmResult<Seed> {
    let _ = (raw_prompt, provider, clock);
    todo!("analyze_prompt: SummarizerCheap extraction of constraints/assumptions/questions from raw_prompt, see module IMPL note")
}
