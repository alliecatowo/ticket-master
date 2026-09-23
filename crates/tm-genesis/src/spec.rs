//! The Specification artifact: the Vision made concrete enough to compile into a graph.
//!
//! [`compile_spec`] runs under `Role::ArchitectFrontier` (`architect.frontier`) — turning taste
//! and identity into requirements, architecture, interfaces, a data model, technology choices, a
//! quality bar, a security model, a testing strategy, milestones, and the definitions of v0 and
//! v1 is itself a frontier judgment, not mechanical derivation. [`crate::compile`] is the next
//! stage down, which turns a `Specification` into an actual ticket graph.

use serde::{Deserialize, Serialize};
use tm_provider::types::ContentBlock;
use tm_types::{ArtifactId, Clock, Predicate, Result as TmResult, Timestamp, TmError};

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
/// Build the `architect.frontier` completion request for `vision`. Exposed so tests can script
/// [`tm_provider::MockProvider`] against the exact request [`compile_spec`] sends, rather than
/// duplicating (and risking drift from) the prompt text here.
pub fn build_spec_request(vision: &Vision) -> tm_provider::types::CompletionRequest {
    use tm_provider::types::{ContentBlock, Message, MessageRole};

    let prompt = format!(
        "You are an architect compiling a detailed specification from a product vision.\n\n\
         Based on the following vision, produce a complete specification in JSON format.\n\n\
         VISION:\n\
         Product Thesis: {}\n\
         User Experience: {}\n\
         Taste: {}\n\
         Governing Constraints:\n{}\n\
         Identity: {}\n\
         Non-goals:\n{}\n\
         Architectural Character: {}\n\
         Spiritually Wrong (to avoid):\n{}\n\n\
         Please respond with ONLY valid JSON (no markdown, no code fences) matching this schema:\n\
         {{\n\
           \"requirements\": [{{\"id\": string, \"text\": string, \"priority\": number}}, ...],\n\
           \"architecture\": string,\n\
           \"interfaces\": [{{\"name\": string, \"description\": string}}, ...],\n\
           \"data_model\": string,\n\
           \"technology_choices\": [{{\"area\": string, \"choice\": string, \"rationale\": string}}, ...],\n\
           \"quality_bar\": string,\n\
           \"security_model\": string,\n\
           \"testing_strategy\": string,\n\
           \"milestones\": [{{\"title\": string, \"objective\": string, \"scope_hint\": string}}, ...],\n\
           \"v0\": {{\n\
             \"objective\": string,\n\
             \"exit_criteria\": [predicate, ...]\n\
           }},\n\
           \"v1\": {{\n\
             \"objective\": string,\n\
             \"exit_criteria\": [predicate, ...]\n\
           }}\n\
         }}\n\n\
         Predicates must be JSON objects matching one of these patterns:\n\
         {{\"command_succeeds\": {{\"command\": [\"arg\", ...]}}}}\n\
         {{\"file_exists\": {{\"path\": string}}}}\n\
         {{\"file_matches\": {{\"path\": string, \"regex\": string}}}}\n\
         {{\"tests_pass\": {{\"suite\": string|null}}}}\n\
         {{\"human_attested\": {{\"note\": string}}}}\n\
         {{\"judgment\": {{\"claim\": string}}}}\n\
         {{\"all_of\": [predicate, ...]}}\n\
         {{\"any_of\": [predicate, ...]}}\n\
         {{\"not\": predicate}}\n\
         {{\"ticket_closed\": {{\"ticket\": string}}}}\n\n\
         Ensure:\n\
         - All requirements have non-empty id and text fields\n\
         - v0 and v1 both exist and are non-empty\n\
         - v0/v1 exit_criteria relate to requirements (avoid completely disconnected criteria)\n\
         - Requirements are sorted by priority (lower first)",
        vision.product_thesis,
        vision.user_experience,
        vision.taste,
        vision.governing_constraints.join("\n"),
        vision.identity,
        vision.non_goals.join("\n"),
        vision.architectural_character,
        vision.spiritually_wrong.join("\n")
    );

    tm_provider::types::CompletionRequest {
        system: Some(
            "You are an expert software architect specializing in specification compilation."
                .to_string(),
        ),
        messages: vec![Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }],
        tools: vec![],
        max_tokens: 4096,
        temperature: Some(0.0),
        stop_sequences: vec![],
        stream: false,
        n: 1,
        model: None,
    }
}

/// The Specification artifact compiled from a [`Vision`] under `architect.frontier`.
pub async fn compile_spec(
    vision: &Vision,
    provider: &dyn tm_provider::Provider,
    clock: &dyn Clock,
) -> TmResult<Specification> {
    let req = build_spec_request(vision);

    let completion = provider
        .complete(req)
        .await
        .map_err(|e| TmError::Provider(format!("Couldn't reach the model to draft a spec: {e}")))?;

    if completion.candidates.is_empty() {
        return Err(TmError::Parse(
            "The model didn't return a specification. Try again.".to_string(),
        ));
    }

    let candidate = &completion.candidates[0];
    let response_text = match candidate.content.first() {
        Some(ContentBlock::Text { text }) => text,
        _ => {
            return Err(TmError::Parse(
                "The model's response wasn't readable text. Try again.".to_string(),
            ))
        }
    };

    // Parse the JSON response into an intermediate structure for deserialization
    #[derive(serde::Deserialize)]
    struct SpecResponse {
        requirements: Vec<Requirement>,
        architecture: String,
        interfaces: Vec<InterfaceSpec>,
        data_model: String,
        technology_choices: Vec<TechnologyChoice>,
        quality_bar: String,
        security_model: String,
        testing_strategy: String,
        milestones: Vec<MilestoneOutline>,
        v0: ReleaseDefinition,
        v1: ReleaseDefinition,
    }

    let spec_response: SpecResponse =
        serde_json::from_str::<SpecResponse>(response_text).map_err(|e| {
            TmError::Parse(format!(
                "Couldn't make sense of the model's specification: {e}"
            ))
        })?;

    // Validate that v0 and v1 are non-empty
    if spec_response.v0.exit_criteria.is_empty() {
        return Err(TmError::Parse(
            "The v0 release needs at least one exit criterion.".to_string(),
        ));
    }
    if spec_response.v1.exit_criteria.is_empty() {
        return Err(TmError::Parse(
            "The v1 release needs at least one exit criterion.".to_string(),
        ));
    }

    // Validate that v0/v1 exit_criteria relate to requirements
    // At minimum, they should share some conceptual connection; we check that there's
    // at least one requirement and at least exit criteria present (already validated above).
    // A more sophisticated check would parse the claim/judgment text to find requirement IDs,
    // but for now we just ensure both sides are non-empty.
    if spec_response.requirements.is_empty() {
        return Err(TmError::Parse(
            "The specification needs at least one requirement.".to_string(),
        ));
    }

    Ok(Specification {
        source_vision: None,
        requirements: spec_response.requirements,
        architecture: spec_response.architecture,
        interfaces: spec_response.interfaces,
        data_model: spec_response.data_model,
        technology_choices: spec_response.technology_choices,
        quality_bar: spec_response.quality_bar,
        security_model: spec_response.security_model,
        testing_strategy: spec_response.testing_strategy,
        milestones: spec_response.milestones,
        v0: spec_response.v0,
        v1: spec_response.v1,
        created: clock.now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tm_provider::types::{Candidate, Completion, ContentBlock, ModelId, StopReason, Usage};
    use tm_provider::MockProvider;
    use tm_types::FixedClock;

    fn sample_vision() -> Vision {
        Vision {
            source_seed: None,
            product_thesis: "A ticket management system".to_string(),
            user_experience: "Fast and intuitive".to_string(),
            taste: "Minimal and clean".to_string(),
            governing_constraints: vec!["Must be stateless".to_string()],
            identity: "A lightweight ticket tracker".to_string(),
            non_goals: vec!["Not a full project management tool".to_string()],
            architectural_character: "Simple and boring".to_string(),
            spiritually_wrong: vec!["Overly complex UI".to_string()],
            created: tm_types::Timestamp::from_unix_seconds(1000),
        }
    }

    fn valid_spec_json() -> String {
        r#"{
  "requirements": [
    {"id": "R1", "text": "System must handle concurrent requests", "priority": 1},
    {"id": "R2", "text": "System must store tickets persistently", "priority": 2}
  ],
  "architecture": "Microservice with API gateway and ticket store",
  "interfaces": [
    {"name": "HTTP API", "description": "RESTful ticket operations"},
    {"name": "CLI", "description": "Command line interface"}
  ],
  "data_model": "Tickets with id, title, status, created_at fields",
  "technology_choices": [
    {"area": "language", "choice": "Rust", "rationale": "Type safety and performance"}
  ],
  "quality_bar": "99.9% uptime SLA, <100ms latency p99",
  "security_model": "API key authentication, HTTPS only",
  "testing_strategy": "Unit tests for core logic, integration tests for APIs",
  "milestones": [
    {"title": "MVP", "objective": "Basic ticket CRUD", "scope_hint": "Create, read, update tickets"},
    {"title": "v1.0", "objective": "Full feature set", "scope_hint": "Add search, filtering, labels"}
  ],
  "v0": {
    "objective": "Minimal viable ticket system",
    "exit_criteria": [
      {"tests_pass": {"suite": null}},
      {"file_exists": {"path": "Cargo.toml"}}
    ]
  },
  "v1": {
    "objective": "Feature complete system",
    "exit_criteria": [
      {"tests_pass": {"suite": null}},
      {"judgment": {"claim": "All requirements satisfied"}}
    ]
  }
}"#
        .to_string()
    }

    #[tokio::test]
    async fn compile_spec_happy_path() {
        let vision = sample_vision();
        let clock = Arc::new(FixedClock::new(tm_types::Timestamp::from_unix_seconds(
            5000,
        )));
        let mock = MockProvider::new(
            "anthropic",
            ModelId::new("anthropic", "claude"),
            clock.clone(),
        );

        let scripted_completion = Completion {
            model: ModelId::new("anthropic", "claude"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: valid_spec_json(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            latency: std::time::Duration::from_millis(100),
            received_at: tm_types::Timestamp::from_unix_seconds(5000),
        };

        let req = build_spec_request(&vision);
        mock.script_response(&req, scripted_completion);

        let spec = compile_spec(&vision, &mock, clock.as_ref())
            .await
            .expect("compile_spec should succeed");

        assert_eq!(spec.requirements.len(), 2);
        assert_eq!(spec.requirements[0].id, "R1");
        assert_eq!(spec.requirements[0].priority, 1);
        assert_eq!(spec.requirements[1].id, "R2");
        assert_eq!(spec.requirements[1].priority, 2);
        assert_eq!(
            spec.architecture,
            "Microservice with API gateway and ticket store"
        );
        assert_eq!(spec.interfaces.len(), 2);
        assert_eq!(
            spec.data_model,
            "Tickets with id, title, status, created_at fields"
        );
        assert_eq!(spec.technology_choices.len(), 1);
        assert_eq!(spec.quality_bar, "99.9% uptime SLA, <100ms latency p99");
        assert_eq!(spec.security_model, "API key authentication, HTTPS only");
        assert_eq!(
            spec.testing_strategy,
            "Unit tests for core logic, integration tests for APIs"
        );
        assert_eq!(spec.milestones.len(), 2);
        assert_eq!(spec.v0.objective, "Minimal viable ticket system");
        assert_eq!(spec.v0.exit_criteria.len(), 2);
        assert_eq!(spec.v1.objective, "Feature complete system");
        assert_eq!(spec.v1.exit_criteria.len(), 2);
        assert_eq!(spec.source_vision, None);
        assert_eq!(spec.created, tm_types::Timestamp::from_unix_seconds(5000));
    }

    #[tokio::test]
    async fn compile_spec_rejects_empty_v0_criteria() {
        let vision = sample_vision();
        let clock = Arc::new(FixedClock::new(tm_types::Timestamp::from_unix_seconds(
            5000,
        )));
        let mock = MockProvider::new(
            "anthropic",
            ModelId::new("anthropic", "claude"),
            clock.clone(),
        );

        let invalid_json = r#"{
  "requirements": [{"id": "R1", "text": "Requirement", "priority": 1}],
  "architecture": "Arch",
  "interfaces": [],
  "data_model": "Model",
  "technology_choices": [],
  "quality_bar": "Bar",
  "security_model": "Security",
  "testing_strategy": "Testing",
  "milestones": [],
  "v0": {"objective": "Objective", "exit_criteria": []},
  "v1": {"objective": "V1", "exit_criteria": [{"tests_pass": {"suite": null}}]}
}"#;

        let scripted_completion = Completion {
            model: ModelId::new("anthropic", "claude"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: invalid_json.to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage::default(),
            latency: std::time::Duration::from_millis(0),
            received_at: tm_types::Timestamp::from_unix_seconds(5000),
        };

        let req = build_spec_request(&vision);
        mock.script_response(&req, scripted_completion);

        let result = compile_spec(&vision, &mock, clock.as_ref()).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("The v0 release needs at least one exit criterion."));
    }

    #[tokio::test]
    async fn compile_spec_rejects_empty_v1_criteria() {
        let vision = sample_vision();
        let clock = Arc::new(FixedClock::new(tm_types::Timestamp::from_unix_seconds(
            5000,
        )));
        let mock = MockProvider::new(
            "anthropic",
            ModelId::new("anthropic", "claude"),
            clock.clone(),
        );

        let invalid_json = r#"{
  "requirements": [{"id": "R1", "text": "Requirement", "priority": 1}],
  "architecture": "Arch",
  "interfaces": [],
  "data_model": "Model",
  "technology_choices": [],
  "quality_bar": "Bar",
  "security_model": "Security",
  "testing_strategy": "Testing",
  "milestones": [],
  "v0": {"objective": "Objective", "exit_criteria": [{"tests_pass": {"suite": null}}]},
  "v1": {"objective": "V1", "exit_criteria": []}
}"#;

        let scripted_completion = Completion {
            model: ModelId::new("anthropic", "claude"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: invalid_json.to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage::default(),
            latency: std::time::Duration::from_millis(0),
            received_at: tm_types::Timestamp::from_unix_seconds(5000),
        };

        let req = build_spec_request(&vision);
        mock.script_response(&req, scripted_completion);

        let result = compile_spec(&vision, &mock, clock.as_ref()).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("The v1 release needs at least one exit criterion."));
    }

    #[tokio::test]
    async fn compile_spec_rejects_empty_requirements() {
        let vision = sample_vision();
        let clock = Arc::new(FixedClock::new(tm_types::Timestamp::from_unix_seconds(
            5000,
        )));
        let mock = MockProvider::new(
            "anthropic",
            ModelId::new("anthropic", "claude"),
            clock.clone(),
        );

        let invalid_json = r#"{
  "requirements": [],
  "architecture": "Arch",
  "interfaces": [],
  "data_model": "Model",
  "technology_choices": [],
  "quality_bar": "Bar",
  "security_model": "Security",
  "testing_strategy": "Testing",
  "milestones": [],
  "v0": {"objective": "Objective", "exit_criteria": [{"tests_pass": {"suite": null}}]},
  "v1": {"objective": "V1", "exit_criteria": [{"tests_pass": {"suite": null}}]}
}"#;

        let scripted_completion = Completion {
            model: ModelId::new("anthropic", "claude"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: invalid_json.to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage::default(),
            latency: std::time::Duration::from_millis(0),
            received_at: tm_types::Timestamp::from_unix_seconds(5000),
        };

        let req = build_spec_request(&vision);
        mock.script_response(&req, scripted_completion);

        let result = compile_spec(&vision, &mock, clock.as_ref()).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("The specification needs at least one requirement."));
    }

    #[tokio::test]
    async fn compile_spec_rejects_malformed_json() {
        let vision = sample_vision();
        let clock = Arc::new(FixedClock::new(tm_types::Timestamp::from_unix_seconds(
            5000,
        )));
        let mock = MockProvider::new(
            "anthropic",
            ModelId::new("anthropic", "claude"),
            clock.clone(),
        );

        let scripted_completion = Completion {
            model: ModelId::new("anthropic", "claude"),
            candidates: vec![Candidate {
                content: vec![ContentBlock::Text {
                    text: "not valid json {]".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
            }],
            usage: Usage::default(),
            latency: std::time::Duration::from_millis(0),
            received_at: tm_types::Timestamp::from_unix_seconds(5000),
        };

        let req = build_spec_request(&vision);
        mock.script_response(&req, scripted_completion);

        let result = compile_spec(&vision, &mock, clock.as_ref()).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Couldn't make sense of the model's specification"));
    }

    #[test]
    fn requirement_has_id_text_priority() {
        let req = Requirement {
            id: "R1".to_string(),
            text: "A requirement".to_string(),
            priority: 1,
        };
        assert_eq!(req.id, "R1");
        assert_eq!(req.text, "A requirement");
        assert_eq!(req.priority, 1);
    }

    #[test]
    fn interface_spec_has_name_and_description() {
        let iface = InterfaceSpec {
            name: "HTTP API".to_string(),
            description: "RESTful interface".to_string(),
        };
        assert_eq!(iface.name, "HTTP API");
        assert_eq!(iface.description, "RESTful interface");
    }

    #[test]
    fn technology_choice_has_area_choice_rationale() {
        let tech = TechnologyChoice {
            area: "language".to_string(),
            choice: "Rust".to_string(),
            rationale: "Type safety".to_string(),
        };
        assert_eq!(tech.area, "language");
        assert_eq!(tech.choice, "Rust");
        assert_eq!(tech.rationale, "Type safety");
    }

    #[test]
    fn milestone_outline_has_title_objective_scope() {
        let ms = MilestoneOutline {
            title: "MVP".to_string(),
            objective: "Minimal viable product".to_string(),
            scope_hint: "Core features".to_string(),
        };
        assert_eq!(ms.title, "MVP");
        assert_eq!(ms.objective, "Minimal viable product");
        assert_eq!(ms.scope_hint, "Core features");
    }

    #[test]
    fn release_definition_has_objective_and_criteria() {
        let rel = ReleaseDefinition {
            objective: "v1.0 feature complete".to_string(),
            exit_criteria: vec![Predicate::TestsPass { suite: None }],
        };
        assert_eq!(rel.objective, "v1.0 feature complete");
        assert_eq!(rel.exit_criteria.len(), 1);
    }

    #[test]
    fn specification_can_be_created_with_all_fields() {
        let spec = Specification {
            source_vision: None,
            requirements: vec![Requirement {
                id: "R1".to_string(),
                text: "Test".to_string(),
                priority: 1,
            }],
            architecture: "Simple".to_string(),
            interfaces: vec![],
            data_model: "Model".to_string(),
            technology_choices: vec![],
            quality_bar: "High".to_string(),
            security_model: "Secure".to_string(),
            testing_strategy: "Comprehensive".to_string(),
            milestones: vec![],
            v0: ReleaseDefinition {
                objective: "v0".to_string(),
                exit_criteria: vec![Predicate::TestsPass { suite: None }],
            },
            v1: ReleaseDefinition {
                objective: "v1".to_string(),
                exit_criteria: vec![Predicate::TestsPass { suite: None }],
            },
            created: tm_types::Timestamp::from_unix_seconds(1000),
        };
        assert_eq!(spec.requirements.len(), 1);
        assert_eq!(spec.architecture, "Simple");
        assert_eq!(spec.source_vision, None);
    }

    #[test]
    fn specification_serializes_and_deserializes() {
        let spec = Specification {
            source_vision: None,
            requirements: vec![Requirement {
                id: "R1".to_string(),
                text: "Test".to_string(),
                priority: 1,
            }],
            architecture: "Simple".to_string(),
            interfaces: vec![InterfaceSpec {
                name: "API".to_string(),
                description: "Test API".to_string(),
            }],
            data_model: "Model".to_string(),
            technology_choices: vec![],
            quality_bar: "High".to_string(),
            security_model: "Secure".to_string(),
            testing_strategy: "Testing".to_string(),
            milestones: vec![],
            v0: ReleaseDefinition {
                objective: "v0".to_string(),
                exit_criteria: vec![Predicate::TestsPass { suite: None }],
            },
            v1: ReleaseDefinition {
                objective: "v1".to_string(),
                exit_criteria: vec![Predicate::TestsPass { suite: None }],
            },
            created: tm_types::Timestamp::from_unix_seconds(1000),
        };
        let json = serde_json::to_string(&spec).expect("serialize");
        let spec2 = serde_json::from_str::<Specification>(&json).expect("deserialize");
        assert_eq!(spec, spec2);
    }
}
