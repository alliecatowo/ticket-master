//! The closed catalogue of event kinds (`SPEC.md` §3.2).
//!
//! `EventKind` is the only vocabulary the log speaks: every [`crate::event::Event`] carries
//! exactly one, and every kind has exactly one typed payload shape in [`crate::payload`].
//! Forward compatibility is a version bump, not leniency — [`EventKind::from_str`] rejects
//! anything outside this list rather than accepting it as opaque data.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use tm_types::TmError;

/// A broad grouping of [`EventKind`]s, derived from the dotted name's first segment.
///
/// Used for coarse filtering (e.g. "show me everything about tickets") without matching on
/// every individual kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventCategory {
    /// `project.*`
    Project,
    /// `ticket.*`
    Ticket,
    /// `decision.*`
    Decision,
    /// `authority.*`
    Authority,
    /// `resource.*`
    Resource,
    /// `artifact.*`
    Artifact,
    /// `command.*`
    Command,
    /// `milestone.*`
    Milestone,
    /// `session.*`
    Session,
    /// `presence.*`
    Presence,
    /// `comment.*`
    Comment,
    /// `approval.*`
    Approval,
    /// `provider.*`
    Provider,
    /// `executor.*`
    Executor,
    /// `usage.*`
    Usage,
    /// `doc.*`
    Doc,
    /// `index.*`
    Index,
    /// `harness.*`
    Harness,
    /// `genesis.*`
    Genesis,
    /// `mirror.*`
    Mirror,
    /// `effect.*`
    Effect,
    /// `goal.*`
    Goal,
    /// `classify.*`
    Classify,
}

/// The closed set of event kinds a project's log may contain.
///
/// Every variant serializes to (and parses from) the dotted name given in `SPEC.md` §3.2, via
/// `#[serde(rename = "...")]`. The enum is exhaustive by design: unknown kinds are a hard parse
/// error everywhere in the system, including at the SQLite boundary (`schema.rs`) and CLI input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EventKind {
    /// `project.created`
    #[serde(rename = "project.created")]
    ProjectCreated,
    /// `project.attached`
    #[serde(rename = "project.attached")]
    ProjectAttached,
    /// `ticket.created`
    #[serde(rename = "ticket.created")]
    TicketCreated,
    /// `ticket.updated`
    #[serde(rename = "ticket.updated")]
    TicketUpdated,
    /// `ticket.state_changed`
    #[serde(rename = "ticket.state_changed")]
    TicketStateChanged,
    /// `ticket.dependency_added`
    #[serde(rename = "ticket.dependency_added")]
    TicketDependencyAdded,
    /// `ticket.dependency_removed`
    #[serde(rename = "ticket.dependency_removed")]
    TicketDependencyRemoved,
    /// `ticket.child_added`
    #[serde(rename = "ticket.child_added")]
    TicketChildAdded,
    /// `ticket.leased`
    #[serde(rename = "ticket.leased")]
    TicketLeased,
    /// `ticket.heartbeat`
    #[serde(rename = "ticket.heartbeat")]
    TicketHeartbeat,
    /// `ticket.lease_expired`
    #[serde(rename = "ticket.lease_expired")]
    TicketLeaseExpired,
    /// `ticket.lease_released`
    #[serde(rename = "ticket.lease_released")]
    TicketLeaseReleased,
    /// `ticket.delegated`
    #[serde(rename = "ticket.delegated")]
    TicketDelegated,
    /// `ticket.submitted`
    #[serde(rename = "ticket.submitted")]
    TicketSubmitted,
    /// `ticket.verified`
    #[serde(rename = "ticket.verified")]
    TicketVerified,
    /// `ticket.verification_failed`
    #[serde(rename = "ticket.verification_failed")]
    TicketVerificationFailed,
    /// `ticket.audited`
    #[serde(rename = "ticket.audited")]
    TicketAudited,
    /// `ticket.audit_rejected`
    #[serde(rename = "ticket.audit_rejected")]
    TicketAuditRejected,
    /// `ticket.closed`
    #[serde(rename = "ticket.closed")]
    TicketClosed,
    /// `ticket.cancelled`
    #[serde(rename = "ticket.cancelled")]
    TicketCancelled,
    /// `ticket.reopened`
    #[serde(rename = "ticket.reopened")]
    TicketReopened,
    /// `ticket.failed`
    #[serde(rename = "ticket.failed")]
    TicketFailed,
    /// `ticket.retry_scheduled`
    #[serde(rename = "ticket.retry_scheduled")]
    TicketRetryScheduled,
    /// `ticket.escalated`
    #[serde(rename = "ticket.escalated")]
    TicketEscalated,
    /// `ticket.budget_exhausted`
    #[serde(rename = "ticket.budget_exhausted")]
    TicketBudgetExhausted,
    /// `ticket.budget_handoff` (`SPEC.md` §31.3, `docs/audit-2026-09-18-fable.md` B-10): a
    /// worker approaching its budget ceiling handed the ticket back clean via
    /// `Trigger::BudgetHandoff` — distinct from `ticket.budget_exhausted`/`ticket.failed`, this
    /// is never a failure: it does not enter `Recovery` and does not consume a retry.
    #[serde(rename = "ticket.budget_handoff")]
    TicketBudgetHandoff,
    /// `ticket.forked` (`docs/decisions/D-008-ticket-checkpoint-fork.md`): pure provenance —
    /// this ticket's starting state was computed from `source`'s materialized state as of
    /// `source_seq`, not typed by a user or another executor. Always accompanied by its own
    /// `ticket.created`/`ticket.updated` pair (this event names no ticket-shape fields itself;
    /// see that decision doc for why the state and the provenance are deliberately two separate
    /// catalogued kinds rather than one payload carrying both).
    #[serde(rename = "ticket.forked")]
    TicketForked,
    /// `decision.created`
    #[serde(rename = "decision.created")]
    DecisionCreated,
    /// `decision.superseded`
    #[serde(rename = "decision.superseded")]
    DecisionSuperseded,
    /// `authority.granted`
    #[serde(rename = "authority.granted")]
    AuthorityGranted,
    /// `authority.delegated`
    #[serde(rename = "authority.delegated")]
    AuthorityDelegated,
    /// `authority.revoked`
    #[serde(rename = "authority.revoked")]
    AuthorityRevoked,
    /// `authority.reverted`
    #[serde(rename = "authority.reverted")]
    AuthorityReverted,
    /// `resource.claimed`
    #[serde(rename = "resource.claimed")]
    ResourceClaimed,
    /// `resource.released`
    #[serde(rename = "resource.released")]
    ResourceReleased,
    /// `resource.conflict_detected`
    #[serde(rename = "resource.conflict_detected")]
    ResourceConflictDetected,
    /// `artifact.created`
    #[serde(rename = "artifact.created")]
    ArtifactCreated,
    /// `command.started`
    #[serde(rename = "command.started")]
    CommandStarted,
    /// `command.completed`
    #[serde(rename = "command.completed")]
    CommandCompleted,
    /// `milestone.created`
    #[serde(rename = "milestone.created")]
    MilestoneCreated,
    /// `milestone.closed`
    #[serde(rename = "milestone.closed")]
    MilestoneClosed,
    /// `milestone.reopened`
    #[serde(rename = "milestone.reopened")]
    MilestoneReopened,
    /// `session.started`
    #[serde(rename = "session.started")]
    SessionStarted,
    /// `session.joined`
    #[serde(rename = "session.joined")]
    SessionJoined,
    /// `session.left`
    #[serde(rename = "session.left")]
    SessionLeft,
    /// `session.ended`
    #[serde(rename = "session.ended")]
    SessionEnded,
    /// `presence.updated`
    #[serde(rename = "presence.updated")]
    PresenceUpdated,
    /// `comment.created`
    #[serde(rename = "comment.created")]
    CommentCreated,
    /// `approval.requested`
    #[serde(rename = "approval.requested")]
    ApprovalRequested,
    /// `approval.decided`
    #[serde(rename = "approval.decided")]
    ApprovalDecided,
    /// `provider.selected`
    #[serde(rename = "provider.selected")]
    ProviderSelected,
    /// `provider.exhausted`
    #[serde(rename = "provider.exhausted")]
    ProviderExhausted,
    /// `provider.degraded`
    #[serde(rename = "provider.degraded")]
    ProviderDegraded,
    /// `provider.recovered`
    #[serde(rename = "provider.recovered")]
    ProviderRecovered,
    /// `executor.failed`
    #[serde(rename = "executor.failed")]
    ExecutorFailed,
    /// `usage.recorded`
    #[serde(rename = "usage.recorded")]
    UsageRecorded,
    /// `doc.registered`
    #[serde(rename = "doc.registered")]
    DocRegistered,
    /// `doc.generated`
    #[serde(rename = "doc.generated")]
    DocGenerated,
    /// `doc.invalidated`
    #[serde(rename = "doc.invalidated")]
    DocInvalidated,
    /// `doc.reconciled`
    #[serde(rename = "doc.reconciled")]
    DocReconciled,
    /// `index.updated`
    #[serde(rename = "index.updated")]
    IndexUpdated,
    /// `harness.changed`
    #[serde(rename = "harness.changed")]
    HarnessChanged,
    /// `harness.benchmarked`
    #[serde(rename = "harness.benchmarked")]
    HarnessBenchmarked,
    /// `harness.promoted`
    #[serde(rename = "harness.promoted")]
    HarnessPromoted,
    /// `genesis.started`
    #[serde(rename = "genesis.started")]
    GenesisStarted,
    /// `genesis.stage_entered`
    #[serde(rename = "genesis.stage_entered")]
    GenesisStageEntered,
    /// `genesis.stage_completed`
    #[serde(rename = "genesis.stage_completed")]
    GenesisStageCompleted,
    /// `genesis.assumption_recorded`
    #[serde(rename = "genesis.assumption_recorded")]
    GenesisAssumptionRecorded,
    /// `genesis.maturity_evaluated`
    #[serde(rename = "genesis.maturity_evaluated")]
    GenesisMaturityEvaluated,
    /// `genesis.completed`
    #[serde(rename = "genesis.completed")]
    GenesisCompleted,
    /// `mirror.linked`
    #[serde(rename = "mirror.linked")]
    MirrorLinked,
    /// `mirror.pushed`
    #[serde(rename = "mirror.pushed")]
    MirrorPushed,
    /// `mirror.pulled`
    #[serde(rename = "mirror.pulled")]
    MirrorPulled,
    /// `effect.journaled` (`SPEC.md` §21.5): an idempotency receipt journal opened for one
    /// `(ticket, attempt, kind, canonical_args)` effect, before it runs.
    #[serde(rename = "effect.journaled")]
    EffectJournaled,
    /// `effect.completed`: the journaled effect ran and its receipt was recorded.
    #[serde(rename = "effect.completed")]
    EffectCompleted,
    /// `effect.failed`: the journaled effect was attempted and is known to have failed
    /// (not "unknown" — a still-open journal with no completion event is the "unknown, crashed
    /// mid-effect" case `confirm()`-shaped recovery probes exist for).
    #[serde(rename = "effect.failed")]
    EffectFailed,
    /// `goal.set` (`SPEC.md` §29): a durable objective set for one ticket's live execution,
    /// distinct from the ticket's own `objective` field — a goal is the worker's decomposition of
    /// how it is getting there, not the ticket's static statement of the work.
    #[serde(rename = "goal.set")]
    GoalSet,
    /// `goal.step_added`: one step added to the current goal's live decomposition.
    #[serde(rename = "goal.step_added")]
    GoalStepAdded,
    /// `goal.step_completed`.
    #[serde(rename = "goal.step_completed")]
    GoalStepCompleted,
    /// `goal.reoriented`: the loop re-read goal state against observed state at the start of a
    /// step, rather than trusting only its own in-memory conversation history (`SPEC.md` §29's
    /// "re-orientation is explicit").
    #[serde(rename = "goal.reoriented")]
    GoalReoriented,
    /// `goal.claimed_complete`: the loop believes the goal is met. A claim only — `SPEC.md` §16's
    /// verification ladder decides whether it was; the goal loop never marks its own work
    /// verified.
    #[serde(rename = "goal.claimed_complete")]
    GoalClaimedComplete,
    /// `classify.decided` (D-020): one `DecisionProvider` call's outcome — site, backend, model,
    /// the redacted input/questions hashes, the answers and their calibrated confidence, the
    /// thresholds applied, the resulting disposition (e.g. `"shadow"` while D-020 is still
    /// shadow-only), latency and cost. Namespaced `classify.*`, not `decision.*` — that prefix
    /// already belongs to the unrelated `DecisionId` ticket-decision domain
    /// (`crates/tm-core/src/decision.rs`).
    #[serde(rename = "classify.decided")]
    ClassifyDecided,
}

/// Every kind, in catalogue order, for iteration in tests and diagnostics.
pub const ALL: &[EventKind] = &[
    EventKind::ProjectCreated,
    EventKind::ProjectAttached,
    EventKind::TicketCreated,
    EventKind::TicketUpdated,
    EventKind::TicketStateChanged,
    EventKind::TicketDependencyAdded,
    EventKind::TicketDependencyRemoved,
    EventKind::TicketChildAdded,
    EventKind::TicketLeased,
    EventKind::TicketHeartbeat,
    EventKind::TicketLeaseExpired,
    EventKind::TicketLeaseReleased,
    EventKind::TicketDelegated,
    EventKind::TicketSubmitted,
    EventKind::TicketVerified,
    EventKind::TicketVerificationFailed,
    EventKind::TicketAudited,
    EventKind::TicketAuditRejected,
    EventKind::TicketClosed,
    EventKind::TicketCancelled,
    EventKind::TicketReopened,
    EventKind::TicketFailed,
    EventKind::TicketRetryScheduled,
    EventKind::TicketEscalated,
    EventKind::TicketBudgetExhausted,
    EventKind::TicketBudgetHandoff,
    EventKind::TicketForked,
    EventKind::DecisionCreated,
    EventKind::DecisionSuperseded,
    EventKind::AuthorityGranted,
    EventKind::AuthorityDelegated,
    EventKind::AuthorityRevoked,
    EventKind::AuthorityReverted,
    EventKind::ResourceClaimed,
    EventKind::ResourceReleased,
    EventKind::ResourceConflictDetected,
    EventKind::ArtifactCreated,
    EventKind::CommandStarted,
    EventKind::CommandCompleted,
    EventKind::MilestoneCreated,
    EventKind::MilestoneClosed,
    EventKind::MilestoneReopened,
    EventKind::SessionStarted,
    EventKind::SessionJoined,
    EventKind::SessionLeft,
    EventKind::SessionEnded,
    EventKind::PresenceUpdated,
    EventKind::CommentCreated,
    EventKind::ApprovalRequested,
    EventKind::ApprovalDecided,
    EventKind::ProviderSelected,
    EventKind::ProviderExhausted,
    EventKind::ProviderDegraded,
    EventKind::ProviderRecovered,
    EventKind::ExecutorFailed,
    EventKind::UsageRecorded,
    EventKind::DocRegistered,
    EventKind::DocGenerated,
    EventKind::DocInvalidated,
    EventKind::DocReconciled,
    EventKind::IndexUpdated,
    EventKind::HarnessChanged,
    EventKind::HarnessBenchmarked,
    EventKind::HarnessPromoted,
    EventKind::GenesisStarted,
    EventKind::GenesisStageEntered,
    EventKind::GenesisStageCompleted,
    EventKind::GenesisAssumptionRecorded,
    EventKind::GenesisMaturityEvaluated,
    EventKind::GenesisCompleted,
    EventKind::MirrorLinked,
    EventKind::MirrorPushed,
    EventKind::MirrorPulled,
    EventKind::EffectJournaled,
    EventKind::EffectCompleted,
    EventKind::EffectFailed,
    EventKind::GoalSet,
    EventKind::GoalStepAdded,
    EventKind::GoalStepCompleted,
    EventKind::GoalReoriented,
    EventKind::GoalClaimedComplete,
    EventKind::ClassifyDecided,
];

impl EventKind {
    /// The dotted name this kind renders as, e.g. `"ticket.leased"`.
    ///
    /// This is the single source of truth for both [`fmt::Display`] and [`FromStr`]; it must
    /// stay in lockstep with the `#[serde(rename = ...)]` on each variant (a unit test in
    /// `kind.rs` checks that every variant round-trips through serde using this string).
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::ProjectCreated => "project.created",
            EventKind::ProjectAttached => "project.attached",
            EventKind::TicketCreated => "ticket.created",
            EventKind::TicketUpdated => "ticket.updated",
            EventKind::TicketStateChanged => "ticket.state_changed",
            EventKind::TicketDependencyAdded => "ticket.dependency_added",
            EventKind::TicketDependencyRemoved => "ticket.dependency_removed",
            EventKind::TicketChildAdded => "ticket.child_added",
            EventKind::TicketLeased => "ticket.leased",
            EventKind::TicketHeartbeat => "ticket.heartbeat",
            EventKind::TicketLeaseExpired => "ticket.lease_expired",
            EventKind::TicketLeaseReleased => "ticket.lease_released",
            EventKind::TicketDelegated => "ticket.delegated",
            EventKind::TicketSubmitted => "ticket.submitted",
            EventKind::TicketVerified => "ticket.verified",
            EventKind::TicketVerificationFailed => "ticket.verification_failed",
            EventKind::TicketAudited => "ticket.audited",
            EventKind::TicketAuditRejected => "ticket.audit_rejected",
            EventKind::TicketClosed => "ticket.closed",
            EventKind::TicketCancelled => "ticket.cancelled",
            EventKind::TicketReopened => "ticket.reopened",
            EventKind::TicketFailed => "ticket.failed",
            EventKind::TicketRetryScheduled => "ticket.retry_scheduled",
            EventKind::TicketEscalated => "ticket.escalated",
            EventKind::TicketBudgetExhausted => "ticket.budget_exhausted",
            EventKind::TicketBudgetHandoff => "ticket.budget_handoff",
            EventKind::TicketForked => "ticket.forked",
            EventKind::DecisionCreated => "decision.created",
            EventKind::DecisionSuperseded => "decision.superseded",
            EventKind::AuthorityGranted => "authority.granted",
            EventKind::AuthorityDelegated => "authority.delegated",
            EventKind::AuthorityRevoked => "authority.revoked",
            EventKind::AuthorityReverted => "authority.reverted",
            EventKind::ResourceClaimed => "resource.claimed",
            EventKind::ResourceReleased => "resource.released",
            EventKind::ResourceConflictDetected => "resource.conflict_detected",
            EventKind::ArtifactCreated => "artifact.created",
            EventKind::CommandStarted => "command.started",
            EventKind::CommandCompleted => "command.completed",
            EventKind::MilestoneCreated => "milestone.created",
            EventKind::MilestoneClosed => "milestone.closed",
            EventKind::MilestoneReopened => "milestone.reopened",
            EventKind::SessionStarted => "session.started",
            EventKind::SessionJoined => "session.joined",
            EventKind::SessionLeft => "session.left",
            EventKind::SessionEnded => "session.ended",
            EventKind::PresenceUpdated => "presence.updated",
            EventKind::CommentCreated => "comment.created",
            EventKind::ApprovalRequested => "approval.requested",
            EventKind::ApprovalDecided => "approval.decided",
            EventKind::ProviderSelected => "provider.selected",
            EventKind::ProviderExhausted => "provider.exhausted",
            EventKind::ProviderDegraded => "provider.degraded",
            EventKind::ProviderRecovered => "provider.recovered",
            EventKind::ExecutorFailed => "executor.failed",
            EventKind::UsageRecorded => "usage.recorded",
            EventKind::DocRegistered => "doc.registered",
            EventKind::DocGenerated => "doc.generated",
            EventKind::DocInvalidated => "doc.invalidated",
            EventKind::DocReconciled => "doc.reconciled",
            EventKind::IndexUpdated => "index.updated",
            EventKind::HarnessChanged => "harness.changed",
            EventKind::HarnessBenchmarked => "harness.benchmarked",
            EventKind::HarnessPromoted => "harness.promoted",
            EventKind::GenesisStarted => "genesis.started",
            EventKind::GenesisStageEntered => "genesis.stage_entered",
            EventKind::GenesisStageCompleted => "genesis.stage_completed",
            EventKind::GenesisAssumptionRecorded => "genesis.assumption_recorded",
            EventKind::GenesisMaturityEvaluated => "genesis.maturity_evaluated",
            EventKind::GenesisCompleted => "genesis.completed",
            EventKind::MirrorLinked => "mirror.linked",
            EventKind::MirrorPushed => "mirror.pushed",
            EventKind::MirrorPulled => "mirror.pulled",
            EventKind::EffectJournaled => "effect.journaled",
            EventKind::EffectCompleted => "effect.completed",
            EventKind::EffectFailed => "effect.failed",
            EventKind::GoalSet => "goal.set",
            EventKind::GoalStepAdded => "goal.step_added",
            EventKind::GoalStepCompleted => "goal.step_completed",
            EventKind::GoalReoriented => "goal.reoriented",
            EventKind::GoalClaimedComplete => "goal.claimed_complete",
            EventKind::ClassifyDecided => "classify.decided",
        }
    }

    /// The broad category this kind belongs to, derived from the dotted name's first segment.
    pub fn category(self) -> EventCategory {
        match self {
            EventKind::ProjectCreated | EventKind::ProjectAttached => EventCategory::Project,
            EventKind::TicketCreated
            | EventKind::TicketUpdated
            | EventKind::TicketStateChanged
            | EventKind::TicketDependencyAdded
            | EventKind::TicketDependencyRemoved
            | EventKind::TicketChildAdded
            | EventKind::TicketLeased
            | EventKind::TicketHeartbeat
            | EventKind::TicketLeaseExpired
            | EventKind::TicketLeaseReleased
            | EventKind::TicketDelegated
            | EventKind::TicketSubmitted
            | EventKind::TicketVerified
            | EventKind::TicketVerificationFailed
            | EventKind::TicketAudited
            | EventKind::TicketAuditRejected
            | EventKind::TicketClosed
            | EventKind::TicketCancelled
            | EventKind::TicketReopened
            | EventKind::TicketFailed
            | EventKind::TicketRetryScheduled
            | EventKind::TicketEscalated
            | EventKind::TicketBudgetExhausted
            | EventKind::TicketBudgetHandoff
            | EventKind::TicketForked => EventCategory::Ticket,
            EventKind::DecisionCreated | EventKind::DecisionSuperseded => EventCategory::Decision,
            EventKind::AuthorityGranted
            | EventKind::AuthorityDelegated
            | EventKind::AuthorityRevoked
            | EventKind::AuthorityReverted => EventCategory::Authority,
            EventKind::ResourceClaimed
            | EventKind::ResourceReleased
            | EventKind::ResourceConflictDetected => EventCategory::Resource,
            EventKind::ArtifactCreated => EventCategory::Artifact,
            EventKind::CommandStarted | EventKind::CommandCompleted => EventCategory::Command,
            EventKind::MilestoneCreated
            | EventKind::MilestoneClosed
            | EventKind::MilestoneReopened => EventCategory::Milestone,
            EventKind::SessionStarted
            | EventKind::SessionJoined
            | EventKind::SessionLeft
            | EventKind::SessionEnded => EventCategory::Session,
            EventKind::PresenceUpdated => EventCategory::Presence,
            EventKind::CommentCreated => EventCategory::Comment,
            EventKind::ApprovalRequested | EventKind::ApprovalDecided => EventCategory::Approval,
            EventKind::ProviderSelected
            | EventKind::ProviderExhausted
            | EventKind::ProviderDegraded
            | EventKind::ProviderRecovered => EventCategory::Provider,
            EventKind::ExecutorFailed => EventCategory::Executor,
            EventKind::UsageRecorded => EventCategory::Usage,
            EventKind::DocRegistered
            | EventKind::DocGenerated
            | EventKind::DocInvalidated
            | EventKind::DocReconciled => EventCategory::Doc,
            EventKind::IndexUpdated => EventCategory::Index,
            EventKind::HarnessChanged
            | EventKind::HarnessBenchmarked
            | EventKind::HarnessPromoted => EventCategory::Harness,
            EventKind::GenesisStarted
            | EventKind::GenesisStageEntered
            | EventKind::GenesisStageCompleted
            | EventKind::GenesisAssumptionRecorded
            | EventKind::GenesisMaturityEvaluated
            | EventKind::GenesisCompleted => EventCategory::Genesis,
            EventKind::MirrorLinked | EventKind::MirrorPushed | EventKind::MirrorPulled => {
                EventCategory::Mirror
            }
            EventKind::EffectJournaled | EventKind::EffectCompleted | EventKind::EffectFailed => {
                EventCategory::Effect
            }
            EventKind::GoalSet
            | EventKind::GoalStepAdded
            | EventKind::GoalStepCompleted
            | EventKind::GoalReoriented
            | EventKind::GoalClaimedComplete => EventCategory::Goal,
            EventKind::ClassifyDecided => EventCategory::Classify,
        }
    }

    /// True when this kind marks a ticket (or verification/audit node) reaching a terminal
    /// state: closed, cancelled or failed with no retry scheduled.
    pub fn is_ticket_terminal(self) -> bool {
        matches!(self, EventKind::TicketClosed | EventKind::TicketCancelled)
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventKind {
    type Err = TmError;

    // IMPL: exhaustive reverse lookup against `ALL` (or an equivalent match), matching
    // `as_str()`'s table exactly. Unknown dotted names are `TmError::parse`, never a silent
    // fallback — this is the enforcement point for "the catalogue is closed" (SPEC.md §3.2).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ALL.iter()
            .copied()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| TmError::parse(format!("unknown event kind: {s:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_through_display_and_from_str() {
        for &k in ALL {
            assert_eq!(k.as_str().parse::<EventKind>().unwrap(), k);
            assert_eq!(k.to_string(), k.as_str());
        }
    }

    #[test]
    fn every_kind_round_trips_through_serde_using_the_same_dotted_name() {
        for &k in ALL {
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()));
            assert_eq!(serde_json::from_str::<EventKind>(&json).unwrap(), k);
        }
    }

    #[test]
    fn unknown_kinds_fail_to_parse() {
        assert!("ticket.nope".parse::<EventKind>().is_err());
        assert!(serde_json::from_str::<EventKind>("\"ticket.nope\"").is_err());
    }

    #[test]
    fn as_str_returns_dotted_name() {
        assert_eq!(EventKind::TicketLeased.as_str(), "ticket.leased");
        assert_eq!(EventKind::AuthorityReverted.as_str(), "authority.reverted");
        assert_eq!(EventKind::ProjectCreated.as_str(), "project.created");
    }

    #[test]
    fn category_maps_project_kinds() {
        assert_eq!(EventKind::ProjectCreated.category(), EventCategory::Project);
        assert_eq!(
            EventKind::ProjectAttached.category(),
            EventCategory::Project
        );
    }

    #[test]
    fn category_maps_ticket_kinds() {
        assert_eq!(EventKind::TicketCreated.category(), EventCategory::Ticket);
        assert_eq!(EventKind::TicketLeased.category(), EventCategory::Ticket);
        assert_eq!(EventKind::TicketClosed.category(), EventCategory::Ticket);
        assert_eq!(
            EventKind::TicketBudgetExhausted.category(),
            EventCategory::Ticket
        );
        assert_eq!(
            EventKind::TicketBudgetHandoff.category(),
            EventCategory::Ticket
        );
        assert_eq!(EventKind::TicketForked.category(), EventCategory::Ticket);
    }

    #[test]
    fn category_maps_decision_kinds() {
        assert_eq!(
            EventKind::DecisionCreated.category(),
            EventCategory::Decision
        );
        assert_eq!(
            EventKind::DecisionSuperseded.category(),
            EventCategory::Decision
        );
    }

    #[test]
    fn category_maps_authority_kinds() {
        assert_eq!(
            EventKind::AuthorityGranted.category(),
            EventCategory::Authority
        );
        assert_eq!(
            EventKind::AuthorityDelegated.category(),
            EventCategory::Authority
        );
        assert_eq!(
            EventKind::AuthorityRevoked.category(),
            EventCategory::Authority
        );
        assert_eq!(
            EventKind::AuthorityReverted.category(),
            EventCategory::Authority
        );
    }

    #[test]
    fn category_maps_resource_kinds() {
        assert_eq!(
            EventKind::ResourceClaimed.category(),
            EventCategory::Resource
        );
        assert_eq!(
            EventKind::ResourceReleased.category(),
            EventCategory::Resource
        );
        assert_eq!(
            EventKind::ResourceConflictDetected.category(),
            EventCategory::Resource
        );
    }

    #[test]
    fn category_maps_artifact_kinds() {
        assert_eq!(
            EventKind::ArtifactCreated.category(),
            EventCategory::Artifact
        );
    }

    #[test]
    fn category_maps_command_kinds() {
        assert_eq!(EventKind::CommandStarted.category(), EventCategory::Command);
        assert_eq!(
            EventKind::CommandCompleted.category(),
            EventCategory::Command
        );
    }

    #[test]
    fn category_maps_milestone_kinds() {
        assert_eq!(
            EventKind::MilestoneCreated.category(),
            EventCategory::Milestone
        );
        assert_eq!(
            EventKind::MilestoneClosed.category(),
            EventCategory::Milestone
        );
        assert_eq!(
            EventKind::MilestoneReopened.category(),
            EventCategory::Milestone
        );
    }

    #[test]
    fn category_maps_session_kinds() {
        assert_eq!(EventKind::SessionStarted.category(), EventCategory::Session);
        assert_eq!(EventKind::SessionJoined.category(), EventCategory::Session);
        assert_eq!(EventKind::SessionLeft.category(), EventCategory::Session);
        assert_eq!(EventKind::SessionEnded.category(), EventCategory::Session);
    }

    #[test]
    fn category_maps_presence_kinds() {
        assert_eq!(
            EventKind::PresenceUpdated.category(),
            EventCategory::Presence
        );
    }

    #[test]
    fn category_maps_comment_kinds() {
        assert_eq!(EventKind::CommentCreated.category(), EventCategory::Comment);
    }

    #[test]
    fn category_maps_approval_kinds() {
        assert_eq!(
            EventKind::ApprovalRequested.category(),
            EventCategory::Approval
        );
        assert_eq!(
            EventKind::ApprovalDecided.category(),
            EventCategory::Approval
        );
    }

    #[test]
    fn category_maps_provider_kinds() {
        assert_eq!(
            EventKind::ProviderSelected.category(),
            EventCategory::Provider
        );
        assert_eq!(
            EventKind::ProviderExhausted.category(),
            EventCategory::Provider
        );
        assert_eq!(
            EventKind::ProviderDegraded.category(),
            EventCategory::Provider
        );
        assert_eq!(
            EventKind::ProviderRecovered.category(),
            EventCategory::Provider
        );
    }

    #[test]
    fn category_maps_executor_kinds() {
        assert_eq!(
            EventKind::ExecutorFailed.category(),
            EventCategory::Executor
        );
    }

    #[test]
    fn category_maps_usage_kinds() {
        assert_eq!(EventKind::UsageRecorded.category(), EventCategory::Usage);
    }

    #[test]
    fn category_maps_doc_kinds() {
        assert_eq!(EventKind::DocRegistered.category(), EventCategory::Doc);
        assert_eq!(EventKind::DocGenerated.category(), EventCategory::Doc);
        assert_eq!(EventKind::DocInvalidated.category(), EventCategory::Doc);
        assert_eq!(EventKind::DocReconciled.category(), EventCategory::Doc);
    }

    #[test]
    fn category_maps_index_kinds() {
        assert_eq!(EventKind::IndexUpdated.category(), EventCategory::Index);
    }

    #[test]
    fn category_maps_harness_kinds() {
        assert_eq!(EventKind::HarnessChanged.category(), EventCategory::Harness);
        assert_eq!(
            EventKind::HarnessBenchmarked.category(),
            EventCategory::Harness
        );
        assert_eq!(
            EventKind::HarnessPromoted.category(),
            EventCategory::Harness
        );
    }

    #[test]
    fn category_maps_genesis_kinds() {
        assert_eq!(EventKind::GenesisStarted.category(), EventCategory::Genesis);
        assert_eq!(
            EventKind::GenesisStageEntered.category(),
            EventCategory::Genesis
        );
        assert_eq!(
            EventKind::GenesisStageCompleted.category(),
            EventCategory::Genesis
        );
        assert_eq!(
            EventKind::GenesisAssumptionRecorded.category(),
            EventCategory::Genesis
        );
        assert_eq!(
            EventKind::GenesisMaturityEvaluated.category(),
            EventCategory::Genesis
        );
        assert_eq!(
            EventKind::GenesisCompleted.category(),
            EventCategory::Genesis
        );
    }

    #[test]
    fn category_maps_mirror_kinds() {
        assert_eq!(EventKind::MirrorLinked.category(), EventCategory::Mirror);
        assert_eq!(EventKind::MirrorPushed.category(), EventCategory::Mirror);
        assert_eq!(EventKind::MirrorPulled.category(), EventCategory::Mirror);
    }

    #[test]
    fn category_maps_effect_kinds() {
        assert_eq!(EventKind::EffectJournaled.category(), EventCategory::Effect);
        assert_eq!(EventKind::EffectCompleted.category(), EventCategory::Effect);
        assert_eq!(EventKind::EffectFailed.category(), EventCategory::Effect);
    }

    #[test]
    fn category_maps_goal_kinds() {
        assert_eq!(EventKind::GoalSet.category(), EventCategory::Goal);
        assert_eq!(EventKind::GoalStepAdded.category(), EventCategory::Goal);
        assert_eq!(EventKind::GoalStepCompleted.category(), EventCategory::Goal);
        assert_eq!(EventKind::GoalReoriented.category(), EventCategory::Goal);
        assert_eq!(
            EventKind::GoalClaimedComplete.category(),
            EventCategory::Goal
        );
    }

    #[test]
    fn is_ticket_terminal_true_for_closed() {
        assert!(EventKind::TicketClosed.is_ticket_terminal());
    }

    #[test]
    fn is_ticket_terminal_true_for_cancelled() {
        assert!(EventKind::TicketCancelled.is_ticket_terminal());
    }

    #[test]
    fn is_ticket_terminal_false_for_created() {
        assert!(!EventKind::TicketCreated.is_ticket_terminal());
    }

    #[test]
    fn is_ticket_terminal_false_for_leased() {
        assert!(!EventKind::TicketLeased.is_ticket_terminal());
    }

    #[test]
    fn is_ticket_terminal_false_for_reopened() {
        assert!(!EventKind::TicketReopened.is_ticket_terminal());
    }

    #[test]
    fn is_ticket_terminal_false_for_failed() {
        assert!(!EventKind::TicketFailed.is_ticket_terminal());
    }

    #[test]
    fn is_ticket_terminal_false_for_non_ticket_kinds() {
        assert!(!EventKind::ProjectCreated.is_ticket_terminal());
        assert!(!EventKind::DecisionCreated.is_ticket_terminal());
        assert!(!EventKind::AuthorityGranted.is_ticket_terminal());
    }

    #[test]
    fn parse_error_includes_kind_name_in_message() {
        let err = "invalid.kind".parse::<EventKind>().unwrap_err();
        assert!(err.to_string().contains("invalid.kind"));
    }

    #[test]
    fn serde_deserialize_error_for_unknown_kind() {
        let result = serde_json::from_str::<EventKind>("\"unknown.kind\"");
        assert!(result.is_err());
    }

    #[test]
    fn all_kinds_sorted_and_complete() {
        assert_eq!(ALL.len(), 82);
        assert_eq!(ALL[0], EventKind::ProjectCreated);
        assert_eq!(ALL[ALL.len() - 1], EventKind::ClassifyDecided);
    }

    #[test]
    fn category_maps_classify_kinds() {
        assert_eq!(
            EventKind::ClassifyDecided.category(),
            EventCategory::Classify
        );
    }

    #[test]
    fn classify_decided_round_trips_as_classify_decided() {
        assert_eq!(EventKind::ClassifyDecided.as_str(), "classify.decided");
        assert_eq!(
            "classify.decided".parse::<EventKind>().unwrap(),
            EventKind::ClassifyDecided
        );
        let json = serde_json::to_string(&EventKind::ClassifyDecided).unwrap();
        assert_eq!(json, "\"classify.decided\"");
        assert_eq!(
            serde_json::from_str::<EventKind>(&json).unwrap(),
            EventKind::ClassifyDecided
        );
    }
}
