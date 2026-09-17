//! A scrollable, single-select list of plain text items.
//!
//! Distinct from [`super::table::Table`]: no columns, no header, and (per the IMPL notes below)
//! optional per-item styling by index rather than by parsed cell content — the shape most of
//! `tm`'s navigation chrome (a ticket list, a session list, a command palette's results) actually
//! wants.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, KeyBinding, KeyChord, Propagation};

/// A scrollable, single-select list.
///
/// IMPL:
/// - `render`: draw `self.items[self.scroll_offset..]` top to bottom until `area` is exhausted,
///   one item per line, truncated with `crate::text::truncate` to `area.width`. Style
///   `self.selected`'s line with `ctx.theme.selection` (full emphasis only when
///   `ctx.focus.is_focused(self.id())`, dimmer otherwise — see `Table`'s render note for the same
///   reasoning).
/// - `handle_event`: identical navigation shape to `Table`'s (Up/`k`, Down/`j`, Home/`g`, End/`G`,
///   PageUp/PageDown against `self.items.len()`), gated on focus. Return `Consumed` for keys
///   acted on.
#[derive(Debug, Clone)]
pub struct List {
    id: ComponentId,
    items: Vec<String>,
    selected: usize,
    scroll_offset: usize,
}

impl List {
    /// An empty list.
    pub fn new(id: ComponentId) -> Self {
        List {
            id,
            items: Vec::new(),
            selected: 0,
            scroll_offset: 0,
        }
    }

    /// Replace the items, clamping the current selection into the new range.
    ///
    /// IMPL: mirrors `Table::set_rows` — clamp `self.selected` and `self.scroll_offset` after
    /// replacing `self.items`.
    pub fn set_items(&mut self, items: Vec<String>) {
        let _ = items;
        todo!("store `items` and clamp selection/scroll per the IMPL note above")
    }

    /// The text of the currently selected item, when the list is non-empty.
    pub fn selected(&self) -> Option<&str> {
        self.items.get(self.selected).map(String::as_str)
    }

    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Up), "move selection up"),
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Down), "move selection down"),
        ]
    }
}

impl Component for List {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        let _ = (area, buf, ctx);
        todo!("draw visible items per the IMPL note above")
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let _ = event;
        todo!("move `self.selected`/`self.scroll_offset` per the IMPL note above")
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            List::bindings()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_list_has_no_selection() {
        let list = List::new(ComponentId::new("test.list"));
        assert_eq!(list.selected(), None);
    }
}
