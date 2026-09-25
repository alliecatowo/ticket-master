//! Reusable, schema-valid canned JSON fixtures for offline Genesis provider stages.
//!
//! These fixtures are extracted from `crates/tm-e2e/tests/genesis_e2e.rs` and can be used
//! to feed a `MockProvider` with [`offline_sequence`] when testing Genesis offline.
//!
//! [`graph_compilation`] in particular is built from the real
//! [`crate::compile::ProposedTicket`]/[`crate::compile::ProposedMilestone`] types and serialized
//! with `serde_json` rather than hand-written as a JSON literal -- a hand-written literal here
//! previously drifted out of sync with `TicketKind`/`Role`/`Tolerance`'s `#[serde(rename_all =
//! "snake_case")]` (it used PascalCase variant names) and `Budget`'s real field shape (it used a
//! `{"kind": "Unlimited"}` shape that was never this type's actual representation), so
//! `tm_genesis::compile::propose_graph`'s real deserializer rejected it outright. Building it from
//! the real types makes that class of drift a compile error instead of a silent runtime failure.

/// Minimal seed response: an empty JSON object, satisfying `analyze_prompt`'s schema.
pub fn seed() -> &'static str {
    "{}"
}

/// Vision stage fixture: a complete vision artifact.
pub fn vision() -> &'static str {
    r#"{
        "product_thesis": "A tiny offline ticket tracker.",
        "user_experience": "Predictable and boring.",
        "taste": "No surprises.",
        "governing_constraints": ["stays offline"],
        "identity": "A minimal ticket tracker for a single project.",
        "non_goals": ["multi-tenant SaaS"],
        "architectural_character": "boring and auditable",
        "spiritually_wrong": ["a hidden queue nobody can inspect"]
    }"#
}

/// Specification stage fixture: a complete specification with v0 and v1 definitions.
pub fn spec() -> &'static str {
    r#"{
        "requirements": [
            {"id": "R1", "text": "tickets persist across restarts", "priority": 0}
        ],
        "architecture": "single embedded store",
        "interfaces": [{"name": "CLI", "description": "create and close tickets"}],
        "data_model": "a ticket has an id, objective and state",
        "technology_choices": [
            {"area": "storage", "choice": "sqlite", "rationale": "simple and embedded"}
        ],
        "quality_bar": "green CI",
        "security_model": "single local user, no auth",
        "testing_strategy": "unit tests per module",
        "milestones": [
            {"title": "v1", "objective": "ship the core loop", "scope_hint": "everything"}
        ],
        "v0": {
            "objective": "a ticket can be created and closed",
            "exit_criteria": [{"tests_pass": {"suite": null}}]
        },
        "v1": {
            "objective": "the core loop is solid",
            "exit_criteria": [{"tests_pass": {"suite": null}}]
        }
    }"#
}

/// Graph compilation stage fixture: a minimal ticket graph with one milestone.
///
/// Built from the real [`crate::compile::ProposedTicket`]/[`crate::compile::ProposedMilestone`]
/// types and serialized with `serde_json`, so it can never drift from what
/// `tm_genesis::compile::propose_graph`'s deserializer actually accepts (see this module's doc
/// comment). Mirrors `crates/tm-e2e/tests/genesis_e2e.rs`'s `graph_compilation_response`: two
/// independent `Work` tickets under one milestone titled `"v1"`.
pub fn graph_compilation() -> String {
    use tm_core::ticket::{
        ExecutorRequirements, RetryPolicy, TicketKind, VerificationPolicy as VerifPolicy,
    };
    use tm_types::{Authority, Budget, Role, Tolerance};

    use crate::compile::{ProposedMilestone, ProposedTicket};

    fn retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay_seconds: 1,
            backoff_multiplier: 2.0,
            max_delay_seconds: 60,
        }
    }

    fn ticket(ticket_ref: &str, objective: &str) -> ProposedTicket {
        ProposedTicket {
            ticket_ref: ticket_ref.to_string(),
            kind: TicketKind::Work,
            objective: objective.to_string(),
            parent_ref: None,
            milestone_ref: Some("v1".to_string()),
            authority: Authority::root(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: Role::CoderFast,
                human_required: false,
                min_capability: Tolerance::Any,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerifPolicy::Single,
            budget: Budget::unlimited(),
            retry: retry(),
            priority: 0,
        }
    }

    let tickets = vec![
        ticket("core", "implement the core ticket loop"),
        ticket("polish", "polish the CLI output"),
    ];
    let milestones = vec![ProposedMilestone {
        milestone_ref: "v1".to_string(),
        title: "v1".to_string(),
        ticket_refs: vec![],
    }];

    serde_json::json!({
        "tickets": tickets,
        "dependencies": Vec::<serde_json::Value>::new(),
        "milestones": milestones,
        "authority_domains": Vec::<serde_json::Value>::new(),
    })
    .to_string()
}

/// Maturity gate fixture: a passing maturity judgment.
pub fn maturity() -> &'static str {
    r#"{"mature": true, "rationale": "V1 is closed and the sole change made it end to end."}"#
}

/// Returns the offline sequence of provider responses in stage-call order.
///
/// Each Genesis provider-calling stage consumes one string from this sequence in order:
/// 1. Seed: validates the raw prompt into structured metadata
/// 2. Vision: compiles a vision artifact from the seed
/// 3. Spec: compiles a specification from the vision
/// 4. GraphCompilation: builds and validates a ticket graph from the spec
/// 5. MaturityGate: evaluates project maturity after V1 is complete
pub fn offline_sequence() -> Vec<String> {
    vec![
        seed().to_string(),
        vision().to_string(),
        spec().to_string(),
        graph_compilation(),
        maturity().to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Test that each fixture deserializes as valid JSON.
    #[test]
    fn all_fixtures_are_valid_json() {
        assert!(serde_json::from_str::<Value>(seed()).is_ok());
        assert!(serde_json::from_str::<Value>(vision()).is_ok());
        assert!(serde_json::from_str::<Value>(spec()).is_ok());
        assert!(serde_json::from_str::<Value>(&graph_compilation()).is_ok());
        assert!(serde_json::from_str::<Value>(maturity()).is_ok());
    }

    /// Test that offline_sequence returns fixtures in the correct order and count.
    #[test]
    fn offline_sequence_has_correct_order_and_count() {
        let seq = offline_sequence();
        assert_eq!(
            seq.len(),
            5,
            "offline_sequence should return exactly 5 fixtures"
        );
        assert_eq!(seq[0], seed());
        assert_eq!(seq[1], vision());
        assert_eq!(seq[2], spec());
        assert_eq!(seq[3], graph_compilation());
        assert_eq!(seq[4], maturity());
    }

    /// Test that seed fixture deserializes into the expected Seed type.
    #[test]
    fn seed_deserializes_correctly() {
        let json = seed();
        // An empty object should deserialize (with all arrays defaulting to empty).
        let result: Result<serde_json::Value, _> = serde_json::from_str(json);
        assert!(result.is_ok(), "seed fixture should be valid JSON");
    }

    /// Test that vision fixture deserializes into a complete vision object.
    #[test]
    fn vision_deserializes_correctly() {
        let json = vision();
        let parsed: Result<Value, _> = serde_json::from_str(json);
        assert!(parsed.is_ok(), "vision fixture should deserialize");
        let obj = parsed.unwrap();
        assert!(obj.get("product_thesis").is_some());
        assert!(obj.get("user_experience").is_some());
        assert!(obj.get("governing_constraints").is_some());
    }

    /// Test that spec fixture deserializes into a complete specification object.
    #[test]
    fn spec_deserializes_correctly() {
        let json = spec();
        let parsed: Result<Value, _> = serde_json::from_str(json);
        assert!(parsed.is_ok(), "spec fixture should deserialize");
        let obj = parsed.unwrap();
        assert!(obj.get("requirements").is_some());
        assert!(obj.get("v0").is_some());
        assert!(obj.get("v1").is_some());
    }

    /// Test that graph_compilation fixture deserializes into a valid payload.
    #[test]
    fn graph_compilation_deserializes_correctly() {
        let json = graph_compilation();
        let parsed: Result<Value, _> = serde_json::from_str(&json);
        assert!(
            parsed.is_ok(),
            "graph_compilation fixture should deserialize"
        );
        let obj = parsed.unwrap();
        assert!(obj.get("tickets").is_some());
        assert!(obj.get("milestones").is_some());
        assert!(obj.get("dependencies").is_some());
    }

    /// Test that maturity fixture deserializes into a judgment object.
    #[test]
    fn maturity_deserializes_correctly() {
        let json = maturity();
        let parsed: Result<Value, _> = serde_json::from_str(json);
        assert!(parsed.is_ok(), "maturity fixture should deserialize");
        let obj = parsed.unwrap();
        assert_eq!(obj.get("mature").and_then(|v| v.as_bool()), Some(true));
        assert!(obj.get("rationale").is_some());
    }
}
