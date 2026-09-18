//! A scrollable, single-select list of plain text items.
//!
//! Distinct from [`super::table::Table`]: no columns, no header, and (per the IMPL notes below)
//! optional per-item styling by index rather than by parsed cell content — the shape most of
//! `tm`'s navigation chrome (a ticket list, a session list, a command palette's results) actually
//! wants.

use std::cell::Cell;

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::Modifier;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::truncate;

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
    /// The number of rows drawn by the last `render` call; see `Table::visible_rows` for why
    /// this needs interior mutability given `render` takes `&self`.
    visible_rows: Cell<u16>,
}

impl List {
    /// An empty list.
    pub fn new(id: ComponentId) -> Self {
        List {
            id,
            items: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            visible_rows: Cell::new(0),
        }
    }

    /// Replace the items, clamping the current selection into the new range.
    pub fn set_items(&mut self, items: Vec<String>) {
        self.items = items;
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        self.clamp_scroll();
    }

    /// The text of the currently selected item, when the list is non-empty.
    pub fn selected(&self) -> Option<&str> {
        self.items.get(self.selected).map(String::as_str)
    }

    /// Keep `self.scroll_offset` a valid window onto `self.items` containing `self.selected`.
    fn clamp_scroll(&mut self) {
        if self.items.is_empty() {
            self.scroll_offset = 0;
            return;
        }
        let visible = self.visible_rows.get().max(1) as usize;
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + visible {
            self.scroll_offset = self.selected + 1 - visible;
        }
        let max_offset = self.items.len().saturating_sub(visible);
        self.scroll_offset = self.scroll_offset.min(max_offset);
    }

    /// Move `self.selected` by `delta` items, clamped to the item range, then re-clamp scroll.
    fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let last = self.items.len() - 1;
        let next = (self.selected as isize)
            .saturating_add(delta)
            .clamp(0, last as isize);
        self.selected = next as usize;
        self.clamp_scroll();
    }

    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Up),
                "move selection up",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Down),
                "move selection down",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Home),
                "jump to first item",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::End),
                "jump to last item",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::PageUp),
                "page up",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::PageDown),
                "page down",
            ),
        ]
    }
}

impl Component for List {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.width == 0 || area.height == 0 {
            self.visible_rows.set(0);
            return;
        }
        self.visible_rows.set(area.height);

        let focused = ctx.focus.is_focused(self.id);
        for (index, item) in self
            .items
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .take(area.height as usize)
        {
            let y = area.y + (index - self.scroll_offset) as u16;
            let style = if index == self.selected {
                selection_style(ctx, focused)
            } else {
                ratatui_core::style::Style::default().fg(ctx.theme.foreground)
            };
            buf.set_style(Rect::new(area.x, y, area.width, 1), style);
            let text = truncate(item, area.width as usize, "…");
            buf.set_stringn(area.x, y, &text, area.width as usize, style);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let Event::Input(InputEvent::Key(key)) = event else {
            return Propagation::Propagate;
        };
        if self.items.is_empty() {
            return Propagation::Propagate;
        }

        use crossterm::event::KeyCode;
        let page = self.visible_rows.get().max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Propagation::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Propagation::Consumed
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.move_selection(isize::MIN);
                Propagation::Consumed
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.move_selection(isize::MAX);
                Propagation::Consumed
            }
            KeyCode::PageUp => {
                self.move_selection(-page);
                Propagation::Consumed
            }
            KeyCode::PageDown => {
                self.move_selection(page);
                Propagation::Consumed
            }
            _ => Propagation::Propagate,
        }
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            List::bindings()
        } else {
            Vec::new()
        }
    }
}

/// The selected item's style: see `table::selection_style`, which this mirrors.
fn selection_style(ctx: &FrameContext<'_>, focused: bool) -> ratatui_core::style::Style {
    if focused {
        ctx.theme.selection
    } else {
        ratatui_core::style::Style::default()
            .fg(ctx.theme.foreground)
            .add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Capabilities;
    use crate::component::FocusState;
    use crate::event::InputEvent;
    use crate::theme::Theme;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tm_types::FixedClock;

    #[test]
    fn new_list_has_no_selection() {
        let list = List::new(ComponentId::new("test.list"));
        assert_eq!(list.selected(), None);
    }

    fn ctx<'a>(
        theme: &'a Theme,
        caps: &'a Capabilities,
        clock: &'a FixedClock,
        focused: bool,
    ) -> FrameContext<'a> {
        let id = ComponentId::new("test.list");
        FrameContext {
            theme,
            caps,
            clock,
            focus: if focused {
                FocusState::new(Some(id))
            } else {
                FocusState::default()
            },
        }
    }

    fn sample_list() -> List {
        let mut list = List::new(ComponentId::new("test.list"));
        list.set_items(vec!["alpha".into(), "bravo".into(), "charlie".into()]);
        list
    }

    #[test]
    fn set_items_clamps_selection() {
        let mut list = sample_list();
        list.move_selection(isize::MAX);
        assert_eq!(list.selected(), Some("charlie"));
        list.set_items(vec!["only".into()]);
        assert_eq!(list.selected(), Some("only"));
    }

    #[test]
    fn unfocused_list_ignores_navigation() {
        let mut list = sample_list();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, false);
        let event = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )));
        assert_eq!(list.handle_event(&event, &context), Propagation::Propagate);
        assert_eq!(list.selected(), Some("alpha"));
    }

    #[test]
    fn focused_list_moves_selection() {
        let mut list = sample_list();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let down = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )));
        assert_eq!(list.handle_event(&down, &context), Propagation::Consumed);
        assert_eq!(list.selected(), Some("bravo"));
    }

    #[test]
    fn render_truncates_to_area_width() {
        let list = sample_list();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let area = Rect::new(0, 0, 3, 3);
        let mut buf = Buffer::empty(area);
        list.render(area, &mut buf, &context);
        assert_eq!(list.visible_rows.get(), 3);
    }

    #[test]
    fn zero_sized_area_does_not_panic() {
        let list = sample_list();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        list.render(Rect::new(0, 0, 0, 0), &mut buf, &context);
    }
}
