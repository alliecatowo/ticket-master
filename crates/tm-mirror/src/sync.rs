//! Bidirectional sync across every attached adapter.
//!
//! Push builds a [`crate::projection::Projection`], hands it to a
//! [`crate::tracker::Tracker`], and records the resulting [`MirrorLink`] plus a `mirror.pushed`
//! event; pull walks a tracker's [`crate::tracker::ExternalChange`]s and translates the
//! semantically meaningful ones — status hint, assignment, comment, priority, human-created
//! issue becoming a `Draft` ticket — into [`tm_events::EventDraft`]s, never into direct ticket
//! state writes. [`ConflictField::owner`] is the one fixed rule for divergence: Ticketmaster
//! wins orchestration fields, the external system wins human-presentation fields. `push` is
//! idempotent by content hash, so syncing twice changes nothing the second time.
//!
//! Only [`Tracker::push`]/[`Tracker::pull`] themselves are I/O; everything in this module is
//! translation logic driven by an injected [`Clock`]/[`IdSource`], never the wall clock.

use std::sync::Arc;

use tm_events::payload::{
    CommentCreatedPayload, MirrorLinkedPayload, MirrorPulledPayload, MirrorPushedPayload,
    TicketCreatedPayload, TicketUpdatedPayload,
};
use tm_events::EventDraft;
use tm_types::{Clock, IdKind, IdSource, ParticipantId, Result, TicketId, Timestamp};

use crate::projection::{Degradation, Projection};
use crate::tracker::{ExternalChange, ExternalChangeKind, ExternalRef, Tracker};

/// The durable record of one ticket's mirror onto one adapter: where it lives externally, what
/// was degraded getting it there, and enough state to make repeat syncs idempotent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorLink {
    /// The ticket this link is for.
    pub ticket: TicketId,
    /// Adapter instance name (matches [`crate::config::AdapterConfig::name`]).
    pub adapter: String,
    /// Where the ticket lives externally.
    pub external: ExternalRef,
    /// Degradations applied the last time this link was pushed.
    pub degradations: Vec<Degradation>,
    /// Content hash of the last pushed projection (see [`SyncEngine::projection_hash`]), used to
    /// make `push` a no-op when nothing about the projection has changed.
    pub content_hash: String,
    /// When this link was last pushed.
    pub pushed_at: Timestamp,
    /// When this link was last pulled, if ever. `None` means pull has never run for this link,
    /// so the next pull uses [`Timestamp::EPOCH`] as `since`.
    pub last_pulled_at: Option<Timestamp>,
}

/// Which side of a divergence wins for a given field (`SPEC.md` §13's conflict rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldOwner {
    /// Ticketmaster's value is authoritative; the external system's is overwritten on next push.
    Ticketmaster,
    /// The external system's value is authoritative; inbound changes to it become events.
    External,
}

/// A field that can diverge between Ticketmaster and an external tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictField {
    /// Ticket lifecycle state. Orchestration field.
    Status,
    /// Dependency/authority/budget/scheduling wiring. Orchestration field.
    Orchestration,
    /// Assignment to a person. Human-presentation field.
    Assignment,
    /// Free-text title/body prose. Human-presentation field.
    Title,
    /// Label set as displayed externally. Human-presentation field.
    Labels,
    /// Priority as displayed externally. Human-presentation field.
    Priority,
    /// Comment threads. Human-presentation field (and append-only, so divergence is moot).
    Comments,
}

impl ConflictField {
    /// Which side owns this field. Fixed by `SPEC.md` §13, not a per-adapter policy choice:
    /// orchestration fields are always Ticketmaster's; human-presentation fields are always the
    /// external system's.
    pub fn owner(self) -> FieldOwner {
        match self {
            ConflictField::Status | ConflictField::Orchestration => FieldOwner::Ticketmaster,
            ConflictField::Assignment
            | ConflictField::Title
            | ConflictField::Labels
            | ConflictField::Priority
            | ConflictField::Comments => FieldOwner::External,
        }
    }
}

/// One inbound external change translated into what Ticketmaster should do about it, after
/// allowlist filtering. `SPEC.md` §13 fixes this to exactly five shapes; anything else observed
/// on an adapter is dropped at translation time, never forwarded.
#[derive(Debug, Clone, PartialEq)]
pub enum InboundAction {
    /// The external state changed; carries a hint only; Ticketmaster owns the real transition
    /// and may refuse it (e.g. dependencies unmet, authority denied).
    StatusHint {
        /// Ticket the hint applies to.
        ticket: TicketId,
        /// The external system's state name, unmapped.
        hint: String,
    },
    /// Assignment changed externally.
    AssignmentChanged {
        /// Ticket the change applies to.
        ticket: TicketId,
        /// New assignee display name/handle, or `None` if unassigned.
        assignee: Option<String>,
    },
    /// A comment was added externally.
    CommentAdded {
        /// Ticket the comment applies to.
        ticket: TicketId,
        /// Comment author, external display name/handle.
        author: String,
        /// Comment body.
        body: String,
    },
    /// Priority changed externally.
    PriorityChanged {
        /// Ticket the change applies to.
        ticket: TicketId,
        /// External priority label.
        priority: String,
    },
    /// A human created an issue externally with no existing mirror link; becomes a new ticket.
    NewDraftTicket {
        /// The externally created issue this ticket will be linked to.
        external: ExternalRef,
        /// Issue title, becomes the ticket objective.
        title: String,
        /// Issue body.
        body: String,
    },
}

/// Drives push/pull across attached adapters. Holds only the injected clock/id source and the
/// actor to attribute mirror-originated events to — no adapter state, no ticket storage;
/// callers own the graph and mirror-link persistence and pass in what each call needs.
pub struct SyncEngine {
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
    actor: ParticipantId,
}

impl SyncEngine {
    /// Build a sync engine with the given injected clock/id source and the actor to attribute
    /// mirror-originated events to (typically [`ParticipantId::system`]).
    pub fn new(clock: Arc<dyn Clock>, ids: Arc<dyn IdSource>, actor: ParticipantId) -> Self {
        SyncEngine { clock, ids, actor }
    }

    /// A stable content hash of a projection's externally-visible fields, used to detect a
    /// no-op push. Two projections with equal `(title, body, state_hint, labels, milestone,
    /// checklist)` must hash equal regardless of allocation history; `degradations` is excluded
    /// (it records how the projection was produced, not what it says).
    pub fn projection_hash(projection: &Projection) -> String {
        // IMPL: hash with blake3 (already a workspace pattern for content hashing, see
        // tm-core's event hash-chaining) over a canonical byte representation: e.g.
        // serde_json::to_vec of a tuple/struct of exactly (title, body, state_hint, labels,
        // milestone, checklist) — labels and checklist are already in a defined order from
        // ProjectionPolicy::project, so no extra sorting is needed as long as that invariant
        // holds; render the digest as lowercase hex. Must not read the clock or an id source.
        todo!("stable content hash of a projection's externally-visible fields")
    }

    /// Push `projection` to `tracker`, updating `existing` if given.
    ///
    /// Idempotent: if `existing.content_hash` already equals `projection`'s hash, this returns
    /// `existing` unchanged with no event and does not call `tracker.push` — this is the
    /// contract a round-trip "push twice" test asserts against every adapter.
    pub async fn push(
        &self,
        tracker: &dyn Tracker,
        projection: &Projection,
        existing: Option<&MirrorLink>,
    ) -> Result<(MirrorLink, Option<EventDraft>)> {
        // IMPL: hash = Self::projection_hash(projection); if existing.is_some_and(|l|
        // l.content_hash == hash), return (existing.unwrap().clone(), None) with no tracker
        // call. Otherwise: external = tracker.push(projection).await?; link = MirrorLink {
        // ticket: projection.ticket.clone(), adapter: tracker.name().to_string(), external,
        // degradations: projection.degradations.clone(), content_hash: hash, pushed_at:
        // self.clock.now(), last_pulled_at: existing.and_then(|l| l.last_pulled_at) }; draft =
        // EventDraft::new(self.actor.clone(), tm_types::Id::from(projection.ticket.clone()),
        // MirrorPushedPayload{remote: tracker.name().to_string(), reference:
        // link.external.external_id.clone()}.into()); return (link, Some(draft)). Emitting the
        // separate `mirror.linked` event (via MirrorLinkedPayload) on first push is left to the
        // caller layer above this engine, since only it knows whether this is truly the first
        // link for the ticket across *all* adapters, not just this one.
        todo!("push a projection idempotently and record the resulting mirror link + event")
    }

    /// Pull every change since `link.last_pulled_at` (or [`Timestamp::EPOCH`] if never pulled)
    /// from `tracker`, translate the allowed ones via [`SyncEngine::translate`], and return the
    /// resulting event drafts plus the updated link (`last_pulled_at` advanced regardless of
    /// whether any change translated to an event).
    pub async fn pull(
        &self,
        tracker: &dyn Tracker,
        link: &MirrorLink,
    ) -> Result<(MirrorLink, Vec<EventDraft>)> {
        // IMPL: since = link.last_pulled_at.unwrap_or(Timestamp::EPOCH); changes =
        // tracker.pull(since).await?; drafts = changes.iter().filter_map(|c|
        // self.translate(Some(link.ticket.clone()), c)).flat_map(|a|
        // self.to_event_drafts(&a)).collect::<Vec<_>>(); always additionally push one
        // EventDraft wrapping MirrorPulledPayload{remote: tracker.name().to_string(), reference:
        // link.external.external_id.clone()} onto `drafts`, marking that the pull happened even
        // with zero translatable changes (freshness signal). updated = link.clone() with
        // last_pulled_at = Some(self.clock.now()). Return (updated, drafts).
        todo!("pull changes since the link's last pull, translate them, and advance the link")
    }

    /// Translate one raw external change into the action Ticketmaster should take, or `None` if
    /// it falls outside the allowlist.
    ///
    /// `known_ticket` is `Some` when the change is on an already-linked ticket; `None` means "no
    /// mirror link was found for this external object", the only path that can yield
    /// `InboundAction::NewDraftTicket`.
    pub fn translate(
        &self,
        known_ticket: Option<TicketId>,
        change: &ExternalChange,
    ) -> Option<InboundAction> {
        // IMPL: match &change.kind:
        //   StatusHint{state} => known_ticket.map(|t| InboundAction::StatusHint{ticket: t, hint:
        //     state.clone()}) — None if known_ticket is None (an update on an object we have no
        //     link for is noise, not a "human created this" event; only IssueCreated-shaped
        //     changes originate new tickets).
        //   Assigned{assignee} => known_ticket.map(|t| InboundAction::AssignmentChanged{ticket:
        //     t, assignee: assignee.clone()}), same None-if-unlinked rule.
        //   CommentAdded{author, body} => known_ticket.map(|t| InboundAction::CommentAdded{
        //     ticket: t, author: author.clone(), body: body.clone()}), same rule.
        //   PriorityChanged{priority} => known_ticket.map(|t| InboundAction::PriorityChanged{
        //     ticket: t, priority: priority.clone()}), same rule.
        //   IssueCreated{title, body, ..} => if known_ticket.is_some(), None (an already-linked
        //     object can't also be "newly created" — that would be a duplicate link, a bug
        //     upstream, not something to translate here); if known_ticket.is_none(),
        //     Some(InboundAction::NewDraftTicket{external: change.external.clone(), title:
        //     title.clone(), body: body.clone()}).
        // Every branch is a pure match, no I/O, no clock/id use (ids are only minted in
        // to_event_drafts, once the caller has committed to acting on the translation).
        todo!("map one ExternalChange to the InboundAction the allowlist permits, if any")
    }

    /// Turn one translated action into the event draft(s) that record it. Mints a fresh
    /// [`TicketId`] via the injected [`IdSource`] for `NewDraftTicket` only — every other
    /// variant already carries the ticket it applies to.
    pub fn to_event_drafts(&self, action: &InboundAction) -> Vec<EventDraft> {
        // IMPL: build a Vec<EventDraft>, always EventDraft::new(self.actor.clone(), <subject>,
        // <payload>.into()):
        //   StatusHint{ticket, hint} => subject = Id::from(ticket.clone()); payload =
        //     TicketUpdatedPayload{ticket: ticket.clone(), fields:
        //     serde_json::json!({"status_hint": hint})} — a hint, never a direct state write;
        //     tm-core's transition machinery decides whether/how to act on it.
        //   AssignmentChanged{ticket, assignee} => same shape, fields =
        //     json!({"assignee": assignee}).
        //   PriorityChanged{ticket, priority} => same shape, fields = json!({"priority":
        //     priority}).
        //   CommentAdded{ticket, author, body} => subject = Id::from(ticket.clone()); payload =
        //     CommentCreatedPayload{ticket: Some(ticket.clone()), author: self.actor.clone(),
        //     body: format!("{author} (via mirror): {body}")} — the external author's identity
        //     is prose, not a ParticipantId, since we don't have an on-platform participant for
        //     an arbitrary external user; self.actor is who *recorded* the comment.
        //   NewDraftTicket{external, title, body} => new_id = TicketId::new(self.ids.next(
        //     IdKind::Ticket).as_str())? — actually IdSource::next returns an already-typed Id
        //     whose kind() is Ticket; convert via TicketId::new(id.as_str()) and treat a
        //     conversion failure as unreachable (IdSource::next(IdKind::Ticket) always yields a
        //     validly-shaped ticket id, by IdSource's own contract) rather than propagating an
        //     error from an infallible path. Emit two drafts: (1) subject = Id::from(new_id
        //     .clone()), payload = TicketCreatedPayload{ticket: new_id.clone(), title:
        //     title.clone(), parent: None} — the ticket lands in `Draft` by tm-core's own
        //     materialization default for a newly created ticket, not by anything set here; (2)
        //     subject = Id::from(new_id), payload = MirrorLinkedPayload{remote:
        //     external.adapter.clone()}, linking the new ticket back to where it came from. The
        //     issue body isn't dropped: fold it into the first draft's title if short, or leave
        //     it for the caller to attach as a follow-up comment event — pick one and note it
        //     inline when implementing, since TicketCreatedPayload has no body field.
        // Return the accumulated Vec<EventDraft> (length 1 for every variant but
        // NewDraftTicket, which is 2).
        todo!("build the event draft(s) that record a translated inbound action")
    }
}
