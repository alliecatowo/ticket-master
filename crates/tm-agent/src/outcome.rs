//! What an [`crate::agent_loop::AgentLoop`] is asked to do ([`AgentTask`]), what it produces
//! ([`AgentOutcome`]), and the durable-shaped records ([`StepRecord`], [`EvidenceBundle`]) that
//! let a fresh worker (or a human) reconstruct what happened without replaying a conversation.
//!
//! Nothing here performs I/O or reads the wall clock: every timestamp arrives already-resolved
//! from an injected `tm_types::Clock`, so this module is pure data plus the small amount of
//! pure logic (e.g. [`AgentOutcome::is_terminal`]) that operates on it.

use serde::{Deserialize, Serialize};
use tm_core::FailureClass;
use tm_types::{ArtifactId, Authority, Budget, DecisionId, SessionId, TicketId, Timestamp};

/// One unit of work handed to an [`crate::agent_loop::AgentLoop`].
///
/// `harness_epoch` is the epoch number the calling session already pinned
/// (`tm_harness::epoch::SessionPin::epoch_number`); the loop never re-resolves it, honoring the
/// pinning guarantee that a session's harness policy never changes mid-run.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTask {
    /// The ticket this task executes.
    pub ticket: TicketId,
    /// The compiled, bounded context pack the worker sees instead of raw files.
    pub context_pack: tm_context::ContextPack,
    /// The authority this task's tool calls are checked against.
    pub authority: Authority,
    /// The budget this task's provider calls and tool costs are checked against.
    pub budget: Budget,
    /// The pinned harness epoch number this task's prompts and tool policy were assembled
    /// under.
    pub harness_epoch: u64,
    /// The session this task executes inside, for transcript/event attribution.
    pub session: SessionId,
}

/// The closed set of reasons an [`AgentLoop`](crate::agent_loop::AgentLoop) run ends.
///
/// Exactly one variant is produced per [`crate::agent_loop::AgentLoop::run`] call; there is no
/// panic path out of the loop.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentOutcome {
    /// The task submitted work: evidence was attached and the ticket transitioned toward
    /// verification.
    Submitted {
        /// The evidence bundle the submission carried.
        evidence: EvidenceBundle,
        /// Full transcript of the run, for audit and replay.
        steps: Vec<StepRecord>,
    },
    /// The task's [`AgentTask::budget`] was exhausted before it could submit.
    ///
    /// Always produced between steps (after a tool result, before the next provider call),
    /// never mid-edit: the budget check happens before `AgentLoop` issues the next
    /// `tm_provider::CompletionRequest`, and [`crate::patch::PatchEngine`] applies are
    /// atomic per call, so a `BudgetExhausted` outcome never leaves a half-applied edit.
    BudgetExhausted {
        /// Steps completed before exhaustion.
        steps: Vec<StepRecord>,
        /// Which budget dimension tripped first.
        exhausted: BudgetDimension,
    },
    /// The task called a tool whose [`tm_types::Action`] maps to
    /// [`tm_types::Decision::NeedsApproval`]; the loop suspended and is waiting on
    /// `approval.decided`.
    AwaitingApproval {
        /// Steps completed before suspension.
        steps: Vec<StepRecord>,
        /// The suspended tool call, so a resumed loop can replay the model's original request
        /// once a decision lands.
        pending_call: PendingApproval,
    },
    /// The task ended without submitting, for a reason other than budget exhaustion or
    /// approval suspension.
    Failed {
        /// Steps completed before failure.
        steps: Vec<StepRecord>,
        /// The classified reason, reused from `tm_core`'s closed failure vocabulary so
        /// `Store::record_failure` can be called directly with it.
        class: FailureClass,
        /// Free-form detail (error message, step limit reached, provider error text).
        detail: String,
    },
}

impl AgentOutcome {
    /// True for every variant except [`AgentOutcome::AwaitingApproval`]: whether this outcome
    /// represents a run that will not resume on its own.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, AgentOutcome::AwaitingApproval { .. })
    }

    /// The transcript steps carried by any variant, for callers that just want the full record
    /// regardless of how the run ended.
    pub fn steps(&self) -> &[StepRecord] {
        match self {
            AgentOutcome::Submitted { steps, .. }
            | AgentOutcome::BudgetExhausted { steps, .. }
            | AgentOutcome::AwaitingApproval { steps, .. }
            | AgentOutcome::Failed { steps, .. } => steps,
        }
    }

    /// How many bytes of tool-result content the working set has pruned from this run's
    /// transcript, relative to re-sending every step's tool output verbatim
    /// (`docs/audit-2026-09-18-fable.md` B-08, `SPEC.md` §30.4).
    ///
    /// Recomputed on demand from [`AgentOutcome::steps`] via the same pure
    /// `crate::pruning::working_set` that [`crate::agent_loop::rebuild_messages`] renders from,
    /// rather than cached on a struct field: `working_set` is cheap (a handful of steps, at most
    /// [`crate::agent_loop::DEFAULT_MAX_STEPS`] of them) and this guarantees the number can never
    /// drift out of sync with what the loop's last rebuild actually rendered. This is deliberately
    /// a method on the outcome enum rather than a new field on any one variant: every variant is
    /// pattern-matched exhaustively (without `..`) somewhere outside this crate
    /// (`crates/tm-cli/src/agent.rs`'s `drive_turn`), so widening a variant's shape here would be
    /// a breaking change this track is not scoped to make.
    ///
    /// Feeds `SPEC.md` §30.2's rent-accounting ledger (`tm_context::ContextPack::rent_report`) a
    /// real number for working-set pruning specifically, once a caller wires the two together —
    /// no such caller exists yet (context-pack rent reporting and the agent loop's outcome are
    /// still separate reporting paths; connecting them touches `tm-context`/`tm-cli` call sites
    /// this track does not own), but the number itself is real, not a placeholder: it is exactly
    /// what [`crate::agent_loop::AgentLoop::drive`] logs via `tracing::debug!` on every turn that
    /// pruned anything.
    pub fn bytes_pruned(&self) -> u64 {
        crate::pruning::working_set(self.steps()).bytes_pruned
    }

    /// The same saving as [`AgentOutcome::bytes_pruned`], estimated in tokens via
    /// `tm_context::estimate_tokens_source`.
    pub fn tokens_pruned(&self) -> u64 {
        crate::pruning::working_set(self.steps()).tokens_pruned
    }
}

/// Which part of a [`tm_types::Budget`] was exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetDimension {
    /// Token budget spent.
    Tokens,
    /// Dollar budget spent.
    Dollars,
    /// Wall-clock seconds spent.
    WallSeconds,
}

/// A model tool call that is suspended pending human or higher-authority approval.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    /// The provider-assigned id correlating this call to its eventual tool result.
    pub tool_use_id: String,
    /// The tool name the model called.
    pub tool_name: String,
    /// The call's arguments, exactly as the model issued them.
    pub input: serde_json::Value,
    /// Human-readable reason the authority decision came back `NeedsApproval`.
    pub reason: String,
    /// When the approval was requested.
    pub requested_at: Timestamp,
}

/// One step of an [`AgentLoop`](crate::agent_loop::AgentLoop) run's transcript: a single
/// provider round-trip plus whatever tool calls it produced.
///
/// This is the promoted, durable shape a fresh worker (or a human reviewing the run) can read
/// without replaying the raw `tm_provider::Completion`; [`crate::session::Session`] is what
/// turns a live conversation into a sequence of these.
#[derive(Debug, Clone, PartialEq)]
pub struct StepRecord {
    /// 1-based index of this step within the run.
    pub index: u32,
    /// The role/model that served this step, as a display string (`"anthropic/claude-..."`).
    pub served_by: String,
    /// The assistant's text content for this step, if any (a step that only calls tools may
    /// have none).
    pub assistant_text: Option<String>,
    /// Every tool call issued this step and how it was resolved.
    pub tool_calls: Vec<ToolCallRecord>,
    /// Token/dollar/time spend charged for this step's provider call.
    pub spend: tm_types::Spend,
    /// When this step completed.
    pub at: Timestamp,
}

/// How one tool call within a [`StepRecord`] was resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRecord {
    /// The provider-assigned id correlating this call to its result.
    pub tool_use_id: String,
    /// The tool name called.
    pub tool_name: String,
    /// The call's arguments.
    pub input: serde_json::Value,
    /// What happened.
    pub resolution: ToolCallResolution,
}

/// The outcome of one tool call, as recorded in the transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallResolution {
    /// The tool ran and returned a result, inlined if small or referenced as an artifact if
    /// large.
    Completed {
        /// Bounded, structured result content shown to the model.
        result: serde_json::Value,
        /// The artifact holding the full result, if the inlined `result` is a truncated view.
        artifact: Option<ArtifactId>,
    },
    /// `Authority::permits` denied the underlying action; the denial text is what the model
    /// saw as its tool result.
    Denied {
        /// Why the action was denied.
        reason: String,
    },
    /// The tool call itself errored (bad arguments, dispatch failure) independent of
    /// authority.
    Errored {
        /// Error detail.
        detail: String,
    },
}

/// The evidence a submission carries, per `SPEC.md` §11's verification-separation rule: the
/// executing agent attaches evidence, but never marks itself verified. A distinct verification
/// ticket consumes this bundle later.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceBundle {
    /// The ticket this evidence is for.
    pub ticket: TicketId,
    /// Every artifact produced or referenced as evidence (patches, command output, test runs).
    pub artifacts: Vec<ArtifactId>,
    /// Decisions recorded during the run that a verifier/auditor should be aware of.
    pub decisions: Vec<DecisionId>,
    /// One-line human-readable submission summary, as passed to `tm_core::Store::submit`.
    pub summary: String,
}
