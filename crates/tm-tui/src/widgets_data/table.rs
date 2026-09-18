//! A scrollable table with a fixed header row.

use std::cell::Cell;

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Constraint, Direction, Layout, Rect};
use ratatui_core::style::Modifier;

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::truncate;

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
        Column {
            title: title.into(),
            weight,
        }
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
    /// The number of body rows drawn by the last `render` call, cached here (rather than passed
    /// separately) so `handle_event` can compute PageUp/PageDown and the visible window without
    /// the `Component` contract growing an area parameter on `handle_event` just for this.
    /// `render` takes `&self`, so this needs interior mutability rather than a plain field.
    visible_rows: Cell<u16>,
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
            visible_rows: Cell::new(0),
        }
    }

    /// Replace the row data, clamping the current selection into the new range.
    pub fn set_rows(&mut self, rows: Vec<Vec<String>>) {
        self.rows = rows;
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.clamp_scroll();
    }

    /// The index of the currently selected row, when there are any rows.
    pub fn selected(&self) -> Option<usize> {
        if self.rows.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    /// This widget's column widths in cells, proportional to each column's weight, for `width`
    /// total columns.
    fn column_widths(&self, width: u16) -> Vec<u16> {
        if self.columns.is_empty() {
            return Vec::new();
        }
        let total_weight: u32 = self.columns.iter().map(|c| c.weight as u32).sum();
        if total_weight == 0 {
            return vec![0; self.columns.len()];
        }
        let constraints: Vec<Constraint> = self
            .columns
            .iter()
            .map(|c| Constraint::Ratio(c.weight as u32, total_weight))
            .collect();
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .split(Rect::new(0, 0, width, 1))
            .iter()
            .map(|r| r.width)
            .collect()
    }

    /// Draw one row's cells (header or data) at `y`, truncating each to its column's width.
    fn render_row(
        &self,
        buf: &mut Buffer,
        area: Rect,
        y: u16,
        widths: &[u16],
        cells: &[String],
        style: ratatui_core::style::Style,
    ) {
        buf.set_style(Rect::new(area.x, y, area.width, 1), style);
        let mut x = area.x;
        for (cell, width) in cells.iter().zip(widths.iter().copied()) {
            let text = truncate(cell, width as usize, "…");
            buf.set_stringn(x, y, &text, width as usize, style);
            x = x.saturating_add(width);
        }
    }

    /// Keep `self.scroll_offset` a valid window onto `self.rows` containing `self.selected`,
    /// given `self.visible_rows` body rows are visible at a time.
    fn clamp_scroll(&mut self) {
        if self.rows.is_empty() {
            self.scroll_offset = 0;
            return;
        }
        let visible = self.visible_rows.get().max(1) as usize;
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + visible {
            self.scroll_offset = self.selected + 1 - visible;
        }
        let max_offset = self.rows.len().saturating_sub(visible);
        self.scroll_offset = self.scroll_offset.min(max_offset);
    }

    /// Move `self.selected` by `delta` rows, clamped to the row range, then re-clamp scroll.
    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let next = (self.selected as isize)
            .saturating_add(delta)
            .clamp(0, last as isize);
        self.selected = next as usize;
        self.clamp_scroll();
    }

    /// This table's declared keybindings, independent of focus (used by `keybindings()` and
    /// reusable by a help overlay listing bindings for widgets that are not currently focused).
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
                "jump to first row",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::End),
                "jump to last row",
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

impl Component for Table {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.width == 0 || area.height == 0 {
            self.visible_rows.set(0);
            return;
        }

        let header_height = 1u16.min(area.height);
        let body_height = area.height - header_height;
        self.visible_rows.set(body_height);

        let widths = self.column_widths(area.width);
        let header: Vec<String> = self.columns.iter().map(|c| c.title.clone()).collect();
        self.render_row(buf, area, area.y, &widths, &header, ctx.theme.muted.into());

        let focused = ctx.focus.is_focused(self.id);
        for (row_index, row) in self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .take(body_height as usize)
        {
            let y = area.y + header_height + (row_index - self.scroll_offset) as u16;
            let style = if row_index == self.selected {
                selection_style(ctx, focused)
            } else {
                ratatui_core::style::Style::default().fg(ctx.theme.foreground)
            };
            self.render_row(buf, area, y, &widths, row, style);
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let Event::Input(InputEvent::Key(key)) = event else {
            return Propagation::Propagate;
        };
        if self.rows.is_empty() {
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
            Table::bindings()
        } else {
            Vec::new()
        }
    }
}

/// The selected row's style: full emphasis (`ctx.theme.selection`, typically an inverted colour
/// block) when this widget holds keyboard focus, a dimmer bold-only indicator otherwise — so a
/// user can still find where they left off in an unfocused table without it competing visually
/// with whatever *is* currently focused.
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
    use crossterm::event::{KeyCode, KeyEvent};
    use tm_types::FixedClock;

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

    fn ctx<'a>(
        theme: &'a Theme,
        caps: &'a Capabilities,
        clock: &'a FixedClock,
        focused: bool,
    ) -> FrameContext<'a> {
        let id = ComponentId::new("test.table");
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

    fn sample_table() -> Table {
        let mut table = Table::new(
            ComponentId::new("test.table"),
            vec![Column::new("Name", 1), Column::new("Status", 1)],
        );
        table.set_rows(vec![
            vec!["alpha".into(), "open".into()],
            vec!["bravo".into(), "closed".into()],
            vec!["charlie".into(), "open".into()],
        ]);
        table
    }

    #[test]
    fn set_rows_clamps_selection_into_range() {
        let mut table = sample_table();
        table.move_selection(isize::MAX);
        assert_eq!(table.selected(), Some(2));
        table.set_rows(vec![vec!["only".into(), "row".into()]]);
        assert_eq!(table.selected(), Some(0));
    }

    #[test]
    fn set_rows_on_empty_rows_has_no_selection() {
        let mut table = sample_table();
        table.set_rows(Vec::new());
        assert_eq!(table.selected(), None);
    }

    #[test]
    fn unfocused_table_ignores_navigation_keys() {
        let mut table = sample_table();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, false);
        let event = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        )));
        let propagation = table.handle_event(&event, &context);
        assert_eq!(propagation, Propagation::Propagate);
        assert_eq!(table.selected(), Some(0));
    }

    #[test]
    fn focused_table_moves_selection_down_and_wraps_at_end_via_clamp() {
        let mut table = sample_table();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let down = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        )));
        assert_eq!(table.handle_event(&down, &context), Propagation::Consumed);
        assert_eq!(table.selected(), Some(1));

        let end = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::End,
            crossterm::event::KeyModifiers::NONE,
        )));
        assert_eq!(table.handle_event(&end, &context), Propagation::Consumed);
        assert_eq!(table.selected(), Some(2));

        let home = Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Home,
            crossterm::event::KeyModifiers::NONE,
        )));
        assert_eq!(table.handle_event(&home, &context), Propagation::Consumed);
        assert_eq!(table.selected(), Some(0));
    }

    #[test]
    fn render_draws_header_and_rows_within_width() {
        let table = sample_table();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        table.render(area, &mut buf, &context);

        let header: String = (0..area.width)
            .map(|x| buf[(x, 0)].symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(header.contains("Name"));
        assert_eq!(table.visible_rows.get(), 4);
    }

    #[test]
    fn zero_sized_area_does_not_panic() {
        let table = sample_table();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let area = Rect::new(0, 0, 0, 0);
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        table.render(area, &mut buf, &context);
    }
}
