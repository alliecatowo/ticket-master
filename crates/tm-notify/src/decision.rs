//! The pure part of desktop notifications: which event kinds are worth one, what they should
//! say, and the opt-out gate. Nothing here touches the filesystem, a clock, or a real
//! notification backend, so all of it is asserted directly rather than through a fake.

use tm_events::{Event, EventKind};

use crate::notifier::Notification;

/// The environment variable that opts a process out of desktop notifications entirely —
/// headless/CI/server contexts where popping a notification is pointless, and where a hung
/// notification daemon or an absent controlling terminal could otherwise turn a no-op into a
/// stall. Following this codebase's own `TM_`-prefixed convention (`TM_ACTOR`, `TM_HOME`,
/// `TM_SERVER_TOKEN`, `TM_COMPUTER_BACKEND`), not a bare `NO_NOTIFY`/`NO_COLOR`-style name.
pub const TM_NOTIFY_ENV_VAR: &str = "TM_NOTIFY";

/// True when `value` (as read from [`TM_NOTIFY_ENV_VAR`]) does **not** spell "disable
/// notifications". This is opt-*out*: unset, empty, or any spelling other than the recognized
/// falsy ones below leaves notifications enabled, matching the audit's "on by default, quiet
/// contexts turn it off" framing rather than requiring every caller to opt in.
///
/// Recognized falsy spellings, matched case-insensitively after trimming whitespace: `"0"`,
/// `"false"`, `"off"`, `"no"`.
pub fn notifications_enabled(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0") | Some("false") | Some("off") | Some("no")
    )
}

/// [`notifications_enabled`] read from the real process environment. The only place in this
/// module that touches `std::env`; kept separate so [`notifications_enabled`] itself stays a
/// plain, directly testable function of its input.
pub fn notifications_enabled_from_env() -> bool {
    notifications_enabled(std::env::var(TM_NOTIFY_ENV_VAR).ok().as_deref())
}

/// Which event kinds are worth interrupting a human for. Closed and exhaustive-by-intent
/// (`docs/audit-2026-09-18-fable.md` M-04's "done looks like": desktop notifications on
/// `approval.requested`/`ticket.escalated`) — extending this list is a deliberate call a future
/// change makes explicitly, not something that falls out of adding an `EventKind` variant
/// elsewhere.
pub fn should_notify(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::ApprovalRequested | EventKind::TicketEscalated
    )
}

/// Render `event` into a [`Notification`], or `None` when [`should_notify`] would say no for its
/// kind, or its payload does not (defensively) match its own kind. `tm_events` guarantees kind/
/// payload agreement at construction, so the `None` branch here is unreachable in practice, not a
/// real failure mode — this function just never panics on it either way.
pub fn format_notification(event: &Event) -> Option<Notification> {
    match event.kind {
        EventKind::ApprovalRequested => {
            let payload = event.payload.as_approval_requested()?;
            let ticket = payload
                .ticket
                .as_ref()
                .map(std::string::ToString::to_string)
                .unwrap_or_else(|| "no ticket".to_string());
            Some(Notification {
                title: format!("Ticketmaster: approval needed ({ticket})"),
                body: payload.note.clone(),
            })
        }
        EventKind::TicketEscalated => {
            let payload = event.payload.as_ticket_escalated()?;
            Some(Notification {
                title: format!("Ticketmaster: {} escalated", payload.ticket),
                body: payload.reason.clone(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_events::payload::{ApprovalRequestedPayload, Payload, TicketEscalatedPayload};
    use tm_types::{Id, ParticipantId, TicketId, Timestamp};

    fn base_event(kind: EventKind, payload: Payload) -> Event {
        Event {
            seq: 1,
            ts: Timestamp::EPOCH,
            kind,
            subject: Id::none(),
            actor: ParticipantId::system(),
            session: None,
            causation: None,
            correlation: None,
            payload,
            hash: "test-hash".to_string(),
        }
    }

    #[test]
    fn notifications_enabled_defaults_to_true_when_unset() {
        assert!(notifications_enabled(None));
    }

    #[test]
    fn notifications_enabled_true_for_nonsense_values() {
        assert!(notifications_enabled(Some("yes")));
        assert!(notifications_enabled(Some("1")));
        assert!(notifications_enabled(Some("")));
    }

    #[test]
    fn notifications_enabled_false_for_recognized_falsy_spellings() {
        for value in ["0", "false", "off", "no", "OFF", "  False  "] {
            assert!(
                !notifications_enabled(Some(value)),
                "expected {value:?} to disable notifications"
            );
        }
    }

    #[test]
    fn should_notify_selects_exactly_approval_requested_and_ticket_escalated() {
        for kind in tm_events::kind::ALL {
            let expected = matches!(
                kind,
                EventKind::ApprovalRequested | EventKind::TicketEscalated
            );
            assert_eq!(
                should_notify(*kind),
                expected,
                "should_notify disagreed with the expected set for {kind:?}"
            );
        }
    }

    #[test]
    fn format_notification_for_approval_requested_names_the_ticket_and_carries_the_reason() {
        let ticket = TicketId::new("T-1").unwrap();
        let event = base_event(
            EventKind::ApprovalRequested,
            Payload::from(ApprovalRequestedPayload {
                ticket: Some(ticket),
                requested_of: ParticipantId::system(),
                note: "wants to run `rm -rf /tmp/scratch`".to_string(),
            }),
        );

        let notification = format_notification(&event).expect("approval.requested should notify");
        assert!(notification.title.contains("T-1"), "{notification:?}");
        assert_eq!(notification.body, "wants to run `rm -rf /tmp/scratch`");
    }

    #[test]
    fn format_notification_for_approval_requested_without_a_ticket_says_so() {
        let event = base_event(
            EventKind::ApprovalRequested,
            Payload::from(ApprovalRequestedPayload {
                ticket: None,
                requested_of: ParticipantId::system(),
                note: "needs a decision".to_string(),
            }),
        );

        let notification = format_notification(&event).expect("approval.requested should notify");
        assert!(notification.title.contains("no ticket"), "{notification:?}");
    }

    #[test]
    fn format_notification_for_ticket_escalated_names_the_ticket_and_reason() {
        let ticket = TicketId::new("T-2").unwrap();
        let event = base_event(
            EventKind::TicketEscalated,
            Payload::from(TicketEscalatedPayload {
                ticket: ticket.clone(),
                reason: "retries exhausted".to_string(),
            }),
        );

        let notification = format_notification(&event).expect("ticket.escalated should notify");
        assert!(notification.title.contains("T-2"), "{notification:?}");
        assert_eq!(notification.body, "retries exhausted");
    }

    #[test]
    fn format_notification_returns_none_for_an_uninteresting_kind() {
        let event = base_event(
            EventKind::TicketCreated,
            Payload::from(tm_events::payload::TicketCreatedPayload {
                ticket: TicketId::new("T-3").unwrap(),
                title: "do the thing".to_string(),
                parent: None,
            }),
        );

        assert!(format_notification(&event).is_none());
    }
}
