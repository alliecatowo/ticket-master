//! A unified diff viewer.
//!
//! This widget renders an already-computed diff; it does not compute one itself (`similar`,
//! which the workspace already uses for diffing, is a `tm-core`/`tm-harness` concern — this
//! crate has no dependency on it, deliberately, since a pure rendering widget should not need
//! one). A screen or provider feeds this widget [`DiffLine`]s from wherever it already computes
//! or receives them (e.g. an [`crate::event::AppMessage::DiffReady`] handler reading a unified
//! diff produced elsewhere).

use std::cell::Cell;

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::Style;

use crate::caps::UnicodeSupport;
use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::truncate;

/// Width of each line-number gutter column. Five digits covers files up to 99,999 lines, past
/// which the number is truncated rather than the gutter widening and shifting every row.
const GUTTER_WIDTH: usize = 5;

/// What kind of line one row of a diff is, for styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// Unchanged context around a change.
    Context,
    /// A line only present in the new version.
    Added,
    /// A line only present in the old version.
    Removed,
    /// A hunk header (`@@ -a,b +c,d @@`).
    HunkHeader,
}

/// One rendered row of a diff.
#[derive(Debug, Clone)]
pub struct DiffLine {
    /// What kind of line this is.
    pub kind: DiffLineKind,
    /// This line's number in the old version, when it has one (absent for `Added` lines and
    /// `HunkHeader`).
    pub old_lineno: Option<u32>,
    /// This line's number in the new version, when it has one (absent for `Removed` lines and
    /// `HunkHeader`).
    pub new_lineno: Option<u32>,
    /// The line's text, without a leading `+`/`-`/` ` marker (the widget draws that itself from
    /// `kind`, so callers pass plain source text).
    pub text: String,
}

/// A scrollable unified diff view over one path.
///
/// IMPL:
/// - `render`: two gutter columns (old/new line numbers, blank where `None`) plus the marker
///   (`+`/`-`/` `, or `ctx.caps.unicode`-aware glyphs for `HunkHeader`) plus `line.text`,
///   truncated with `crate::text::truncate` to the remaining width. Style `Added` with
///   `ctx.theme.success`, `Removed` with `ctx.theme.danger`, `HunkHeader` with `ctx.theme.muted`,
///   `Context` with `ctx.theme.foreground`. Draw from `self.scroll_offset` like the widgets in
///   `widgets_data`.
/// - `handle_event`: Up/Down/PageUp/PageDown/Home/End scroll `self.scroll_offset` over
///   `self.lines.len()`, gated on focus, mirroring `Table`'s scrolling.
#[derive(Debug, Clone)]
pub struct Diff {
    id: ComponentId,
    path: String,
    lines: Vec<DiffLine>,
    scroll_offset: usize,
    /// Rows the last [`Component::render`] had available, so PageUp/PageDown move by a screenful.
    visible_rows: Cell<u16>,
}

impl Diff {
    /// An empty diff view over `path`, with no lines yet.
    pub fn new(id: ComponentId, path: impl Into<String>) -> Self {
        Diff {
            id,
            path: path.into(),
            lines: Vec::new(),
            scroll_offset: 0,
            visible_rows: Cell::new(0),
        }
    }

    /// The path this view is currently showing.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Replace the diff's path and lines, resetting scroll to the top.
    pub fn set_diff(&mut self, path: impl Into<String>, lines: Vec<DiffLine>) {
        self.path = path.into();
        self.lines = lines;
        self.scroll_offset = 0;
    }

    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Up), "scroll up"),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Down),
                "scroll down",
            ),
        ]
    }
}

impl Component for Diff {
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
        for (index, line) in self
            .lines
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .take(area.height as usize)
        {
            let y = area.y + (index - self.scroll_offset) as u16;

            let colour = match line.kind {
                DiffLineKind::Added => ctx.theme.success,
                DiffLineKind::Removed => ctx.theme.danger,
                DiffLineKind::HunkHeader => ctx.theme.muted,
                DiffLineKind::Context => ctx.theme.foreground,
            };
            let style = Style::default().fg(colour);

            // A hunk header has no line numbers on either side, so it spans the gutter instead
            // of drawing two columns of blanks.
            let row = if line.kind == DiffLineKind::HunkHeader {
                let marker = match ctx.caps.unicode {
                    UnicodeSupport::AsciiOnly => "@@",
                    _ => "❯❯",
                };
                format!("{marker} {}", line.text)
            } else {
                let marker = match line.kind {
                    DiffLineKind::Added => '+',
                    DiffLineKind::Removed => '-',
                    _ => ' ',
                };
                format!(
                    "{:>OLD_W$} {:>NEW_W$} {marker}{}",
                    line.old_lineno.map(|n| n.to_string()).unwrap_or_default(),
                    line.new_lineno.map(|n| n.to_string()).unwrap_or_default(),
                    line.text,
                    OLD_W = GUTTER_WIDTH,
                    NEW_W = GUTTER_WIDTH,
                )
            };

            let text = truncate(&row, width, "…");
            buf.set_stringn(area.x, y, &text, width, style);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let Event::Input(InputEvent::Key(key)) = event else {
            return Propagation::Propagate;
        };
        if self.lines.is_empty() {
            return Propagation::Propagate;
        }

        use crossterm::event::KeyCode;
        let page = self.visible_rows.get().max(1) as usize;
        // Stop scrolling when the last line reaches the top of the viewport, so the view cannot
        // be scrolled past the end into empty space.
        let max_offset = self.lines.len().saturating_sub(1);

        self.scroll_offset = match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_offset.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_offset.saturating_add(1).min(max_offset)
            }
            KeyCode::PageUp => self.scroll_offset.saturating_sub(page),
            KeyCode::PageDown => self.scroll_offset.saturating_add(page).min(max_offset),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => max_offset,
            _ => return Propagation::Propagate,
        };
        Propagation::Consumed
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            Diff::bindings()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_diff_replaces_path_and_resets_scroll() {
        let mut diff = Diff::new(ComponentId::new("test.diff"), "a.rs");
        diff.scroll_offset = 5;
        diff.set_diff(
            "b.rs",
            vec![DiffLine {
                kind: DiffLineKind::Added,
                old_lineno: None,
                new_lineno: Some(1),
                text: "fn main() {}".to_string(),
            }],
        );
        assert_eq!(diff.path(), "b.rs");
        assert_eq!(diff.scroll_offset, 0);
        assert_eq!(diff.lines.len(), 1);
    }
}
