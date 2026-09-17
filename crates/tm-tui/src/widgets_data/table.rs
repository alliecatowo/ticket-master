//! A scrollable table with a fixed header row.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, KeyBinding, KeyChord, Propagation};

/// One column's header text and its relative width weight (see `theme::split_horizontal`'s
/// weight semantics).
#[derive(Debug, Clone)]
pub struct Column {
    /// The header text.
    pub title: String,
    /// This column's share of the table's width, relative to the other columns' weights.
    pub weight: u16,
}

impl Column {
    /// A column with the given title and weight.
    pub fn new(title: impl Into<String>, weight: u16) -> Self {
        Column { title: title.into(), weight }
    }
}

/// A scrollable table over row data, each row a `Vec<String>` of already-formatted cell text
/// (one entry per [`Column`]).
///
/// IMPL:
/// - `render`: split `area` into a one-row header (styled with `ctx.theme.muted` or similar) and
///   the remaining rows using `crate::theme::split_horizontal` for column widths within each row.
///   Draw only the rows from `self.scroll_offset` that fit in the remaining height. Style the row
///   at `self.selected` with `ctx.theme.selection`, but only apply the "this is focused" visual
///   (e.g. `ctx.theme.focus_border`) when `ctx.focus.is_focused(self.id())` — an unfocused
///   selection should still be visible (dimmer) so the user remembers where they left off. Use
///   `crate::text::truncate` to fit each cell to its column's width rather than letting it spill
///   into the next column.
/// - `handle_event`: only act when `ctx.focus.is_focused(self.id())` — an unfocused table must
///   not react to keys meant for whatever else has focus. Up/`k` and Down/`j` move `self.selected`
///   by one, clamped to `0..self.rows.len()`; Home/`g` and End/`G` jump to the first/last row;
///   PageUp/PageDown move by the number of visible rows computed from the last `render`'s area
///   (store it in `self.visible_rows` on render, or recompute from a stored last-known height).
///   After changing `self.selected`, clamp `self.scroll_offset` so the selection stays within the
///   visible window. Return `Propagation::Consumed` for every key it acts on, `Propagate`
///   otherwise (including when unfocused).
#[derive(Debug, Clone)]
pub struct Table {
    id: ComponentId,
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
    selected: usize,
    scroll_offset: usize,
}

impl Table {
    /// An empty table with the given columns.
    pub fn new(id: ComponentId, columns: Vec<Column>) -> Self {
        Table {
            id,
            columns,
            rows: Vec::new(),
            selected: 0,
            scroll_offset: 0,
        }
    }

    /// Replace the row data, clamping the current selection into the new range.
    ///
    /// IMPL: `self.rows = rows`; then `self.selected = self.selected.min(self.rows.len().saturating_sub(1))`
    /// and re-clamp `self.scroll_offset` the same way `handle_event`'s scrolling does.
    pub fn set_rows(&mut self, rows: Vec<Vec<String>>) {
        let _ = rows;
        todo!("store `rows` and clamp selection/scroll per the IMPL note above")
    }

    /// The index of the currently selected row, when there are any rows.
    pub fn selected(&self) -> Option<usize> {
        if self.rows.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    /// This table's declared keybindings, independent of focus (used by `keybindings()` and
    /// reusable by a help overlay listing bindings for widgets that are not currently focused).
    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Up), "move selection up"),
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Down), "move selection down"),
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Home), "jump to first row"),
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::End), "jump to last row"),
        ]
    }
}

impl Component for Table {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let _ = (area, buf, ctx);
        todo!("draw the header and visible rows per the IMPL note above")
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let _ = event;
        todo!("move `self.selected`/`self.scroll_offset` on Up/Down/Home/End/PageUp/PageDown per the IMPL note above")
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            Table::bindings()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_table_has_no_selection() {
        let table = Table::new(ComponentId::new("test.table"), vec![Column::new("Name", 1)]);
        assert_eq!(table.selected(), None);
    }

    #[test]
    fn column_stores_title_and_weight() {
        let col = Column::new("Status", 2);
        assert_eq!(col.title, "Status");
        assert_eq!(col.weight, 2);
    }
}
