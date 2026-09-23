//! The conversation: what was said and done, how it is laid out at a given width, and which part
//! of it is on screen.
//!
//! Two lists make up the transcript. `entries` is settled history. `live` is the running turn,
//! replaced wholesale on every progress update (the agent reports its steps cumulatively, so
//! replacing rather than appending is what keeps a re-sent step from ever showing twice); it is
//! folded into `entries` when the turn finishes.
//!
//! Layout (wrapping every entry to the viewport width, Markdown included) is cached and only
//! recomputed when the content, the width, or the theme changes — the runtime redraws every
//! 100ms tick, and re-wrapping a long conversation that often would be wasted work.

use std::cell::{Cell, Ref, RefCell};

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};

use crate::chat::glyphs::Glyphs;
use crate::chat::lines::{draw_line, truncate_spans, wrap_with_prefix, Line, Span};
use crate::chat::markdown::{self, block_style};
use crate::chat::sanitize::sanitize;
use crate::text::display_width;
use crate::theme::Theme;

/// How a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// It ran and succeeded.
    Ok,
    /// It errored, or a command it ran exited non-zero.
    Failed,
    /// The authority check refused it.
    Denied,
}

/// One tool call, already reduced to what is worth showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallView {
    /// How it ended.
    pub status: ToolStatus,
    /// The tool's name (`shell.run`).
    pub name: String,
    /// The salient argument: the command, the path, the query. May be empty.
    pub target: String,
    /// A short outcome note: `exit 0`, `12 results`, an error message, a denial reason.
    pub detail: Option<String>,
    /// A few lines of output worth seeing inline (the tail of a failing command's stderr).
    pub preview: Vec<String>,
}

/// How loud a notice is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    /// Routine: "Attached to T-4".
    Info,
    /// Something worked that the human asked for.
    Success,
    /// Worth a second look.
    Warning,
    /// Something failed.
    Error,
}

/// One item in the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// What the human typed.
    User(String),
    /// The model's text, as Markdown.
    Assistant(String),
    /// A tool the model called.
    Tool(ToolCallView),
    /// A message from tm itself (command results, failures, hints).
    Notice {
        /// How loud.
        level: NoticeLevel,
        /// The message.
        text: String,
    },
}

impl Entry {
    /// This entry with every piece of displayable text passed through [`sanitize`].
    pub fn sanitized(self) -> Entry {
        match self {
            Entry::User(text) => Entry::User(sanitize(&text)),
            Entry::Assistant(text) => Entry::Assistant(sanitize(&text)),
            Entry::Notice { level, text } => Entry::Notice {
                level,
                text: sanitize(&text),
            },
            Entry::Tool(view) => Entry::Tool(ToolCallView {
                status: view.status,
                name: sanitize(&view.name),
                // A command's newlines would break the one-line summary; show them as spaces.
                target: sanitize(&view.target).replace('\n', " "),
                detail: view.detail.map(|d| sanitize(&d)),
                preview: view
                    .preview
                    .iter()
                    .flat_map(|l| sanitize(l).lines().map(str::to_string).collect::<Vec<_>>())
                    .collect(),
            }),
        }
    }
}

/// The cached layout and the inputs it was computed from.
#[derive(Debug)]
struct Layout {
    width: u16,
    version: u64,
    theme: Theme,
    glyphs: Glyphs,
    lines: Vec<Line>,
}

/// The conversation model plus its scroll position.
#[derive(Debug, Default)]
pub struct Transcript {
    entries: Vec<Entry>,
    live: Vec<Entry>,
    version: u64,
    /// `None` follows the bottom (new content scrolls into view); `Some(row)` pins the first
    /// visible row, so reading back through history is not yanked away by a running turn.
    scroll_top: Option<usize>,
    layout: RefCell<Option<Layout>>,
    /// `(total rows, viewport rows)` from the last render, for scroll commands issued between
    /// frames.
    last_view: Cell<(usize, usize)>,
}

/// The most lines of a tool line (summary plus wrapped detail) shown before truncating.
const MAX_TOOL_ROWS: usize = 3;

impl Transcript {
    /// An empty conversation.
    pub fn new() -> Self {
        Transcript::default()
    }

    /// True when nothing has been said yet (settled or live).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.live.is_empty()
    }

    /// The settled entries.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The running turn's entries.
    pub fn live(&self) -> &[Entry] {
        &self.live
    }

    fn touch(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// Append a settled entry.
    pub fn push(&mut self, entry: Entry) {
        self.entries.push(entry.sanitized());
        self.touch();
    }

    /// Replace the running turn's entries.
    pub fn set_live(&mut self, entries: Vec<Entry>) {
        let entries: Vec<Entry> = entries.into_iter().map(Entry::sanitized).collect();
        if entries != self.live {
            self.live = entries;
            self.touch();
        }
    }

    /// Settle the running turn's entries into history.
    pub fn commit_live(&mut self) {
        if !self.live.is_empty() {
            self.entries.append(&mut self.live);
            self.touch();
        }
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.live.clear();
        self.scroll_top = None;
        self.touch();
    }

    /// Whether the view is following the newest content.
    pub fn is_following(&self) -> bool {
        self.scroll_top.is_none()
    }

    /// Jump back to following the newest content.
    pub fn follow(&mut self) {
        self.scroll_top = None;
    }

    fn current_top(&self) -> usize {
        let (total, height) = self.last_view.get();
        let bottom_top = total.saturating_sub(height);
        match self.scroll_top {
            Some(top) => top.min(bottom_top),
            None => bottom_top,
        }
    }

    /// Scroll toward older content by `rows`.
    pub fn scroll_up(&mut self, rows: usize) {
        let top = self.current_top().saturating_sub(rows);
        self.scroll_top = Some(top);
    }

    /// Scroll toward newer content by `rows`; reaching the bottom resumes following.
    pub fn scroll_down(&mut self, rows: usize) {
        let (total, height) = self.last_view.get();
        let bottom_top = total.saturating_sub(height);
        let top = self.current_top() + rows;
        self.scroll_top = if top >= bottom_top { None } else { Some(top) };
    }

    /// One viewport (less two rows of overlap) toward older content.
    pub fn page_up(&mut self) {
        let (_, height) = self.last_view.get();
        self.scroll_up(height.saturating_sub(2).max(1));
    }

    /// One viewport toward newer content.
    pub fn page_down(&mut self) {
        let (_, height) = self.last_view.get();
        self.scroll_down(height.saturating_sub(2).max(1));
    }

    /// The whole conversation laid out at `width`, from cache when nothing changed.
    pub fn lines(&self, width: u16, theme: &Theme, glyphs: &Glyphs) -> Ref<'_, Vec<Line>> {
        let stale = match &*self.layout.borrow() {
            Some(layout) => {
                layout.width != width
                    || layout.version != self.version
                    || layout.theme != *theme
                    || layout.glyphs != *glyphs
            }
            None => true,
        };
        if stale {
            let lines = layout_entries(
                self.entries.iter().chain(self.live.iter()),
                width as usize,
                theme,
                glyphs,
            );
            *self.layout.borrow_mut() = Some(Layout {
                width,
                version: self.version,
                theme: *theme,
                glyphs: *glyphs,
                lines,
            });
        }
        Ref::map(self.layout.borrow(), |layout| match layout {
            Some(layout) => &layout.lines,
            None => &EMPTY,
        })
    }

    /// Draw the visible slice of the conversation into `area`, followed by `tail` (the running
    /// turn's spinner row), which always stays visible while following.
    pub fn render(
        &self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        glyphs: &Glyphs,
        tail: &[Line],
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let lines = self.lines(area.width, theme, glyphs);
        let height = area.height as usize;
        let total = lines.len() + tail.len();
        self.last_view.set((total, height));

        let top = self.current_top();
        let row_at = |i: usize| -> Option<&Line> {
            if i < lines.len() {
                lines.get(i)
            } else {
                tail.get(i - lines.len())
            }
        };
        for (offset, y) in (area.y..area.y + area.height).enumerate() {
            if let Some(line) = row_at(top + offset) {
                draw_line(buf, area.x, y, area.width, line);
            }
        }

        let below = total.saturating_sub(top + height);
        if below > 0 {
            let note = format!(" {below} more below {} PgDn ", glyphs.sep.trim());
            let width = display_width(&note) as u16;
            if width < area.width {
                let y = area.y + area.height - 1;
                let x = area.x + area.width - width;
                buf.set_stringn(
                    x,
                    y,
                    &note,
                    width as usize,
                    Style::default().fg(theme.background).bg(theme.muted),
                );
            }
        }
    }
}

static EMPTY: Vec<Line> = Vec::new();

/// Lay every entry out at `width`, with the conversation's vertical rhythm: a blank row between
/// entries, except between consecutive tool calls, which read as one compact group.
pub fn layout_entries<'a>(
    entries: impl Iterator<Item = &'a Entry>,
    width: usize,
    theme: &Theme,
    glyphs: &Glyphs,
) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut prev: Option<&Entry> = None;
    for entry in entries {
        let grouped = matches!((prev, entry), (Some(Entry::Tool(_)), Entry::Tool(_)));
        if prev.is_some() && !grouped {
            out.push(Line::blank());
        }
        out.extend(entry_lines(entry, width, theme, glyphs));
        prev = Some(entry);
    }
    out
}

/// Lay out one entry.
pub fn entry_lines(entry: &Entry, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let width = width.max(8);
    match entry {
        Entry::User(text) => user_lines(text, width, theme, glyphs),
        Entry::Assistant(text) => assistant_lines(text, width, theme, glyphs),
        Entry::Tool(view) => tool_lines(view, width, theme, glyphs),
        Entry::Notice { level, text } => notice_lines(*level, text, width, theme, glyphs),
    }
}

/// The echoed prompt: a raised band so the human's turns are easy to find when scrolling back.
fn user_lines(text: &str, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let band = block_style(theme);
    let marker = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let body = band;
    let prefix_text = format!(" {} ", glyphs.prompt);
    let indent = " ".repeat(display_width(&prefix_text));
    let mut out = Vec::new();
    for (i, source) in text.lines().enumerate() {
        let prefix = if i == 0 {
            vec![Span::new(prefix_text.clone(), marker.patch(band_bg(band)))]
        } else {
            vec![Span::new(indent.clone(), band)]
        };
        let rows = wrap_with_prefix(
            &[Span::new(source, body)],
            width.saturating_sub(1),
            prefix,
            &indent,
        );
        for row in rows {
            out.push(row.with_fill(band));
        }
    }
    if out.is_empty() {
        out.push(Line::plain(prefix_text, marker).with_fill(band));
    }
    out
}

/// Only the background half of a style (so an accent marker can sit on the band).
fn band_bg(style: Style) -> Style {
    match style.bg {
        Some(bg) => Style::default().bg(bg),
        None => Style::default(),
    }
}

/// The model's reply: Markdown, with a leading marker on the first row and a two-column hang.
fn assistant_lines(text: &str, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let body = markdown::render(text, width.saturating_sub(2), theme, glyphs);
    body.into_iter()
        .enumerate()
        .map(|(i, line)| {
            let prefix = if i == 0 {
                vec![
                    Span::new(glyphs.assistant, Style::default()),
                    Span::new(" ", Style::default()),
                ]
            } else {
                vec![Span::new("  ", Style::default())]
            };
            line.prefixed(prefix)
        })
        .collect()
}

/// A tool call as a compact, scannable line: status glyph, name, the salient argument, and the
/// outcome — plus, for a failure worth reading, a few gutter-marked lines of output.
pub fn tool_lines(view: &ToolCallView, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let muted = Style::default().fg(theme.muted);
    let (glyph, glyph_style, detail_style) = match view.status {
        ToolStatus::Ok => (glyphs.ok, Style::default().fg(theme.success), muted),
        ToolStatus::Failed => (
            glyphs.fail,
            Style::default().fg(theme.danger),
            Style::default().fg(theme.danger),
        ),
        ToolStatus::Denied => (
            glyphs.denied,
            Style::default().fg(theme.warning),
            Style::default().fg(theme.warning),
        ),
    };

    let mut spans = vec![Span::new(
        view.name.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if !view.target.is_empty() {
        spans.push(Span::new("  ", Style::default()));
        spans.push(Span::new(view.target.clone(), Style::default()));
    }
    if let Some(detail) = &view.detail {
        match view.status {
            ToolStatus::Ok => spans.push(Span::new(format!("  ({detail})"), detail_style)),
            _ => spans.push(Span::new(
                format!(" {} {detail}", dash(glyphs)),
                detail_style,
            )),
        }
    }

    let prefix = vec![
        Span::new(glyph, glyph_style),
        Span::new(" ", Style::default()),
    ];
    let mut rows = wrap_with_prefix(&spans, width, prefix, "  ");
    if rows.len() > MAX_TOOL_ROWS {
        rows.truncate(MAX_TOOL_ROWS);
        if let Some(last) = rows.last_mut() {
            let mut spans = last.spans.clone();
            spans.push(Span::new(glyphs.ellipsis, detail_style));
            last.spans = truncate_spans(&spans, width, glyphs.ellipsis);
        }
    }

    for line in &view.preview {
        let prefix = format!("  {} ", glyphs.gutter);
        let spans = vec![Span::new(prefix, muted), Span::new(line.clone(), muted)];
        rows.push(Line::from_spans(truncate_spans(
            &spans,
            width,
            glyphs.ellipsis,
        )));
    }
    rows
}

fn dash(glyphs: &Glyphs) -> &'static str {
    if glyphs.unicode {
        "—"
    } else {
        "-"
    }
}

/// A message from tm itself.
fn notice_lines(
    level: NoticeLevel,
    text: &str,
    width: usize,
    theme: &Theme,
    glyphs: &Glyphs,
) -> Vec<Line> {
    let (glyph, glyph_style, text_style) = match level {
        NoticeLevel::Info => (
            glyphs.info,
            Style::default().fg(theme.muted),
            Style::default().fg(theme.muted),
        ),
        NoticeLevel::Success => (
            glyphs.ok,
            Style::default().fg(theme.success),
            Style::default(),
        ),
        NoticeLevel::Warning => (
            glyphs.warn,
            Style::default().fg(theme.warning),
            Style::default().fg(theme.warning),
        ),
        NoticeLevel::Error => (
            glyphs.fail,
            Style::default().fg(theme.danger),
            Style::default().fg(theme.danger),
        ),
    };
    let mut out = Vec::new();
    for (i, source) in text.lines().enumerate() {
        let prefix = if i == 0 {
            vec![
                Span::new(glyph, glyph_style),
                Span::new(" ", Style::default()),
            ]
        } else {
            vec![Span::new("  ", Style::default())]
        };
        out.extend(wrap_with_prefix(
            &[Span::new(source, text_style)],
            width,
            prefix,
            "  ",
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::text).collect()
    }

    fn tool(status: ToolStatus, name: &str, target: &str, detail: Option<&str>) -> ToolCallView {
        ToolCallView {
            status,
            name: name.to_string(),
            target: target.to_string(),
            detail: detail.map(str::to_string),
            preview: Vec::new(),
        }
    }

    #[test]
    fn a_successful_command_is_one_scannable_line() {
        let theme = Theme::dark();
        let lines = tool_lines(
            &tool(
                ToolStatus::Ok,
                "shell.run",
                "python3 -m pytest -q",
                Some("exit 0"),
            ),
            80,
            &theme,
            &Glyphs::UNICODE,
        );
        assert_eq!(
            texts(&lines),
            vec!["✓ shell.run  python3 -m pytest -q  (exit 0)"]
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.success));
    }

    #[test]
    fn a_failed_edit_shows_the_error_in_the_danger_color() {
        let theme = Theme::dark();
        let lines = tool_lines(
            &tool(
                ToolStatus::Failed,
                "edit.apply_patch",
                "calc.py",
                Some("patch did not apply"),
            ),
            80,
            &theme,
            &Glyphs::UNICODE,
        );
        assert_eq!(
            texts(&lines),
            vec!["✗ edit.apply_patch  calc.py — patch did not apply"]
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.danger));
        let error = lines[0]
            .spans
            .iter()
            .find(|s| s.text.contains("patch did not apply"))
            .expect("the error text is on the line");
        assert_eq!(error.style.fg, Some(theme.danger));
    }

    #[test]
    fn a_denied_call_uses_its_own_glyph() {
        let theme = Theme::dark();
        let lines = tool_lines(
            &tool(
                ToolStatus::Denied,
                "shell.run",
                "rm -rf /",
                Some("not permitted"),
            ),
            80,
            &theme,
            &Glyphs::ASCII,
        );
        assert_eq!(texts(&lines), vec!["- shell.run  rm -rf / - not permitted"]);
    }

    #[test]
    fn long_tool_lines_wrap_with_a_hang_and_are_capped() {
        let theme = Theme::dark();
        let long = "x ".repeat(200);
        let lines = tool_lines(
            &tool(ToolStatus::Failed, "shell.run", "make", Some(&long)),
            40,
            &theme,
            &Glyphs::UNICODE,
        );
        assert_eq!(lines.len(), MAX_TOOL_ROWS);
        for line in &lines {
            assert!(line.width() <= 40, "{:?}", line.text());
        }
        assert!(lines[1].text().starts_with("  "));
        assert!(lines[MAX_TOOL_ROWS - 1].text().ends_with('…'));
    }

    #[test]
    fn previews_render_under_a_gutter_and_never_wrap() {
        let theme = Theme::dark();
        let mut view = tool(ToolStatus::Failed, "test.run", "pytest -q", Some("exit 1"));
        view.preview = vec!["FAILED test_calc.py::test_div".to_string(), "y".repeat(200)];
        let lines = tool_lines(&view, 50, &theme, &Glyphs::UNICODE);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].text(), "  │ FAILED test_calc.py::test_div");
        assert!(lines[2].width() <= 50);
    }

    #[test]
    fn sanitizing_strips_ansi_from_every_field() {
        let entry = Entry::Tool(ToolCallView {
            status: ToolStatus::Failed,
            name: "shell.run".to_string(),
            target: "echo\x1b[2J hi\nthere".to_string(),
            detail: Some("\x1b[31mred\x1b[0m".to_string()),
            preview: vec!["a\x1b[1mb\nc".to_string()],
        })
        .sanitized();
        let Entry::Tool(view) = entry else {
            panic!("still a tool entry");
        };
        assert_eq!(view.target, "echo hi there");
        assert_eq!(view.detail.as_deref(), Some("red"));
        assert_eq!(view.preview, vec!["ab", "c"]);
    }

    #[test]
    fn entries_are_separated_but_tool_runs_stay_compact() {
        let theme = Theme::dark();
        let entries = [
            Entry::User("fix it".to_string()),
            Entry::Tool(tool(ToolStatus::Ok, "fs.read", "calc.py", None)),
            Entry::Tool(tool(ToolStatus::Ok, "shell.run", "pytest", Some("exit 0"))),
            Entry::Assistant("Done.".to_string()),
        ];
        let lines = layout_entries(entries.iter(), 60, &theme, &Glyphs::UNICODE);
        let text: Vec<String> = texts(&lines)
            .iter()
            .map(|s| s.trim_end().to_string())
            .collect();
        assert_eq!(
            text,
            vec![
                " › fix it",
                "",
                "✓ fs.read  calc.py",
                "✓ shell.run  pytest  (exit 0)",
                "",
                "● Done.",
            ]
        );
    }

    #[test]
    fn the_user_band_is_filled_and_multiline_prompts_hang() {
        let theme = Theme::dark();
        let lines = user_lines("first\nsecond", 40, &theme, &Glyphs::UNICODE);
        assert_eq!(texts(&lines), vec![" › first", "   second"]);
        assert!(lines.iter().all(|l| l.fill.is_some()));
    }

    #[test]
    fn assistant_code_blocks_keep_their_band_inside_the_hang() {
        let theme = Theme::dark();
        let lines = assistant_lines("Look:\n```\nx = 1\n```", 40, &theme, &Glyphs::UNICODE);
        assert_eq!(texts(&lines), vec!["● Look:", "   x = 1"]);
        assert_eq!(lines[1].fill_start, 2);
    }

    #[test]
    fn live_entries_replace_rather_than_append() {
        let mut t = Transcript::new();
        t.push(Entry::User("hi".to_string()));
        t.set_live(vec![Entry::Assistant("one".to_string())]);
        t.set_live(vec![
            Entry::Assistant("one".to_string()),
            Entry::Tool(tool(ToolStatus::Ok, "fs.read", "a", None)),
        ]);
        assert_eq!(t.live().len(), 2);
        t.commit_live();
        assert_eq!(t.entries().len(), 3);
        assert!(t.live().is_empty());
    }

    #[test]
    fn layout_is_cached_until_content_or_width_changes() {
        let theme = Theme::dark();
        let mut t = Transcript::new();
        t.push(Entry::Assistant("hello".to_string()));
        let first = t.lines(40, &theme, &Glyphs::UNICODE).as_ptr();
        let again = t.lines(40, &theme, &Glyphs::UNICODE).as_ptr();
        assert_eq!(first, again, "unchanged content must not re-layout");
        t.push(Entry::Assistant("more".to_string()));
        assert_eq!(t.lines(40, &theme, &Glyphs::UNICODE).len(), 3);
        assert_eq!(t.lines(10, &theme, &Glyphs::UNICODE).len(), 3);
    }

    fn render_rows(t: &Transcript, width: u16, height: u16) -> Vec<String> {
        let theme = Theme::dark();
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        t.render(area, &mut buf, &theme, &Glyphs::UNICODE, &[]);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn following_shows_the_newest_rows_and_scrolling_pins_the_view() {
        let mut t = Transcript::new();
        for i in 0..20 {
            t.push(Entry::Notice {
                level: NoticeLevel::Info,
                text: format!("line {i}"),
            });
        }
        let rows = render_rows(&t, 40, 5);
        assert!(rows[4].contains("line 19"), "{rows:?}");

        t.scroll_up(10);
        let rows = render_rows(&t, 40, 5);
        assert!(!rows.iter().any(|r| r.contains("line 19")));
        assert!(rows[4].contains("more below"), "{rows:?}");

        // New content while scrolled back does not move the view.
        let before = render_rows(&t, 40, 5);
        t.push(Entry::Notice {
            level: NoticeLevel::Info,
            text: "line 20".to_string(),
        });
        assert_eq!(render_rows(&t, 40, 5)[0], before[0]);

        t.scroll_down(1000);
        assert!(t.is_following());
        assert!(render_rows(&t, 40, 5)[4].contains("line 20"));
    }

    #[test]
    fn very_long_unbroken_text_never_overflows_the_viewport() {
        let theme = Theme::dark();
        let entries = [
            Entry::User("a".repeat(500)),
            Entry::Assistant("b".repeat(500)),
            Entry::Notice {
                level: NoticeLevel::Error,
                text: "c".repeat(500),
            },
        ];
        for width in [8usize, 20, 80] {
            for line in layout_entries(entries.iter(), width, &theme, &Glyphs::UNICODE) {
                assert!(line.width() <= width, "{width}: {:?}", line.text());
            }
        }
    }
}
