//! Typed payloads: one struct per [`EventKind`], plus the [`Payload`] enum that carries any of
//! them at runtime.
//!
//! `events.payload` in SQLite stores only the inner struct's JSON object — the `kind` column is
//! the discriminator, so payload JSON is never itself tagged. [`Payload::to_json`] and
//! [`Payload::from_json`] are the (kind, JSON) <-> typed-struct bridge; [`Payload::kind`] and
//! the generated `From<Struct> for Payload` / `Payload::as_*` methods are the typed
//! construction and extraction helpers `SPEC.md` §3.2 asks for.
//!
//! Field choices favor concrete `tm-types` identifiers over free strings so payloads stay
//! machine-checkable; open-ended detail (e.g. an arbitrary diff) is `serde_json::Value`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tm_types::{
    ArtifactId, Authority, DecisionId, LeaseId, MilestoneId, ParticipantId, SessionId, TicketId,
    Timestamp,
};

use crate::kind::EventKind;

/// Defines one payload struct plus its `Payload` enum arm, `From` impl and typed accessor per
/// event kind, and the `Payload::kind` / `to_json` / `from_json` dispatch tables that tie every
/// arm to its [`EventKind`]. Every arm here is pure data plumbing — there is no per-kind logic
/// to design, so (unlike `log.rs`/`stream.rs`) this is written out in full rather than left as
/// `todo!()`.
macro_rules! payload_kinds {
    ($(
        $doc:literal, $struct:ident, $variant:ident, $kind:ident, $accessor:ident, {
            $($field:ident : $ty:ty),* $(,)?
        }
    );* $(;)?) => {
        $(
            #[doc = $doc]
            #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
            pub struct $struct {
                $(
                    #[allow(missing_docs)]
                    pub $field: $ty,
                )*
            }

            impl From<$struct> for Payload {
                fn from(p: $struct) -> Self {
                    Payload::$variant(p)
                }
            }
        )*

        #[doc = "A typed event payload; one variant per [`EventKind`]."]
        #[derive(Debug, Clone, PartialEq)]
        pub enum Payload {
            $(
                #[doc = $doc]
                $variant($struct),
            )*
        }

        impl Payload {
            /// The [`EventKind`] this payload belongs to.
            pub fn kind(&self) -> EventKind {
                match self {
                    $(Payload::$variant(_) => EventKind::$kind,)*
                }
            }

            /// Serialize the inner struct to the JSON object stored in `events.payload`.
            pub fn to_json(&self) -> tm_types::Result<Value> {
                match self {
                    $(Payload::$variant(p) => Ok(serde_json::to_value(p)?),)*
                }
            }

            /// Deserialize a stored JSON payload into its typed form, given the `kind` column
            /// that says which struct shape to expect.
            pub fn from_json(kind: EventKind, value: Value) -> tm_types::Result<Self> {
                match kind {
                    $(EventKind::$kind => Ok(Payload::$variant(serde_json::from_value(value)?)),)*
                }
            }

            $(
                #[doc = concat!("Extract the payload if it is a [`", stringify!($struct), "`].")]
                pub fn $accessor(&self) -> Option<&$struct> {
                    match self {
                        Payload::$variant(p) => Some(p),
                        #[allow(unreachable_patterns)]
                        _ => None,
                    }
                }
            )*
        }
    };
}

payload_kinds! {
    "Payload for `project.created`.", ProjectCreatedPayload, ProjectCreated, ProjectCreated, as_project_created, {
        name: String,
        root: String,
    };
    "Payload for `project.attached`.", ProjectAttachedPayload, ProjectAttached, ProjectAttached, as_project_attached, {
        name: String,
        root: String,
    };
    "Payload for `ticket.created`.", TicketCreatedPayload, TicketCreated, TicketCreated, as_ticket_created, {
        ticket: TicketId,
        title: String,
        parent: Option<TicketId>,
    };
    "Payload for `ticket.updated`; `fields` is a JSON object of changed-field -> new-value.", TicketUpdatedPayload, TicketUpdated, TicketUpdated, as_ticket_updated, {
        ticket: TicketId,
        fields: Value,
    };
    "Payload for `ticket.state_changed`.", TicketStateChangedPayload, TicketStateChanged, TicketStateChanged, as_ticket_state_changed, {
        ticket: TicketId,
        from: String,
        to: String,
    };
    "Payload for `ticket.dependency_added`.", TicketDependencyAddedPayload, TicketDependencyAdded, TicketDependencyAdded, as_ticket_dependency_added, {
        ticket: TicketId,
        depends_on: TicketId,
    };
    "Payload for `ticket.dependency_removed`.", TicketDependencyRemovedPayload, TicketDependencyRemoved, TicketDependencyRemoved, as_ticket_dependency_removed, {
        ticket: TicketId,
        depends_on: TicketId,
    };
    "Payload for `ticket.child_added`.", TicketChildAddedPayload, TicketChildAdded, TicketChildAdded, as_ticket_child_added, {
        parent: TicketId,
        child: TicketId,
    };
    "Payload for `ticket.leased`.", TicketLeasedPayload, TicketLeased, TicketLeased, as_ticket_leased, {
        ticket: TicketId,
        lease: LeaseId,
        holder: ParticipantId,
        expires_at: Timestamp,
    };
    "Payload for `ticket.heartbeat`.", TicketHeartbeatPayload, TicketHeartbeat, TicketHeartbeat, as_ticket_heartbeat, {
        ticket: TicketId,
        lease: LeaseId,
        expires_at: Timestamp,
    };
    "Payload for `ticket.lease_expired`.", TicketLeaseExpiredPayload, TicketLeaseExpired, TicketLeaseExpired, as_ticket_lease_expired, {
        ticket: TicketId,
        lease: LeaseId,
    };
    "Payload for `ticket.lease_released`.", TicketLeaseReleasedPayload, TicketLeaseReleased, TicketLeaseReleased, as_ticket_lease_released, {
        ticket: TicketId,
        lease: LeaseId,
    };
    "Payload for `ticket.delegated`.", TicketDelegatedPayload, TicketDelegated, TicketDelegated, as_ticket_delegated, {
        parent: TicketId,
        child: TicketId,
        delegate: ParticipantId,
    };
    "Payload for `ticket.submitted`.", TicketSubmittedPayload, TicketSubmitted, TicketSubmitted, as_ticket_submitted, {
        ticket: TicketId,
        summary: String,
    };
    "Payload for `ticket.verified`.", TicketVerifiedPayload, TicketVerified, TicketVerified, as_ticket_verified, {
        ticket: TicketId,
        verifier: TicketId,
    };
    "Payload for `ticket.verification_failed`.", TicketVerificationFailedPayload, TicketVerificationFailed, TicketVerificationFailed, as_ticket_verification_failed, {
        ticket: TicketId,
        verifier: TicketId,
        reason: String,
    };
    "Payload for `ticket.audited`.", TicketAuditedPayload, TicketAudited, TicketAudited, as_ticket_audited, {
        ticket: TicketId,
        auditor: TicketId,
    };
    "Payload for `ticket.audit_rejected`.", TicketAuditRejectedPayload, TicketAuditRejected, TicketAuditRejected, as_ticket_audit_rejected, {
        ticket: TicketId,
        auditor: TicketId,
        reason: String,
    };
    "Payload for `ticket.closed`.", TicketClosedPayload, TicketClosed, TicketClosed, as_ticket_closed, {
        ticket: TicketId,
        reason: Option<String>,
    };
    "Payload for `ticket.cancelled`.", TicketCancelledPayload, TicketCancelled, TicketCancelled, as_ticket_cancelled, {
        ticket: TicketId,
        reason: Option<String>,
    };
    "Payload for `ticket.reopened`.", TicketReopenedPayload, TicketReopened, TicketReopened, as_ticket_reopened, {
        ticket: TicketId,
        reason: Option<String>,
    };
    "Payload for `ticket.failed`.", TicketFailedPayload, TicketFailed, TicketFailed, as_ticket_failed, {
        ticket: TicketId,
        reason: String,
    };
    "Payload for `ticket.retry_scheduled`.", TicketRetryScheduledPayload, TicketRetryScheduled, TicketRetryScheduled, as_ticket_retry_scheduled, {
        ticket: TicketId,
        attempt: u32,
        not_before: Timestamp,
    };
    "Payload for `ticket.escalated`.", TicketEscalatedPayload, TicketEscalated, TicketEscalated, as_ticket_escalated, {
        ticket: TicketId,
        reason: String,
    };
    "Payload for `ticket.budget_exhausted`.", TicketBudgetExhaustedPayload, TicketBudgetExhausted, TicketBudgetExhausted, as_ticket_budget_exhausted, {
        ticket: TicketId,
        dimension: String,
        limit: u64,
        spent: u64,
    };
    "Payload for `decision.created`.", DecisionCreatedPayload, DecisionCreated, DecisionCreated, as_decision_created, {
        decision: DecisionId,
        ticket: Option<TicketId>,
        summary: String,
    };
    "Payload for `decision.superseded`.", DecisionSupersededPayload, DecisionSuperseded, DecisionSuperseded, as_decision_superseded, {
        decision: DecisionId,
        superseded_by: DecisionId,
    };
    "Payload for `authority.granted`.", AuthorityGrantedPayload, AuthorityGranted, AuthorityGranted, as_authority_granted, {
        subject: ParticipantId,
        ticket: Option<TicketId>,
        grant: Authority,
    };
    "Payload for `authority.delegated`.", AuthorityDelegatedPayload, AuthorityDelegated, AuthorityDelegated, as_authority_delegated, {
        from: ParticipantId,
        to: ParticipantId,
        ticket: Option<TicketId>,
        grant: Authority,
    };
    "Payload for `authority.revoked`.", AuthorityRevokedPayload, AuthorityRevoked, AuthorityRevoked, as_authority_revoked, {
        subject: ParticipantId,
        ticket: Option<TicketId>,
    };
    "Payload for `authority.reverted`; `to_seq` names the log position being restored to.", AuthorityRevertedPayload, AuthorityReverted, AuthorityReverted, as_authority_reverted, {
        subject: ParticipantId,
        ticket: Option<TicketId>,
        to_seq: u64,
    };
    "Payload for `resource.claimed`.", ResourceClaimedPayload, ResourceClaimed, ResourceClaimed, as_resource_claimed, {
        resource: String,
        holder: ParticipantId,
    };
    "Payload for `resource.released`.", ResourceReleasedPayload, ResourceReleased, ResourceReleased, as_resource_released, {
        resource: String,
        holder: ParticipantId,
    };
    "Payload for `resource.conflict_detected`.", ResourceConflictDetectedPayload, ResourceConflictDetected, ResourceConflictDetected, as_resource_conflict_detected, {
        resource: String,
        holders: Vec<ParticipantId>,
    };
    "Payload for `artifact.created`.", ArtifactCreatedPayload, ArtifactCreated, ArtifactCreated, as_artifact_created, {
        artifact: ArtifactId,
        ticket: Option<TicketId>,
        path: String,
        media_type: String,
    };
    "Payload for `command.started`.", CommandStartedPayload, CommandStarted, CommandStarted, as_command_started, {
        command: String,
        ticket: Option<TicketId>,
        session: Option<SessionId>,
    };
    "Payload for `command.completed`.", CommandCompletedPayload, CommandCompleted, CommandCompleted, as_command_completed, {
        command: String,
        ticket: Option<TicketId>,
        session: Option<SessionId>,
        exit_code: i32,
        duration_ms: u64,
    };
    "Payload for `milestone.created`.", MilestoneCreatedPayload, MilestoneCreated, MilestoneCreated, as_milestone_created, {
        milestone: MilestoneId,
        title: String,
    };
    "Payload for `milestone.closed`.", MilestoneClosedPayload, MilestoneClosed, MilestoneClosed, as_milestone_closed, {
        milestone: MilestoneId,
    };
    "Payload for `milestone.reopened`.", MilestoneReopenedPayload, MilestoneReopened, MilestoneReopened, as_milestone_reopened, {
        milestone: MilestoneId,
    };
    "Payload for `session.started`.", SessionStartedPayload, SessionStarted, SessionStarted, as_session_started, {
        session: SessionId,
        participant: ParticipantId,
    };
    "Payload for `session.joined`.", SessionJoinedPayload, SessionJoined, SessionJoined, as_session_joined, {
        session: SessionId,
        participant: ParticipantId,
    };
    "Payload for `session.left`.", SessionLeftPayload, SessionLeft, SessionLeft, as_session_left, {
        session: SessionId,
        participant: ParticipantId,
    };
    "Payload for `session.ended`.", SessionEndedPayload, SessionEnded, SessionEnded, as_session_ended, {
        session: SessionId,
    };
    "Payload for `presence.updated`.", PresenceUpdatedPayload, PresenceUpdated, PresenceUpdated, as_presence_updated, {
        participant: ParticipantId,
        status: String,
    };
    "Payload for `comment.created`.", CommentCreatedPayload, CommentCreated, CommentCreated, as_comment_created, {
        ticket: Option<TicketId>,
        author: ParticipantId,
        body: String,
    };
    "Payload for `approval.requested`.", ApprovalRequestedPayload, ApprovalRequested, ApprovalRequested, as_approval_requested, {
        ticket: Option<TicketId>,
        requested_of: ParticipantId,
        note: String,
    };
    "Payload for `approval.decided`.", ApprovalDecidedPayload, ApprovalDecided, ApprovalDecided, as_approval_decided, {
        ticket: Option<TicketId>,
        decided_by: ParticipantId,
        approved: bool,
        note: Option<String>,
    };
    "Payload for `provider.selected`.", ProviderSelectedPayload, ProviderSelected, ProviderSelected, as_provider_selected, {
        role: String,
        provider: String,
        model: String,
    };
    "Payload for `provider.exhausted`.", ProviderExhaustedPayload, ProviderExhausted, ProviderExhausted, as_provider_exhausted, {
        role: String,
        provider: String,
        reason: String,
    };
    "Payload for `provider.degraded`.", ProviderDegradedPayload, ProviderDegraded, ProviderDegraded, as_provider_degraded, {
        provider: String,
        reason: String,
    };
    "Payload for `provider.recovered`.", ProviderRecoveredPayload, ProviderRecovered, ProviderRecovered, as_provider_recovered, {
        provider: String,
    };
    "Payload for `executor.failed`.", ExecutorFailedPayload, ExecutorFailed, ExecutorFailed, as_executor_failed, {
        ticket: Option<TicketId>,
        reason: String,
    };
    "Payload for `usage.recorded`.", UsageRecordedPayload, UsageRecorded, UsageRecorded, as_usage_recorded, {
        ticket: Option<TicketId>,
        session: Option<SessionId>,
        tokens: u64,
        dollars_micros: u64,
        wall_seconds: u64,
    };
    "Payload for `doc.registered`.", DocRegisteredPayload, DocRegistered, DocRegistered, as_doc_registered, {
        path: String,
        ticket: Option<TicketId>,
    };
    "Payload for `doc.generated`.", DocGeneratedPayload, DocGenerated, DocGenerated, as_doc_generated, {
        path: String,
        ticket: Option<TicketId>,
    };
    "Payload for `doc.invalidated`.", DocInvalidatedPayload, DocInvalidated, DocInvalidated, as_doc_invalidated, {
        path: String,
        reason: String,
    };
    "Payload for `doc.reconciled`.", DocReconciledPayload, DocReconciled, DocReconciled, as_doc_reconciled, {
        path: String,
    };
    "Payload for `index.updated`.", IndexUpdatedPayload, IndexUpdated, IndexUpdated, as_index_updated, {
        path: String,
        entries: u64,
    };
    "Payload for `harness.changed`.", HarnessChangedPayload, HarnessChanged, HarnessChanged, as_harness_changed, {
        field: String,
        from: Value,
        to: Value,
    };
    "Payload for `harness.benchmarked`.", HarnessBenchmarkedPayload, HarnessBenchmarked, HarnessBenchmarked, as_harness_benchmarked, {
        suite: String,
        score: f64,
    };
    "Payload for `harness.promoted`.", HarnessPromotedPayload, HarnessPromoted, HarnessPromoted, as_harness_promoted, {
        candidate: String,
    };
    "Payload for `genesis.started`.", GenesisStartedPayload, GenesisStarted, GenesisStarted, as_genesis_started, {
        project: String,
    };
    "Payload for `genesis.stage_entered`.", GenesisStageEnteredPayload, GenesisStageEntered, GenesisStageEntered, as_genesis_stage_entered, {
        stage: String,
    };
    "Payload for `genesis.stage_completed`.", GenesisStageCompletedPayload, GenesisStageCompleted, GenesisStageCompleted, as_genesis_stage_completed, {
        stage: String,
    };
    "Payload for `genesis.assumption_recorded`.", GenesisAssumptionRecordedPayload, GenesisAssumptionRecorded, GenesisAssumptionRecorded, as_genesis_assumption_recorded, {
        assumption: String,
    };
    "Payload for `genesis.maturity_evaluated`.", GenesisMaturityEvaluatedPayload, GenesisMaturityEvaluated, GenesisMaturityEvaluated, as_genesis_maturity_evaluated, {
        stage: String,
        score: f64,
    };
    "Payload for `genesis.completed`.", GenesisCompletedPayload, GenesisCompleted, GenesisCompleted, as_genesis_completed, {
    };
    "Payload for `mirror.linked`.", MirrorLinkedPayload, MirrorLinked, MirrorLinked, as_mirror_linked, {
        remote: String,
    };
    "Payload for `mirror.pushed`.", MirrorPushedPayload, MirrorPushed, MirrorPushed, as_mirror_pushed, {
        remote: String,
        reference: String,
    };
    "Payload for `mirror.pulled`.", MirrorPulledPayload, MirrorPulled, MirrorPulled, as_mirror_pulled, {
        remote: String,
        reference: String,
    };
    "Payload for `effect.journaled` (`SPEC.md` §21.5).", EffectJournaledPayload, EffectJournaled, EffectJournaled, as_effect_journaled, {
        key: String,
        ticket: TicketId,
        attempt: u32,
        kind: String,
    };
    "Payload for `effect.completed`.", EffectCompletedPayload, EffectCompleted, EffectCompleted, as_effect_completed, {
        key: String,
        ticket: TicketId,
        receipt_artifact: Option<String>,
    };
    "Payload for `effect.failed`.", EffectFailedPayload, EffectFailed, EffectFailed, as_effect_failed, {
        key: String,
        ticket: TicketId,
        reason: String,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_mapping_and_json_round_trip_agree_for_every_variant() {
        let p = Payload::from(MilestoneClosedPayload {
            milestone: MilestoneId::new("M-1").unwrap(),
        });
        assert_eq!(p.kind(), EventKind::MilestoneClosed);
        let json = p.to_json().unwrap();
        let back = Payload::from_json(EventKind::MilestoneClosed, json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn accessors_return_none_for_the_wrong_variant() {
        let p = Payload::from(MilestoneClosedPayload {
            milestone: MilestoneId::new("M-1").unwrap(),
        });
        assert!(p.as_milestone_closed().is_some());
        assert!(p.as_milestone_reopened().is_none());
    }

    #[test]
    fn payload_json_is_a_bare_object_with_no_enum_tag() {
        let p = Payload::from(MirrorLinkedPayload {
            remote: "origin".into(),
        });
        let json = p.to_json().unwrap();
        assert!(json.is_object());
        assert!(json.get("MirrorLinked").is_none());
        assert_eq!(json.get("remote").unwrap(), "origin");
    }

    #[test]
    fn ticket_created_round_trip_with_parent() {
        let parent = TicketId::new("T-1").unwrap();
        let ticket = TicketId::new("T-2").unwrap();
        let original = Payload::from(TicketCreatedPayload {
            ticket: ticket.clone(),
            title: "Fix login bug".into(),
            parent: Some(parent.clone()),
        });

        assert_eq!(original.kind(), EventKind::TicketCreated);
        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::TicketCreated, json).unwrap();
        assert_eq!(restored, original);

        let inner = restored.as_ticket_created().unwrap();
        assert_eq!(inner.ticket, ticket);
        assert_eq!(inner.title, "Fix login bug");
        assert_eq!(inner.parent, Some(parent));
    }

    #[test]
    fn ticket_created_round_trip_without_parent() {
        let ticket = TicketId::new("T-1").unwrap();
        let original = Payload::from(TicketCreatedPayload {
            ticket: ticket.clone(),
            title: "Implement feature".into(),
            parent: None,
        });

        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::TicketCreated, json).unwrap();

        let inner = restored.as_ticket_created().unwrap();
        assert!(inner.parent.is_none());
    }

    #[test]
    fn ticket_closed_with_reason() {
        let ticket = TicketId::new("T-1").unwrap();
        let original = Payload::from(TicketClosedPayload {
            ticket: ticket.clone(),
            reason: Some("Fixed in production".into()),
        });

        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::TicketClosed, json).unwrap();

        let inner = restored.as_ticket_closed().unwrap();
        assert_eq!(inner.reason, Some("Fixed in production".into()));
    }

    #[test]
    fn ticket_closed_without_reason() {
        let ticket = TicketId::new("T-1").unwrap();
        let original = Payload::from(TicketClosedPayload {
            ticket: ticket.clone(),
            reason: None,
        });

        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::TicketClosed, json).unwrap();

        let inner = restored.as_ticket_closed().unwrap();
        assert!(inner.reason.is_none());
    }

    #[test]
    fn deserialization_fails_for_wrong_json_type() {
        let wrong_type = serde_json::json!(["not", "an", "object"]);
        let result = Payload::from_json(EventKind::MilestoneCreated, wrong_type);
        assert!(result.is_err());
    }

    #[test]
    fn deserialization_fails_for_missing_required_field() {
        let incomplete = serde_json::json!({
            "title": "Missing milestone ID"
        });
        let result = Payload::from_json(EventKind::MilestoneCreated, incomplete);
        assert!(result.is_err());
    }

    #[test]
    fn deserialization_fails_for_invalid_id_format() {
        let invalid_id = serde_json::json!({
            "milestone": "INVALID-ID-FORMAT",
            "title": "Test"
        });
        let result = Payload::from_json(EventKind::MilestoneCreated, invalid_id);
        assert!(result.is_err());
    }

    #[test]
    fn decision_created_with_ticket_reference() {
        let decision = DecisionId::new("D-1").unwrap();
        let ticket = TicketId::new("T-1").unwrap();
        let original = Payload::from(DecisionCreatedPayload {
            decision: decision.clone(),
            ticket: Some(ticket.clone()),
            summary: "Use async Rust runtime".into(),
        });

        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::DecisionCreated, json).unwrap();

        let inner = restored.as_decision_created().unwrap();
        assert_eq!(inner.decision, decision);
        assert_eq!(inner.ticket, Some(ticket));
        assert_eq!(inner.summary, "Use async Rust runtime");
    }

    #[test]
    fn decision_created_without_ticket_reference() {
        let decision = DecisionId::new("D-1").unwrap();
        let original = Payload::from(DecisionCreatedPayload {
            decision: decision.clone(),
            ticket: None,
            summary: "Project governance".into(),
        });

        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::DecisionCreated, json).unwrap();

        let inner = restored.as_decision_created().unwrap();
        assert!(inner.ticket.is_none());
    }

    #[test]
    fn resource_conflict_with_multiple_holders() {
        let p1 = ParticipantId::new("agent:alice/1").unwrap();
        let p2 = ParticipantId::new("agent:bob/2").unwrap();
        let original = Payload::from(ResourceConflictDetectedPayload {
            resource: "database_connection".into(),
            holders: vec![p1.clone(), p2.clone()],
        });

        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::ResourceConflictDetected, json).unwrap();

        let inner = restored.as_resource_conflict_detected().unwrap();
        assert_eq!(inner.holders.len(), 2);
        assert!(inner.holders.contains(&p1));
        assert!(inner.holders.contains(&p2));
    }

    #[test]
    fn artifact_created_with_embedded_json_value() {
        let artifact = ArtifactId::new("ART-deadbeefcafe").unwrap();
        let ticket = TicketId::new("T-1").unwrap();
        let metadata = serde_json::json!({
            "size_bytes": 1024,
            "tags": ["production", "audit"]
        });
        let original = Payload::from(ArtifactCreatedPayload {
            artifact: artifact.clone(),
            ticket: Some(ticket),
            path: "/artifacts/report.pdf".into(),
            media_type: "application/pdf".into(),
        });

        let mut json = original.to_json().unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("metadata".into(), metadata.clone());
        let restored = Payload::from_json(EventKind::ArtifactCreated, json).unwrap();

        let inner = restored.as_artifact_created().unwrap();
        assert_eq!(inner.artifact, artifact);
        assert_eq!(inner.media_type, "application/pdf");
    }

    #[test]
    fn ticket_updated_with_complex_fields_json() {
        let ticket = TicketId::new("T-1").unwrap();
        let fields = serde_json::json!({
            "status": "in_progress",
            "assigned_to": "alice",
            "priority": 5,
            "tags": ["critical", "backend"]
        });
        let original = Payload::from(TicketUpdatedPayload {
            ticket: ticket.clone(),
            fields: fields.clone(),
        });

        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::TicketUpdated, json).unwrap();

        let inner = restored.as_ticket_updated().unwrap();
        assert_eq!(inner.fields, fields);
    }

    #[test]
    fn harness_changed_preserves_arbitrary_json_values() {
        let from = serde_json::json!({"version": "1.0", "debug": false});
        let to = serde_json::json!({"version": "2.0", "debug": true, "features": ["tracing"]});
        let original = Payload::from(HarnessChangedPayload {
            field: "config".into(),
            from: from.clone(),
            to: to.clone(),
        });

        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::HarnessChanged, json).unwrap();

        let inner = restored.as_harness_changed().unwrap();
        assert_eq!(inner.from, from);
        assert_eq!(inner.to, to);
    }

    #[test]
    fn genesis_completed_has_no_fields() {
        let original = Payload::from(GenesisCompletedPayload {});

        assert_eq!(original.kind(), EventKind::GenesisCompleted);
        let json = original.to_json().unwrap();
        assert!(json.is_object());

        let restored = Payload::from_json(EventKind::GenesisCompleted, json).unwrap();
        assert_eq!(restored, original);
        assert!(restored.as_genesis_completed().is_some());
    }

    #[test]
    fn multiple_accessor_methods_on_same_payload() {
        let ticket = TicketId::new("T-1").unwrap();
        let p = Payload::from(TicketClosedPayload {
            ticket,
            reason: Some("resolved".into()),
        });

        assert!(p.as_ticket_closed().is_some());
        assert!(p.as_ticket_cancelled().is_none());
        assert!(p.as_ticket_reopened().is_none());
        assert!(p.as_ticket_created().is_none());
    }

    #[test]
    fn command_completed_with_timestamps() {
        let session = SessionId::new("S-1").unwrap();
        let original = Payload::from(CommandCompletedPayload {
            command: "cargo test".into(),
            ticket: None,
            session: Some(session),
            exit_code: 0,
            duration_ms: 5000,
        });

        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::CommandCompleted, json).unwrap();

        let inner = restored.as_command_completed().unwrap();
        assert_eq!(inner.exit_code, 0);
        assert_eq!(inner.duration_ms, 5000);
    }

    #[test]
    fn usage_recorded_with_budget_values() {
        let original = Payload::from(UsageRecordedPayload {
            ticket: None,
            session: None,
            tokens: 1000,
            dollars_micros: 50000,
            wall_seconds: 30,
        });

        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::UsageRecorded, json).unwrap();

        let inner = restored.as_usage_recorded().unwrap();
        assert_eq!(inner.tokens, 1000);
        assert_eq!(inner.dollars_micros, 50000);
        assert_eq!(inner.wall_seconds, 30);
    }

    #[test]
    fn approval_decided_with_optional_note() {
        let participant = ParticipantId::new("human:alice").unwrap();
        let original = Payload::from(ApprovalDecidedPayload {
            ticket: None,
            decided_by: participant,
            approved: true,
            note: Some("Looks good to me".into()),
        });

        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::ApprovalDecided, json).unwrap();

        let inner = restored.as_approval_decided().unwrap();
        assert!(inner.approved);
        assert_eq!(inner.note, Some("Looks good to me".into()));
    }

    #[test]
    fn approval_decided_without_note() {
        let participant = ParticipantId::new("human:bob").unwrap();
        let original = Payload::from(ApprovalDecidedPayload {
            ticket: None,
            decided_by: participant,
            approved: false,
            note: None,
        });

        let json = original.to_json().unwrap();
        let restored = Payload::from_json(EventKind::ApprovalDecided, json).unwrap();

        let inner = restored.as_approval_decided().unwrap();
        assert!(!inner.approved);
        assert!(inner.note.is_none());
    }
}
