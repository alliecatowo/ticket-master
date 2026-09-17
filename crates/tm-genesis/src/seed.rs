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
use tm_provider::types::{ContentBlock, Message, MessageRole};
use tm_types::{Clock, Result as TmResult, Timestamp, TmError};

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
        Constraint {
            text: text.into(),
            rationale: rationale.into(),
        }
    }
}

/// A reasonable default Genesis adopted instead of asking a human, so it can proceed. Recorded
/// as a `genesis.assumption_recorded` event; cheap to supersede later by recording a new
/// [`Assumption`] (or a full [`tm_core::Decision`]) rather than editing this one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        Assumption {
            text: text.into(),
            rationale: rationale.into(),
            confidence,
        }
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
        Question {
            text: text.into(),
            blocking,
            rationale: rationale.into(),
            resolution: None,
        }
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
        self.unresolved_questions
            .iter()
            .filter(|q| q.blocking && q.is_open())
            .collect()
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
    // Build a request for JSON extraction under SummarizerCheap role.
    let system = Some(
        "You are an expert project analyst. Extract and infer structured metadata from a user's prompt. \
         Return a JSON object with this exact structure (all fields required, arrays may be empty):\n\
         {\n  \"explicit_constraints\": [{\"text\": \"...\", \"rationale\": \"\"}],\n  \
         \"inferred_constraints\": [{\"text\": \"...\", \"rationale\": \"...\"}],\n  \
         \"assumptions\": [{\"text\": \"...\", \"rationale\": \"...\", \"confidence\": 0.8}],\n  \
         \"unresolved_questions\": [{\"text\": \"...\", \"blocking\": true, \"rationale\": \"...\", \"resolution\": null}]\n\
         }\n\n\
         Explicit constraints are stated directly in the prompt. \
         Inferred constraints come from domain conventions or implied expectations. \
         Assumptions are reasonable defaults you adopt instead of asking. \
         Questions with blocking=true should be rare—only when no reasonable assumption exists. \
         Questions with blocking=false should instead become assumptions for bias to action."
            .into()
    );

    let user_message = Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: format!(
                "Analyze this project prompt and extract all constraints, assumptions, and questions:\n\n{}",
                raw_prompt
            ),
        }],
    };

    let req = tm_provider::types::CompletionRequest {
        system,
        messages: vec![user_message],
        tools: vec![],
        max_tokens: 2048,
        temperature: Some(0.2),
        stop_sequences: vec![],
        stream: false,
        n: 1,
    };

    // Call the provider with SummarizerCheap role.
    let completion = provider
        .complete(req)
        .await
        .map_err(|e| TmError::Provider(e.to_string()))?;

    // Extract text from the first candidate.
    let response_text = completion
        .candidates
        .first()
        .and_then(|c| {
            c.content.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .ok_or_else(|| TmError::Parse("completion has no text content".into()))?;

    // Parse the JSON response. Use a helper to extract JSON from potential markdown blocks.
    let json_text = extract_json_block(&response_text)
        .or_else(|| Some(response_text.clone()))
        .ok_or_else(|| TmError::Parse("no JSON found in response".into()))?;

    #[derive(Deserialize)]
    struct ParsedResponse {
        explicit_constraints: Option<Vec<Constraint>>,
        inferred_constraints: Option<Vec<Constraint>>,
        assumptions: Option<Vec<Assumption>>,
        unresolved_questions: Option<Vec<Question>>,
    }

    let parsed: ParsedResponse = serde_json::from_str(&json_text)?;

    let explicit_constraints = parsed.explicit_constraints.unwrap_or_default();
    let inferred_constraints = parsed.inferred_constraints.unwrap_or_default();
    let mut assumptions = parsed.assumptions.unwrap_or_default();
    let unresolved_questions = parsed.unresolved_questions.unwrap_or_default();

    // Bias to action: convert trivial or non-blocking questions to assumptions.
    let inferred_constraint_texts: std::collections::HashSet<_> = inferred_constraints
        .iter()
        .map(|c| c.text.to_lowercase())
        .collect();
    let explicit_constraint_texts: std::collections::HashSet<_> = explicit_constraints
        .iter()
        .map(|c| c.text.to_lowercase())
        .collect();

    let mut truly_blocking_questions = Vec::new();
    for question in unresolved_questions {
        if !question.blocking {
            // Non-blocking questions become assumptions.
            assumptions.push(Assumption {
                text: question.text,
                rationale: question.rationale,
                confidence: 0.5,
            });
        } else if is_trivial_question(
            &question,
            &inferred_constraint_texts,
            &explicit_constraint_texts,
        ) {
            // Trivial blocking questions become assumptions.
            assumptions.push(Assumption {
                text: question.text,
                rationale: format!(
                    "Treated as assumption instead of blocking: {}",
                    question.rationale
                ),
                confidence: 0.7,
            });
        } else {
            // Only truly non-trivial blocking questions remain.
            truly_blocking_questions.push(question);
        }
    }

    Ok(Seed {
        raw_prompt,
        explicit_constraints,
        inferred_constraints,
        assumptions,
        unresolved_questions: truly_blocking_questions,
        created: clock.now(),
    })
}

/// Extract a JSON block from a markdown code block or return the text as-is if it looks like JSON.
fn extract_json_block(text: &str) -> Option<String> {
    let trimmed = text.trim();
    // Try to find a ```json...``` block.
    if let Some(start) = trimmed.find("```json") {
        if let Some(end) = trimmed[start + 7..].find("```") {
            return Some(trimmed[start + 7..start + 7 + end].trim().to_string());
        }
    }
    // Try to find any ```...``` block.
    if let Some(start) = trimmed.find("```") {
        if let Some(end) = trimmed[start + 3..].find("```") {
            return Some(trimmed[start + 3..start + 3 + end].trim().to_string());
        }
    }
    // If it starts with { and ends with }, it's probably JSON.
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed.to_string());
    }
    None
}

/// Check if a question is trivially answerable by comparing its text to known constraints.
fn is_trivial_question(
    question: &Question,
    inferred_texts: &std::collections::HashSet<String>,
    explicit_texts: &std::collections::HashSet<String>,
) -> bool {
    let q_lower = question.text.to_lowercase();
    // Words longer than 3 characters, so common stopwords ("is", "it", "the") don't cause
    // false positives; a shared distinctive word is enough to call the question a duplicate.
    let words = |s: &str| -> std::collections::HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 3)
            .map(str::to_string)
            .collect()
    };
    let q_words = words(&q_lower);
    for text in inferred_texts.iter().chain(explicit_texts.iter()) {
        if !q_words.is_disjoint(&words(text)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::FixedClock;

    #[test]
    fn constraint_new_builds_constraint() {
        let c = Constraint::new("must be fast", "latency requirement");
        assert_eq!(c.text, "must be fast");
        assert_eq!(c.rationale, "latency requirement");
    }

    #[test]
    fn constraint_new_accepts_empty_rationale() {
        let c = Constraint::new("explicit constraint", "");
        assert_eq!(c.text, "explicit constraint");
        assert_eq!(c.rationale, "");
    }

    #[test]
    fn assumption_new_builds_assumption() {
        let a = Assumption::new("Rust project", "common for performance-critical work", 0.8);
        assert_eq!(a.text, "Rust project");
        assert_eq!(a.rationale, "common for performance-critical work");
        assert_eq!(a.confidence, 0.8);
    }

    #[test]
    fn assumption_new_accepts_any_confidence() {
        let a = Assumption::new("text", "reason", 0.0);
        assert_eq!(a.confidence, 0.0);
        let a = Assumption::new("text", "reason", 1.0);
        assert_eq!(a.confidence, 1.0);
    }

    #[test]
    fn question_new_builds_open_question() {
        let q = Question::new(
            "Should we use async?",
            true,
            "parallelism preference unclear",
        );
        assert_eq!(q.text, "Should we use async?");
        assert!(q.blocking);
        assert_eq!(q.rationale, "parallelism preference unclear");
        assert_eq!(q.resolution, None);
    }

    #[test]
    fn question_is_open_when_unresolved() {
        let q = Question::new("text", true, "reason");
        assert!(q.is_open());
    }

    #[test]
    fn question_is_not_open_when_resolved() {
        let mut q = Question::new("text", true, "reason");
        q.resolution = Some("answer".into());
        assert!(!q.is_open());
    }

    #[test]
    fn seed_new_creates_empty_seed() {
        let clock = FixedClock::epoch();
        let seed = Seed::new("my prompt".into(), &clock);
        assert_eq!(seed.raw_prompt, "my prompt");
        assert!(seed.explicit_constraints.is_empty());
        assert!(seed.inferred_constraints.is_empty());
        assert!(seed.assumptions.is_empty());
        assert!(seed.unresolved_questions.is_empty());
        assert_eq!(seed.created, clock.now());
    }

    #[test]
    fn seed_blocking_open_questions_filters_correctly() {
        let mut seed = Seed::new("prompt".into(), &FixedClock::epoch());
        let q1 = Question::new("blocking open", true, "reason");
        let mut q2 = Question::new("blocking resolved", true, "reason");
        q2.resolution = Some("answer".into());
        let q3 = Question::new("non-blocking open", false, "reason");

        seed.unresolved_questions = vec![q1, q2, q3];

        let blocking_open = seed.blocking_open_questions();
        assert_eq!(blocking_open.len(), 1);
        assert_eq!(blocking_open[0].text, "blocking open");
    }

    #[test]
    fn seed_is_unblocked_when_no_blocking_open_questions() {
        let mut seed = Seed::new("prompt".into(), &FixedClock::epoch());
        assert!(seed.is_unblocked());

        let mut q = Question::new("blocking", true, "reason");
        seed.unresolved_questions = vec![q.clone()];
        assert!(!seed.is_unblocked());

        q.resolution = Some("answer".into());
        seed.unresolved_questions = vec![q];
        assert!(seed.is_unblocked());
    }

    #[test]
    fn extract_json_block_finds_json_code_block() {
        let input = "Here is the JSON:\n```json\n{\"key\": \"value\"}\n```";
        let result = extract_json_block(input);
        assert_eq!(result, Some("{\"key\": \"value\"}".to_string()));
    }

    #[test]
    fn extract_json_block_finds_generic_code_block() {
        let input = "Result:\n```\n{\"key\": \"value\"}\n```";
        let result = extract_json_block(input);
        assert_eq!(result, Some("{\"key\": \"value\"}".to_string()));
    }

    #[test]
    fn extract_json_block_handles_raw_json() {
        let input = "{\"key\": \"value\"}";
        let result = extract_json_block(input);
        assert_eq!(result, Some("{\"key\": \"value\"}".to_string()));
    }

    #[test]
    fn extract_json_block_returns_none_for_non_json() {
        let input = "This is just text";
        let result = extract_json_block(input);
        assert_eq!(result, None);
    }

    #[test]
    fn is_trivial_question_detects_duplicate_constraints() {
        let question = Question::new("Is it offline?", true, "unclear");
        let mut inferred = std::collections::HashSet::new();
        inferred.insert("must run fully offline".to_lowercase());

        let explicit = std::collections::HashSet::new();

        // Should be trivial because question is a substring of constraint.
        assert!(is_trivial_question(&question, &inferred, &explicit));
    }

    #[test]
    fn is_trivial_question_allows_distinct_questions() {
        let question = Question::new("Should we use async?", true, "unclear");
        let inferred = std::collections::HashSet::new();
        let explicit = std::collections::HashSet::new();

        assert!(!is_trivial_question(&question, &inferred, &explicit));
    }

    #[test]
    fn extract_json_handles_whitespace() {
        let input = "```json\n\n{\"key\": \"value\"}\n\n```";
        let result = extract_json_block(input);
        assert_eq!(result, Some("{\"key\": \"value\"}".to_string()));
    }

    #[test]
    fn extract_json_first_match_wins() {
        let input = "```json\n{\"a\": 1}\n``` and ```json\n{\"b\": 2}\n```";
        let result = extract_json_block(input);
        assert_eq!(result, Some("{\"a\": 1}".to_string()));
    }
}
