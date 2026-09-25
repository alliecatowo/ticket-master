//! The Kanban board: tickets as cards in caller-labelled columns — in practice the same five
//! group labels `screens::tickets::TicketsScreen` groups its rows under (Needs input / Working /
//! Ready for review / Queued / Completed, D-024's one-set-of-display-labels rule), though this
//! widget has no opinion on that and never hardcodes a column count or name (this crate has no
//! `tm_core` dependency, so the caller hands over already-labelled [`KanbanColumn`]s — see
//! [`KanbanCard`]'s docs).
//!
//! Unlike a flat ticket table (one list, sorted, scrolled vertically), this is a genuinely
//! two-dimensional board: columns scroll horizontally, each
//! column's cards scroll vertically and independently of every other column's scroll position —
//! the real point of a Kanban board over a table, and the literal reading of "paradigm-shifting"
//! the caller's own product brief asked for over a second ticket table.
//!
//! IMPL:
//! - This crate's other multi-pane screens (e.g. `TicketDetailScreen`) each fix their
//!   focus to one pane at a time and forward events to whichever `Table`/`List` child owns it.
//!   `Kanban` deliberately does not compose `Table`/`List` children the same way: this app's
//!   `component::FocusTree` only ever advances focus for entries actually present in
//!   `focusable_children()`, and nothing in this crate's `runtime.rs` yet calls
//!   `FocusTree::focus_next` (`Home`'s own docs call this out as a known, pre-existing gap this
//!   screen does not attempt to fix). A `Kanban` built from N+1 separately-focusable `List`s (one
//!   per column) would be unreachable past the first one under that gap. Instead `Kanban` owns
//!   `selected_column`/`selected_card`/`scroll_offset` directly and answers every key itself
//!   while `ctx.focus.is_focused(self.id())` — the same trick `App` (`tm-cli`) uses to make this
//!   screen reachable at all: list `kanban.id()` as the *only* focusable child while this screen
//!   is the one on screen, so focus has nowhere else to go.

use std::cell::Cell;

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::Modifier;

use crate::chat::glyphs::Glyphs;
use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::{display_width, truncate};
use crate::theme::Theme;

/// One card: a ticket's id and a short label. Both already-formatted strings, not a `TicketId`/
/// domain type — this crate has no `tm_core` dependency (see the module doc comment), matching
/// this crate's other multi-pane screens' `Table` rows, which are `Vec<String>` for the same
/// reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KanbanCard {
    /// The ticket's id, e.g. `"T-42"` — carried back out via [`Kanban::take_activation`] when
    /// this card is opened, so the caller (which does know `TicketId`) can parse it back.
    pub id: String,
    /// A short label for the card body — typically the ticket's objective, already truncated by
    /// the caller if a hard cap matters, though [`Kanban::render`] also truncates to the column's
    /// own width regardless.
    pub title: String,
}

impl KanbanCard {
    /// A card with the given id and title.
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        KanbanCard {
            id: id.into(),
            title: title.into(),
        }
    }
}

/// One column: a heading (the caller's own display label — a ticket-screen group name like
/// `"Working"` in `tm-cli`'s board, though this crate has no opinion on what a column means) and
/// its cards, top to bottom in whatever order the caller decided (typically the same
/// deterministic order `tm_core::ProjectView::tickets` iterates in).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KanbanColumn {
    /// The column heading.
    pub title: String,
    /// This column's cards.
    pub cards: Vec<KanbanCard>,
}

impl KanbanColumn {
    /// A column with the given title and cards.
    pub fn new(title: impl Into<String>, cards: Vec<KanbanCard>) -> Self {
        KanbanColumn {
            title: title.into(),
            cards,
        }
    }
}

/// The hub's shared identity line, shown above the board's own tab strip — `tm vX.Y.Z`, the
/// model, and the project's place, the same three facts
/// `screens::tickets::TicketsScreen`'s compact header line shows. Plain strings, not a
/// `tm_core`/`Project` type (see the module doc comment): the caller (`tm-cli`) already computes
/// these once per refresh for the tickets screen and hands the same values over here via
/// [`Kanban::set_header`], so the two screens cannot disagree about them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KanbanHeader {
    /// `tm`'s version (`0.1.0`).
    pub version: String,
    /// The model dispatched workers use.
    pub model: String,
    /// Where the project is (`~/src/app (main)`).
    pub place: String,
}

/// The hub's tab strip (Tickets · Board · Milestones · Timeline · Graph), in the order
/// `tm-cli`'s `next_tab` cycles. Duplicated from `screens::tickets::TicketsScreen::TABS` (private
/// to that screen) rather than shared, since D-024's one-set-of-display-labels rule is about the
/// *labels* matching, not the two screens sharing code — this board highlights "Board" instead of
/// "Tickets", the one difference between the two renderings.
const TABS: [&str; 5] = ["Tickets", "Board", "Milestones", "Timeline", "Graph"];

/// Which of [`TABS`] this screen highlights.
const BOARD_TAB: usize = 1;

/// Rows reserved at the top of the board for the shared identity line and tab strip, before the
/// usage hint and the columns themselves.
const HEADER_HEIGHT: u16 = 2;

/// Default width, in terminal columns, of one Kanban column (heading + cards) — used once the
/// area is too narrow to give every column at least [`MIN_COLUMN_WIDTH`] (`render` shares the
/// width evenly instead; see there). Card text is truncated to fit via `crate::text::truncate`,
/// matching `Table`/`List`'s own per-cell truncation. Chosen wide enough for a real ticket id
/// (`"T-1234"`) plus a few words of objective to read as more than an id, narrow enough that a
/// common 80-column terminal still shows more than two columns without any horizontal scrolling
/// at all.
const COLUMN_WIDTH: u16 = 24;

/// The narrowest a column is ever squeezed to when every column fits the area at once (`render`
/// shares the width evenly in that case) — still wide enough for a short ticket id and a couple
/// of truncated words, e.g. `"Ready for review (1)"` at exactly this width. Below this, `render`
/// falls back to [`COLUMN_WIDTH`] and horizontal scrolling instead of squeezing further.
const MIN_COLUMN_WIDTH: u16 = 20;

/// One row reserved at the top of the board for a static usage hint — the same "how do I use
/// this" affordance `screens::home::Home`'s always-visible input label gives the chat screen,
/// since this screen's own keys (as opposed to `Table`/`List`'s now-conventional Up/Down/Home/
/// End/PageUp/PageDown) are new enough in this crate to be worth spelling out on screen rather
/// than only in `keybindings()`.
const HINT_HEIGHT: u16 = 1;

const HINT_TEXT: &str = "Left/Right: columns   Up/Down: cards   Enter: open   Esc: back";

/// The Kanban board: N columns (never hardcoded — see `set_columns`), each independently
/// vertically scrollable. When every column can have at least `MIN_COLUMN_WIDTH` at once, `render`
/// shares the area's width evenly across them so all N are visible with no scrolling at all (the
/// common case for `tm-cli`'s 5-group board); otherwise the whole set of columns scrolls
/// horizontally at a fixed width rather than being squeezed, paginated, or relabelled to fit.
#[derive(Debug)]
pub struct Kanban {
    id: ComponentId,
    columns: Vec<KanbanColumn>,
    selected_column: usize,
    /// Parallel to `columns`: which card is selected within each column. Kept per-column (not
    /// one shared index) so scrolling away from a column and back remembers where the user left
    /// off in it, the same "unfocused selection stays visible, just dimmer" courtesy
    /// `widgets_data::table::Table::selected` already gives a single list.
    selected_card: Vec<usize>,
    /// Parallel to `columns`: each column's own vertical scroll offset.
    scroll_offset: Vec<usize>,
    /// Horizontal scroll: the index of the first visible column.
    column_offset: usize,
    /// How many columns fit in the last `render`'s area, cached for `handle_event`'s clamping —
    /// see `widgets_data::table::Table::visible_rows` for why this needs interior mutability
    /// given `render` takes `&self`.
    visible_columns: Cell<usize>,
    /// How many card rows are visible per column in the last `render`'s area.
    visible_rows: Cell<u16>,
    /// Set by `handle_event` on Enter over a card, taken (and cleared) by
    /// [`Kanban::take_activation`] — the same poll-based handoff
    /// `screens::home::Home::take_submission` uses, for the same reason: this crate has no
    /// `tm_core` dependency, so opening a ticket's detail screen is the caller's job once it
    /// knows a card was activated.
    pending_activation: Option<String>,
    /// The shared hub header shown above the board (identity line + tab strip). Starts empty
    /// (`KanbanHeader::default()`); the caller sets it via [`Kanban::set_header`] once it knows
    /// the version/model/place, the same way `screens::tickets::TicketsScreen` receives them in
    /// its `TicketsData`.
    header: KanbanHeader,
}

impl Kanban {
    /// A board over the given columns, nothing selected past the first column/card in each and no
    /// horizontal scroll yet.
    pub fn new(id: ComponentId, columns: Vec<KanbanColumn>) -> Self {
        let selected_card = vec![0; columns.len()];
        let scroll_offset = vec![0; columns.len()];
        Kanban {
            id,
            columns,
            selected_column: 0,
            selected_card,
            scroll_offset,
            column_offset: 0,
            visible_columns: Cell::new(0),
            visible_rows: Cell::new(0),
            pending_activation: None,
            header: KanbanHeader::default(),
        }
    }

    /// Replace the shared hub header (identity line + tab strip) this board renders above its
    /// hint line and columns.
    pub fn set_header(&mut self, header: KanbanHeader) {
        self.header = header;
    }

    /// Draws the identity line (`tm vX.Y.Z · model · place`) then the tab strip
    /// (Tickets · Board · Milestones · Timeline · Graph, "Board" highlighted) into `area`'s first
    /// two rows — mirrors `screens::tickets::TicketsScreen::render_header`'s compact form, minus
    /// the ticket counts (this screen has no notion of ticket groups; the caller already grouped
    /// cards into columns).
    fn render_header(&self, area: Rect, buf: &mut Buffer, theme: &Theme, glyphs: &Glyphs) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let muted = ratatui_core::style::Style::default().fg(theme.muted);
        let sep = glyphs.sep;
        let identity = format!(
            "tm v{}{sep}{}{sep}{}",
            self.header.version, self.header.model, self.header.place
        );
        buf.set_stringn(
            area.x,
            area.y,
            truncate(&identity, area.width as usize, glyphs.ellipsis),
            area.width as usize,
            muted,
        );
        if area.height < 2 {
            return;
        }
        let right = area.x + area.width;
        let mut x = area.x;
        for (i, tab) in TABS.iter().enumerate() {
            if x >= right {
                break;
            }
            if i > 0 {
                let seg = truncate(sep, (right - x) as usize, glyphs.ellipsis);
                buf.set_stringn(x, area.y + 1, &seg, (right - x) as usize, muted);
                x += display_width(&seg) as u16;
                if x >= right {
                    break;
                }
            }
            let style = if i == BOARD_TAB {
                ratatui_core::style::Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted
            };
            let seg = truncate(tab, (right - x) as usize, glyphs.ellipsis);
            buf.set_stringn(x, area.y + 1, &seg, (right - x) as usize, style);
            x += display_width(&seg) as u16;
        }
    }

    /// Replace the column/card data (e.g. after `tm-cli`'s `tui.rs` re-reads
    /// `tm_core::ProjectView` on an `AppMessage::TicketChanged`, the same trigger
    /// `screens::home::Home::set_dashboard` reacts to), clamping every selection/scroll index
    /// into the new ranges rather than resetting them — a refresh mid-browse should not throw the
    /// user back to the first column.
    ///
    /// Column count changing (a real `TicketState` variant added or removed) is handled the same
    /// way `Table::set_rows` handles a shrinking row count: clamp, never panic. This function
    /// never reads a hardcoded column count anywhere in its own logic, by construction — the
    /// number of columns is exactly `columns.len()`, whatever the caller passed.
    pub fn set_columns(&mut self, columns: Vec<KanbanColumn>) {
        let n = columns.len();
        self.columns = columns;
        self.selected_column = clamp_index(self.selected_column, n);
        self.selected_card.resize(n, 0);
        self.scroll_offset.resize(n, 0);
        let visible_rows = self.visible_rows.get().max(1) as usize;
        for i in 0..n {
            let len = self.columns[i].cards.len();
            self.selected_card[i] = clamp_index(self.selected_card[i], len);
            self.scroll_offset[i] = clamp_scroll(
                self.selected_card[i],
                self.scroll_offset[i],
                len,
                visible_rows,
            );
        }
        self.clamp_column_offset();
    }

    /// The currently selected column's index, for tests asserting on navigation.
    pub fn selected_column(&self) -> usize {
        self.selected_column
    }

    /// The currently selected card's id, if the selected column has any cards.
    pub fn selected_card_id(&self) -> Option<&str> {
        let column = self.columns.get(self.selected_column)?;
        let index = *self.selected_card.get(self.selected_column)?;
        column.cards.get(index).map(|c| c.id.as_str())
    }

    /// Take the most recently activated card's ticket id, if any, clearing it — mirrors
    /// `screens::home::Home::take_submission`'s poll-once-per-event contract.
    pub fn take_activation(&mut self) -> Option<String> {
        self.pending_activation.take()
    }

    /// Move `self.selected_column` by `delta`, clamped to the column range, then re-clamp the
    /// horizontal scroll so the new selection stays visible.
    fn move_column(&mut self, delta: isize) {
        if self.columns.is_empty() {
            return;
        }
        let last = self.columns.len() - 1;
        let next = (self.selected_column as isize + delta).clamp(0, last as isize) as usize;
        self.selected_column = next;
        self.clamp_column_offset();
    }

    /// Move the selected column's own selected-card index by `delta`, clamped to that column's
    /// card range, then re-clamp that column's own vertical scroll.
    fn move_card(&mut self, delta: isize) {
        let Some(column) = self.columns.get(self.selected_column) else {
            return;
        };
        if column.cards.is_empty() {
            return;
        }
        let last = column.cards.len() - 1;
        let current = self
            .selected_card
            .get(self.selected_column)
            .copied()
            .unwrap_or(0);
        let next = (current as isize + delta).clamp(0, last as isize) as usize;
        if let Some(slot) = self.selected_card.get_mut(self.selected_column) {
            *slot = next;
        }
        let visible = self.visible_rows.get().max(1) as usize;
        let len = column.cards.len();
        if let Some(scroll) = self.scroll_offset.get_mut(self.selected_column) {
            *scroll = clamp_scroll(next, *scroll, len, visible);
        }
    }

    /// Keep `self.column_offset` a valid window onto `self.columns` containing
    /// `self.selected_column`, given `self.visible_columns` columns are visible at a time.
    fn clamp_column_offset(&mut self) {
        let visible = self.visible_columns.get().max(1);
        self.column_offset = clamp_scroll(
            self.selected_column,
            self.column_offset,
            self.columns.len(),
            visible,
        );
    }

    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Left),
                "previous column",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Right),
                "next column",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Up),
                "previous card",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Down),
                "next card",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Enter),
                "open ticket",
            ),
        ]
    }
}

/// Clamp `value` into `0..len` (or `0` when `len` is `0`) — the shared "an index survived a data
/// refresh that shrank the collection it pointed into" rule `Table::set_rows`/`List::set_items`
/// each inline separately; factored out here since [`Kanban::set_columns`] needs it twice per
/// column (selection and scroll) across every column at once.
fn clamp_index(value: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        value.min(len - 1)
    }
}

/// Keep a scroll offset a valid window of size `visible` onto a `len`-long sequence, containing
/// `selected` — the same windowing rule `Table::clamp_scroll`/`List::clamp_scroll` each implement
/// for their own single axis; factored out here since `Kanban` needs the identical shape for two
/// independent axes (horizontal column scroll, and each column's own vertical card scroll).
fn clamp_scroll(selected: usize, scroll: usize, len: usize, visible: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let visible = visible.max(1);
    let mut scroll = scroll;
    if selected < scroll {
        scroll = selected;
    } else if selected >= scroll + visible {
        scroll = selected + 1 - visible;
    }
    let max_offset = len.saturating_sub(visible);
    scroll.min(max_offset)
}

/// A card's style: full emphasis when it is both selected and this board holds keyboard focus, a
/// dimmer bold-only indicator when selected but unfocused — mirrors
/// `widgets_data::table::selection_style`/`widgets_data::list::selection_style` exactly.
fn selection_style(ctx: &FrameContext<'_>, focused: bool) -> ratatui_core::style::Style {
    if focused {
        ctx.theme.selection
    } else {
        ratatui_core::style::Style::default()
            .fg(ctx.theme.foreground)
            .add_modifier(Modifier::BOLD)
    }
}

impl Component for Kanban {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.width == 0 || area.height == 0 {
            self.visible_columns.set(0);
            self.visible_rows.set(0);
            return;
        }

        let hub_header_height = HEADER_HEIGHT.min(area.height);
        let glyphs = Glyphs::for_caps(ctx.caps);
        self.render_header(
            Rect {
                height: hub_header_height,
                ..area
            },
            buf,
            ctx.theme,
            &glyphs,
        );
        let below_header = Rect {
            y: area.y + hub_header_height,
            height: area.height.saturating_sub(hub_header_height),
            ..area
        };
        if below_header.width == 0 || below_header.height == 0 {
            self.visible_columns.set(0);
            self.visible_rows.set(0);
            return;
        }

        let hint_height = HINT_HEIGHT.min(below_header.height);
        buf.set_stringn(
            below_header.x,
            below_header.y,
            HINT_TEXT,
            below_header.width as usize,
            ctx.theme.muted,
        );
        let body = Rect {
            y: below_header.y + hint_height,
            height: below_header.height.saturating_sub(hint_height),
            ..below_header
        };
        if body.width == 0 || body.height == 0 {
            self.visible_columns.set(0);
            self.visible_rows.set(0);
            return;
        }

        // Make columns fit the width: when every column can have at least `MIN_COLUMN_WIDTH` at
        // once, share `body.width` evenly across all of them (no scrolling) rather than always
        // using the fixed `COLUMN_WIDTH` — a 5-column board (the group labels D-024 asks for)
        // otherwise scrolls needlessly at, say, 100 columns wide (100 / 24 = 4 visible) even
        // though 100 / 5 = 20 fits every column. Too narrow for that (e.g. this crate's other
        // multi-pane screens' typical width with many columns) falls back to the fixed width and
        // real horizontal scrolling, same as before.
        let column_count = self.columns.len().max(1) as u16;
        let column_width = if body.width >= column_count.saturating_mul(MIN_COLUMN_WIDTH) {
            (body.width / column_count).max(1)
        } else {
            COLUMN_WIDTH.min(body.width).max(1)
        };
        let raw_visible_columns = (body.width / column_width).max(1) as usize;
        let visible_columns = raw_visible_columns.min(self.columns.len().max(1));
        self.visible_columns.set(visible_columns);

        let header_height = 1u16.min(body.height);
        let card_area_height = body.height - header_height;
        self.visible_rows.set(card_area_height);

        let focused = ctx.focus.is_focused(self.id);
        let end = (self.column_offset + visible_columns).min(self.columns.len());
        for (slot, col_index) in (self.column_offset..end).enumerate() {
            let column = &self.columns[col_index];
            let x = body.x + slot as u16 * column_width;
            let width = column_width.min((body.x + body.width).saturating_sub(x));
            let is_selected_column = col_index == self.selected_column;

            let header_style = if is_selected_column && focused {
                ratatui_core::style::Style::default()
                    .fg(ctx.theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else if is_selected_column {
                ratatui_core::style::Style::default()
                    .fg(ctx.theme.foreground)
                    .add_modifier(Modifier::BOLD)
            } else {
                ratatui_core::style::Style::default().fg(ctx.theme.muted)
            };
            let heading = format!("{} ({})", column.title, column.cards.len());
            buf.set_stringn(
                x,
                body.y,
                truncate(&heading, width as usize, "…"),
                width as usize,
                header_style,
            );

            let selected_card = self.selected_card.get(col_index).copied().unwrap_or(0);
            let scroll = self.scroll_offset.get(col_index).copied().unwrap_or(0);
            for (row_index, card) in column
                .cards
                .iter()
                .enumerate()
                .skip(scroll)
                .take(card_area_height as usize)
            {
                let y = body.y + header_height + (row_index - scroll) as u16;
                let style = if is_selected_column && row_index == selected_card {
                    selection_style(ctx, focused)
                } else {
                    ratatui_core::style::Style::default().fg(ctx.theme.foreground)
                };
                let text = format!("{} {}", card.id, card.title);
                buf.set_stringn(
                    x,
                    y,
                    truncate(&text, width as usize, "…"),
                    width as usize,
                    style,
                );
            }
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &FrameContext<'_>) -> Propagation {
        if !ctx.focus.is_focused(self.id) {
            return Propagation::Propagate;
        }
        let Event::Input(InputEvent::Key(key)) = event else {
            return Propagation::Propagate;
        };
        if self.columns.is_empty() {
            return Propagation::Propagate;
        }

        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Left => {
                self.move_column(-1);
                Propagation::Consumed
            }
            KeyCode::Right => {
                self.move_column(1);
                Propagation::Consumed
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_card(-1);
                Propagation::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_card(1);
                Propagation::Consumed
            }
            KeyCode::Enter => {
                if let Some(id) = self.selected_card_id() {
                    self.pending_activation = Some(id.to_string());
                }
                Propagation::Consumed
            }
            _ => Propagation::Propagate,
        }
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            Kanban::bindings()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Capabilities;
    use crate::component::FocusState;
    use crate::testing::Harness;
    use crate::theme::Theme;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tm_types::FixedClock;

    fn column(title: &str, cards: &[&str]) -> KanbanColumn {
        KanbanColumn::new(
            title,
            cards
                .iter()
                .map(|c| KanbanCard::new(format!("T-{c}"), format!("card {c}")))
                .collect(),
        )
    }

    fn sample() -> Kanban {
        Kanban::new(
            ComponentId::new("test.kanban"),
            vec![
                column("Draft", &["1", "2"]),
                column("Ready", &["3"]),
                column("Running", &[]),
            ],
        )
    }

    fn ctx<'a>(
        theme: &'a Theme,
        caps: &'a Capabilities,
        clock: &'a FixedClock,
        focused: bool,
    ) -> FrameContext<'a> {
        FrameContext {
            theme,
            caps,
            clock,
            focus: if focused {
                FocusState::new(Some(ComponentId::new("test.kanban")))
            } else {
                FocusState::default()
            },
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Input(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    #[test]
    fn a_fresh_board_starts_on_the_first_column_and_card() {
        let board = sample();
        assert_eq!(board.selected_column(), 0);
        assert_eq!(board.selected_card_id(), Some("T-1"));
    }

    #[test]
    fn unfocused_board_ignores_navigation() {
        let mut board = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, false);
        let propagation = board.handle_event(&key(KeyCode::Right), &context);
        assert_eq!(propagation, Propagation::Propagate);
        assert_eq!(board.selected_column(), 0);
    }

    #[test]
    fn right_and_left_move_between_columns_and_clamp_at_the_edges() {
        let mut board = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);

        board.handle_event(&key(KeyCode::Right), &context);
        assert_eq!(board.selected_column(), 1);
        board.handle_event(&key(KeyCode::Right), &context);
        assert_eq!(board.selected_column(), 2);
        board.handle_event(&key(KeyCode::Right), &context);
        assert_eq!(board.selected_column(), 2, "clamps at the last column");

        board.handle_event(&key(KeyCode::Left), &context);
        board.handle_event(&key(KeyCode::Left), &context);
        board.handle_event(&key(KeyCode::Left), &context);
        assert_eq!(board.selected_column(), 0, "clamps at the first column");
    }

    #[test]
    fn up_and_down_move_within_the_selected_columns_own_cards() {
        let mut board = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);

        assert_eq!(board.selected_card_id(), Some("T-1"));
        board.handle_event(&key(KeyCode::Down), &context);
        assert_eq!(board.selected_card_id(), Some("T-2"));
        board.handle_event(&key(KeyCode::Down), &context);
        assert_eq!(
            board.selected_card_id(),
            Some("T-2"),
            "clamps at the last card"
        );
        board.handle_event(&key(KeyCode::Up), &context);
        assert_eq!(board.selected_card_id(), Some("T-1"));
    }

    #[test]
    fn each_column_remembers_its_own_selected_card_independently() {
        let mut board = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);

        board.handle_event(&key(KeyCode::Down), &context); // column 0 -> card 1 ("T-2")
        board.handle_event(&key(KeyCode::Right), &context); // move to column 1 ("T-3")
        assert_eq!(board.selected_card_id(), Some("T-3"));
        board.handle_event(&key(KeyCode::Left), &context); // back to column 0
        assert_eq!(
            board.selected_card_id(),
            Some("T-2"),
            "column 0's own scroll position must survive visiting another column"
        );
    }

    #[test]
    fn an_empty_column_has_no_selected_card_and_ignores_vertical_navigation() {
        let mut board = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);

        board.handle_event(&key(KeyCode::Right), &context);
        board.handle_event(&key(KeyCode::Right), &context); // column 2, "Running", empty
        assert_eq!(board.selected_card_id(), None);
        board.handle_event(&key(KeyCode::Down), &context);
        assert_eq!(board.selected_card_id(), None, "still nothing to select");
    }

    #[test]
    fn enter_activates_the_selected_card_and_take_activation_clears_it() {
        let mut board = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);

        assert_eq!(board.take_activation(), None);
        board.handle_event(&key(KeyCode::Enter), &context);
        assert_eq!(board.take_activation(), Some("T-1".to_string()));
        assert_eq!(board.take_activation(), None, "take_activation clears it");
    }

    #[test]
    fn enter_on_an_empty_column_activates_nothing() {
        let mut board = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);

        board.handle_event(&key(KeyCode::Right), &context);
        board.handle_event(&key(KeyCode::Right), &context); // "Running", empty
        board.handle_event(&key(KeyCode::Enter), &context);
        assert_eq!(board.take_activation(), None);
    }

    #[test]
    fn set_columns_clamps_selection_into_the_new_range_rather_than_resetting_it() {
        let mut board = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        board.handle_event(&key(KeyCode::Right), &context); // column 1, "Ready"

        board.set_columns(vec![column("Draft", &["1"]), column("Ready", &["9"])]);
        assert_eq!(
            board.selected_column(),
            1,
            "the column index itself is still valid and must not reset"
        );
        assert_eq!(
            board.selected_card_id(),
            Some("T-9"),
            "the refreshed column's own (single) card is selected"
        );
    }

    #[test]
    fn set_columns_with_fewer_columns_clamps_the_selected_column() {
        let mut board = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        board.handle_event(&key(KeyCode::Right), &context);
        board.handle_event(&key(KeyCode::Right), &context); // column 2

        board.set_columns(vec![column("Draft", &["1"])]);
        assert_eq!(board.selected_column(), 0);
        assert_eq!(board.selected_card_id(), Some("T-1"));
    }

    #[test]
    fn render_shows_the_hint_line_and_every_visible_columns_heading() {
        let board = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let mut harness = Harness::new(80, 10);
        let lines = harness.render_lines(&board, &context);

        assert!(
            lines.iter().any(|l| l.contains("Left/Right")),
            "hint line: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Draft")),
            "Draft column heading missing: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Ready")),
            "Ready column heading missing: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("T-1") && l.contains("card 1")),
            "the first card's id and title must both be on screen: {lines:?}"
        );
    }

    /// u1-hub-board-display-labels' own acceptance check: at a real 120x36 PTY, with the group
    /// labels the tickets screen uses (not raw state names), every column fits under the tab
    /// strip with "Board" highlighted, with no horizontal scrolling needed.
    #[test]
    fn render_at_120_cols_shows_the_tab_strip_with_board_highlighted_and_every_group_column() {
        let group_titles = [
            "Needs input",
            "Working",
            "Ready for review",
            "Queued",
            "Completed",
        ];
        let columns: Vec<KanbanColumn> = group_titles
            .iter()
            .map(|title| column(title, &["1"]))
            .collect();
        let mut board = Kanban::new(ComponentId::new("test.kanban"), columns);
        board.set_header(KanbanHeader {
            version: "0.1.0".to_string(),
            model: "anthropic/claude-sonnet-5".to_string(),
            place: "~/src/app (main)".to_string(),
        });
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let mut harness = Harness::new(120, 36);
        let lines = harness.render_lines(&board, &context);

        assert!(
            lines[0].contains("tm v0.1.0")
                && lines[0].contains("anthropic/claude-sonnet-5")
                && lines[0].contains("~/src/app (main)"),
            "identity line: {lines:?}"
        );
        for tab in TABS {
            assert!(
                lines[1].contains(tab),
                "{tab} missing from the tab strip: {lines:?}"
            );
        }
        let pos = |needle: &str| lines[1].find(needle).unwrap();
        assert!(pos("Tickets") < pos("Board"));
        assert!(pos("Board") < pos("Milestones"));
        assert!(pos("Milestones") < pos("Timeline"));
        assert!(pos("Timeline") < pos("Graph"));

        for title in group_titles {
            assert!(
                lines.iter().any(|l| l.contains(title)),
                "{title} column missing: {lines:?}"
            );
        }
        // All 5 columns fit side by side in one row at 120 columns (`render` shares the width
        // evenly since 120 / 5 = 24 >= MIN_COLUMN_WIDTH), so every heading is on the same line,
        // not spread across a scrolled/paginated view.
        let heading_line = lines
            .iter()
            .find(|l| l.contains("Needs input"))
            .expect("a heading line");
        for title in group_titles {
            assert!(
                heading_line.contains(title),
                "{title} not on the same row as the other headings: {heading_line:?}"
            );
        }
    }

    /// "make columns fit the width" (the task's own words): the board must not fall back to its
    /// fixed 24-wide `COLUMN_WIDTH` and scroll away "Completed" just because the terminal is
    /// narrower than 120 — 100 / 5 = 20, exactly `MIN_COLUMN_WIDTH`, so every column still shares
    /// the width evenly and all five headings stay on one line.
    #[test]
    fn render_at_100_cols_still_fits_all_five_group_columns_with_no_scroll() {
        let group_titles = [
            "Needs input",
            "Working",
            "Ready for review",
            "Queued",
            "Completed",
        ];
        let columns: Vec<KanbanColumn> = group_titles
            .iter()
            .map(|title| column(title, &["1"]))
            .collect();
        let board = Kanban::new(ComponentId::new("test.kanban"), columns);
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let mut harness = Harness::new(100, 20);
        let lines = harness.render_lines(&board, &context);

        let heading_line = lines
            .iter()
            .find(|l| l.contains("Needs input"))
            .expect("a heading line");
        for title in group_titles {
            assert!(
                heading_line.contains(title),
                "{title} not on the same row as the other headings at 100 cols: {heading_line:?}"
            );
        }
    }

    #[test]
    fn render_reflects_column_navigation() {
        let mut board = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let mut harness = Harness::new(80, 10);

        let before = harness.render_lines(&board, &context);
        assert!(before.iter().any(|l| l.contains("T-1")));

        let after = harness.send_and_render(&mut board, &key(KeyCode::Down), &context);
        assert!(
            after.iter().any(|l| l.contains("T-2")),
            "after moving down, the second card must still be visible: {after:?}"
        );
    }

    /// With this many columns (14, far more than `MIN_COLUMN_WIDTH` lets an 80-wide area share
    /// evenly — see `render`'s doc comment), the board falls back to its fixed `COLUMN_WIDTH` and
    /// real horizontal scrolling instead of paginating or relabelling — this is the test that
    /// actually exercises that windowing at a realistic viewport, rather than only the 3-column
    /// `sample()` board every other navigation test above uses (too few columns to ever force a
    /// scroll) or moving without ever calling `render` first (which would leave `visible_columns`
    /// at its zero-initialized default and exercise `clamp_scroll`'s `visible = 1` fallback path
    /// instead of the real `body.width / column_width` arithmetic).
    #[test]
    fn horizontal_scroll_reveals_far_columns_and_hides_the_first_one() {
        let titles = [
            "Draft",
            "Blocked",
            "Ready",
            "Leased",
            "Running",
            "Submitted",
            "Verifying",
            "Auditing",
            "Rework",
            "Replan",
            "Recovery",
            "Escalated",
            "Closed",
            "Cancelled",
        ];
        let columns: Vec<KanbanColumn> = titles.iter().map(|title| column(title, &["1"])).collect();
        // Same id the `ctx` helper below hardcodes as "focused" — a mismatched id here would make
        // `ctx.focus.is_focused(self.id)` false for this board, silently turning every navigation
        // key into a no-op (`Kanban::handle_event`'s very first guard) rather than a real,
        // observable test failure pointing at the actual cause.
        let mut board = Kanban::new(ComponentId::new("test.kanban"), columns);
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let mut harness = Harness::new(80, 12);

        // Populate `visible_columns` at this realistic viewport before navigating at all — an
        // 80-wide area fits `80 / COLUMN_WIDTH` = 3 columns, so the last column starts well out
        // of view.
        let initial = harness.render_lines(&board, &context);
        assert!(initial.iter().any(|l| l.contains("Draft")));
        assert!(
            !initial.iter().any(|l| l.contains("Escalated")),
            "the far column must not already be visible before any navigation: {initial:?}"
        );

        // Move to the last column one step at a time, the same way a human pressing Right
        // repeatedly on a real terminal does.
        let mut last_frame = initial;
        for _ in 0..titles.len() - 1 {
            last_frame = harness.send_and_render(&mut board, &key(KeyCode::Right), &context);
        }

        assert_eq!(board.selected_column(), titles.len() - 1);
        assert!(
            last_frame.iter().any(|l| l.contains("Cancelled")),
            "the last column must have scrolled into view once selected: {last_frame:?}"
        );
        assert!(
            !last_frame.iter().any(|l| l.contains("Draft")),
            "the first column must have scrolled out of view by now: {last_frame:?}"
        );
    }

    #[test]
    fn a_zero_sized_area_does_not_panic() {
        let board = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        board.render(Rect::new(0, 0, 0, 0), &mut buf, &context);
    }

    #[test]
    fn clamp_index_is_zero_for_an_empty_collection() {
        assert_eq!(clamp_index(5, 0), 0);
        assert_eq!(clamp_index(5, 3), 2);
        assert_eq!(clamp_index(1, 3), 1);
    }

    #[test]
    fn clamp_scroll_keeps_the_selection_inside_the_visible_window() {
        assert_eq!(clamp_scroll(0, 0, 10, 3), 0);
        assert_eq!(
            clamp_scroll(9, 0, 10, 3),
            7,
            "scrolls forward to reveal the tail"
        );
        assert_eq!(
            clamp_scroll(0, 7, 10, 3),
            0,
            "scrolls back to reveal the head"
        );
        assert_eq!(
            clamp_scroll(0, 0, 0, 3),
            0,
            "an empty sequence never scrolls"
        );
    }
}
