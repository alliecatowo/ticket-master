//! The prompt editor: a multi-line, soft-wrapping text box with a grapheme-aware cursor,
//! shell-style history, readline editing (a kill ring for Ctrl+K/U/W and Ctrl+Y, undo for
//! Ctrl+_), and Claude Code's collapsed pastes (`[Pasted text #1 +41 lines]`, expanded on send).
//!
//! The editor is plain state plus pure layout — which keys do what lives in the chat screen, so
//! the key map (and its "typing never triggers a shortcut" rule) is readable in one place.

use std::cell::Cell;

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Color, Modifier, Style};

use crate::chat::glyphs::Glyphs;
use crate::chat::lines::draw_box;
use crate::text::{display_width, graphemes, truncate};
use crate::theme::Theme;

/// A paste longer than this many characters collapses to a placeholder.
pub const PASTE_COLLAPSE_CHARS: usize = 800;
/// A paste with more than this many lines collapses to a placeholder.
pub const PASTE_COLLAPSE_LINES: usize = 3;

/// The most undo steps kept.
const UNDO_LIMIT: usize = 100;

/// What the last edit was, so a run of typed characters undoes as one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum EditKind {
    #[default]
    Other,
    Typing,
}

/// How the box is drawn: its prompt marker, the marker's colour, and an optional title on the
/// top border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxStyle<'a> {
    /// The marker before the first row (`›`, or `!` in shell mode).
    pub marker: &'a str,
    /// The marker's colour (and the border's, when `tinted`).
    pub color: Color,
    /// Whether the border takes `color` too (shell mode, history search).
    pub tinted: bool,
    /// Text set into the top border (`search history`).
    pub title: Option<&'a str>,
}

/// One visual (wrapped) row of the input: the byte range of `text` it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualRow {
    /// First byte of the row.
    pub start: usize,
    /// One past the last byte shown (excludes the newline that ended the row, if any).
    pub end: usize,
}

/// The editor state.
#[derive(Debug, Default)]
pub struct InputBox {
    text: String,
    /// Byte offset of the cursor into `text`, always on a grapheme boundary.
    cursor: usize,
    history: Vec<String>,
    /// While browsing history: the index being shown, plus what was typed before browsing began.
    browsing: Option<(usize, String)>,
    /// The text width the last render wrapped at, so vertical movement between frames uses the
    /// same rows the human is looking at.
    last_width: Cell<usize>,
    /// Killed (Ctrl+K/U/W, Alt+D) text, newest last, for Ctrl+Y.
    kill_ring: Vec<String>,
    /// Earlier `(text, cursor)` states, newest last, for Ctrl+_.
    undo: Vec<(String, usize)>,
    last_edit: EditKind,
    /// Collapsed pastes, by number: the placeholder `[Pasted text #N ...]` expands to the text.
    pastes: Vec<(usize, String)>,
    /// History entries added since the application last persisted them, already expanded.
    unsaved: Vec<String>,
}

/// The most history entries kept.
const HISTORY_LIMIT: usize = 500;

/// A run of letters and digits: what Alt+B/F/D treat as a word (Claude Code's readline rule;
/// punctuation such as `/`, `.`, `_` separates words).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// The placeholder a collapsed paste shows as.
pub fn paste_placeholder(number: usize, text: &str) -> String {
    let extra_lines = text.trim_end_matches('\n').matches('\n').count();
    if extra_lines == 0 {
        format!("[Pasted text #{number}]")
    } else {
        format!("[Pasted text #{number} +{extra_lines} lines]")
    }
}

/// Whether a paste is big enough to collapse.
pub fn should_collapse(text: &str) -> bool {
    text.chars().count() > PASTE_COLLAPSE_CHARS || text.lines().count() > PASTE_COLLAPSE_LINES
}

impl InputBox {
    /// An empty editor.
    pub fn new() -> Self {
        InputBox::default()
    }

    /// The current text (collapsed pastes appear as their placeholders).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cursor's byte offset.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// True when nothing is typed.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn checkpoint(&mut self, kind: EditKind) {
        if kind == EditKind::Typing && self.last_edit == EditKind::Typing {
            return;
        }
        self.last_edit = kind;
        if self.undo.last().map(|(t, _)| t) == Some(&self.text) {
            return;
        }
        self.undo.push((self.text.clone(), self.cursor));
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
    }

    /// Undo the last edit (Ctrl+_). A run of typed characters is one edit. Returns false when
    /// there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some((text, cursor)) = self.undo.pop() else {
            return false;
        };
        self.text = text;
        self.cursor = cursor.min(self.text.len());
        self.browsing = None;
        self.last_edit = EditKind::Other;
        true
    }

    /// Replace the text, cursor at the end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.checkpoint(EditKind::Other);
        self.text = text.into();
        self.cursor = self.text.len();
        self.browsing = None;
    }

    /// Remove and return the text, leaving the editor empty. Non-blank text is recorded in
    /// history (consecutive duplicates collapse). Collapsed pastes stay as placeholders here;
    /// [`InputBox::expand`] turns them back into the pasted text.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.browsing = None;
        self.undo.clear();
        self.last_edit = EditKind::Other;
        self.remember(&text);
        text
    }

    /// Add `text` to history without touching the editor.
    pub fn remember(&mut self, text: &str) {
        if text.trim().is_empty() || self.history.last().map(String::as_str) == Some(text) {
            return;
        }
        self.history.push(text.to_string());
        self.unsaved.push(self.expand(text));
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
    }

    /// Seed history with entries persisted by an earlier run, oldest first. They go before
    /// anything already remembered this run and are not reported as unsaved.
    pub fn load_history(&mut self, entries: Vec<String>) {
        let mut merged: Vec<String> = Vec::with_capacity(entries.len() + self.history.len());
        for entry in entries.into_iter().chain(self.history.drain(..)) {
            if !entry.trim().is_empty() && merged.last() != Some(&entry) {
                merged.push(entry);
            }
        }
        let skip = merged.len().saturating_sub(HISTORY_LIMIT);
        self.history = merged.split_off(skip);
    }

    /// Every history entry, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// History entries remembered since the last call, for the application to persist.
    pub fn take_unsaved_history(&mut self) -> Vec<String> {
        std::mem::take(&mut self.unsaved)
    }

    /// Clear all the text, stopping any history browse.
    pub fn clear(&mut self) {
        if !self.text.is_empty() {
            self.checkpoint(EditKind::Other);
        }
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
    }

    /// Insert `s` at the cursor. `\r\n` and `\r` become `\n` (pasted text from any platform).
    pub fn insert(&mut self, s: &str) {
        self.checkpoint(EditKind::Other);
        self.insert_raw(s);
    }

    fn insert_raw(&mut self, s: &str) {
        let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &normalized);
        self.cursor += normalized.len();
        self.browsing = None;
    }

    /// Insert a single typed character.
    pub fn insert_char(&mut self, c: char) {
        let kind = if c.is_whitespace() {
            EditKind::Other
        } else {
            EditKind::Typing
        };
        self.checkpoint(kind);
        let mut buf = [0u8; 4];
        self.insert_raw(c.encode_utf8(&mut buf));
    }

    /// Insert a paste: short ones as text, long ones (over [`PASTE_COLLAPSE_CHARS`] characters or
    /// [`PASTE_COLLAPSE_LINES`] lines) as a `[Pasted text #N +M lines]` placeholder that
    /// [`InputBox::expand`] turns back into the text. Returns whether it collapsed.
    pub fn insert_paste(&mut self, text: &str) -> bool {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if !should_collapse(&normalized) {
            self.insert(&normalized);
            return false;
        }
        let number = self.pastes.last().map_or(1, |(n, _)| n + 1);
        let placeholder = paste_placeholder(number, &normalized);
        self.pastes.push((number, normalized));
        self.insert(&placeholder);
        true
    }

    /// `text` with every collapsed paste's placeholder replaced by what was pasted.
    pub fn expand(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (number, pasted) in &self.pastes {
            let placeholder = paste_placeholder(*number, pasted);
            if out.contains(&placeholder) {
                out = out.replace(&placeholder, pasted);
            }
        }
        out
    }

    /// The collapsed paste placeholder ending exactly at byte `at`, if any, as its start.
    fn placeholder_ending_at(&self, at: usize) -> Option<usize> {
        let before = &self.text[..at];
        self.pastes.iter().find_map(|(number, pasted)| {
            let placeholder = paste_placeholder(*number, pasted);
            before
                .ends_with(&placeholder)
                .then(|| at - placeholder.len())
        })
    }

    fn prev_boundary(&self, from: usize) -> Option<usize> {
        graphemes(&self.text[..from])
            .last()
            .map(|g| from - g.text.len())
    }

    fn next_boundary(&self, from: usize) -> Option<usize> {
        graphemes(&self.text[from..])
            .first()
            .map(|g| from + g.text.len())
    }

    /// Delete the grapheme before the cursor — or the whole collapsed paste placeholder the
    /// cursor sits just after, so a paste is removed in one keystroke, never half-edited.
    pub fn backspace(&mut self) {
        let start = self
            .placeholder_ending_at(self.cursor)
            .or_else(|| self.prev_boundary(self.cursor));
        if let Some(start) = start {
            self.checkpoint(EditKind::Other);
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
        self.browsing = None;
    }

    /// Delete the grapheme under the cursor.
    pub fn delete(&mut self) {
        if let Some(end) = self.next_boundary(self.cursor) {
            self.checkpoint(EditKind::Other);
            self.text.replace_range(self.cursor..end, "");
        }
        self.browsing = None;
    }

    fn kill(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        self.checkpoint(EditKind::Other);
        let killed: String = self.text.drain(start..end).collect();
        self.cursor = start;
        self.kill_ring.push(killed);
        if self.kill_ring.len() > 20 {
            self.kill_ring.remove(0);
        }
        self.browsing = None;
    }

    /// Delete back to the previous whitespace (Ctrl+W): one press removes a whole path or
    /// `--flag=value`. The text goes to the kill ring.
    pub fn delete_word_back(&mut self) {
        let start = self.whitespace_word_start(self.cursor);
        self.kill(start, self.cursor);
    }

    /// Delete to the end of the current (or next) alphanumeric word (Alt+D), into the kill ring.
    pub fn delete_word_forward(&mut self) {
        let end = self.word_end_after(self.cursor);
        self.kill(self.cursor, end);
    }

    /// Delete from the cursor to the end of the logical line (Ctrl+K); at the end of a line,
    /// delete the newline instead. The text goes to the kill ring.
    pub fn kill_to_end(&mut self) {
        let end = match self.text[self.cursor..].find('\n') {
            Some(0) => self.cursor + 1,
            Some(i) => self.cursor + i,
            None => self.text.len(),
        };
        self.kill(self.cursor, end);
    }

    /// Delete from the start of the logical line to the cursor (Ctrl+U); at the start of a line,
    /// delete the newline before it, so repeating clears a multi-line prompt.
    pub fn kill_to_start(&mut self) {
        let line_start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let start = if line_start == self.cursor && self.cursor > 0 {
            self.cursor - 1
        } else {
            line_start
        };
        self.kill(start, self.cursor);
    }

    /// Insert the most recently killed text (Ctrl+Y). Returns false when nothing was killed.
    pub fn yank(&mut self) -> bool {
        match self.kill_ring.last().cloned() {
            Some(text) => {
                self.insert(&text);
                true
            }
            None => false,
        }
    }

    fn whitespace_word_start(&self, from: usize) -> usize {
        let before = &self.text[..from];
        let trimmed = before.trim_end_matches(|c: char| c.is_whitespace());
        trimmed
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8))
            .unwrap_or(0)
    }

    fn word_start_before(&self, from: usize) -> usize {
        let before = &self.text[..from];
        let trimmed = before.trim_end_matches(|c: char| !is_word_char(c));
        trimmed
            .rfind(|c: char| !is_word_char(c))
            .map(|i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8))
            .unwrap_or(0)
    }

    fn word_end_after(&self, from: usize) -> usize {
        let after = &self.text[from..];
        let skip = after
            .find(is_word_char)
            .unwrap_or(after.len());
        let rest = &after[skip..];
        let word = rest.find(|c: char| !is_word_char(c)).unwrap_or(rest.len());
        from + skip + word
    }

    /// Move one grapheme left.
    pub fn left(&mut self) {
        if let Some(p) = self.prev_boundary(self.cursor) {
            self.cursor = p;
        }
        self.last_edit = EditKind::Other;
    }

    /// Move one grapheme right.
    pub fn right(&mut self) {
        if let Some(n) = self.next_boundary(self.cursor) {
            self.cursor = n;
        }
        self.last_edit = EditKind::Other;
    }

    /// Move to the start of the previous word (Alt+B; a word is a run of letters and digits).
    pub fn word_left(&mut self) {
        self.cursor = self.word_start_before(self.cursor);
        self.last_edit = EditKind::Other;
    }

    /// Move to the end of the current or next word (Alt+F).
    pub fn word_right(&mut self) {
        self.cursor = self.word_end_after(self.cursor);
        self.last_edit = EditKind::Other;
    }

    /// Move to the start of the current logical line.
    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        self.last_edit = EditKind::Other;
    }

    /// Move to the end of the current logical line.
    pub fn end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i);
        self.last_edit = EditKind::Other;
    }

    /// Replace `start..self.cursor()` with `replacement` (an `@` mention being completed).
    pub fn replace_before_cursor(&mut self, start: usize, replacement: &str) {
        if start > self.cursor || !self.text.is_char_boundary(start) {
            return;
        }
        self.checkpoint(EditKind::Other);
        self.text.replace_range(start..self.cursor, replacement);
        self.cursor = start + replacement.len();
        self.browsing = None;
    }

    /// The rows `text` wraps into at `width` columns: word-wrapped where a space allows it,
    /// hard-broken otherwise, and split at every newline.
    pub fn rows(&self, width: usize) -> Vec<VisualRow> {
        wrap_rows(&self.text, width)
    }

    fn cursor_row(&self, rows: &[VisualRow]) -> usize {
        // The cursor belongs to the last row that starts at or before it; a cursor exactly at a
        // soft-wrap boundary is drawn at the start of the next row, like every editor.
        rows.iter()
            .rposition(|r| r.start <= self.cursor)
            .unwrap_or(0)
    }

    /// Whether the cursor is on the first visual row (Up should browse history, not move).
    pub fn on_first_row(&self) -> bool {
        let rows = self.rows(self.last_width.get().max(1));
        self.cursor_row(&rows) == 0
    }

    /// Whether the cursor is on the last visual row.
    pub fn on_last_row(&self) -> bool {
        let rows = self.rows(self.last_width.get().max(1));
        self.cursor_row(&rows) + 1 >= rows.len()
    }

    /// Move the cursor one visual row up (`-1`) or down (`1`), keeping its column.
    pub fn move_vertical(&mut self, delta: isize) {
        let rows = self.rows(self.last_width.get().max(1));
        let current = self.cursor_row(&rows);
        let target = current as isize + delta;
        if target < 0 || target as usize >= rows.len() {
            return;
        }
        let col = display_width(&self.text[rows[current].start..self.cursor]);
        let row = rows[target as usize];
        let mut pos = row.start;
        let mut used = 0;
        for g in graphemes(&self.text[row.start..row.end]) {
            if used + g.width > col {
                break;
            }
            used += g.width;
            pos += g.text.len();
        }
        self.cursor = pos;
    }

    /// Whether Up/Down are currently walking history.
    pub fn is_browsing(&self) -> bool {
        self.browsing.is_some()
    }

    /// Show the previous history entry. Returns false when there is nothing older.
    pub fn history_prev(&mut self) -> bool {
        let next_index = match &self.browsing {
            Some((i, _)) if *i == 0 => return false,
            Some((i, _)) => i - 1,
            None if self.history.is_empty() => return false,
            None => self.history.len() - 1,
        };
        let draft = match self.browsing.take() {
            Some((_, draft)) => draft,
            None => self.text.clone(),
        };
        self.text = self.history[next_index].clone();
        self.cursor = self.text.len();
        self.browsing = Some((next_index, draft));
        true
    }

    /// Show the next history entry, or restore the draft past the newest. Returns false when not
    /// browsing.
    pub fn history_next(&mut self) -> bool {
        let Some((index, draft)) = self.browsing.take() else {
            return false;
        };
        if index + 1 < self.history.len() {
            self.text = self.history[index + 1].clone();
            self.browsing = Some((index + 1, draft));
        } else {
            self.text = draft;
        }
        self.cursor = self.text.len();
        true
    }

    /// The box's height (borders included) at `width` columns, capped at `max_height`.
    pub fn height(&self, width: u16, max_height: u16) -> u16 {
        let text_width = text_width(width);
        let rows = self.rows(text_width).len().max(1) as u16;
        (rows + 2).clamp(3, max_height.max(3))
    }

    /// Draw the box. `placeholder` shows (muted) while empty; `busy` dims the prompt marker while
    /// a turn runs.
    pub fn render(
        &self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        glyphs: &Glyphs,
        placeholder: &str,
        busy: bool,
    ) {
        let style = BoxStyle {
            marker: glyphs.prompt,
            color: theme.accent,
            tinted: false,
            title: None,
        };
        self.render_styled(area, buf, theme, glyphs, placeholder, busy, style);
    }

    /// [`InputBox::render`] with an explicit marker, colour and title (shell mode, history
    /// search).
    #[allow(clippy::too_many_arguments)]
    pub fn render_styled(
        &self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        glyphs: &Glyphs,
        placeholder: &str,
        busy: bool,
        style: BoxStyle<'_>,
    ) {
        if area.width < 6 || area.height < 3 {
            return;
        }
        let border_style = if style.tinted {
            Style::default().fg(style.color)
        } else {
            Style::default().fg(theme.muted)
        };
        draw_box(buf, area, &glyphs.border, border_style);
        if let Some(title) = style.title {
            let title = format!(" {title} ");
            buf.set_stringn(
                area.x + 2,
                area.y,
                &title,
                (area.width as usize).saturating_sub(4),
                Style::default()
                    .fg(style.color)
                    .add_modifier(Modifier::BOLD),
            );
        }

        let marker_style = if busy && !style.tinted {
            Style::default().fg(theme.muted)
        } else {
            Style::default()
                .fg(style.color)
                .add_modifier(Modifier::BOLD)
        };
        buf.set_stringn(area.x + 2, area.y + 1, style.marker, 1, marker_style);

        let text_x = area.x + 4;
        let width = text_width(area.width);
        self.last_width.set(width);
        let visible_rows = (area.height - 2) as usize;
        // An explicit block colour where the theme has one (it survives terminals and screen
        // captures that ignore the reverse attribute); plain reverse video otherwise.
        let cursor_style = if theme.foreground == Color::Reset {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme.background).bg(theme.foreground)
        };

        if self.text.is_empty() {
            let shown = truncate(placeholder, width.saturating_sub(1), glyphs.ellipsis);
            // The cursor block sits on the placeholder's first cell, the way a real terminal
            // cursor would.
            buf.set_stringn(
                text_x,
                area.y + 1,
                &shown,
                width,
                Style::default().fg(theme.muted),
            );
            buf.set_style(Rect::new(text_x, area.y + 1, 1, 1), cursor_style);
            return;
        }

        let rows = self.rows(width);
        let cursor_row = self.cursor_row(&rows);
        // Keep the cursor's row in view when the text is taller than the box.
        let first = cursor_row.saturating_sub(visible_rows.saturating_sub(1));
        for (i, row) in rows.iter().enumerate().skip(first).take(visible_rows) {
            let y = area.y + 1 + (i - first) as u16;
            buf.set_stringn(
                text_x,
                y,
                &self.text[row.start..row.end],
                width,
                Style::default(),
            );
            if i == cursor_row {
                let col = display_width(&self.text[row.start..self.cursor.max(row.start)]);
                let x = text_x + col.min(width) as u16;
                if x < area.x + area.width - 1 {
                    buf.set_style(Rect::new(x, y, 1, 1), cursor_style);
                }
            }
        }
        if first > 0 {
            let more = if glyphs.unicode { "↑" } else { "^" };
            buf.set_string(area.x + area.width - 3, area.y, more, border_style);
        }
    }
}

/// Columns available for text inside a box `width` wide: border, space, marker, space on the
/// left, one cell of cursor room plus the border on the right.
fn text_width(width: u16) -> usize {
    (width as usize).saturating_sub(6).max(1)
}

/// Wrap `text` into visual rows at `width` columns.
pub fn wrap_rows(text: &str, width: usize) -> Vec<VisualRow> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut line_start = 0;
    for logical in text.split('\n') {
        let base = line_start;
        let mut row_start = base;
        let mut row_width = 0;
        // Byte offset just past the last space seen in the current row, a candidate break.
        let mut last_break: Option<usize> = None;
        let mut offset = base;
        for g in graphemes(logical) {
            if row_width + g.width > width && row_width > 0 {
                let split = match last_break {
                    Some(b) if b > row_start => b,
                    _ => offset,
                };
                rows.push(VisualRow {
                    start: row_start,
                    end: split,
                });
                row_start = split;
                row_width = display_width(&text[row_start..offset]);
                last_break = None;
                // The carried-over word plus this grapheme can still overflow (a wide character
                // at a tiny width): break again, hard, right here.
                if row_width > 0 && row_width + g.width > width {
                    rows.push(VisualRow {
                        start: row_start,
                        end: offset,
                    });
                    row_start = offset;
                    row_width = 0;
                }
            }
            row_width += g.width;
            offset += g.text.len();
            if g.text == " " {
                last_break = Some(offset);
            }
        }
        rows.push(VisualRow {
            start: row_start,
            end: base + logical.len(),
        });
        line_start = base + logical.len() + 1;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> InputBox {
        let mut input = InputBox::new();
        input.insert(s);
        input
    }

    fn row_texts(text: &str, width: usize) -> Vec<String> {
        wrap_rows(text, width)
            .iter()
            .map(|r| text[r.start..r.end].to_string())
            .collect()
    }

    fn before_cursor(input: &InputBox) -> &str {
        &input.text()[..input.cursor()]
    }

    #[test]
    fn editing_is_grapheme_aware() {
        let mut input = typed("héllo 中文 👍🏽");
        input.backspace();
        assert_eq!(input.text(), "héllo 中文 ");
        input.home();
        input.right();
        input.right();
        input.delete();
        assert_eq!(input.text(), "hélo 中文 ");
        input.end();
        input.insert_char('!');
        assert_eq!(input.text(), "hélo 中文 !");
    }

    #[test]
    fn ctrl_w_deletes_back_to_whitespace_and_alt_words_stop_at_punctuation() {
        let mut input = typed("open src/utils/foo.ts now");
        input.word_left();
        assert_eq!(&input.text()[input.cursor()..], "now");
        input.word_left();
        assert_eq!(&input.text()[input.cursor()..], "ts now");
        input.word_left();
        assert_eq!(&input.text()[input.cursor()..], "foo.ts now");
        input.word_right();
        assert_eq!(before_cursor(&input), "open src/utils/foo");

        let mut input = typed("open src/utils/foo.ts");
        input.delete_word_back();
        assert_eq!(input.text(), "open ");
    }

    #[test]
    fn kills_go_to_the_ring_and_yank_brings_them_back() {
        let mut input = typed("alpha beta\ngamma");
        input.home();
        input.kill_to_end();
        assert_eq!(input.text(), "alpha beta\n");
        assert!(input.yank());
        assert_eq!(input.text(), "alpha beta\ngamma");

        let mut input = typed("one two three");
        input.home();
        input.delete_word_forward();
        assert_eq!(input.text(), " two three");
        input.end();
        input.kill_to_start();
        assert_eq!(input.text(), "");
        input.yank();
        assert_eq!(input.text(), " two three");
    }

    #[test]
    fn ctrl_u_repeats_across_lines() {
        let mut input = typed("first\nsecond");
        input.kill_to_start();
        assert_eq!(input.text(), "first\n");
        input.kill_to_start();
        assert_eq!(input.text(), "first");
        input.kill_to_start();
        assert_eq!(input.text(), "");
        let mut ends = typed("a\nb");
        ends.home();
        ends.left();
        ends.kill_to_end();
        assert_eq!(ends.text(), "ab", "Ctrl+K at a line end joins the lines");
    }

    #[test]
    fn undo_restores_whole_typing_runs_and_single_edits() {
        let mut input = InputBox::new();
        for c in "fix the".chars() {
            input.insert_char(c);
        }
        input.delete_word_back();
        assert_eq!(input.text(), "fix ");
        assert!(input.undo());
        assert_eq!(input.text(), "fix the");
        assert!(input.undo());
        assert_eq!(input.text(), "fix ");
        assert!(input.undo());
        assert_eq!(input.text(), "fix");
        assert!(input.undo());
        assert_eq!(input.text(), "");
        assert!(!input.undo());
    }

    #[test]
    fn long_pastes_collapse_and_expand_on_send() {
        let mut input = InputBox::new();
        input.insert("look: ");
        let pasted = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(input.insert_paste(&pasted));
        assert_eq!(input.text(), "look: [Pasted text #1 +9 lines]");
        assert!(!input.insert_paste(" short"));
        let sent = input.take();
        assert_eq!(input.expand(&sent), format!("look: {pasted} short"));
        assert_eq!(
            input.take_unsaved_history(),
            vec![format!("look: {pasted} short")],
            "persisted history holds the real text"
        );

        let mut wide = InputBox::new();
        assert!(wide.insert_paste(&"x".repeat(900)));
        assert_eq!(wide.text(), "[Pasted text #1]");
        wide.backspace();
        assert_eq!(wide.text(), "", "a placeholder deletes as one unit");
    }

    #[test]
    fn pasted_line_endings_normalize() {
        let input = typed("a\r\nb\rc");
        assert_eq!(input.text(), "a\nb\nc");
    }

    #[test]
    fn rows_word_wrap_and_split_on_newlines() {
        assert_eq!(
            row_texts("the quick brown fox", 10),
            vec!["the quick ", "brown fox"]
        );
        assert_eq!(row_texts("abcdefghijkl", 5), vec!["abcde", "fghij", "kl"]);
        assert_eq!(row_texts("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(row_texts("", 10), vec![""]);
    }

    #[test]
    fn rows_cover_the_whole_text_in_order() {
        let text = "lorem ipsum dolor sit amet, consectetur\nadipiscing elit 中文中文中文";
        for width in 1..30 {
            let rows = wrap_rows(text, width);
            let mut joined = String::new();
            for (i, r) in rows.iter().enumerate() {
                if i > 0 && r.start > rows[i - 1].end {
                    joined.push_str(&text[rows[i - 1].end..r.start]);
                }
                joined.push_str(&text[r.start..r.end]);
            }
            assert_eq!(joined, text, "width {width}");
            for r in &rows {
                let w = display_width(&text[r.start..r.end]);
                assert!(
                    w <= width.max(2),
                    "width {width}: row {:?}",
                    &text[r.start..r.end]
                );
            }
        }
    }

    #[test]
    fn history_walks_back_and_restores_the_draft() {
        let mut input = InputBox::new();
        input.insert("first");
        input.take();
        input.insert("second");
        input.take();
        input.insert("draft");
        assert!(input.history_prev());
        assert_eq!(input.text(), "second");
        assert!(input.history_prev());
        assert_eq!(input.text(), "first");
        assert!(!input.history_prev(), "nothing older than the first entry");
        assert!(input.history_next());
        assert_eq!(input.text(), "second");
        assert!(input.history_next());
        assert_eq!(input.text(), "draft");
        assert!(!input.is_browsing());
    }

    #[test]
    fn history_skips_blanks_and_consecutive_duplicates() {
        let mut input = InputBox::new();
        for s in ["a", "a", "  ", "b"] {
            input.insert(s);
            input.take();
        }
        assert!(input.history_prev());
        assert_eq!(input.text(), "b");
        assert!(input.history_prev());
        assert_eq!(input.text(), "a");
        assert!(!input.history_prev());
    }

    #[test]
    fn loaded_history_goes_first_and_is_not_unsaved() {
        let mut input = InputBox::new();
        input.insert("this run");
        input.take();
        input.load_history(vec!["old one".into(), "old two".into()]);
        assert_eq!(input.history(), ["old one", "old two", "this run"]);
        assert_eq!(input.take_unsaved_history(), vec!["this run".to_string()]);
    }

    #[test]
    fn vertical_movement_keeps_the_column() {
        let mut input = typed("abcdef\nxy\nlonger line");
        input.last_width.set(40);
        input.move_vertical(-1);
        assert_eq!(&input.text()[..input.cursor()], "abcdef\nxy");
        input.move_vertical(-1);
        assert_eq!(&input.text()[..input.cursor()], "ab");
        input.move_vertical(1);
        input.move_vertical(1);
        assert_eq!(&input.text()[..input.cursor()], "abcdef\nxy\nlo");
        assert!(input.on_last_row());
    }

    #[test]
    fn height_grows_with_content_up_to_the_cap() {
        let mut input = InputBox::new();
        assert_eq!(input.height(40, 8), 3);
        input.insert("a\nb\nc");
        assert_eq!(input.height(40, 8), 5);
        input.insert(&"\nx".repeat(20));
        assert_eq!(input.height(40, 8), 8);
    }

    fn render_rows(input: &InputBox, width: u16, height: u16, placeholder: &str) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        input.render(
            area,
            &mut buf,
            &Theme::dark(),
            &Glyphs::UNICODE,
            placeholder,
            false,
        );
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn empty_input_renders_the_placeholder_inside_a_rounded_box() {
        let rows = render_rows(&InputBox::new(), 40, 3, "Ask tm anything");
        assert!(rows[0].starts_with('╭') && rows[0].ends_with('╮'));
        assert!(rows[1].contains("› Ask tm anything"), "{rows:?}");
        assert!(rows[2].starts_with('╰'));
    }

    #[test]
    fn tall_text_scrolls_to_keep_the_cursor_visible() {
        let input = typed(
            &(0..10)
                .map(|i| format!("row{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let rows = render_rows(&input, 30, 5, "");
        assert!(rows[3].contains("row9"), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("row0")));
    }
}
