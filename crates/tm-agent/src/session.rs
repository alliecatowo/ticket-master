//! Session bookkeeping: the transcript, the pinned harness epoch, and promotion of anything
//! that matters out of conversation into durable state.
//!
//! `SPEC.md` §10's harness-pinning guarantee means a [`Session`] never re-reads its harness
//! epoch mid-run; it carries the epoch number it started with. And because a conversation is
//! not itself durable state, every decision, artifact, evidence record and ticket a step's tool
//! calls produced is promoted (via [`Session::promote`]) into ids a fresh worker — or a human —
//! can look up directly in `tm-core`, without ever needing to replay the transcript.

use tm_types::{ArtifactId, DecisionId, SessionId, TicketId, Timestamp};

use crate::outcome::StepRecord;

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
    pub fn new(
        id: SessionId,
        ticket: TicketId,
        harness_epoch: u64,
        started_at: Timestamp,
    ) -> Self {
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
    // IMPL: walk `self.transcript` in order; for each `StepRecord::tool_calls` entry whose
    // `resolution` is `ToolCallResolution::Completed`, inspect `tool_name`: `"decision.record"`
    // results carry a `DecisionId` in their JSON body (under a `"decision"` key, minted by the
    // dispatch in `tools.rs`) — collect it into `decisions`; `"artifact.store"` and any
    // `Completed { artifact: Some(id), .. }` resolution (the bound-large-result case) collect
    // into `artifacts`; `"evidence.attach"` results collect the referenced artifact into
    // `artifacts` as well; `"ticket.create_child"`/`"ticket.delegate"` results carry a
    // `TicketId` under a `"ticket"` key — collect into `tickets`. Deduplicate each collection
    // while preserving first-seen order (a `Vec` plus a `BTreeSet`/`HashSet` guard is fine).
    pub fn promote(&self) -> DurablePromotion {
        todo!("scan completed tool-call resolutions across the transcript and collect the decision/artifact/ticket ids they minted, per the IMPL note")
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
