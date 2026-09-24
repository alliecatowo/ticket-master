//! The Timeline tab: ticket bars against the event log (`s1-tui-timeline-view`).
//!
//! One row per ticket, grouped by milestone (via the row's own already-formatted label — no
//! `tm_core` dependency here, see `screens::kanban::Kanban`'s module doc for why), spanning from
//! when it was first created to when it closed/was cancelled (`tm-cli`'s `tui.rs` reads that from
//! `tickets/overview.rs`'s `ActivityIndex`, which folds `ticket.closed`/`ticket.cancelled` into a
//! `closed_at`) or "now" while still open. A row's due date comes from `Ticket::due`
//! (`s1-ticket-due-date`), converted to midnight UTC by `tm-cli`'s `tui.rs`.
//!
//! `+`/`-` zoom the day axis between one column per day, one column per week, and a full month
//! calendar grid (due tickets listed per day below the grid). Timeline has no text input of its
//! own, so — like `Kanban`/`MilestonesScreen` — it follows the focus-tree pattern:
//! `ctx.focus.is_focused` gates input, Up/Down move the selected ticket (day/week zoom only),
//! Enter opens its detail.

use std::collections::BTreeMap;

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};

use crate::component::{Component, ComponentId, FrameContext};
use crate::event::{Event, InputEvent, KeyBinding, KeyChord, Propagation};
use crate::text::truncate;
use tm_types::Timestamp;

/// Seconds in a day, for bucketing timestamps into axis columns.
const DAY_SECS: i64 = 86_400;

/// Width, in cells, reserved for a row's milestone/id/title label before its bar.
const LABEL_WIDTH: usize = 30;

/// One row reserved at the top for the hint line.
const HINT_HEIGHT: u16 = 1;

/// One row reserved for the date axis, in day/week zoom.
const AXIS_HEIGHT: u16 = 1;

/// The empty-state message, shown when there are no tickets at all.
const EMPTY_TEXT: &str = "No tickets yet. Create one with tm ticket new";

/// Hint line drawn above the timeline.
const HINT_TEXT: &str = "+/- zoom   Up/Down select   Enter: ticket   Esc: back   ▼ today   ◆ due";

/// How far zoomed in the day axis is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zoom {
    /// One column per day.
    Day,
    /// One column per week.
    Week,
    /// A full month calendar grid, due tickets listed per day.
    Month,
}

impl Zoom {
    fn days_per_column(self) -> i64 {
        match self {
            Zoom::Day => 1,
            Zoom::Week => 7,
            // Unused: month zoom draws a calendar grid instead of a day-column bar.
            Zoom::Month => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Zoom::Day => "day",
            Zoom::Week => "week",
            Zoom::Month => "month",
        }
    }

    /// One step toward finer granularity (`+`).
    fn zoom_in(self) -> Self {
        match self {
            Zoom::Month => Zoom::Week,
            Zoom::Week => Zoom::Day,
            Zoom::Day => Zoom::Day,
        }
    }

    /// One step toward coarser granularity (`-`).
    fn zoom_out(self) -> Self {
        match self {
            Zoom::Day => Zoom::Week,
            Zoom::Week => Zoom::Month,
            Zoom::Month => Zoom::Month,
        }
    }
}

/// One ticket, already formatted for display — no `tm_core` types here (see the module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineRow {
    /// The ticket's id, e.g. `"T-1"` — carried back out via
    /// [`TimelineScreen::take_activation`] so the caller (which does know `TicketId`) can parse
    /// it back.
    pub id: String,
    /// The ticket's short title.
    pub title: String,
    /// The milestone title it belongs to, if any.
    pub milestone: Option<String>,
    /// When it was first created.
    pub created: Timestamp,
    /// When it closed or was cancelled, or "now" while it is still open.
    pub end: Timestamp,
    /// Its due date, if it has one (`s1-ticket-due-date`).
    pub due: Option<Timestamp>,
}

impl TimelineRow {
    /// A row spanning `created` to `end`, optionally with a milestone label and a due date.
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        milestone: Option<String>,
        created: Timestamp,
        end: Timestamp,
        due: Option<Timestamp>,
    ) -> Self {
        TimelineRow {
            id: id.into(),
            title: title.into(),
            milestone,
            created,
            end,
            due,
        }
    }
}

/// The Timeline tab: a day/week bar chart, or a month calendar grid, of every ticket's span.
#[derive(Debug)]
pub struct TimelineScreen {
    id: ComponentId,
    rows: Vec<TimelineRow>,
    zoom: Zoom,
    selected: usize,
    scroll_offset: usize,
    /// Set by `handle_event` on Enter over a row, taken (and cleared) by
    /// [`TimelineScreen::take_activation`] — the same poll-based handoff
    /// `screens::kanban::Kanban::take_activation` uses.
    pending_activation: Option<String>,
}

impl TimelineScreen {
    /// A screen over the given rows, starting zoomed to one column per day.
    pub fn new(id: ComponentId, rows: Vec<TimelineRow>) -> Self {
        TimelineScreen {
            id,
            rows,
            zoom: Zoom::Day,
            selected: 0,
            scroll_offset: 0,
            pending_activation: None,
        }
    }

    /// Replace the row data (e.g. after `tm-cli`'s `tui.rs` re-reads `tm_core::ProjectView`),
    /// clamping the selection/scroll into the new range rather than resetting it.
    pub fn set_rows(&mut self, rows: Vec<TimelineRow>) {
        self.rows = rows;
        let last = self.rows.len().saturating_sub(1);
        self.selected = self.selected.min(last);
        self.clamp_scroll();
    }

    /// The current zoom level.
    pub fn zoom(&self) -> Zoom {
        self.zoom
    }

    /// Take the most recently activated row's ticket id, if any, clearing it.
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

    fn bindings(&self) -> Vec<KeyBinding> {
        let mut bindings = vec![
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Char('+')),
                "zoom in",
            ),
            KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Char('-')),
                "zoom out",
            ),
        ];
        if self.zoom != Zoom::Month {
            bindings.push(KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Up),
                "previous",
            ));
            bindings.push(KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Down),
                "next",
            ));
            bindings.push(KeyBinding::new(
                KeyChord::plain(crossterm::event::KeyCode::Enter),
                "ticket detail",
            ));
        }
        bindings
    }
}

/// Whole days since the Unix epoch `t` falls on.
fn day_of(t: Timestamp) -> i64 {
    t.unix_seconds().div_euclid(DAY_SECS)
}

/// Days since the Unix epoch for the proleptic Gregorian date `y`-`m`-`d` (Howard Hinnant's
/// `days_from_civil`, public domain — http://howardhinnant.github.io/date_algorithms.html). Pure
/// integer arithmetic: this workspace has no date/calendar crate dependency to reach for instead.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 }.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: the proleptic Gregorian `(year, month, day)` for `z` days
/// since the Unix epoch.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 }.div_euclid(146097);
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// A row's display label: `"<milestone> · <id> <title>"`, or just `"<id> <title>"` with no
/// milestone, truncated (never padded — the caller pads to [`LABEL_WIDTH`]).
fn row_label(row: &TimelineRow) -> String {
    let text = match &row.milestone {
        Some(m) => format!("{m} · {} {}", row.id, row.title),
        None => format!("{} {}", row.id, row.title),
    };
    truncate(&text, LABEL_WIDTH.saturating_sub(1), "…")
}

/// A `width`-wide bar: `·` outside `[start_col, end_col]`, `█` inside, `◆` at `due_col` (wins
/// over `█`). All columns are clamped into `[0, width)` first.
fn bar_line(width: usize, start_col: usize, end_col: usize, due_col: Option<usize>) -> String {
    let mut line = String::with_capacity(width);
    for i in 0..width {
        let ch = if due_col == Some(i) {
            '◆'
        } else if i >= start_col && i <= end_col {
            '█'
        } else {
            '·'
        };
        line.push(ch);
    }
    line
}

/// The date axis: `·` per column, `▼` at `today_col` when it falls within `[0, width)`.
fn axis_line(width: usize, today_col: Option<usize>) -> String {
    let mut line = String::with_capacity(width);
    for i in 0..width {
        line.push(if today_col == Some(i) { '▼' } else { '·' });
    }
    line
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

impl TimelineScreen {
    fn render_bars(
        &self,
        area: Rect,
        body_y: u16,
        body_height: u16,
        buf: &mut Buffer,
        ctx: &FrameContext<'_>,
    ) {
        let bar_width = (area.width as usize).saturating_sub(LABEL_WIDTH);
        if bar_width == 0 {
            return;
        }
        let now = ctx.clock.now();
        let axis_start_day = self
            .rows
            .iter()
            .map(|r| day_of(r.created))
            .min()
            .unwrap_or_else(|| day_of(now));
        let days_per_col = self.zoom.days_per_column();
        let col_of = |t: Timestamp| -> usize {
            ((day_of(t) - axis_start_day).max(0) / days_per_col) as usize
        };
        let clamp = |c: usize| c.min(bar_width.saturating_sub(1));
        let today_col = {
            let c = col_of(now);
            (c < bar_width).then_some(c)
        };

        // Axis row.
        let axis_label = format!("{} zoom", self.zoom.label());
        buf.set_stringn(
            area.x,
            body_y,
            format!("{axis_label:<LABEL_WIDTH$}"),
            LABEL_WIDTH,
            Style::default().fg(ctx.theme.muted),
        );
        buf.set_stringn(
            area.x + LABEL_WIDTH as u16,
            body_y,
            axis_line(bar_width, today_col),
            bar_width,
            Style::default().fg(ctx.theme.muted),
        );

        if body_height <= AXIS_HEIGHT {
            return;
        }
        let rows_y = body_y + AXIS_HEIGHT;
        let rows_height = (body_height - AXIS_HEIGHT) as usize;
        let focused = ctx.focus.is_focused(self.id);
        let end = (self.scroll_offset + rows_height).min(self.rows.len());
        for (slot, row_index) in (self.scroll_offset..end).enumerate() {
            let row = &self.rows[row_index];
            let y = rows_y + slot as u16;
            let is_selected = row_index == self.selected;
            let style = if is_selected {
                selection_style(ctx, focused)
            } else {
                Style::default().fg(ctx.theme.foreground)
            };
            let start_col = clamp(col_of(row.created));
            let end_col = clamp(col_of(row.end)).max(start_col);
            let due_col = row.due.map(col_of).map(clamp).filter(|c| {
                let due_day = row.due.map(day_of).unwrap_or_default();
                due_day >= axis_start_day && *c < bar_width
            });
            let label = format!("{:<LABEL_WIDTH$}", row_label(row));
            let bar = bar_line(bar_width, start_col, end_col, due_col);
            buf.set_stringn(area.x, y, &label, LABEL_WIDTH, style);
            buf.set_stringn(area.x + LABEL_WIDTH as u16, y, &bar, bar_width, style);
        }
    }

    fn render_month(
        &self,
        area: Rect,
        body_y: u16,
        body_height: u16,
        buf: &mut Buffer,
        ctx: &FrameContext<'_>,
    ) {
        let now = ctx.clock.now();
        let (year, month, today_day) = civil_from_days(day_of(now));
        let first_of_month = days_from_civil(year, month, 1);
        let (next_year, next_month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        let days_in_month = (days_from_civil(next_year, next_month, 1) - first_of_month) as u32;
        // Sunday-first weekday of the 1st (1970-01-01 was a Thursday, weekday index 4).
        let weekday0 = ((first_of_month + 4).rem_euclid(7)) as u32;

        let mut due_by_day: BTreeMap<u32, Vec<&str>> = BTreeMap::new();
        for row in &self.rows {
            if let Some(due) = row.due {
                let (dy, dm, dd) = civil_from_days(day_of(due));
                if dy == year && dm == month {
                    due_by_day.entry(dd).or_default().push(row.id.as_str());
                }
            }
        }

        let mut y = body_y;
        let end_y = body_y + body_height;
        buf.set_stringn(
            area.x,
            y,
            format!("{year:04}-{month:02}"),
            area.width as usize,
            Style::default().fg(ctx.theme.muted),
        );
        y += 1;
        if y >= end_y {
            return;
        }
        const HEADERS: [&str; 7] = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];
        let header: String = HEADERS.iter().map(|h| format!("{h:^5}")).collect();
        buf.set_stringn(
            area.x,
            y,
            &header,
            area.width as usize,
            Style::default().fg(ctx.theme.muted),
        );
        y += 1;

        let total_cells = weekday0 as usize + days_in_month as usize;
        let weeks = total_cells.div_ceil(7);
        for week in 0..weeks {
            if y >= end_y {
                break;
            }
            let mut line = String::new();
            for wd in 0..7u32 {
                let cell_index = week * 7 + wd as usize;
                let day = cell_index
                    .checked_sub(weekday0 as usize)
                    .map(|d| d as u32 + 1);
                let cell = match day.filter(|d| *d <= days_in_month) {
                    None => "     ".to_string(),
                    Some(d) => {
                        let (l, r) = if d == today_day {
                            ('[', ']')
                        } else {
                            (' ', ' ')
                        };
                        let mark = if due_by_day.contains_key(&d) {
                            '*'
                        } else {
                            ' '
                        };
                        format!("{l}{d:>2}{r}{mark}")
                    }
                };
                line.push_str(&cell);
            }
            buf.set_stringn(
                area.x,
                y,
                &line,
                area.width as usize,
                Style::default().fg(ctx.theme.foreground),
            );
            y += 1;
        }
        y += 1;
        if y < end_y && !due_by_day.is_empty() {
            buf.set_stringn(
                area.x,
                y,
                "Due:",
                area.width as usize,
                Style::default().fg(ctx.theme.muted),
            );
            y += 1;
        }
        for (day, ids) in &due_by_day {
            if y >= end_y {
                break;
            }
            let text = format!("{year:04}-{month:02}-{day:02}: {}", ids.join(", "));
            buf.set_stringn(
                area.x,
                y,
                truncate(&text, area.width as usize, "…"),
                area.width as usize,
                Style::default().fg(ctx.theme.foreground),
            );
            y += 1;
        }
    }
}

impl Component for TimelineScreen {
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

        match self.zoom {
            Zoom::Day | Zoom::Week => self.render_bars(area, body_y, body_height, buf, ctx),
            Zoom::Month => self.render_month(area, body_y, body_height, buf, ctx),
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
        match key.code {
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.zoom = self.zoom.zoom_in();
                Propagation::Consumed
            }
            KeyCode::Char('-') => {
                self.zoom = self.zoom.zoom_out();
                Propagation::Consumed
            }
            KeyCode::Up | KeyCode::Char('k') if self.zoom != Zoom::Month => {
                self.move_selection(-1);
                Propagation::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') if self.zoom != Zoom::Month => {
                self.move_selection(1);
                Propagation::Consumed
            }
            KeyCode::Enter if self.zoom != Zoom::Month => {
                if let Some(row) = self.rows.get(self.selected) {
                    self.pending_activation = Some(row.id.clone());
                }
                Propagation::Consumed
            }
            _ => Propagation::Propagate,
        }
    }

    fn keybindings(&self, ctx: &FrameContext<'_>) -> Vec<KeyBinding> {
        if ctx.focus.is_focused(self.id) {
            self.bindings()
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

    fn ts(days: i64) -> Timestamp {
        Timestamp::from_unix_seconds(days * DAY_SECS)
    }

    fn sample() -> Vec<TimelineRow> {
        vec![
            TimelineRow::new(
                "T-1",
                "Ship the beta",
                Some("M-1".into()),
                ts(0),
                ts(5),
                None,
            ),
            TimelineRow::new(
                "T-2",
                "Fix the crash",
                Some("M-1".into()),
                ts(2),
                ts(9),
                Some(ts(8)),
            ),
            TimelineRow::new("T-3", "Write the docs", None, ts(1), ts(3), None),
        ]
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
                FocusState::new(Some(ComponentId::new("test.timeline")))
            } else {
                FocusState::default()
            },
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Input(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    #[test]
    fn civil_date_roundtrips() {
        // 2026-09-24 (today's date, this session) — a known, fixed anchor.
        let days = days_from_civil(2026, 9, 24);
        assert_eq!(civil_from_days(days), (2026, 9, 24));
        // The Unix epoch itself.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn a_fresh_screen_zooms_to_day_and_selects_the_first_row() {
        let screen = TimelineScreen::new(ComponentId::new("test.timeline"), sample());
        assert_eq!(screen.zoom(), Zoom::Day);
        assert_eq!(screen.selected, 0);
    }

    #[test]
    fn plus_and_minus_cycle_the_zoom_level() {
        let mut screen = TimelineScreen::new(ComponentId::new("test.timeline"), sample());
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::new(ts(3));
        let context = ctx(&theme, &caps, &clock, true);

        screen.handle_event(&key(KeyCode::Char('-')), &context);
        assert_eq!(screen.zoom(), Zoom::Week);
        screen.handle_event(&key(KeyCode::Char('-')), &context);
        assert_eq!(screen.zoom(), Zoom::Month);
        // Clamped at the coarse end.
        screen.handle_event(&key(KeyCode::Char('-')), &context);
        assert_eq!(screen.zoom(), Zoom::Month);
        screen.handle_event(&key(KeyCode::Char('+')), &context);
        assert_eq!(screen.zoom(), Zoom::Week);
        screen.handle_event(&key(KeyCode::Char('+')), &context);
        assert_eq!(screen.zoom(), Zoom::Day);
    }

    #[test]
    fn unfocused_screen_ignores_navigation_and_zoom() {
        let mut screen = TimelineScreen::new(ComponentId::new("test.timeline"), sample());
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::new(ts(3));
        let context = ctx(&theme, &caps, &clock, false);

        screen.handle_event(&key(KeyCode::Down), &context);
        screen.handle_event(&key(KeyCode::Char('-')), &context);
        assert_eq!(screen.selected, 0);
        assert_eq!(screen.zoom(), Zoom::Day);
    }

    #[test]
    fn down_then_enter_activates_the_second_row() {
        let mut screen = TimelineScreen::new(ComponentId::new("test.timeline"), sample());
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::new(ts(3));
        let context = ctx(&theme, &caps, &clock, true);

        screen.handle_event(&key(KeyCode::Down), &context);
        screen.handle_event(&key(KeyCode::Enter), &context);
        assert_eq!(screen.take_activation(), Some("T-2".to_string()));
        assert_eq!(screen.take_activation(), None);
    }

    #[test]
    fn render_shows_bars_in_the_right_columns_with_a_due_marker() {
        let screen = TimelineScreen::new(ComponentId::new("test.timeline"), sample());
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        // "Today" is day 3 — inside every row's span, so the axis's today marker and each row's
        // bar are both exercised by one fixed clock reading.
        let clock = FixedClock::new(ts(3));
        let context = ctx(&theme, &caps, &clock, true);
        let mut harness = Harness::new(80, 6);
        let lines = harness.render_lines(&screen, &context);

        assert!(lines[0].contains("Enter"), "hint line: {lines:?}");
        // Every bar glyph is single-width, so the exact column past the label boundary pins down
        // "bars in the right columns", not just "a marker appears somewhere in the row".
        let cell = |line: &str, offset: usize| line.chars().nth(LABEL_WIDTH + offset);
        // Axis row: today (day 3) is at column 3.
        assert_eq!(
            cell(&lines[1], 3),
            Some('▼'),
            "today marker: {:?}",
            lines[1]
        );
        // T-1 (row 0, day 0..5): filled from column 0 through 5, nothing past it.
        let t1 = &lines[2];
        assert_eq!(cell(t1, 0), Some('█'), "T-1 start: {t1:?}");
        assert_eq!(cell(t1, 5), Some('█'), "T-1 end: {t1:?}");
        assert_eq!(
            cell(t1, 6),
            Some('·'),
            "T-1 must not spill past day 5: {t1:?}"
        );
        // T-2 (row 1, day 2..9, due day 8): empty before day 2, filled from 2 through 9 except
        // for the due marker at day 8, which wins over the fill.
        let t2 = &lines[3];
        assert_eq!(cell(t2, 1), Some('·'), "T-2 before its start: {t2:?}");
        assert_eq!(cell(t2, 2), Some('█'), "T-2 start: {t2:?}");
        assert_eq!(cell(t2, 8), Some('◆'), "T-2 due marker: {t2:?}");
        assert_eq!(cell(t2, 9), Some('█'), "T-2 end: {t2:?}");
        assert_eq!(
            cell(t2, 10),
            Some('·'),
            "T-2 must not spill past day 9: {t2:?}"
        );
        // T-3 (row 2, day 1..3): filled from 1 through 3 only.
        let t3 = &lines[4];
        assert_eq!(cell(t3, 0), Some('·'), "T-3 before its start: {t3:?}");
        assert_eq!(cell(t3, 1), Some('█'), "T-3 start: {t3:?}");
        assert_eq!(cell(t3, 3), Some('█'), "T-3 end: {t3:?}");
        assert_eq!(
            cell(t3, 4),
            Some('·'),
            "T-3 must not spill past day 3: {t3:?}"
        );
    }

    #[test]
    fn render_with_no_tickets_shows_the_empty_state() {
        let screen = TimelineScreen::new(ComponentId::new("test.timeline"), Vec::new());
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::epoch();
        let context = ctx(&theme, &caps, &clock, true);
        let mut harness = Harness::new(80, 6);
        let lines = harness.render_lines(&screen, &context);

        assert!(
            lines.iter().any(|l| l.contains("No tickets yet")),
            "empty state missing: {lines:?}"
        );
    }

    #[test]
    fn month_zoom_shows_the_calendar_grid_and_lists_due_tickets_per_day() {
        let mut screen = TimelineScreen::new(ComponentId::new("test.timeline"), sample());
        let theme = Theme::default();
        let caps = Capabilities::minimal();
        let clock = FixedClock::new(ts(3));
        let context = ctx(&theme, &caps, &clock, true);
        screen.handle_event(&key(KeyCode::Char('-')), &context);
        screen.handle_event(&key(KeyCode::Char('-')), &context);
        assert_eq!(screen.zoom(), Zoom::Month);

        let mut harness = Harness::new(60, 12);
        let lines = harness.render_lines(&screen, &context);
        assert!(
            lines.iter().any(|l| l.contains("Su") && l.contains("Mo")),
            "calendar header missing: {lines:?}"
        );
        // T-2 is due on day 8 (1970-01-09), inside this month's grid.
        assert!(
            lines.iter().any(|l| l.contains("T-2")),
            "due-ticket listing missing: {lines:?}"
        );
    }
}
