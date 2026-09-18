//! The session stream screen: one agent/session's live streaming output, full-screen.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use tm_types::SessionId;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{AppMessage, Event, KeyBinding, Propagation};
use crate::widgets_viz::stream::StreamPane;

/// A single session's streaming output: a heading naming the session, and a [`StreamPane`]
/// filling the rest of the screen.
///
/// IMPL:
/// - `render`: draw a heading line showing `self.session` (e.g. `"Session S-7"`), styled with
///   `ctx.theme.accent`, in the top row of `area`; render `self.pane` into the remainder.
/// - `handle_event`: an `Event::App(AppMessage::StreamChunk { session, text, .. })` whose
///   `session` matches `self.session` is this screen's own concern rather than something to
///   forward to `self.pane` unconditionally — call `self.pane.push_chunk(&text)` and return
///   `Propagation::Consumed`; a chunk for a *different* session should `Propagate` past this
///   screen untouched (some other open session stream owns it). Every other event (scrolling,
///   focus) forwards straight to `self.pane` and returns its `Propagation`, since this screen has
///   exactly one focusable child.
#[derive(Debug)]
pub struct SessionStreamScreen {
    id: ComponentId,
    session: SessionId,
    pane: StreamPane,
}

impl SessionStreamScreen {
    /// A session stream screen for `session`, over the given pane.
    pub fn new(id: ComponentId, session: SessionId, pane: StreamPane) -> Self {
        SessionStreamScreen { id, session, pane }
    }

    /// The session this screen is streaming.
    pub fn session(&self) -> &SessionId {
        &self.session
    }
}

impl Component for SessionStreamScreen {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.height == 0 {
            return;
        }
        let heading = format!("Session {}", self.session.as_str());
        buf.set_stringn(
            area.x,
            area.y,
            heading,
            area.width as usize,
            ctx.theme.accent,
        );

        let pane_area = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        self.pane.render(pane_area, buf, ctx);
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if let Event::App(AppMessage::StreamChunk { session, .. }) = event {
            if session != &self.session {
                return Propagation::Propagate;
            }
        }
        self.pane.handle_event(event, ctx)
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        self.pane.keybindings(ctx)
    }

    fn focusable_children(&self) -> Vec<ComponentId> {
        vec![self.pane.id()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_screen_reports_the_pane_as_its_only_focusable_child() {
        let screen = SessionStreamScreen::new(
            ComponentId::new("session_stream"),
            SessionId::new("S-1").expect("S-1 is a valid SessionId in this test"),
            StreamPane::new(ComponentId::new("session_stream.pane"), 1000),
        );
        assert_eq!(
            screen.focusable_children(),
            vec![ComponentId::new("session_stream.pane")]
        );
    }

    #[test]
    fn stream_screen_remembers_its_session() {
        let screen = SessionStreamScreen::new(
            ComponentId::new("session_stream"),
            SessionId::new("S-1").expect("S-1 is a valid SessionId in this test"),
            StreamPane::new(ComponentId::new("session_stream.pane"), 1000),
        );
        assert_eq!(screen.session().as_str(), "S-1");
    }
}
