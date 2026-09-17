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
        // Canonical byte representation over exactly (title, body, state_hint, labels,
        // milestone, checklist), joined with control-character separators unlikely to appear
        // in prose so distinct field boundaries can't collide. Labels/checklist are already in
        // a defined order from ProjectionPolicy::project.
        let mut buf = String::new();
        buf.push_str(&projection.title);
        buf.push('\u{1}');
        buf.push_str(&projection.body);
        buf.push('\u{1}');
        buf.push_str(&projection.state_hint);
        buf.push('\u{1}');
        for label in &projection.labels {
            buf.push_str(label);
            buf.push('\u{2}');
        }
        buf.push('\u{1}');
        if let Some(milestone) = &projection.milestone {
            buf.push_str(milestone);
        }
        buf.push('\u{1}');
        for item in &projection.checklist {
            buf.push_str(item.ticket.as_str());
            buf.push('\u{3}');
            buf.push_str(&item.label);
            buf.push('\u{3}');
            buf.push(if item.done { '1' } else { '0' });
            buf.push('\u{2}');
        }
        tm_core::artifact::hash_bytes(buf.as_bytes())
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
        let hash = Self::projection_hash(projection);
        if let Some(link) = existing {
            if link.content_hash == hash {
                return Ok((link.clone(), None));
            }
        }
        let external = tracker.push(projection).await?;
        let link = MirrorLink {
            ticket: projection.ticket.clone(),
            adapter: tracker.name().to_string(),
            external,
            degradations: projection.degradations.clone(),
            content_hash: hash,
            pushed_at: self.clock.now(),
            last_pulled_at: existing.and_then(|l| l.last_pulled_at),
        };
        let draft = EventDraft::new(
            self.actor.clone(),
            tm_types::Id::from(link.ticket.clone()),
            MirrorPushedPayload {
                remote: tracker.name().to_string(),
                reference: link.external.external_id.clone(),
            }
            .into(),
        );
        // Emitting the separate `mirror.linked` event on first push is left to the caller
        // layer above this engine, since only it knows whether this is truly the first link
        // for the ticket across *all* adapters, not just this one.
        Ok((link, Some(draft)))
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
        let since = link.last_pulled_at.unwrap_or(Timestamp::EPOCH);
        let changes = tracker.pull(since).await?;
        let mut drafts: Vec<EventDraft> = changes
            .iter()
            .filter_map(|change| self.translate(Some(link.ticket.clone()), change))
            .flat_map(|action| self.to_event_drafts(&action))
            .collect();
        // Recorded even with zero translatable changes: a freshness signal that pull ran.
        drafts.push(EventDraft::new(
            self.actor.clone(),
            tm_types::Id::from(link.ticket.clone()),
            MirrorPulledPayload {
                remote: tracker.name().to_string(),
                reference: link.external.external_id.clone(),
            }
            .into(),
        ));
        let mut updated = link.clone();
        updated.last_pulled_at = Some(self.clock.now());
        Ok((updated, drafts))
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
        match &change.kind {
            ExternalChangeKind::StatusHint { state } => {
                known_ticket.map(|ticket| InboundAction::StatusHint {
                    ticket,
                    hint: state.clone(),
                })
            }
            ExternalChangeKind::Assigned { assignee } => {
                known_ticket.map(|ticket| InboundAction::AssignmentChanged {
                    ticket,
                    assignee: assignee.clone(),
                })
            }
            ExternalChangeKind::CommentAdded { author, body } => {
                known_ticket.map(|ticket| InboundAction::CommentAdded {
                    ticket,
                    author: author.clone(),
                    body: body.clone(),
                })
            }
            ExternalChangeKind::PriorityChanged { priority } => {
                known_ticket.map(|ticket| InboundAction::PriorityChanged {
                    ticket,
                    priority: priority.clone(),
                })
            }
            ExternalChangeKind::IssueCreated { title, body, .. } => {
                // An already-linked object can't also be "newly created" — that would be a
                // duplicate link, a bug upstream, not something to translate here.
                if known_ticket.is_some() {
                    None
                } else {
                    Some(InboundAction::NewDraftTicket {
                        external: change.external.clone(),
                        title: title.clone(),
                        body: body.clone(),
                    })
                }
            }
        }
    }

    /// Turn one translated action into the event draft(s) that record it. Mints a fresh
    /// [`TicketId`] via the injected [`IdSource`] for `NewDraftTicket` only — every other
    /// variant already carries the ticket it applies to.
    pub fn to_event_drafts(&self, action: &InboundAction) -> Vec<EventDraft> {
        match action {
            InboundAction::StatusHint { ticket, hint } => vec![EventDraft::new(
                self.actor.clone(),
                tm_types::Id::from(ticket.clone()),
                TicketUpdatedPayload {
                    ticket: ticket.clone(),
                    fields: serde_json::json!({ "status_hint": hint }),
                }
                .into(),
            )],
            InboundAction::AssignmentChanged { ticket, assignee } => vec![EventDraft::new(
                self.actor.clone(),
                tm_types::Id::from(ticket.clone()),
                TicketUpdatedPayload {
                    ticket: ticket.clone(),
                    fields: serde_json::json!({ "assignee": assignee }),
                }
                .into(),
            )],
            InboundAction::PriorityChanged { ticket, priority } => vec![EventDraft::new(
                self.actor.clone(),
                tm_types::Id::from(ticket.clone()),
                TicketUpdatedPayload {
                    ticket: ticket.clone(),
                    fields: serde_json::json!({ "priority": priority }),
                }
                .into(),
            )],
            InboundAction::CommentAdded {
                ticket,
                author,
                body,
            } => vec![EventDraft::new(
                self.actor.clone(),
                tm_types::Id::from(ticket.clone()),
                // The external author's identity is prose, not a ParticipantId, since we don't
                // have an on-platform participant for an arbitrary external user; self.actor is
                // who *recorded* the comment.
                CommentCreatedPayload {
                    ticket: Some(ticket.clone()),
                    author: self.actor.clone(),
                    body: format!("{author} (via mirror): {body}"),
                }
                .into(),
            )],
            InboundAction::NewDraftTicket {
                external,
                title,
                body,
            } => {
                let raw_id = self.ids.next(IdKind::Ticket);
                let new_id = TicketId::new(raw_id.as_str()).expect(
                    "IdSource::next(IdKind::Ticket) always yields a validly-shaped ticket id",
                );
                // TicketCreatedPayload has no body field; fold a short body into the title,
                // otherwise attach it as a follow-up comment so it isn't dropped.
                const FOLD_LIMIT: usize = 200;
                let combined_title = if body.is_empty() {
                    title.clone()
                } else if title.len() + body.len() <= FOLD_LIMIT {
                    format!("{title} — {body}")
                } else {
                    title.clone()
                };
                let mut drafts = vec![
                    EventDraft::new(
                        self.actor.clone(),
                        tm_types::Id::from(new_id.clone()),
                        // The ticket lands in `Draft` by tm-core's own materialization default
                        // for a newly created ticket, not by anything set here.
                        TicketCreatedPayload {
                            ticket: new_id.clone(),
                            title: combined_title,
                            parent: None,
                        }
                        .into(),
                    ),
                    EventDraft::new(
                        self.actor.clone(),
                        tm_types::Id::from(new_id.clone()),
                        MirrorLinkedPayload {
                            remote: external.adapter.clone(),
                        }
                        .into(),
                    ),
                ];
                if !body.is_empty() && title.len() + body.len() > FOLD_LIMIT {
                    drafts.push(EventDraft::new(
                        self.actor.clone(),
                        tm_types::Id::from(new_id.clone()),
                        CommentCreatedPayload {
                            ticket: Some(new_id),
                            author: self.actor.clone(),
                            body: body.clone(),
                        }
                        .into(),
                    ));
                }
                drafts
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::{NullTracker, RecordingTracker, TrackerCapabilities};
    use tm_events::Payload;
    use tm_types::{CounterIds, FixedClock};

    fn engine() -> SyncEngine {
        SyncEngine::new(
            Arc::new(FixedClock::epoch()),
            Arc::new(CounterIds::new()),
            ParticipantId::system(),
        )
    }

    fn full_capabilities() -> TrackerCapabilities {
        TrackerCapabilities {
            parent_child: true,
            arbitrary_states: true,
            milestones: true,
            labels: true,
            comments: true,
            max_body_bytes: 1024,
        }
    }

    fn projection(ticket: &str) -> Projection {
        Projection {
            ticket: TicketId::new(ticket).expect("valid ticket id"),
            title: "Title".to_string(),
            body: "Body".to_string(),
            state_hint: "open".to_string(),
            labels: vec!["bug".to_string()],
            milestone: Some("M1".to_string()),
            checklist: vec![],
            degradations: vec![],
        }
    }

    fn change(kind: ExternalChangeKind) -> ExternalChange {
        ExternalChange {
            external: ExternalRef {
                adapter: "github".to_string(),
                external_id: "42".to_string(),
                url: None,
            },
            kind,
            observed_at: Timestamp::EPOCH,
        }
    }

    #[test]
    fn projection_hash_is_stable_across_equal_projections() {
        assert_eq!(
            SyncEngine::projection_hash(&projection("T-1")),
            SyncEngine::projection_hash(&projection("T-1"))
        );
    }

    #[test]
    fn projection_hash_ignores_degradations() {
        let mut with_degradation = projection("T-1");
        with_degradation.degradations = vec![Degradation::BodyTruncated { original_bytes: 99 }];
        assert_eq!(
            SyncEngine::projection_hash(&projection("T-1")),
            SyncEngine::projection_hash(&with_degradation)
        );
    }

    #[test]
    fn projection_hash_differs_when_title_changes() {
        let mut other = projection("T-1");
        other.title = "Different".to_string();
        assert_ne!(
            SyncEngine::projection_hash(&projection("T-1")),
            SyncEngine::projection_hash(&other)
        );
    }

    #[tokio::test]
    async fn push_calls_tracker_and_records_a_link_and_event_on_first_push() {
        let tracker = RecordingTracker::new("github", full_capabilities());
        let proj = projection("T-1");

        let (link, draft) = engine()
            .push(&tracker, &proj, None)
            .await
            .expect("push succeeds");

        assert_eq!(link.ticket, proj.ticket);
        assert_eq!(link.adapter, "github");
        assert_eq!(link.content_hash, SyncEngine::projection_hash(&proj));
        assert_eq!(tracker.pushed().len(), 1);
        assert!(matches!(
            draft.expect("push emits an event").payload,
            Payload::MirrorPushed(_)
        ));
    }

    #[tokio::test]
    async fn pushing_twice_with_no_change_is_a_no_op() {
        let tracker = RecordingTracker::new("github", full_capabilities());
        let proj = projection("T-1");
        let e = engine();

        let (link, _) = e.push(&tracker, &proj, None).await.expect("first push");
        let (link_again, draft) = e
            .push(&tracker, &proj, Some(&link))
            .await
            .expect("second push");

        assert_eq!(link_again, link);
        assert!(draft.is_none());
        assert_eq!(
            tracker.pushed().len(),
            1,
            "tracker.push must not be called again"
        );
    }

    #[tokio::test]
    async fn pushing_a_changed_projection_pushes_again() {
        let tracker = RecordingTracker::new("github", full_capabilities());
        let proj = projection("T-1");
        let e = engine();

        let (link, _) = e.push(&tracker, &proj, None).await.expect("first push");
        let mut changed = proj.clone();
        changed.title = "New title".to_string();
        let (link2, draft) = e
            .push(&tracker, &changed, Some(&link))
            .await
            .expect("second push");

        assert_ne!(link2.content_hash, link.content_hash);
        assert!(draft.is_some());
        assert_eq!(tracker.pushed().len(), 2);
    }

    #[tokio::test]
    async fn push_propagates_tracker_errors() {
        struct FailingTracker;
        #[async_trait::async_trait]
        impl Tracker for FailingTracker {
            fn name(&self) -> &str {
                "failing"
            }
            fn capabilities(&self) -> TrackerCapabilities {
                TrackerCapabilities {
                    parent_child: true,
                    arbitrary_states: true,
                    milestones: true,
                    labels: true,
                    comments: true,
                    max_body_bytes: 1024,
                }
            }
            async fn push(&self, _projection: &Projection) -> Result<ExternalRef> {
                Err(tm_types::TmError::storage("adapter unreachable"))
            }
            async fn pull(&self, _since: Timestamp) -> Result<Vec<ExternalChange>> {
                Ok(Vec::new())
            }
        }

        let result = engine()
            .push(&FailingTracker, &projection("T-1"), None)
            .await;
        assert!(result.is_err());
    }

    fn linked(ticket: &str, since: Option<Timestamp>) -> MirrorLink {
        MirrorLink {
            ticket: TicketId::new(ticket).expect("valid ticket id"),
            adapter: "github".to_string(),
            external: ExternalRef {
                adapter: "github".to_string(),
                external_id: "42".to_string(),
                url: None,
            },
            degradations: vec![],
            content_hash: "irrelevant".to_string(),
            pushed_at: Timestamp::EPOCH,
            last_pulled_at: since,
        }
    }

    #[tokio::test]
    async fn pull_advances_last_pulled_at_even_with_no_changes() {
        let tracker = RecordingTracker::new("github", full_capabilities());
        let link = linked("T-1", None);

        let (updated, drafts) = engine().pull(&tracker, &link).await.expect("pull succeeds");

        assert!(updated.last_pulled_at.is_some());
        assert_eq!(tracker.pull_calls(), vec![Timestamp::EPOCH]);
        // Only the freshness-signalling mirror.pulled event.
        assert_eq!(drafts.len(), 1);
        assert!(matches!(drafts[0].payload, Payload::MirrorPulled(_)));
    }

    #[tokio::test]
    async fn pull_uses_epoch_as_since_when_never_pulled_before() {
        let tracker = RecordingTracker::new("github", full_capabilities());
        let link = linked("T-1", None);

        engine().pull(&tracker, &link).await.expect("pull succeeds");

        assert_eq!(tracker.pull_calls(), vec![Timestamp::EPOCH]);
    }

    #[tokio::test]
    async fn pull_uses_last_pulled_at_as_since_on_repeat_pulls() {
        let tracker = RecordingTracker::new("github", full_capabilities());
        let previous = Timestamp::EPOCH.plus_millis(1_000);
        let link = linked("T-1", Some(previous));

        engine().pull(&tracker, &link).await.expect("pull succeeds");

        assert_eq!(tracker.pull_calls(), vec![previous]);
    }

    #[tokio::test]
    async fn pull_translates_scripted_changes_into_event_drafts() {
        let tracker = RecordingTracker::new("github", full_capabilities());
        tracker.script_pull(vec![change(ExternalChangeKind::StatusHint {
            state: "closed".to_string(),
        })]);
        let link = linked("T-1", None);

        let (_, drafts) = engine().pull(&tracker, &link).await.expect("pull succeeds");

        assert_eq!(
            drafts.len(),
            2,
            "the translated change plus the freshness event"
        );
        assert!(matches!(drafts[0].payload, Payload::TicketUpdated(_)));
    }

    #[test]
    fn translate_drops_updates_on_unlinked_objects() {
        let e = engine();
        for kind in [
            ExternalChangeKind::StatusHint {
                state: "closed".to_string(),
            },
            ExternalChangeKind::Assigned {
                assignee: Some("alice".to_string()),
            },
            ExternalChangeKind::CommentAdded {
                author: "alice".to_string(),
                body: "hi".to_string(),
            },
            ExternalChangeKind::PriorityChanged {
                priority: "high".to_string(),
            },
        ] {
            assert_eq!(e.translate(None, &change(kind)), None);
        }
    }

    #[test]
    fn translate_maps_status_hint_on_a_known_ticket() {
        let e = engine();
        let ticket = TicketId::new("T-1").expect("valid ticket id");
        let action = e.translate(
            Some(ticket.clone()),
            &change(ExternalChangeKind::StatusHint {
                state: "closed".to_string(),
            }),
        );
        assert_eq!(
            action,
            Some(InboundAction::StatusHint {
                ticket,
                hint: "closed".to_string()
            })
        );
    }

    #[test]
    fn translate_drops_issue_created_on_an_already_linked_object() {
        let e = engine();
        let ticket = TicketId::new("T-1").expect("valid ticket id");
        let action = e.translate(
            Some(ticket),
            &change(ExternalChangeKind::IssueCreated {
                title: "New".to_string(),
                body: "Body".to_string(),
                author: "bob".to_string(),
            }),
        );
        assert_eq!(action, None);
    }

    #[test]
    fn translate_turns_an_unlinked_issue_created_into_a_new_draft_ticket() {
        let e = engine();
        let external = ExternalRef {
            adapter: "github".to_string(),
            external_id: "99".to_string(),
            url: None,
        };
        let action = e.translate(
            None,
            &ExternalChange {
                external: external.clone(),
                kind: ExternalChangeKind::IssueCreated {
                    title: "New".to_string(),
                    body: "Body".to_string(),
                    author: "bob".to_string(),
                },
                observed_at: Timestamp::EPOCH,
            },
        );
        assert_eq!(
            action,
            Some(InboundAction::NewDraftTicket {
                external,
                title: "New".to_string(),
                body: "Body".to_string(),
            })
        );
    }

    #[test]
    fn status_hint_becomes_a_ticket_updated_event_never_a_direct_write() {
        let e = engine();
        let ticket = TicketId::new("T-1").expect("valid ticket id");
        let drafts = e.to_event_drafts(&InboundAction::StatusHint {
            ticket: ticket.clone(),
            hint: "closed".to_string(),
        });
        assert_eq!(drafts.len(), 1);
        match &drafts[0].payload {
            Payload::TicketUpdated(p) => {
                assert_eq!(p.ticket, ticket);
                assert_eq!(p.fields, serde_json::json!({ "status_hint": "closed" }));
            }
            other => panic!("expected TicketUpdated, got {other:?}"),
        }
    }

    #[test]
    fn comment_added_attributes_the_recording_actor_and_notes_the_external_author() {
        let e = engine();
        let ticket = TicketId::new("T-1").expect("valid ticket id");
        let drafts = e.to_event_drafts(&InboundAction::CommentAdded {
            ticket: ticket.clone(),
            author: "alice".to_string(),
            body: "looks good".to_string(),
        });
        assert_eq!(drafts.len(), 1);
        match &drafts[0].payload {
            Payload::CommentCreated(p) => {
                assert_eq!(p.ticket, Some(ticket));
                assert_eq!(p.author, ParticipantId::system());
                assert_eq!(p.body, "alice (via mirror): looks good");
            }
            other => panic!("expected CommentCreated, got {other:?}"),
        }
    }

    #[test]
    fn new_draft_ticket_mints_an_id_folds_a_short_body_and_links_it() {
        let e = engine();
        let external = ExternalRef {
            adapter: "github".to_string(),
            external_id: "99".to_string(),
            url: None,
        };
        let drafts = e.to_event_drafts(&InboundAction::NewDraftTicket {
            external,
            title: "New".to_string(),
            body: "short body".to_string(),
        });

        assert_eq!(drafts.len(), 2);
        match &drafts[0].payload {
            Payload::TicketCreated(p) => {
                assert_eq!(p.ticket.as_str(), "T-1");
                assert!(p.title.contains("New"));
                assert!(p.title.contains("short body"));
                assert_eq!(p.parent, None);
            }
            other => panic!("expected TicketCreated, got {other:?}"),
        }
        assert!(matches!(drafts[1].payload, Payload::MirrorLinked(_)));
    }

    #[test]
    fn new_draft_ticket_attaches_a_long_body_as_a_follow_up_comment() {
        let e = engine();
        let external = ExternalRef {
            adapter: "github".to_string(),
            external_id: "99".to_string(),
            url: None,
        };
        let long_body = "x".repeat(500);
        let drafts = e.to_event_drafts(&InboundAction::NewDraftTicket {
            external,
            title: "New".to_string(),
            body: long_body.clone(),
        });

        assert_eq!(drafts.len(), 3);
        match &drafts[0].payload {
            Payload::TicketCreated(p) => assert_eq!(p.title, "New"),
            other => panic!("expected TicketCreated, got {other:?}"),
        }
        match &drafts[2].payload {
            Payload::CommentCreated(p) => assert_eq!(p.body, long_body),
            other => panic!("expected CommentCreated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn null_tracker_round_trip_never_produces_translatable_events() {
        let tracker = NullTracker::new("none");
        let link = linked("T-1", None);
        let (_, drafts) = engine().pull(&tracker, &link).await.expect("pull succeeds");
        assert_eq!(drafts.len(), 1, "only the freshness event");
    }
}
