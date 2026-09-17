//! A unified diff viewer.
//!
//! This widget renders an already-computed diff; it does not compute one itself (`similar`,
//! which the workspace already uses for diffing, is a `tm-core`/`tm-harness` concern — this
//! crate has no dependency on it, deliberately, since a pure rendering widget should not need
//! one). A screen or provider feeds this widget [`DiffLine`]s from wherever it already computes
//! or receives them (e.g. an [`crate::event::AppMessage::DiffReady`] handler reading a unified
//! diff produced elsewhere).

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, KeyBinding, KeyChord, Propagation};

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
}

impl Diff {
    /// An empty diff view over `path`, with no lines yet.
    pub fn new(id: ComponentId, path: impl Into<String>) -> Self {
        Diff {
            id,
            path: path.into(),
            lines: Vec::new(),
            scroll_offset: 0,
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
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Down), "scroll down"),
        ]
    }
}

impl Component for Diff {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let _ = (area, buf, ctx);
        todo!("draw the gutter, marker and text for visible lines per the IMPL note above")
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let _ = event;
        todo!("scroll `self.scroll_offset` over `self.lines` per the IMPL note above")
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
