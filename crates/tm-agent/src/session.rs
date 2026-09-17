//! Session bookkeeping: the transcript, the pinned harness epoch, and promotion of anything
//! that matters out of conversation into durable state.
//!
//! `SPEC.md` §10's harness-pinning guarantee means a [`Session`] never re-reads its harness
//! epoch mid-run; it carries the epoch number it started with. And because a conversation is
//! not itself durable state, every decision, artifact, evidence record and ticket a step's tool
//! calls produced is promoted (via [`Session::promote`]) into ids a fresh worker — or a human —
//! can look up directly in `tm-core`, without ever needing to replay the transcript.

use std::collections::HashSet;

use tm_types::{ArtifactId, DecisionId, SessionId, TicketId, Timestamp};

use crate::outcome::{StepRecord, ToolCallResolution};

/// One agent session: the running transcript plus the fixed harness epoch it was pinned to at
/// start.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// This session's id.
    pub id: SessionId,
    /// The ticket this session is executing.
    pub ticket: TicketId,
    /// The harness epoch number pinned at session start
    /// (`tm_harness::epoch::SessionPin::epoch_number`); never re-resolved during the session.
    pub harness_epoch: u64,
    /// Every step recorded so far, in order.
    pub transcript: Vec<StepRecord>,
    /// When the session started.
    pub started_at: Timestamp,
}

impl Session {
    /// Start a fresh session pinned to `harness_epoch`, with an empty transcript.
    pub fn new(id: SessionId, ticket: TicketId, harness_epoch: u64, started_at: Timestamp) -> Self {
        Session {
            id,
            ticket,
            harness_epoch,
            transcript: Vec::new(),
            started_at,
        }
    }

    /// Append one completed step to the transcript.
    pub fn record_step(&mut self, step: StepRecord) {
        self.transcript.push(step);
    }

    /// The most recent step recorded, if any.
    pub fn last_step(&self) -> Option<&StepRecord> {
        self.transcript.last()
    }

    /// Promote everything durable out of this session's transcript so far.
    ///
    /// A fresh worker resuming this ticket must be able to reconstruct what happened from
    /// [`DurablePromotion`] plus `tm-core` state alone — never from re-reading `self.transcript`,
    /// which is conversation, not durable state.
    pub fn promote(&self) -> DurablePromotion {
        let mut decisions = Vec::new();
        let mut decision_set = HashSet::new();
        let mut artifacts = Vec::new();
        let mut artifact_set = HashSet::new();
        let mut tickets = Vec::new();
        let mut ticket_set = HashSet::new();

        for step in &self.transcript {
            for tool_call in &step.tool_calls {
                if let ToolCallResolution::Completed { result, artifact } = &tool_call.resolution {
                    if let Some(artifact_id) = artifact {
                        if artifact_set.insert(artifact_id.clone()) {
                            artifacts.push(artifact_id.clone());
                        }
                    }

                    match tool_call.tool_name.as_str() {
                        "decision.record" => {
                            if let Some(decision_val) = result.get("decision") {
                                if let Ok(id) =
                                    serde_json::from_value::<DecisionId>(decision_val.clone())
                                {
                                    if decision_set.insert(id.clone()) {
                                        decisions.push(id);
                                    }
                                }
                            }
                        }
                        "artifact.store" => {
                            if let Some(artifact_val) = result.get("artifact") {
                                if let Ok(id) =
                                    serde_json::from_value::<ArtifactId>(artifact_val.clone())
                                {
                                    if artifact_set.insert(id.clone()) {
                                        artifacts.push(id);
                                    }
                                }
                            }
                        }
                        "evidence.attach" => {
                            if let Some(artifact_val) = result.get("artifact") {
                                if let Ok(id) =
                                    serde_json::from_value::<ArtifactId>(artifact_val.clone())
                                {
                                    if artifact_set.insert(id.clone()) {
                                        artifacts.push(id);
                                    }
                                }
                            }
                        }
                        "ticket.create_child" | "ticket.delegate" => {
                            if let Some(ticket_val) = result.get("ticket") {
                                if let Ok(id) =
                                    serde_json::from_value::<TicketId>(ticket_val.clone())
                                {
                                    if ticket_set.insert(id.clone()) {
                                        tickets.push(id);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        DurablePromotion {
            decisions,
            artifacts,
            tickets,
        }
    }
}

/// Everything durable that a [`Session`]'s transcript has produced so far: decisions,
/// artifacts, and tickets a fresh worker can look up in `tm-core` directly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DurablePromotion {
    /// Decisions recorded during the session.
    pub decisions: Vec<DecisionId>,
    /// Artifacts produced or referenced during the session (patches, command output, evidence).
    pub artifacts: Vec<ArtifactId>,
    /// Tickets created or delegated to during the session.
    pub tickets: Vec<TicketId>,
}

impl DurablePromotion {
    /// True when nothing durable has been produced yet.
    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty() && self.artifacts.is_empty() && self.tickets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::{ToolCallRecord, ToolCallResolution};
    use tm_types::Spend;
    use tm_types::Timestamp;

    fn make_session(transcript: Vec<StepRecord>) -> Session {
        Session {
            id: SessionId::new("S-1").unwrap(),
            ticket: TicketId::new("T-1").unwrap(),
            harness_epoch: 1,
            transcript,
            started_at: Timestamp::EPOCH,
        }
    }

    fn make_step_with_tools(tool_calls: Vec<ToolCallRecord>) -> StepRecord {
        StepRecord {
            index: 1,
            served_by: "test".to_string(),
            assistant_text: None,
            tool_calls,
            spend: Spend::default(),
            at: Timestamp::EPOCH,
        }
    }

    fn completed_tool_call(
        name: &str,
        result: serde_json::Value,
        artifact: Option<ArtifactId>,
    ) -> ToolCallRecord {
        ToolCallRecord {
            tool_use_id: "call-1".to_string(),
            tool_name: name.to_string(),
            input: serde_json::json!({}),
            resolution: ToolCallResolution::Completed { result, artifact },
        }
    }

    #[test]
    fn empty_transcript_yields_empty_promotion() {
        let session = make_session(vec![]);
        let promotion = session.promote();
        assert!(promotion.is_empty());
        assert!(promotion.decisions.is_empty());
        assert!(promotion.artifacts.is_empty());
        assert!(promotion.tickets.is_empty());
    }

    #[test]
    fn decision_record_extracts_decision_id() {
        let decision_id = "D-001";
        let tool_call = completed_tool_call(
            "decision.record",
            serde_json::json!({ "decision": decision_id }),
            None,
        );
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert_eq!(promotion.decisions.len(), 1);
        assert_eq!(promotion.decisions[0].as_str(), decision_id);
        assert!(promotion.artifacts.is_empty());
        assert!(promotion.tickets.is_empty());
    }

    #[test]
    fn artifact_store_extracts_artifact_id() {
        let artifact_id = "ART-000000000001";
        let tool_call = completed_tool_call(
            "artifact.store",
            serde_json::json!({ "artifact": artifact_id }),
            None,
        );
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert_eq!(promotion.artifacts.len(), 1);
        assert_eq!(promotion.artifacts[0].as_str(), artifact_id);
        assert!(promotion.decisions.is_empty());
        assert!(promotion.tickets.is_empty());
    }

    #[test]
    fn large_result_spilled_to_artifact() {
        let artifact_id = ArtifactId::new("ART-000000000002").unwrap();
        let tool_call = completed_tool_call(
            "shell.run",
            serde_json::json!({ "truncated": true }),
            Some(artifact_id.clone()),
        );
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert_eq!(promotion.artifacts.len(), 1);
        assert_eq!(promotion.artifacts[0], artifact_id);
        assert!(promotion.decisions.is_empty());
        assert!(promotion.tickets.is_empty());
    }

    #[test]
    fn evidence_attach_extracts_artifact() {
        let artifact_id = "ART-000000000003";
        let tool_call = completed_tool_call(
            "evidence.attach",
            serde_json::json!({ "artifact": artifact_id }),
            None,
        );
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert_eq!(promotion.artifacts.len(), 1);
        assert_eq!(promotion.artifacts[0].as_str(), artifact_id);
        assert!(promotion.decisions.is_empty());
        assert!(promotion.tickets.is_empty());
    }

    #[test]
    fn ticket_create_child_extracts_ticket_id() {
        let ticket_id = "T-42";
        let tool_call = completed_tool_call(
            "ticket.create_child",
            serde_json::json!({ "ticket": ticket_id }),
            None,
        );
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert_eq!(promotion.tickets.len(), 1);
        assert_eq!(promotion.tickets[0].as_str(), ticket_id);
        assert!(promotion.decisions.is_empty());
        assert!(promotion.artifacts.is_empty());
    }

    #[test]
    fn ticket_delegate_extracts_ticket_id() {
        let ticket_id = "T-99";
        let tool_call = completed_tool_call(
            "ticket.delegate",
            serde_json::json!({ "ticket": ticket_id }),
            None,
        );
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert_eq!(promotion.tickets.len(), 1);
        assert_eq!(promotion.tickets[0].as_str(), ticket_id);
        assert!(promotion.decisions.is_empty());
        assert!(promotion.artifacts.is_empty());
    }

    #[test]
    fn deduplicates_repeated_ids() {
        let decision_id = "D-001";
        let tool_call1 = completed_tool_call(
            "decision.record",
            serde_json::json!({ "decision": decision_id }),
            None,
        );
        let tool_call2 = completed_tool_call(
            "decision.record",
            serde_json::json!({ "decision": decision_id }),
            None,
        );
        let step = make_step_with_tools(vec![tool_call1, tool_call2]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert_eq!(promotion.decisions.len(), 1);
        assert_eq!(promotion.decisions[0].as_str(), decision_id);
    }

    #[test]
    fn preserves_first_seen_order_with_deduplication() {
        let id1 = "D-001";
        let id2 = "D-002";
        let id1_again = "D-001";

        let tool_call1 = completed_tool_call(
            "decision.record",
            serde_json::json!({ "decision": id1 }),
            None,
        );
        let tool_call2 = completed_tool_call(
            "decision.record",
            serde_json::json!({ "decision": id2 }),
            None,
        );
        let tool_call3 = completed_tool_call(
            "decision.record",
            serde_json::json!({ "decision": id1_again }),
            None,
        );

        let step = make_step_with_tools(vec![tool_call1, tool_call2, tool_call3]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert_eq!(promotion.decisions.len(), 2);
        assert_eq!(promotion.decisions[0].as_str(), id1);
        assert_eq!(promotion.decisions[1].as_str(), id2);
    }

    #[test]
    fn ignores_denied_resolutions() {
        let tool_call = ToolCallRecord {
            tool_use_id: "call-1".to_string(),
            tool_name: "decision.record".to_string(),
            input: serde_json::json!({}),
            resolution: ToolCallResolution::Denied {
                reason: "Access denied".to_string(),
            },
        };
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert!(promotion.is_empty());
    }

    #[test]
    fn ignores_errored_resolutions() {
        let tool_call = ToolCallRecord {
            tool_use_id: "call-1".to_string(),
            tool_name: "artifact.store".to_string(),
            input: serde_json::json!({}),
            resolution: ToolCallResolution::Errored {
                detail: "Failed to parse".to_string(),
            },
        };
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert!(promotion.is_empty());
    }

    #[test]
    fn ignores_malformed_json_values() {
        let tool_call = completed_tool_call(
            "decision.record",
            serde_json::json!({ "decision": 123 }),
            None,
        );
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert!(promotion.is_empty());
    }

    #[test]
    fn ignores_missing_keys() {
        let tool_call = completed_tool_call("decision.record", serde_json::json!({}), None);
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert!(promotion.is_empty());
    }

    #[test]
    fn collects_multiple_types_in_single_step() {
        let decision_id = "D-001";
        let artifact_id = "ART-000000000004";
        let ticket_id = "T-10";

        let decision_call = completed_tool_call(
            "decision.record",
            serde_json::json!({ "decision": decision_id }),
            None,
        );
        let artifact_call = completed_tool_call(
            "artifact.store",
            serde_json::json!({ "artifact": artifact_id }),
            None,
        );
        let ticket_call = completed_tool_call(
            "ticket.create_child",
            serde_json::json!({ "ticket": ticket_id }),
            None,
        );

        let step = make_step_with_tools(vec![decision_call, artifact_call, ticket_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert_eq!(promotion.decisions.len(), 1);
        assert_eq!(promotion.artifacts.len(), 1);
        assert_eq!(promotion.tickets.len(), 1);
        assert_eq!(promotion.decisions[0].as_str(), decision_id);
        assert_eq!(promotion.artifacts[0].as_str(), artifact_id);
        assert_eq!(promotion.tickets[0].as_str(), ticket_id);
    }

    #[test]
    fn collects_across_multiple_steps() {
        let decision_id1 = "D-001";
        let decision_id2 = "D-002";

        let decision_call1 = completed_tool_call(
            "decision.record",
            serde_json::json!({ "decision": decision_id1 }),
            None,
        );
        let step1 = make_step_with_tools(vec![decision_call1]);

        let decision_call2 = completed_tool_call(
            "decision.record",
            serde_json::json!({ "decision": decision_id2 }),
            None,
        );
        let step2 = make_step_with_tools(vec![decision_call2]);

        let session = make_session(vec![step1, step2]);
        let promotion = session.promote();

        assert_eq!(promotion.decisions.len(), 2);
        assert_eq!(promotion.decisions[0].as_str(), decision_id1);
        assert_eq!(promotion.decisions[1].as_str(), decision_id2);
    }

    #[test]
    fn ignores_unknown_tool_names() {
        let tool_call = completed_tool_call(
            "unknown.tool",
            serde_json::json!({ "some_field": "value" }),
            None,
        );
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert!(promotion.is_empty());
    }

    #[test]
    fn combines_large_result_artifact_and_extracted_id() {
        let large_artifact_id = ArtifactId::new("ART-000000000005").unwrap();
        let extracted_artifact_id = "ART-000000000006";

        let tool_call = completed_tool_call(
            "artifact.store",
            serde_json::json!({ "artifact": extracted_artifact_id }),
            Some(large_artifact_id.clone()),
        );
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert_eq!(promotion.artifacts.len(), 2);
        assert_eq!(promotion.artifacts[0], large_artifact_id);
        assert_eq!(promotion.artifacts[1].as_str(), extracted_artifact_id);
    }

    #[test]
    fn ticket_ids_include_verification_nodes() {
        let ticket_id = "V-5";
        let tool_call = completed_tool_call(
            "ticket.delegate",
            serde_json::json!({ "ticket": ticket_id }),
            None,
        );
        let step = make_step_with_tools(vec![tool_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert_eq!(promotion.tickets.len(), 1);
        assert_eq!(promotion.tickets[0].as_str(), ticket_id);
    }

    #[test]
    fn deduplicates_across_different_tool_types() {
        let artifact_id = "ART-000000000007";

        let artifact_call = completed_tool_call(
            "artifact.store",
            serde_json::json!({ "artifact": artifact_id }),
            None,
        );

        let evidence_call = completed_tool_call(
            "evidence.attach",
            serde_json::json!({ "artifact": artifact_id }),
            None,
        );

        let step = make_step_with_tools(vec![artifact_call, evidence_call]);
        let session = make_session(vec![step]);
        let promotion = session.promote();

        assert_eq!(promotion.artifacts.len(), 1);
        assert_eq!(promotion.artifacts[0].as_str(), artifact_id);
    }
}
