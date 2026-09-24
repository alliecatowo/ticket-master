//! The Milestones tab: progress per milestone (`s1-tui-milestones-view`).
//!
//! One row per milestone, sourced from `tm_core::ProjectView::milestones`: title, state label,
//! a done/total ticket progress bar, and its derived due date (the max of its member tickets'
//! due dates, once `s1-ticket-due-date` lands — until then `due` is always `None` and the column
//! reads "—"). Like every screen here it is plain data in (see `screens::kanban::Kanban`'s module
//! doc for why this crate has no `tm_core` dependency): `tm-cli`'s `tui.rs` builds
//! [`MilestoneRow`]s from the real project state and this module only decides how they look and
//! which keys do what.
//!
//! Enter over a row activates it (`MilestonesScreen::take_activation`), which `tm-cli` turns into
//! "open Tickets filtered to this milestone" (the header shows the filter; Esc there clears it —
//! `tm-cli`'s `tui.rs` owns that, this screen only reports which row was picked). This screen
//! follows `Kanban`'s focus-tree pattern (own selection, `ctx.focus.is_focused` gates input)
//! rather than `Tickets`/`Chat`'s "take input straight from `App`" pattern, since — like the
//! board — it has no text input of its own to protect from stray keys.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::truncate;

/// Width, in cells, of the done/total progress bar drawn for each row.
const BAR_WIDTH: usize = 12;

/// One milestone, already formatted for display — no `tm_core` types here (see the module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MilestoneRow {
    /// The milestone's id, e.g. `"M-1"` — carried back out via
    /// [`MilestonesScreen::take_activation`] so the caller (which does know `MilestoneId`) can
    /// parse it back.
    pub id: String,
    /// The milestone's title.
    pub title: String,
    /// Already-formatted state label, e.g. `"Open"`/`"Closed"`.
    pub state: String,
    /// Member tickets closed or cancelled.
    pub done: usize,
    /// Total member tickets.
    pub total: usize,
    /// The milestone's derived due date, already formatted (e.g. `"2026-10-01"`), or `None` when
    /// no member ticket has one (or none are due-date-aware yet).
    pub due: Option<String>,
}

impl MilestoneRow {
    /// A row for a milestone with `done` of `total` member tickets closed/cancelled.
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        state: impl Into<String>,
        done: usize,
        total: usize,
        due: Option<String>,
    ) -> Self {
        MilestoneRow {
            id: id.into(),
            title: title.into(),
            state: state.into(),
            done,
            total,
            due,
        }
    }
}

/// Hint line drawn above the list.
const HINT_TEXT: &str = "Up/Down: select   Enter: filter tickets   Esc: back";

/// The empty-state message, shown instead of the list when there are no milestones at all.
const EMPTY_TEXT: &str = "No milestones. Create one with tm milestone new";

/// One row reserved at the top for the hint line, matching `screens::kanban::Kanban::HINT_HEIGHT`.
const HINT_HEIGHT: u16 = 1;

/// The Milestones tab: a vertically scrollable list of [`MilestoneRow`]s.
#[derive(Debug)]
pub struct MilestonesScreen {
    id: ComponentId,
    rows: Vec<MilestoneRow>,
    selected: usize,
    scroll_offset: usize,
    /// Set by `handle_event` on Enter over a row, taken (and cleared) by
    /// [`MilestonesScreen::take_activation`] — the same poll-based handoff
    /// `screens::kanban::Kanban::take_activation` uses.
    pending_activation: Option<String>,
}

impl MilestonesScreen {
    /// A screen over the given rows, nothing selected past the first.
    pub fn new(id: ComponentId, rows: Vec<MilestoneRow>) -> Self {
        MilestonesScreen {
            id,
            rows,
            selected: 0,
            scroll_offset: 0,
            pending_activation: None,
        }
    }

    /// Replace the row data (e.g. after `tm-cli`'s `tui.rs` re-reads `tm_core::ProjectView`),
    /// clamping the selection/scroll into the new range rather than resetting it.
    pub fn set_rows(&mut self, rows: Vec<MilestoneRow>) {
        self.rows = rows;
        let last = self.rows.len().saturating_sub(1);
        self.selected = self.selected.min(last);
        self.clamp_scroll();
    }

    /// The currently selected row's milestone id, if any.
    pub fn selected_id(&self) -> Option<&str> {
        self.rows.get(self.selected).map(|r| r.id.as_str())
    }

    /// Take the most recently activated row's milestone id, if any, clearing it.
    pub fn take_activation(&mut self) -> Option<String> {
        self.pending_activation.take()
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let next = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        self.selected = next;
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
    }

    fn bindings() -> Vec<KeyBinding> {
        vec![
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Up), "previous"),
            KeyBinding::new(KeyChord::plain(crossterm::event::KeyCode::Down), "next"),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Enter),
                "filter tickets",
            ),
        ]
    }
}

/// A textual done/total bar, `[████········] 3/8`.
fn bar(done: usize, total: usize) -> String {
    if total == 0 {
        return format!("{:width$}  0/0", "", width = BAR_WIDTH);
    }
    let filled = ((done as f64 / total as f64) * BAR_WIDTH as f64).round() as usize;
    let filled = filled.min(BAR_WIDTH);
    let mut b = String::with_capacity(BAR_WIDTH);
    for _ in 0..filled {
        b.push('█');
    }
    for _ in filled..BAR_WIDTH {
        b.push('·');
    }
    format!("[{b}] {done}/{total}")
}

fn selection_style(ctx: &FrameContext<'_>, focused: bool) -> Style {
    if focused {
        ctx.theme.selection
    } else {
        Style::default()
            .fg(ctx.theme.foreground)
            .add_modifier(Modifier::BOLD)
    }
}

impl Component for MilestonesScreen {
    fn id(&self) -> ComponentId {
        self.id
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &FrameContext<'_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        buf.set_stringn(
            area.x,
            area.y,
            HINT_TEXT,
            area.width as usize,
            Style::default().fg(ctx.theme.muted),
        );
        if area.height <= HINT_HEIGHT {
            return;
        }
        let body_y = area.y + HINT_HEIGHT;
        let body_height = area.height - HINT_HEIGHT;

        if self.rows.is_empty() {
            buf.set_stringn(
                area.x,
                body_y,
                EMPTY_TEXT,
                area.width as usize,
                Style::default().fg(ctx.theme.muted),
            );
            return;
        }

        let focused = ctx.focus.is_focused(self.id);
        let visible = body_height as usize;
        let end = (self.scroll_offset + visible).min(self.rows.len());
        for (slot, row_index) in (self.scroll_offset..end).enumerate() {
            let row = &self.rows[row_index];
            let y = body_y + slot as u16;
            let is_selected = row_index == self.selected;
            let style = if is_selected {
                selection_style(ctx, focused)
            } else {
                Style::default().fg(ctx.theme.foreground)
            };
            let due = row.due.as_deref().unwrap_or("—");
            let text = format!(
                "{} {}  [{}]  {}  due {}",
                row.id,
                row.title,
                row.state,
                bar(row.done, row.total),
                due,
            );
            buf.set_stringn(
                area.x,
                y,
                truncate(&text, area.width as usize, "…"),
                area.width as usize,
                style,
            );
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
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Propagation::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Propagation::Consumed
            }
            KeyCode::Enter => {
                if let Some(id) = self.selected_id() {
                    self.pending_activation = Some(id.to_string());
                }
                Propagation::Consumed
            }
            _ => Propagation::Propagate,
        }
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            MilestonesScreen::bindings()
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

    fn sample() -> MilestonesScreen {
        MilestonesScreen::new(
            ComponentId::new("test.milestones"),
            vec![
                MilestoneRow::new(
                    "M-1",
                    "Ship the beta",
                    "Open",
                    3,
                    5,
                    Some("2026-10-01".into()),
                ),
                MilestoneRow::new("M-2", "Launch", "Open", 0, 2, None),
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
                FocusState::new(Some(ComponentId::new("test.milestones")))
            } else {
                FocusState::default()
            },
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Input(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    #[test]
    fn a_fresh_screen_starts_on_the_first_row() {
        let screen = sample();
        assert_eq!(screen.selected_id(), Some("M-1"));
    }

    #[test]
    fn unfocused_screen_ignores_navigation_and_enter() {
        let mut screen = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, false);
        assert_eq!(
            screen.handle_event(&key(KeyCode::Down), &context),
            Propagation::Propagate
        );
        assert_eq!(screen.selected_id(), Some("M-1"));
        screen.handle_event(&key(KeyCode::Enter), &context);
        assert_eq!(screen.take_activation(), None);
    }

    #[test]
    fn down_then_enter_activates_the_second_row() {
        let mut screen = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);

        screen.handle_event(&key(KeyCode::Down), &context);
        assert_eq!(screen.selected_id(), Some("M-2"));
        screen.handle_event(&key(KeyCode::Enter), &context);
        assert_eq!(screen.take_activation(), Some("M-2".to_string()));
        // Taken once; a second poll finds nothing left.
        assert_eq!(screen.take_activation(), None);
    }

    #[test]
    fn selection_clamps_at_the_edges() {
        let mut screen = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);

        screen.handle_event(&key(KeyCode::Up), &context);
        assert_eq!(screen.selected_id(), Some("M-1"));
        screen.handle_event(&key(KeyCode::Down), &context);
        screen.handle_event(&key(KeyCode::Down), &context);
        assert_eq!(screen.selected_id(), Some("M-2"));
    }

    #[test]
    fn render_shows_the_hint_line_titles_counts_and_due_date() {
        let screen = sample();
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let mut harness = Harness::new(80, 6);
        let lines = harness.render_lines(&screen, &context);

        assert!(lines[0].contains("Enter"), "hint line: {lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("M-1") && l.contains("Ship the beta") && l.contains("3/5")),
            "first row missing id/title/count: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("2026-10-01")),
            "due date missing: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("M-2") && l.contains("0/2") && l.contains("due —")),
            "second row (no due date) missing: {lines:?}"
        );
    }

    #[test]
    fn render_with_no_milestones_shows_the_empty_state() {
        let screen = MilestonesScreen::new(ComponentId::new("test.milestones"), Vec::new());
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let mut harness = Harness::new(80, 6);
        let lines = harness.render_lines(&screen, &context);

        assert!(
            lines.iter().any(|l| l.contains("tm milestone new")),
            "empty state missing: {lines:?}"
        );
    }
}
