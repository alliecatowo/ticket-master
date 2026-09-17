//! A live, auto-scrolling pane for an agent/session's streaming output — one of the D-002
//! callouts ("streaming panes") for the app-level component layer this crate builds.

use std::collections::VecDeque;

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, KeyBinding, KeyChord, Propagation};

/// A bounded ring buffer of output lines with auto-scroll ("follow") behaviour: new lines keep
/// the view pinned to the bottom until the user scrolls up, at which point new output accumulates
/// without yanking the view back down until they explicitly return to the bottom.
///
/// IMPL:
/// - `push_chunk`: agent/session output arrives as arbitrary text chunks, not pre-split lines
///   (see `crate::event::AppMessage::StreamChunk`) — split `chunk` on `\n`, appending the first
///   piece to `self.lines`' current last element (a chunk boundary can land mid-line) and pushing
///   the rest as new entries. After appending, if `self.lines.len() > self.max_lines`, pop from
///   the front (`VecDeque::pop_front`) until it is back at the cap — this changes what
///   `self.scroll_offset` (measured from the top) points at, so re-derive it from "distance from
///   the bottom" instead of leaving it as a raw index, or recompute it after trimming.
/// - `render`: when `self.follow` (the common case), draw the last `area.height` lines,
///   ignoring `self.scroll_offset`. When not following, draw `self.scroll_offset` lines up from
///   the bottom. Wrap long lines with `crate::text::wrap` rather than truncating — streaming
///   output (stack traces, long tool output) is usually more useful wrapped than clipped, unlike
///   the fixed-column widgets in `widgets_data`.
/// - `handle_event`: Up/PageUp/Home while following sets `self.follow = false` and starts
///   scrolling from the bottom; Down/PageDown/End move back toward the bottom and, on reaching
///   it, set `self.follow = true` again (so it re-locks to new output). Gate on focus like the
///   other widgets in this crate.
#[derive(Debug, Clone)]
pub struct StreamPane {
    id: ComponentId,
    lines: VecDeque<String>,
    max_lines: usize,
    /// Lines scrolled up from the bottom. Only meaningful when `!follow`.
    scroll_offset: usize,
    /// Whether new output should keep the view pinned to the bottom.
    follow: bool,
}

impl StreamPane {
    /// A pane that keeps at most `max_lines` of history, starting in follow mode.
    pub fn new(id: ComponentId, max_lines: usize) -> Self {
        StreamPane {
            id,
            lines: VecDeque::new(),
            max_lines,
            scroll_offset: 0,
            follow: true,
        }
    }

    /// Append a chunk of streamed text, splitting it into lines and trimming to `max_lines`.
    pub fn push_chunk(&mut self, chunk: &str) {
        let _ = chunk;
        todo!("split `chunk` into lines, append, and trim to `self.max_lines` per the IMPL note above")
    }

    /// Whether the pane is currently pinned to the bottom of the stream.
    pub fn is_following(&self) -> bool {
        self.follow
    }

    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Up), "scroll up, pause following"),
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::End), "jump to bottom, resume following"),
        ]
    }
}

impl Component for StreamPane {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let _ = (area, buf, ctx);
        todo!("draw the visible window of lines per the IMPL note above")
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let _ = event;
        todo!("scroll and toggle `self.follow` per the IMPL note above")
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            StreamPane::bindings()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_pane_starts_following_and_empty() {
        let pane = StreamPane::new(ComponentId::new("test.stream"), 1000);
        assert!(pane.is_following());
        assert!(pane.lines.is_empty());
    }
}
