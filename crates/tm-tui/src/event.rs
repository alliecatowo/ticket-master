//! The application event type and the propagation result components return from handling one.
//!
//! This is one of the two contracts every other file in this crate — and every one of the nine
//! parts scaffolded alongside it — codes against (see the crate root docs). Everything here is
//! finished and tested, not a stub: the shape must be right before anyone builds on it.
//!
//! An [`Event`] is the single currency the runtime (`runtime.rs`) hands to the root
//! [`crate::component::Component`], which bubbles it through the focus tree (`component.rs`).
//! Four kinds cover everything a component can react to: raw terminal input, a viewport resize,
//! a scheduled redraw tick, and an [`AppMessage`] produced by application logic running outside
//! the component tree (a streaming agent session, an event-stream subscription, a timer).
//!
//! [`AppMessage`] is a closed, concrete enum rather than a generic type parameter threaded
//! through every widget and screen: `tm-tui` does not depend on `tm-core`/`tm-events`, so its
//! variants describe *categories* of application news (a ticket changed, a stream produced a
//! chunk, a diff became ready, ...) using only `tm-types` identifiers, not full domain objects.
//! Screens translate between this and richer domain types at the boundary where they do have
//! that dependency.

use tm_types::{SessionId, Timestamp};

/// Raw terminal input, one layer above crossterm's own event type: crossterm's `Event::Resize`
/// is lifted out to [`Event::Resize`] at the runtime boundary so `InputEvent` is exactly the
/// subset a focused component reacts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// A key was pressed (or, on backends that report it, released/repeated).
    Key(crossterm::event::KeyEvent),
    /// A mouse button, movement, or scroll.
    Mouse(crossterm::event::MouseEvent),
    /// A bracketed paste delivered the enclosed text in one piece.
    Paste(String),
    /// The terminal window gained input focus (distinct from *component* focus).
    FocusGained,
    /// The terminal window lost input focus.
    FocusLost,
}

/// A key combination, used both to declare a [`KeyBinding`] and to match one at dispatch time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    /// The key itself.
    pub code: crossterm::event::KeyCode,
    /// Modifier keys held with `code`.
    pub modifiers: crossterm::event::KeyModifiers,
}

impl KeyChord {
    /// A chord with no modifiers held.
    pub fn plain(code: crossterm::event::KeyCode) -> Self {
        KeyChord {
            code,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    /// True when `event` (as delivered in an [`InputEvent::Key`]) matches this chord.
    ///
    /// Only `code` and `modifiers` participate: crossterm's `KeyEvent` also carries `kind`
    /// (press/release/repeat) and `state`, which a binding does not care about — a repeat should
    /// activate a binding exactly like a fresh press.
    pub fn matches(&self, event: &crossterm::event::KeyEvent) -> bool {
        self.code == event.code && self.modifiers == event.modifiers
    }
}

/// A key combination a component currently responds to, surfaced for a help overlay, the
/// command palette, and conflict detection across the focus chain.
#[derive(Debug, Clone)]
pub struct KeyBinding {
    /// The chord that activates this binding.
    pub chord: KeyChord,
    /// A short, user-facing description ("move selection down"), not the implementation detail.
    pub description: &'static str,
}

impl KeyBinding {
    /// Construct a binding from its chord and description.
    pub fn new(chord: KeyChord, description: &'static str) -> Self {
        KeyBinding { chord, description }
    }
}

/// How urgently a [`AppMessage::Toast`] should be presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    /// Routine confirmation ("saved", "ticket moved").
    Info,
    /// Worth a second look but not blocking.
    Warning,
    /// An operation failed.
    Error,
}

/// News produced by application logic outside the component tree, delivered on the next event
/// loop iteration via [`Event::App`].
///
/// This enum is deliberately closed (no catch-all `Custom(String)` escape hatch): every screen
/// that matches on it does so exhaustively, so adding a variant here is a compile-time nudge to
/// every call site that might care, rather than a silent no-op for consumers that forgot to
/// handle it. Extending this enum is expected as new screens land; do it here, in the shared
/// contract, not by inventing a parallel message type in `screens.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMessage {
    /// A ticket's summary state changed (status, assignee, or verification state moved).
    ///
    /// `id` uses `tm_types::TicketId`, which already covers `T-`/`V-`/`A-` prefixed identifiers
    /// (tickets, verification nodes, and audit nodes share one identifier shape).
    TicketChanged {
        /// The ticket, verification node, or audit node that changed.
        id: tm_types::TicketId,
    },
    /// A new chunk of an agent/session's streaming output arrived.
    StreamChunk {
        /// The session the chunk belongs to.
        session: SessionId,
        /// The chunk's text, already decoded (not raw bytes).
        text: String,
        /// True when this is the last chunk for the session's current turn.
        final_chunk: bool,
    },
    /// A unified diff became available or was updated for a path.
    DiffReady {
        /// The repo-relative path the diff covers.
        path: String,
    },
    /// A chat turn made progress or finished (`screens::chat::ChatScreen`). Carries plain
    /// transcript data ([`crate::chat::transcript::Entry`]), already translated from the agent's
    /// own step records by the application.
    Turn {
        /// The session the turn belongs to; a screen showing a different session ignores it.
        session: SessionId,
        /// What happened.
        update: crate::screens::chat::TurnUpdate,
    },
    /// A short-lived status message for the toast/notification area.
    Toast {
        /// How urgently to present it.
        level: ToastLevel,
        /// The message text.
        text: String,
    },
}

/// Something the root component tree may react to: input, a resize, a scheduled tick, or an
/// [`AppMessage`] from outside the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Raw terminal input.
    Input(InputEvent),
    /// The terminal viewport changed size, in columns and rows.
    Resize {
        /// New width, in terminal columns.
        width: u16,
        /// New height, in terminal rows.
        height: u16,
    },
    /// A scheduled redraw/animation tick fired.
    ///
    /// `at` comes from the runtime's injected `tm_types::Clock`, never a direct
    /// `std::time::Instant::now()` read — the hygiene check enforces this outside
    /// `tm-types::clock`, and it is what makes a recorded session replay identically.
    Tick {
        /// The tick's timestamp, per the injected clock.
        at: Timestamp,
    },
    /// A message produced by application logic outside the component tree.
    App(AppMessage),
}

/// What a component did with an [`Event`] it was offered, and whether the event should keep
/// bubbling to that component's ancestors.
///
/// Dispatch (the stubbed focus-tree walk in `component.rs`) starts at the focused leaf and offers
/// the event to each ancestor in turn until one returns `Consumed` or the root is reached. This
/// is what lets a screen-level binding ("q" to quit) fire regardless of which leaf widget has
/// focus, while a widget that *does* want the key (a text input's "q") can still claim it first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Propagation {
    /// The event was handled; do not offer it to this component's ancestors.
    Consumed,
    /// The event was not relevant to this component; keep bubbling.
    Propagate,
}

impl Propagation {
    /// True when this is [`Propagation::Consumed`].
    pub fn is_consumed(self) -> bool {
        matches!(self, Propagation::Consumed)
    }

    /// Short-circuiting combinator for a chain of handlers: stops at the first `Consumed` and
    /// otherwise falls through to `next`.
    ///
    /// This is how the focus-tree walk composes one ancestor's result with the next without
    /// re-checking `is_consumed()` at every step:
    /// `handler_a(event).or_else(|| handler_b(event))`.
    pub fn or_else(self, next: impl FnOnce() -> Propagation) -> Propagation {
        match self {
            Propagation::Consumed => Propagation::Consumed,
            Propagation::Propagate => next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn key_chord_matches_ignores_kind_and_state() {
        let chord = KeyChord::plain(KeyCode::Char('q'));
        let mut event = crossterm::event::KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(chord.matches(&event));

        event.kind = crossterm::event::KeyEventKind::Repeat;
        assert!(
            chord.matches(&event),
            "repeat should still match a plain press binding"
        );
    }

    #[test]
    fn key_chord_requires_matching_modifiers() {
        let chord = KeyChord::plain(KeyCode::Char('s'));
        let ctrl_s = crossterm::event::KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(!chord.matches(&ctrl_s));
    }

    #[test]
    fn propagation_is_consumed() {
        assert!(Propagation::Consumed.is_consumed());
        assert!(!Propagation::Propagate.is_consumed());
    }

    #[test]
    fn or_else_short_circuits_on_consumed() {
        let mut second_called = false;
        let result = Propagation::Consumed.or_else(|| {
            second_called = true;
            Propagation::Propagate
        });
        assert_eq!(result, Propagation::Consumed);
        assert!(
            !second_called,
            "or_else must not evaluate `next` once already consumed"
        );
    }

    #[test]
    fn or_else_falls_through_on_propagate() {
        let result = Propagation::Propagate.or_else(|| Propagation::Consumed);
        assert_eq!(result, Propagation::Consumed);
    }

    #[test]
    fn events_are_comparable_for_test_assertions() {
        let a = Event::Resize {
            width: 80,
            height: 24,
        };
        let b = Event::Resize {
            width: 80,
            height: 24,
        };
        let c = Event::Resize {
            width: 80,
            height: 25,
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn app_message_ticket_changed_round_trips_eq() {
        let id = tm_types::TicketId::new("T-1").expect("T-1 is a valid TicketId in this test");
        let msg = AppMessage::TicketChanged { id: id.clone() };
        assert_eq!(msg, AppMessage::TicketChanged { id });
    }
}
