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
/// Builds a single `CompletionRequest` (see `tm_provider::types`) whose prompt embeds
/// `seed.raw_prompt` plus its explicit/inferred constraints and assumptions, and asks the model to
/// produce prose for each `Vision` field plus JSON lists for `governing_constraints`, `non_goals`
/// and `spiritually_wrong`. Route it through `provider.complete` with `req.role ==
/// Role::VisionFrontier` (frontier judgment, per `SPEC.md` §12 — do not substitute a cheaper
/// role). Parse the structured portion of the response as JSON; treat any missing required field
/// as `TmError::parse`, not a silently empty string, since a Vision with a blank `identity` would
/// silently propagate into every later stage. `source_seed` is left `None` here; the caller sets
/// it once the returned `Seed` has actually been persisted as an artifact (this function has no
/// access to a `Store`, keeping it a pure provider round-trip). `clock` stamps `created`.
///
/// # Errors
///
/// Returns `TmError::Provider` on completion failure, `TmError::Parse` on malformed/incomplete
/// output.
pub async fn compile_vision(
    seed: &Seed,
    provider: &dyn tm_provider::Provider,
    clock: &dyn Clock,
) -> TmResult<Vision> {
    // Build the prompt by embedding the seed's raw prompt, constraints, and assumptions.
    let mut prompt = format!(
        "You are compiling a Vision artifact for a new software project. \
The Vision expresses what the project IS and WHY it should exist, \
independent of HOW to build it.\n\n\
User's original prompt:\n{}\n\n",
        seed.raw_prompt
    );

    if !seed.explicit_constraints.is_empty() {
        prompt.push_str("Constraints explicitly stated in the prompt:\n");
        for constraint in &seed.explicit_constraints {
            prompt.push_str(&format!("- {}\n", constraint.text));
        }
        prompt.push('\n');
    }

    if !seed.inferred_constraints.is_empty() {
        prompt.push_str("Constraints inferred from the prompt and project type:\n");
        for constraint in &seed.inferred_constraints {
            prompt.push_str(&format!(
                "- {} (because: {})\n",
                constraint.text, constraint.rationale
            ));
        }
        prompt.push('\n');
    }

    if !seed.assumptions.is_empty() {
        prompt.push_str("Reasonable default assumptions made during analysis:\n");
        for assumption in &seed.assumptions {
            prompt.push_str(&format!(
                "- {} (reasoning: {}, confidence: {:.0}%)\n",
                assumption.text,
                assumption.rationale,
                assumption.confidence * 100.0
            ));
        }
        prompt.push('\n');
    }

    prompt.push_str(
        "Now produce the Vision artifact. Return a JSON object with these fields:\n\
- product_thesis (string): What the project is for and why it should exist at all.\n\
- user_experience (string): What using the finished product should feel like.\n\
- taste (string): The aesthetic and quality judgments that don't reduce to a requirement.\n\
- governing_constraints (array of strings): Constraints this vision itself imposes.\n\
- identity (string): The project's identity in one paragraph a stranger could repeat back.\n\
- non_goals (array of strings): Explicitly out of scope.\n\
- architectural_character (string): The architectural character this vision implies.\n\
- spiritually_wrong (array of strings): Things technically valid but spiritually wrong.\n\n\
Return ONLY the JSON object, with no markdown formatting or code fences.",
    );

    // Build the completion request.
    let req = tm_provider::CompletionRequest {
        system: None,
        messages: vec![tm_provider::Message {
            role: tm_provider::MessageRole::User,
            content: vec![tm_provider::ContentBlock::Text { text: prompt }],
        }],
        tools: vec![],
        max_tokens: 2048,
        temperature: None,
        stop_sequences: vec![],
        stream: false,
        n: 1,
        model: None,
    };

    // Call the provider.
    let completion = provider
        .complete(req)
        .await
        .map_err(|e| tm_types::TmError::Provider(format!("vision compilation failed: {}", e)))?;

    // Extract the text content from the first candidate.
    let response_text = if completion.candidates.is_empty() {
        return Err(tm_types::TmError::Parse(
            "vision compilation returned no candidates".to_string(),
        ));
    } else {
        let mut text_parts = Vec::new();
        for block in &completion.candidates[0].content {
            if let tm_provider::ContentBlock::Text { text } = block {
                text_parts.push(text.as_str());
            }
        }
        text_parts.join("")
    };

    // Parse the JSON response.
    let parsed: serde_json::Value = serde_json::from_str(&response_text).map_err(|e| {
        tm_types::TmError::Parse(format!("failed to parse vision response as JSON: {}", e))
    })?;

    // Extract each required field, treating missing fields as errors.
    let product_thesis = parsed
        .get("product_thesis")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            tm_types::TmError::Parse("missing required field: product_thesis".to_string())
        })?
        .to_string();

    let user_experience = parsed
        .get("user_experience")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            tm_types::TmError::Parse("missing required field: user_experience".to_string())
        })?
        .to_string();

    let taste = parsed
        .get("taste")
        .and_then(|v| v.as_str())
        .ok_or_else(|| tm_types::TmError::Parse("missing required field: taste".to_string()))?
        .to_string();

    let governing_constraints = parsed
        .get("governing_constraints")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            tm_types::TmError::Parse(
                "missing or non-array field: governing_constraints".to_string(),
            )
        })?
        .iter()
        .map(|v| {
            v.as_str().map(|s| s.to_string()).ok_or_else(|| {
                tm_types::TmError::Parse(
                    "non-string value in governing_constraints array".to_string(),
                )
            })
        })
        .collect::<TmResult<Vec<String>>>()?;

    let identity = parsed
        .get("identity")
        .and_then(|v| v.as_str())
        .ok_or_else(|| tm_types::TmError::Parse("missing required field: identity".to_string()))?
        .to_string();

    let non_goals = parsed
        .get("non_goals")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            tm_types::TmError::Parse("missing or non-array field: non_goals".to_string())
        })?
        .iter()
        .map(|v| {
            v.as_str().map(|s| s.to_string()).ok_or_else(|| {
                tm_types::TmError::Parse("non-string value in non_goals array".to_string())
            })
        })
        .collect::<TmResult<Vec<String>>>()?;

    let architectural_character = parsed
        .get("architectural_character")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            tm_types::TmError::Parse("missing required field: architectural_character".to_string())
        })?
        .to_string();

    let spiritually_wrong = parsed
        .get("spiritually_wrong")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            tm_types::TmError::Parse("missing or non-array field: spiritually_wrong".to_string())
        })?
        .iter()
        .map(|v| {
            v.as_str().map(|s| s.to_string()).ok_or_else(|| {
                tm_types::TmError::Parse("non-string value in spiritually_wrong array".to_string())
            })
        })
        .collect::<TmResult<Vec<String>>>()?;

    Ok(Vision {
        source_seed: None,
        product_thesis,
        user_experience,
        taste,
        governing_constraints,
        identity,
        non_goals,
        architectural_character,
        spiritually_wrong,
        created: clock.now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_provider::{Candidate, Completion, ContentBlock, ModelId, StopReason, Usage};
    use tm_types::FixedClock;

    struct TestProvider {
        response: String,
    }

    impl TestProvider {
        fn new(response: String) -> Self {
            TestProvider { response }
        }
    }

    #[async_trait::async_trait]
    impl tm_provider::fabric::Provider for TestProvider {
        fn id(&self) -> &str {
            "test"
        }

        async fn complete(
            &self,
            _req: tm_provider::CompletionRequest,
        ) -> Result<Completion, tm_provider::ProviderError> {
            Ok(Completion {
                model: ModelId::new("test", "test-model"),
                candidates: vec![Candidate {
                    content: vec![ContentBlock::Text {
                        text: self.response.clone(),
                    }],
                    stop_reason: StopReason::EndTurn,
                }],
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 100,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                latency: std::time::Duration::from_millis(100),
                received_at: tm_types::Timestamp::from_unix_seconds(0),
            })
        }

        async fn embed(
            &self,
            _req: tm_provider::EmbedRequest,
        ) -> Result<tm_provider::Embeddings, tm_provider::ProviderError> {
            Err(tm_provider::ProviderError::Unscripted(
                "embed not implemented".into(),
            ))
        }
    }

    #[tokio::test]
    async fn test_compile_vision_happy_path() {
        let response = r#"{
            "product_thesis": "A tool for managing projects",
            "user_experience": "Intuitive and fast",
            "taste": "Clean, modern design",
            "governing_constraints": ["must be secure", "must scale"],
            "identity": "ProjectManager is a project management tool",
            "non_goals": ["replacing Excel", "mobile apps"],
            "architectural_character": "boring and auditable",
            "spiritually_wrong": ["storing PII", "using blockchain"]
        }"#;

        let seed = Seed::new("build a project manager".to_string(), &FixedClock::epoch());
        let provider = TestProvider::new(response.to_string());
        let clock = FixedClock::epoch();

        let vision = compile_vision(&seed, &provider, &clock).await.unwrap();

        assert_eq!(vision.product_thesis, "A tool for managing projects");
        assert_eq!(vision.user_experience, "Intuitive and fast");
        assert_eq!(vision.taste, "Clean, modern design");
        assert_eq!(
            vision.governing_constraints,
            vec!["must be secure", "must scale"]
        );
        assert_eq!(
            vision.identity,
            "ProjectManager is a project management tool"
        );
        assert_eq!(vision.non_goals, vec!["replacing Excel", "mobile apps"]);
        assert_eq!(vision.architectural_character, "boring and auditable");
        assert_eq!(
            vision.spiritually_wrong,
            vec!["storing PII", "using blockchain"]
        );
        assert!(vision.source_seed.is_none());
        assert_eq!(vision.created, tm_types::Timestamp::from_unix_seconds(0));
    }

    #[tokio::test]
    async fn test_compile_vision_provider_error() {
        struct FailingProvider;

        #[async_trait::async_trait]
        impl tm_provider::fabric::Provider for FailingProvider {
            fn id(&self) -> &str {
                "test"
            }

            async fn complete(
                &self,
                _req: tm_provider::CompletionRequest,
            ) -> Result<Completion, tm_provider::ProviderError> {
                Err(tm_provider::ProviderError::Unavailable(
                    "service unavailable".into(),
                ))
            }

            async fn embed(
                &self,
                _req: tm_provider::EmbedRequest,
            ) -> Result<tm_provider::Embeddings, tm_provider::ProviderError> {
                Err(tm_provider::ProviderError::Unscripted(
                    "embed not implemented".into(),
                ))
            }
        }

        let seed = Seed::new("test".to_string(), &FixedClock::epoch());
        let provider = FailingProvider;
        let clock = FixedClock::epoch();

        let result = compile_vision(&seed, &provider, &clock).await;
        assert!(result.is_err());
        match result {
            Err(tm_types::TmError::Provider(_)) => {}
            _ => panic!("expected Provider error"),
        }
    }

    #[tokio::test]
    async fn test_compile_vision_parse_error_invalid_json() {
        let response = "not valid json";
        let seed = Seed::new("test".to_string(), &FixedClock::epoch());
        let provider = TestProvider::new(response.to_string());
        let clock = FixedClock::epoch();

        let result = compile_vision(&seed, &provider, &clock).await;
        assert!(result.is_err());
        match result {
            Err(tm_types::TmError::Parse(_)) => {}
            _ => panic!("expected Parse error"),
        }
    }

    #[tokio::test]
    async fn test_compile_vision_missing_product_thesis() {
        let response = r#"{
            "user_experience": "Intuitive",
            "taste": "Clean",
            "governing_constraints": [],
            "identity": "A tool",
            "non_goals": [],
            "architectural_character": "boring",
            "spiritually_wrong": []
        }"#;

        let seed = Seed::new("test".to_string(), &FixedClock::epoch());
        let provider = TestProvider::new(response.to_string());
        let clock = FixedClock::epoch();

        let result = compile_vision(&seed, &provider, &clock).await;
        assert!(result.is_err());
        match result {
            Err(tm_types::TmError::Parse(msg)) => {
                assert!(msg.contains("product_thesis"));
            }
            _ => panic!("expected Parse error"),
        }
    }

    #[tokio::test]
    async fn test_compile_vision_missing_identity() {
        let response = r#"{
            "product_thesis": "A tool",
            "user_experience": "Intuitive",
            "taste": "Clean",
            "governing_constraints": [],
            "non_goals": [],
            "architectural_character": "boring",
            "spiritually_wrong": []
        }"#;

        let seed = Seed::new("test".to_string(), &FixedClock::epoch());
        let provider = TestProvider::new(response.to_string());
        let clock = FixedClock::epoch();

        let result = compile_vision(&seed, &provider, &clock).await;
        assert!(result.is_err());
        match result {
            Err(tm_types::TmError::Parse(msg)) => {
                assert!(msg.contains("identity"));
            }
            _ => panic!("expected Parse error"),
        }
    }

    #[tokio::test]
    async fn test_compile_vision_non_string_in_array() {
        let response = r#"{
            "product_thesis": "A tool",
            "user_experience": "Intuitive",
            "taste": "Clean",
            "governing_constraints": ["must be secure", 123],
            "identity": "A tool",
            "non_goals": [],
            "architectural_character": "boring",
            "spiritually_wrong": []
        }"#;

        let seed = Seed::new("test".to_string(), &FixedClock::epoch());
        let provider = TestProvider::new(response.to_string());
        let clock = FixedClock::epoch();

        let result = compile_vision(&seed, &provider, &clock).await;
        assert!(result.is_err());
        match result {
            Err(tm_types::TmError::Parse(msg)) => {
                assert!(msg.contains("non-string"));
            }
            _ => panic!("expected Parse error"),
        }
    }

    #[tokio::test]
    async fn test_compile_vision_non_array_governing_constraints() {
        let response = r#"{
            "product_thesis": "A tool",
            "user_experience": "Intuitive",
            "taste": "Clean",
            "governing_constraints": "must be secure",
            "identity": "A tool",
            "non_goals": [],
            "architectural_character": "boring",
            "spiritually_wrong": []
        }"#;

        let seed = Seed::new("test".to_string(), &FixedClock::epoch());
        let provider = TestProvider::new(response.to_string());
        let clock = FixedClock::epoch();

        let result = compile_vision(&seed, &provider, &clock).await;
        assert!(result.is_err());
        match result {
            Err(tm_types::TmError::Parse(msg)) => {
                assert!(msg.contains("governing_constraints"));
            }
            _ => panic!("expected Parse error"),
        }
    }

    #[tokio::test]
    async fn test_compile_vision_empty_arrays() {
        let response = r#"{
            "product_thesis": "A tool",
            "user_experience": "Intuitive",
            "taste": "Clean",
            "governing_constraints": [],
            "identity": "A tool",
            "non_goals": [],
            "architectural_character": "boring",
            "spiritually_wrong": []
        }"#;

        let seed = Seed::new("test".to_string(), &FixedClock::epoch());
        let provider = TestProvider::new(response.to_string());
        let clock = FixedClock::epoch();

        let vision = compile_vision(&seed, &provider, &clock).await.unwrap();

        assert!(vision.governing_constraints.is_empty());
        assert!(vision.non_goals.is_empty());
        assert!(vision.spiritually_wrong.is_empty());
    }

    #[tokio::test]
    async fn test_compile_vision_no_candidates() {
        struct EmptyProvider;

        #[async_trait::async_trait]
        impl tm_provider::fabric::Provider for EmptyProvider {
            fn id(&self) -> &str {
                "test"
            }

            async fn complete(
                &self,
                _req: tm_provider::CompletionRequest,
            ) -> Result<Completion, tm_provider::ProviderError> {
                Ok(Completion {
                    model: ModelId::new("test", "test-model"),
                    candidates: vec![],
                    usage: Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                    latency: std::time::Duration::from_millis(0),
                    received_at: tm_types::Timestamp::from_unix_seconds(0),
                })
            }

            async fn embed(
                &self,
                _req: tm_provider::EmbedRequest,
            ) -> Result<tm_provider::Embeddings, tm_provider::ProviderError> {
                Err(tm_provider::ProviderError::Unscripted(
                    "embed not implemented".into(),
                ))
            }
        }

        let seed = Seed::new("test".to_string(), &FixedClock::epoch());
        let provider = EmptyProvider;
        let clock = FixedClock::epoch();

        let result = compile_vision(&seed, &provider, &clock).await;
        assert!(result.is_err());
        match result {
            Err(tm_types::TmError::Parse(msg)) => {
                assert!(msg.contains("no candidates"));
            }
            _ => panic!("expected Parse error"),
        }
    }

    #[test]
    fn test_vision_is_anchored_false() {
        let vision = Vision {
            source_seed: None,
            product_thesis: "test".to_string(),
            user_experience: "test".to_string(),
            taste: "test".to_string(),
            governing_constraints: vec![],
            identity: "test".to_string(),
            non_goals: vec![],
            architectural_character: "test".to_string(),
            spiritually_wrong: vec![],
            created: tm_types::Timestamp::from_unix_seconds(0),
        };

        assert!(!vision.is_anchored());
    }

    #[test]
    fn test_vision_is_anchored_true() {
        let vision = Vision {
            source_seed: Some(
                tm_types::ArtifactId::new("ART-9f2a1c0b77de").expect("valid test id"),
            ),
            product_thesis: "test".to_string(),
            user_experience: "test".to_string(),
            taste: "test".to_string(),
            governing_constraints: vec![],
            identity: "test".to_string(),
            non_goals: vec![],
            architectural_character: "test".to_string(),
            spiritually_wrong: vec![],
            created: tm_types::Timestamp::from_unix_seconds(0),
        };

        assert!(vision.is_anchored());
    }
}
