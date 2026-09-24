//! The `DecisionProvider` trait: System One decision backends (D-020).
//!
//! A decider answers typed questions about a bounded `state` — "is this a question or a job",
//! "which tool family", "is this command risky" — with a calibrated, schema-valid answer. It
//! never generates text, which is why it is a separate trait from [`crate::fabric::Provider`]
//! rather than another method on it: forcing `complete`/`embed` stubs on a decider, or a `decide`
//! stub on every LLM-backed provider, would put lies in the trait.
//!
//! Shapes here follow the sketch in `docs/vision/system-one-decisions.md` §4 (adopting
//! TypeSafe's `/v1/systemone` wire contract, not inventing one) and
//! `docs/decisions/D-020-system-one-decision-providers.md`. `decide` returns
//! [`crate::types::ProviderError`], the same error type every other provider call uses, so a
//! decider reuses the fabric's breakers and retry policy instead of a parallel error path.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::types::{ModelId, ProviderError};

/// The caller-chosen id a [`Question`] is keyed by in a [`DecideRequest`], and the id its
/// [`Answer`] comes back under in the matching [`DecideResponse`].
pub type QuestionId = String;

/// One option offered to a [`Question::Choice`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionSpec {
    /// The label returned in [`Answer::value`] when this option is chosen.
    pub label: String,
}

/// One level of a [`Question::Score`]'s ordinal rubric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelSpec {
    /// The label for this level, e.g. `"low"`, `"medium"`, `"high"`.
    pub label: String,
}

/// A typed question posed to a decider, alongside the `state` it should be answered against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// Pick one of a closed set of labeled options (Jev allows up to 255).
    Choice {
        /// The question text.
        instructions: String,
        /// The candidate options, in the order presented to the decider.
        options: Vec<OptionSpec>,
    },
    /// An ordinal rubric of 2 to 10 levels, returning a probability per level.
    Score {
        /// The question text.
        instructions: String,
        /// The rubric's levels, from lowest to highest.
        levels: Vec<LevelSpec>,
    },
    /// P(statement is true): a single true/false judgment about the state.
    Noul {
        /// The statement being judged.
        statement: String,
    },
}

/// The typed value of one [`Answer`], matching the [`Question`] variant it answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnswerValue {
    /// The chosen [`OptionSpec::label`], for a [`Question::Choice`].
    Choice(String),
    /// The chosen [`LevelSpec::label`], for a [`Question::Score`].
    Score(String),
    /// Whether the statement is true, for a [`Question::Noul`].
    Noul(bool),
}

/// A request to a [`DecisionProvider`]: which model to answer with, the bounded `state`, and one
/// or more typed [`Question`]s about it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecideRequest {
    /// The decider model to answer with.
    pub model: ModelId,
    /// The text or JSON state the questions are asked about, e.g. a ticket objective or a
    /// pending tool call. Already redacted by the time it reaches a remote backend — see
    /// `redact_decide_request` (D-020 decision 7).
    pub state: String,
    /// The questions to answer, keyed by [`QuestionId`] so a multi-question request's answers
    /// can be matched back up. A `BTreeMap` for a stable iteration order and a stable request
    /// hash.
    pub questions: std::collections::BTreeMap<QuestionId, Question>,
}

/// One answer to a [`Question`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    /// The typed answer, matching the question's variant.
    pub value: AnswerValue,
    /// Per-option (or per-level) probabilities, in the same order as the question's
    /// `options`/`levels`. Empty for a `Noul` question, which reports its probability through
    /// `confidence` instead.
    pub probabilities: Vec<f32>,
    /// The decider's calibrated confidence in `value`, in `[0.0, 1.0]`.
    pub confidence: f32,
}

/// A [`DecisionProvider`]'s reply to a [`DecideRequest`]: one [`Answer`] per question id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecideResponse {
    /// The model that actually answered (may carry a more specific revision than the request's
    /// `model`, e.g. request `"jev-latest"` versus response `"jev-1.13.0"`).
    pub model: ModelId,
    /// Answers, keyed by the same question id the request used.
    pub answers: std::collections::BTreeMap<QuestionId, Answer>,
}

/// A kind of [`Question`] a [`DecisionProvider`] can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// [`Question::Choice`].
    Choice,
    /// [`Question::Score`].
    Score,
    /// [`Question::Noul`].
    Noul,
}

/// What a [`DecisionProvider`] can accept, reported so the fabric can skip a candidate whose
/// limits can't hold a given request — the same way it skips an unhealthy candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecideLimits {
    /// The largest `state` this decider can accept, in tokens.
    pub max_context_tokens: u32,
    /// The most options a single `Choice` question can offer.
    pub max_options: u32,
    /// The [`QuestionKind`]s this decider supports.
    pub kinds: Vec<QuestionKind>,
}

/// A System One decision backend: answers bounded, typed questions with a calibrated,
/// schema-valid response instead of generating text. See the module docs and D-020.
#[async_trait]
pub trait DecisionProvider: Send + Sync {
    /// The provider slug this implementation registers under, e.g. `"laya-local"`, `"typesafe"`,
    /// `"mock"`.
    fn id(&self) -> &str;

    /// What this decider can accept.
    fn limits(&self) -> DecideLimits;

    /// Answer `req`'s questions.
    async fn decide(&self, req: DecideRequest) -> Result<DecideResponse, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_choice_round_trips_through_json() {
        let q = Question::Choice {
            instructions: "which tool family?".into(),
            options: vec![
                OptionSpec { label: "fs".into() },
                OptionSpec {
                    label: "shell".into(),
                },
            ],
        };
        let json = serde_json::to_string(&q).unwrap();
        let back: Question = serde_json::from_str(&json).unwrap();
        assert_eq!(q, back);
    }

    #[test]
    fn decide_request_round_trips_through_json() {
        let mut questions = std::collections::BTreeMap::new();
        questions.insert(
            "risk".to_string(),
            Question::Score {
                instructions: "how risky is this command?".into(),
                levels: vec![
                    LevelSpec {
                        label: "low".into(),
                    },
                    LevelSpec {
                        label: "high".into(),
                    },
                ],
            },
        );
        let req = DecideRequest {
            model: ModelId::new("mock", "decider"),
            state: "rm -rf /tmp/scratch".into(),
            questions,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: DecideRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn noul_answer_round_trips_through_json() {
        let answer = Answer {
            value: AnswerValue::Noul(true),
            probabilities: vec![],
            confidence: 0.93,
        };
        let json = serde_json::to_string(&answer).unwrap();
        let back: Answer = serde_json::from_str(&json).unwrap();
        assert_eq!(answer, back);
    }
}
