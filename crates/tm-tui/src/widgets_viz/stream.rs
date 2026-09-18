//! A live, auto-scrolling pane for an agent/session's streaming output — one of the D-002
//! callouts ("streaming panes") for the app-level component layer this crate builds.

use std::cell::Cell;
use std::collections::VecDeque;

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::Style;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::wrap;

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
    ///
    /// Measured as a distance from the bottom rather than an index from the top, so trimming
    /// old lines off the front in [`StreamPane::push_chunk`] does not silently move the view.
    scroll_offset: usize,
    /// Whether new output should keep the view pinned to the bottom.
    follow: bool,
    /// Rows the last [`Component::render`] had available, so PageUp/PageDown can move by a
    /// screenful without the event handler needing to know the layout.
    visible_rows: Cell<u16>,
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
            visible_rows: Cell::new(0),
        }
    }

    /// Append a chunk of streamed text, splitting it into lines and trimming to `max_lines`.
    pub fn push_chunk(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }

        // A chunk boundary can land mid-line, so the first piece continues whatever line is
        // currently last rather than starting a new one.
        let mut pieces = chunk.split('\n');
        if let Some(first) = pieces.next() {
            match self.lines.back_mut() {
                Some(last) => last.push_str(first),
                None => self.lines.push_back(first.to_string()),
            }
        }
        for piece in pieces {
            self.lines.push_back(piece.to_string());
        }

        while self.lines.len() > self.max_lines {
            self.lines.pop_front();
        }

        // `scroll_offset` is a distance from the bottom, so trimming the front cannot invalidate
        // it — but it can now point past the start of a shortened history.
        let max_offset = self.lines.len().saturating_sub(1);
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
    }

    /// The number of lines currently held, after trimming to `max_lines`.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the pane holds no output yet.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Whether the pane is currently pinned to the bottom of the stream.
    pub fn is_following(&self) -> bool {
        self.follow
    }

    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Up),
                "scroll up, pause following",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::End),
                "jump to bottom, resume following",
            ),
        ]
    }
}

impl Component for StreamPane {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.width == 0 || area.height == 0 {
            self.visible_rows.set(0);
            return;
        }
        self.visible_rows.set(area.height);

        let width = area.width as usize;
        let height = area.height as usize;
        let offset = if self.follow { 0 } else { self.scroll_offset };
        let needed = height.saturating_add(offset);

        // Walk backwards from the newest line and wrap only as much history as the viewport can
        // show. Wrapping the whole ring buffer every frame would make cost grow with history
        // rather than with screen size.
        let mut from_bottom: Vec<String> = Vec::with_capacity(needed);
        'lines: for line in self.lines.iter().rev() {
            for row in wrap(line, width).into_iter().rev() {
                from_bottom.push(row);
                if from_bottom.len() >= needed {
                    break 'lines;
                }
            }
        }

        let style = Style::default().fg(ctx.theme.foreground);
        // `from_bottom[0]` is the bottom-most row; skip the scrolled-past rows, take a screenful,
        // then draw oldest-first so output reads top-to-bottom.
        for (index, row) in from_bottom
            .iter()
            .skip(offset)
            .take(height)
            .rev()
            .enumerate()
        {
            let y = area.y + index as u16;
            buf.set_stringn(area.x, y, row, width, style);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let Event::Input(InputEvent::Key(key)) = event else {
            return Propagation::Propagate;
        };

        use crossterm::event::KeyCode;
        let page = self.visible_rows.get().max(1) as usize;
        let max_offset = self.lines.len().saturating_sub(1);

        // Scrolling up unpins the view so new output accumulates instead of yanking it back
        // down; reaching the bottom again re-locks it.
        let scrolled = match key.code {
            KeyCode::Up => self.scroll_offset.saturating_add(1).min(max_offset),
            KeyCode::PageUp => self.scroll_offset.saturating_add(page).min(max_offset),
            KeyCode::Home => max_offset,
            KeyCode::Down => self.scroll_offset.saturating_sub(1),
            KeyCode::PageDown => self.scroll_offset.saturating_sub(page),
            KeyCode::End => 0,
            _ => return Propagation::Propagate,
        };

        self.scroll_offset = scrolled;
        self.follow = scrolled == 0;
        Propagation::Consumed
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

    fn pane() -> StreamPane {
        StreamPane::new(ComponentId::new("test.stream"), 1000)
    }

    #[test]
    fn new_pane_starts_following_and_empty() {
        let pane = StreamPane::new(ComponentId::new("test.stream"), 1000);
        assert!(pane.is_following());
        assert!(pane.lines.is_empty());
    }

    #[test]
    fn chunk_boundary_landing_mid_line_continues_that_line() {
        // Streamed output arrives as arbitrary chunks, so "hello world" can be split anywhere.
        // Treating each chunk as a line would corrupt the stream into two lines.
        let mut pane = pane();
        pane.push_chunk("hel");
        pane.push_chunk("lo world\n");
        assert_eq!(pane.lines[0], "hello world");
    }

    #[test]
    fn newlines_split_into_separate_lines() {
        let mut pane = pane();
        pane.push_chunk("a\nb\nc");
        assert_eq!(pane.lines.len(), 3);
        assert_eq!(pane.lines[2], "c");
    }

    #[test]
    fn history_is_trimmed_to_max_lines_from_the_front() {
        let mut pane = StreamPane::new(ComponentId::new("test.stream"), 3);
        pane.push_chunk("1\n2\n3\n4\n5");
        assert_eq!(pane.lines.len(), 3);
        assert_eq!(pane.lines[0], "3");
        assert_eq!(pane.lines[2], "5");
    }

    #[test]
    fn trimming_does_not_move_the_scrolled_view() {
        // `scroll_offset` is a distance from the bottom precisely so that dropping old lines
        // off the front leaves the user looking at the same content.
        let mut pane = StreamPane::new(ComponentId::new("test.stream"), 4);
        pane.push_chunk("1\n2\n3\n4");
        pane.scroll_offset = 2;
        pane.follow = false;
        pane.push_chunk("\n5");
        assert_eq!(pane.scroll_offset, 2);
    }

    #[test]
    fn scroll_offset_is_clamped_to_available_history() {
        let mut pane = StreamPane::new(ComponentId::new("test.stream"), 2);
        pane.push_chunk("1\n2\n3\n4\n5\n6");
        pane.scroll_offset = 99;
        pane.push_chunk("\n7");
        assert!(pane.scroll_offset <= pane.lines.len().saturating_sub(1));
    }
}
