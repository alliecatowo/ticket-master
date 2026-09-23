//! The conversation: what was said and done, how it is laid out at a given width, and which part
//! of it is on screen.
//!
//! Two lists make up the transcript. `entries` is settled history. `live` is the running turn,
//! replaced wholesale on every progress update (the agent reports its steps cumulatively, so
//! replacing rather than appending is what keeps a re-sent step from ever showing twice); it is
//! folded into `entries` when the turn finishes.
//!
//! # How a tool call reads (`docs/decisions/D-019-claude-code-parity-shell.md`)
//!
//! The way Claude Code draws it, so its users read it without thinking: a `⏺` coloured by outcome,
//! the tool under the name Claude Code uses (`Bash`, `Read`, `Update`, `Write`, `Search`, `List`;
//! see [`tool_label`]) with its salient argument in parentheses, then the result hanging under a
//! `⎿`: the first lines of a command's output and `… +N lines (ctrl+o to expand)`, `Read N lines`,
//! or `Updated <path> with X additions and Y removals` followed by a numbered inline diff with
//! removals on a red band and additions on a green one. Errors are red under the `⎿`. The real
//! tool name and the full input and output stay on the [`ToolCallView`] for the transcript viewer
//! (`crate::chat::viewer`).
//!
//! Layout (wrapping every entry to the viewport width, Markdown included) is cached and only
//! recomputed when the content, the width, or the theme changes — the runtime redraws every
//! 100ms tick, and re-wrapping a long conversation that often would be wasted work.

use std::cell::{Cell, Ref, RefCell};

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Color, Modifier, Style};

use crate::chat::diff::{DiffLine, DiffLineKind};
use crate::chat::glyphs::Glyphs;
use crate::chat::lines::{draw_line, truncate_spans, wrap_with_prefix, Line, Span};
use crate::chat::markdown::{self, block_style};
use crate::chat::sanitize::sanitize;
use crate::text::{display_width, truncate};
use crate::theme::Theme;

/// How a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolStatus {
    /// It ran and succeeded.
    #[default]
    Ok,
    /// It errored, or a command it ran exited non-zero.
    Failed,
    /// The authority check refused it.
    Denied,
    /// It is still running (a `!` shell command in flight).
    Running,
}

/// What a tool call produced, in the shape the transcript draws it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolBody {
    /// Nothing beyond [`ToolCallView::detail`].
    #[default]
    None,
    /// A command's output lines, all of them; the transcript shows the first (or, for a failure,
    /// the last) few and the viewer shows the rest.
    Output(Vec<String>),
    /// One line of result: `Read 42 lines`, `Found 3 results`.
    Summary(String),
    /// An edit, as numbered diff lines.
    Diff {
        /// What the result line says before the diff: `Updated calc.py with 2 additions and 1
        /// removal`.
        summary: String,
        /// The diff itself.
        lines: Vec<DiffLine>,
    },
}

/// One tool call, already reduced to what is worth showing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolCallView {
    /// How it ended.
    pub status: ToolStatus,
    /// The tool's real name (`shell.run`). The transcript shows [`tool_label`] of it; the viewer
    /// shows both.
    pub name: String,
    /// The salient argument: the command, the path, the query. May be empty.
    pub target: String,
    /// A short outcome note: `Exit code 1`, an error message, a denial reason.
    pub detail: Option<String>,
    /// What it produced.
    pub body: ToolBody,
    /// The full input, for the transcript viewer (pretty JSON, or the command).
    pub input: String,
    /// The full output, for the transcript viewer.
    pub output: String,
}

impl ToolCallView {
    /// A call named `name` about `target` that ended with `status`, with nothing else filled in.
    pub fn new(status: ToolStatus, name: impl Into<String>, target: impl Into<String>) -> Self {
        ToolCallView {
            status,
            name: name.into(),
            target: target.into(),
            ..ToolCallView::default()
        }
    }

    /// A shell command's result, the way both the model's `shell.run` and the human's `!` mode
    /// show it: status from the exit code, the output lines (stdout then stderr) as the body.
    pub fn command(name: &str, command: &str, exit_code: i64, stdout: &str, stderr: &str) -> Self {
        let mut lines: Vec<String> = stdout.lines().map(str::to_string).collect();
        lines.extend(stderr.lines().map(str::to_string));
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        let mut output = stdout.to_string();
        if !stderr.trim().is_empty() {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(stderr);
        }
        ToolCallView {
            status: if exit_code == 0 {
                ToolStatus::Ok
            } else {
                ToolStatus::Failed
            },
            name: name.to_string(),
            target: command.to_string(),
            detail: (exit_code != 0).then(|| format!("Exit code {exit_code}")),
            body: ToolBody::Output(lines),
            input: command.to_string(),
            output,
        }
    }
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
    /// A command the human ran with `!` (shell mode): the command on the human's band, its
    /// result hanging under it.
    Shell(ToolCallView),
    /// A message from tm itself (command results, failures, hints).
    Notice {
        /// How loud.
        level: NoticeLevel,
        /// The message.
        text: String,
    },
}

fn sanitize_view(view: ToolCallView) -> ToolCallView {
    let lines = |lines: Vec<String>| -> Vec<String> {
        lines
            .iter()
            .flat_map(|l| sanitize(l).lines().map(str::to_string).collect::<Vec<_>>())
            .collect()
    };
    ToolCallView {
        status: view.status,
        name: sanitize(&view.name),
        // A command's newlines would break the one-line header; show them as spaces.
        target: sanitize(&view.target).replace('\n', " "),
        detail: view.detail.map(|d| sanitize(&d)),
        body: match view.body {
            ToolBody::None => ToolBody::None,
            ToolBody::Output(out) => ToolBody::Output(lines(out)),
            ToolBody::Summary(s) => ToolBody::Summary(sanitize(&s).replace('\n', " ")),
            ToolBody::Diff { summary, lines } => ToolBody::Diff {
                summary: sanitize(&summary).replace('\n', " "),
                lines: lines
                    .into_iter()
                    .map(|l| DiffLine {
                        text: sanitize(&l.text).replace('\n', " "),
                        ..l
                    })
                    .collect(),
            },
        },
        input: sanitize(&view.input),
        output: sanitize(&view.output),
    }
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
            Entry::Tool(view) => Entry::Tool(sanitize_view(view)),
            Entry::Shell(view) => Entry::Shell(sanitize_view(view)),
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

/// The most rows a tool call's header (label plus wrapped argument) takes before truncating.
const MAX_TOOL_ROWS: usize = 3;
/// How many output lines a command shows under its `⎿` before `… +N lines`.
pub const OUTPUT_PREVIEW_LINES: usize = 4;
/// How many diff lines an edit shows before `… +N lines`.
pub const DIFF_PREVIEW_LINES: usize = 20;

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

    /// A counter that changes whenever the content does (for caches over it).
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Append a settled entry.
    pub fn push(&mut self, entry: Entry) {
        self.entries.push(entry.sanitized());
        self.touch();
    }

    /// Replace the most recent still-running `!` command's entry with its result. Returns false
    /// when there is none (the conversation was cleared while it ran).
    pub fn finish_shell(&mut self, view: ToolCallView) -> bool {
        let running = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| matches!(e, Entry::Shell(v) if v.status == ToolStatus::Running));
        match running {
            Some(entry) => {
                let mut view = view;
                if let (true, Entry::Shell(was)) = (view.target.is_empty(), &*entry) {
                    // An interruption knows nothing about the command; keep what was shown.
                    view.target = was.target.clone();
                    view.input = was.input.clone();
                }
                *entry = Entry::Shell(view).sanitized();
                self.touch();
                true
            }
            None => false,
        }
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
    /// turn's spinner row, queued messages), which always stays visible while following.
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
            let note = format!(" {below} more below {} PgDn to scroll ", glyphs.sep.trim());
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

/// Lay every entry out at `width`, with a blank row between entries (Claude Code's rhythm: each
/// `⏺` starts its own block).
pub fn layout_entries<'a>(
    entries: impl Iterator<Item = &'a Entry>,
    width: usize,
    theme: &Theme,
    glyphs: &Glyphs,
) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    for (i, entry) in entries.enumerate() {
        if i > 0 {
            out.push(Line::blank());
        }
        out.extend(entry_lines(entry, width, theme, glyphs));
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
        Entry::Shell(view) => shell_lines(view, width, theme, glyphs),
        Entry::Notice { level, text } => notice_lines(*level, text, width, theme, glyphs),
    }
}

/// The echoed prompt: a raised band so the human's turns are easy to find when scrolling back.
fn user_lines(text: &str, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let marker = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    band_lines(text, glyphs.prompt, marker, width, theme)
}

/// `text` on the human's raised band, its first row led by `mark` in `mark_style`.
fn band_lines(text: &str, mark: &str, mark_style: Style, width: usize, theme: &Theme) -> Vec<Line> {
    let band = block_style(theme);
    let prefix_text = format!(" {mark} ");
    let indent = " ".repeat(display_width(&prefix_text));
    let mut out = Vec::new();
    for (i, source) in text.lines().enumerate() {
        let prefix = if i == 0 {
            vec![Span::new(
                prefix_text.clone(),
                mark_style.patch(band_bg(band)),
            )]
        } else {
            vec![Span::new(indent.clone(), band)]
        };
        let rows = wrap_with_prefix(
            &[Span::new(source, band)],
            width.saturating_sub(1),
            prefix,
            &indent,
        );
        for row in rows {
            out.push(row.with_fill(band));
        }
    }
    if out.is_empty() {
        out.push(Line::plain(prefix_text, mark_style).with_fill(band));
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

/// The model's reply: Markdown, led by `⏺` and hung two columns in.
fn assistant_lines(text: &str, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let body = markdown::render(text, width.saturating_sub(2), theme, glyphs);
    body.into_iter()
        .enumerate()
        .map(|(i, line)| {
            let prefix = if i == 0 {
                vec![
                    Span::new(glyphs.record, Style::default().fg(theme.foreground)),
                    Span::new(" ", Style::default()),
                ]
            } else {
                vec![Span::new("  ", Style::default())]
            };
            line.prefixed(prefix)
        })
        .collect()
}

/// The name Claude Code users know a tool by: `shell.run` is `Bash`, `edit.apply_patch` is
/// `Update`. Tools with no Claude Code counterpart keep their own name.
pub fn tool_label(name: &str) -> String {
    match name {
        // `shell` is the human's own `!` command.
        "shell.run" | "test.run" | "build.run" | "shell" => "Bash".to_string(),
        "fs.read" | "fs.read_range" => "Read".to_string(),
        "fs.list" => "List".to_string(),
        "edit.apply_patch" | "edit.write_file" => "Update".to_string(),
        "edit.create_file" => "Write".to_string(),
        "edit.delete_file" => "Delete".to_string(),
        "compact" => "Compact".to_string(),
        n if n.starts_with("search.") => "Search".to_string(),
        n if n.starts_with("ticket.") => "Ticket".to_string(),
        n => n.to_string(),
    }
}

/// The `⏺` colour for a call's outcome.
fn status_style(status: ToolStatus, theme: &Theme) -> Style {
    match status {
        ToolStatus::Ok => Style::default().fg(theme.success),
        ToolStatus::Failed => Style::default().fg(theme.danger),
        ToolStatus::Denied => Style::default().fg(theme.warning),
        ToolStatus::Running => Style::default().fg(theme.muted),
    }
}

/// `… +12 lines (ctrl+o to expand)`.
pub fn expand_hint(hidden: usize, glyphs: &Glyphs) -> String {
    let unit = if hidden == 1 { "line" } else { "lines" };
    format!("{} +{hidden} {unit} (ctrl+o to expand)", glyphs.ellipsis)
}

/// A tool call the way Claude Code draws one: `⏺ Label(target)` and its result under a `⎿`.
pub fn tool_lines(view: &ToolCallView, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let mut spans = vec![Span::new(
        tool_label(&view.name),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if !view.target.is_empty() {
        spans.push(Span::new(format!("({})", view.target), Style::default()));
    }
    let prefix = vec![
        Span::new(glyphs.record, status_style(view.status, theme)),
        Span::new(" ", Style::default()),
    ];
    let mut rows = wrap_with_prefix(&spans, width, prefix, "  ");
    if rows.len() > MAX_TOOL_ROWS {
        rows.truncate(MAX_TOOL_ROWS);
        if let Some(last) = rows.last_mut() {
            let mut spans = last.spans.clone();
            spans.push(Span::new(glyphs.ellipsis, Style::default()));
            last.spans = truncate_spans(&spans, width, glyphs.ellipsis);
        }
    }
    rows.extend(result_lines(view, width, theme, glyphs));
    rows
}

/// A `!` command: the command on the human's band (marked `!`), its result under a `⎿`.
fn shell_lines(view: &ToolCallView, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let mark = Style::default()
        .fg(theme.warning)
        .add_modifier(Modifier::BOLD);
    let mut rows = band_lines(&view.target, "!", mark, width, theme);
    rows.extend(result_lines(view, width, theme, glyphs));
    rows
}

/// The rows under a call's `⎿`: the first carries the elbow, the rest hang under it.
fn result_lines(view: &ToolCallView, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let muted = Style::default().fg(theme.muted);
    let danger = Style::default().fg(theme.danger);
    let warning = Style::default().fg(theme.warning);
    let plain = Style::default();
    let mut body: Vec<Line> = Vec::new();
    let text_width = width.saturating_sub(5).max(1);
    let wrapped = |text: &str, style: Style, max_rows: usize| -> Vec<Line> {
        let mut rows = wrap_with_prefix(&[Span::new(text, style)], text_width, Vec::new(), "");
        if rows.len() > max_rows {
            rows.truncate(max_rows);
            if let Some(last) = rows.last_mut() {
                let mut spans = last.spans.clone();
                spans.push(Span::new(glyphs.ellipsis, style));
                last.spans = truncate_spans(&spans, text_width, glyphs.ellipsis);
            }
        }
        rows
    };
    let clipped = |text: &str, style: Style| -> Line {
        Line::plain(truncate(text, text_width, glyphs.ellipsis), style)
    };

    match (&view.status, &view.body) {
        (ToolStatus::Running, _) => {
            body.push(Line::plain(format!("Running{}", glyphs.ellipsis), muted));
        }
        (ToolStatus::Denied, _) => {
            let reason = view.detail.as_deref().unwrap_or("not permitted");
            body.extend(wrapped(&format!("Denied: {reason}"), warning, 3));
        }
        (ToolStatus::Failed, ToolBody::Output(lines)) => {
            let detail = view.detail.as_deref().unwrap_or("failed");
            body.extend(wrapped(&format!("Error: {detail}"), danger, 2));
            // A failure's cause is at the end of its output: show the tail.
            let shown = lines.len().min(OUTPUT_PREVIEW_LINES);
            let hidden = lines.len() - shown;
            if hidden > 0 {
                body.push(Line::plain(expand_hint(hidden, glyphs), muted));
            }
            for line in &lines[hidden..] {
                body.push(clipped(line, plain));
            }
        }
        (ToolStatus::Failed, _) => {
            let detail = view.detail.as_deref().unwrap_or("failed");
            body.extend(wrapped(&format!("Error: {detail}"), danger, 3));
        }
        (ToolStatus::Ok, ToolBody::Output(lines)) => {
            if lines.is_empty() {
                body.push(Line::plain("(No content)", muted));
            }
            for line in lines.iter().take(OUTPUT_PREVIEW_LINES) {
                body.push(clipped(line, plain));
            }
            if lines.len() > OUTPUT_PREVIEW_LINES {
                body.push(Line::plain(
                    expand_hint(lines.len() - OUTPUT_PREVIEW_LINES, glyphs),
                    muted,
                ));
            }
        }
        (ToolStatus::Ok, ToolBody::Summary(text)) => body.extend(wrapped(text, plain, 2)),
        (ToolStatus::Ok, ToolBody::Diff { summary, lines }) => {
            body.extend(wrapped(summary, plain, 2));
            body.extend(diff_rows(lines, text_width, theme, glyphs));
        }
        (ToolStatus::Ok, ToolBody::None) => {
            if let Some(detail) = &view.detail {
                body.extend(wrapped(detail, muted, 2));
            }
        }
    }

    body.into_iter()
        .enumerate()
        .map(|(i, line)| {
            let prefix = if i == 0 {
                vec![
                    Span::new("  ", plain),
                    Span::new(glyphs.result, muted),
                    Span::new("  ", plain),
                ]
            } else {
                vec![Span::new("     ", plain)]
            };
            line.prefixed(prefix)
        })
        .collect()
}

/// `over` laid on `base` at `alpha` (0..=1), when both are true colours.
fn tint(base: Color, over: Color, alpha: f32) -> Option<Color> {
    match (base, over) {
        (Color::Rgb(br, bg, bb), Color::Rgb(or, og, ob)) => {
            let mix = |b: u8, o: u8| (b as f32 + (o as f32 - b as f32) * alpha).round() as u8;
            Some(Color::Rgb(mix(br, or), mix(bg, og), mix(bb, ob)))
        }
        _ => None,
    }
}

/// The styles of a removed and an added diff line: a dark red/green band on a true-colour theme,
/// red/green text otherwise.
pub fn diff_styles(theme: &Theme) -> (Style, Style) {
    let band = |accent: Color, indexed: u8| match tint(theme.background, accent, 0.25) {
        Some(bg) => Style::default().fg(theme.foreground).bg(bg),
        // A 256-colour terminal: xterm's dark red (52) and dark green (22) bands.
        None if matches!(theme.background, Color::Indexed(_)) => Style::default()
            .fg(theme.foreground)
            .bg(Color::Indexed(indexed)),
        None => Style::default().fg(accent),
    };
    (band(theme.danger, 52), band(theme.success, 22))
}

/// An edit's diff lines, numbered, capped at [`DIFF_PREVIEW_LINES`].
pub fn diff_rows(lines: &[DiffLine], width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    diff_rows_capped(lines, width, theme, glyphs, Some(DIFF_PREVIEW_LINES))
}

/// [`diff_rows`] with an explicit cap (`None` shows everything, for the transcript viewer).
pub fn diff_rows_capped(
    lines: &[DiffLine],
    width: usize,
    theme: &Theme,
    glyphs: &Glyphs,
    cap: Option<usize>,
) -> Vec<Line> {
    let (removed, added) = diff_styles(theme);
    let muted = Style::default().fg(theme.muted);
    let number_width = lines
        .iter()
        .filter_map(|l| l.number)
        .max()
        .map_or(1, |n| n.to_string().len());
    let shown = cap.map_or(lines.len(), |c| lines.len().min(c));
    let mut out = Vec::new();
    for line in &lines[..shown] {
        let number = line
            .number
            .map(|n| format!("{n:>number_width$}"))
            .unwrap_or_else(|| " ".repeat(number_width));
        let (sign, style, fill) = match line.kind {
            DiffLineKind::Removed => ("-", removed, Some(removed)),
            DiffLineKind::Added => ("+", added, Some(added)),
            DiffLineKind::Context => (" ", Style::default(), None),
            DiffLineKind::Gap => {
                out.push(Line::plain(
                    format!("{:>number_width$}", glyphs.vellipsis),
                    muted,
                ));
                continue;
            }
        };
        let number_style = match fill {
            Some(fill) => fill,
            None => muted,
        };
        let spans = vec![
            Span::new(format!("{number} "), number_style),
            Span::new(format!("{sign} "), style),
            Span::new(line.text.clone(), style),
        ];
        let mut row = Line::from_spans(truncate_spans(&spans, width, glyphs.ellipsis));
        if let Some(fill) = fill {
            row = row.with_fill(fill);
        }
        out.push(row);
    }
    if shown < lines.len() {
        out.push(Line::plain(expand_hint(lines.len() - shown, glyphs), muted));
    }
    out
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
    use crate::chat::diff::parse_unified;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.text().trim_end().to_string())
            .collect()
    }

    fn tool(status: ToolStatus, name: &str, target: &str, detail: Option<&str>) -> ToolCallView {
        ToolCallView {
            detail: detail.map(str::to_string),
            ..ToolCallView::new(status, name, target)
        }
    }

    #[test]
    fn tools_render_under_claude_code_names() {
        for (name, label) in [
            ("shell.run", "Bash"),
            ("test.run", "Bash"),
            ("build.run", "Bash"),
            ("fs.read", "Read"),
            ("fs.list", "List"),
            ("search.hybrid", "Search"),
            ("edit.apply_patch", "Update"),
            ("edit.write_file", "Update"),
            ("edit.create_file", "Write"),
            ("ticket.transition", "Ticket"),
            ("git.status", "git.status"),
        ] {
            assert_eq!(tool_label(name), label, "{name}");
        }
    }

    #[test]
    fn a_bash_call_shows_the_first_lines_and_how_many_more() {
        let theme = Theme::dark();
        let stdout: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        let view = ToolCallView::command("shell.run", "cargo test", 0, &stdout, "");
        let lines = tool_lines(&view, 80, &theme, &Glyphs::UNICODE);
        assert_eq!(
            texts(&lines),
            vec![
                "⏺ Bash(cargo test)",
                "  ⎿  line 1",
                "     line 2",
                "     line 3",
                "     line 4",
                "     … +6 lines (ctrl+o to expand)",
            ]
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.success));
    }

    #[test]
    fn a_command_with_no_output_says_so() {
        let theme = Theme::dark();
        let view = ToolCallView::command("shell.run", "true", 0, "", "\n");
        assert_eq!(
            texts(&tool_lines(&view, 80, &theme, &Glyphs::UNICODE)),
            vec!["⏺ Bash(true)", "  ⎿  (No content)"]
        );
    }

    #[test]
    fn a_failing_command_is_red_and_shows_the_tail() {
        let theme = Theme::dark();
        let stderr: String = (1..=6).map(|i| format!("err {i}\n")).collect();
        let view = ToolCallView::command("test.run", "pytest -q", 1, "", &stderr);
        let lines = tool_lines(&view, 80, &theme, &Glyphs::UNICODE);
        assert_eq!(
            texts(&lines),
            vec![
                "⏺ Bash(pytest -q)",
                "  ⎿  Error: Exit code 1",
                "     … +2 lines (ctrl+o to expand)",
                "     err 3",
                "     err 4",
                "     err 5",
                "     err 6",
            ]
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.danger));
        let error = lines[1]
            .spans
            .iter()
            .find(|s| s.text.contains("Exit code 1"))
            .expect("the error is on the ⎿ row");
        assert_eq!(error.style.fg, Some(theme.danger));
    }

    #[test]
    fn a_read_says_how_many_lines() {
        let theme = Theme::dark();
        let view = ToolCallView {
            body: ToolBody::Summary("Read 42 lines".to_string()),
            ..ToolCallView::new(ToolStatus::Ok, "fs.read", "src/calc.py")
        };
        assert_eq!(
            texts(&tool_lines(&view, 80, &theme, &Glyphs::UNICODE)),
            vec!["⏺ Read(src/calc.py)", "  ⎿  Read 42 lines"]
        );
    }

    #[test]
    fn an_update_shows_its_counts_and_a_numbered_coloured_diff() {
        let theme = Theme::dark();
        let lines = parse_unified(
            "--- a/calc.py\n+++ b/calc.py\n@@ -9,3 +9,4 @@\n def div(a, b):\n-    return a / b\n+    if b == 0:\n+        raise ValueError\n+    return a / b\n",
        );
        let view = ToolCallView {
            body: ToolBody::Diff {
                summary: "Updated calc.py with 3 additions and 1 removal".to_string(),
                lines,
            },
            ..ToolCallView::new(ToolStatus::Ok, "edit.apply_patch", "calc.py")
        };
        let rendered = tool_lines(&view, 80, &theme, &Glyphs::UNICODE);
        assert_eq!(
            texts(&rendered),
            vec![
                "⏺ Update(calc.py)",
                "  ⎿  Updated calc.py with 3 additions and 1 removal",
                "      9   def div(a, b):",
                "     10 -     return a / b",
                "     10 +     if b == 0:",
                "     11 +         raise ValueError",
                "     12 +     return a / b",
            ]
        );
        let (removed, added) = diff_styles(&theme);
        assert_eq!(rendered[3].fill, Some(removed));
        assert_eq!(rendered[4].fill, Some(added));
        assert_ne!(removed.bg, added.bg);
        assert!(removed.bg.is_some(), "a true-colour theme gets a band");
        assert_eq!(rendered[2].fill, None, "context lines are not banded");
        assert_eq!(rendered[3].fill_start, 5, "the band starts inside the hang");
    }

    #[test]
    fn long_diffs_are_capped_with_an_expand_hint() {
        let theme = Theme::dark();
        let diff = format!(
            "@@ -0,0 +1,30 @@\n{}",
            (1..=30).map(|i| format!("+l{i}\n")).collect::<String>()
        );
        let rows = diff_rows(&parse_unified(&diff), 60, &theme, &Glyphs::UNICODE);
        assert_eq!(rows.len(), DIFF_PREVIEW_LINES + 1);
        assert!(rows[DIFF_PREVIEW_LINES].text().contains("+10 lines"));
    }

    #[test]
    fn errors_and_denials_hang_under_the_elbow() {
        let theme = Theme::dark();
        let failed = tool_lines(
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
            texts(&failed),
            vec!["⏺ Update(calc.py)", "  ⎿  Error: patch did not apply"]
        );
        let denied = tool_lines(
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
        assert_eq!(
            texts(&denied),
            vec!["* Bash(rm -rf /)", "  L  Denied: not permitted"]
        );
    }

    #[test]
    fn long_headers_wrap_with_a_hang_and_are_capped() {
        let theme = Theme::dark();
        let long = "x ".repeat(200);
        let lines = tool_lines(
            &tool(ToolStatus::Ok, "shell.run", &long, None),
            40,
            &theme,
            &Glyphs::UNICODE,
        );
        assert_eq!(lines.len(), MAX_TOOL_ROWS);
        for line in &lines {
            assert!(line.width() <= 40, "{:?}", line.text());
        }
        assert!(lines[1].text().starts_with("  "));
        assert!(lines[MAX_TOOL_ROWS - 1].text().trim_end().ends_with('…'));
    }

    #[test]
    fn output_lines_never_wrap_or_overflow() {
        let theme = Theme::dark();
        let view = ToolCallView::command("shell.run", "cat", 0, &"y".repeat(200), "");
        for line in tool_lines(&view, 50, &theme, &Glyphs::UNICODE) {
            assert!(line.width() <= 50, "{:?}", line.text());
        }
    }

    #[test]
    fn sanitizing_strips_ansi_from_every_field() {
        let entry = Entry::Tool(ToolCallView {
            status: ToolStatus::Failed,
            name: "shell.run".to_string(),
            target: "echo\x1b[2J hi\nthere".to_string(),
            detail: Some("\x1b[31mred\x1b[0m".to_string()),
            body: ToolBody::Output(vec!["a\x1b[1mb\nc".to_string()]),
            input: "\x1b]0;title\x07x".to_string(),
            output: "\x1b[32mok\x1b[0m".to_string(),
        })
        .sanitized();
        let Entry::Tool(view) = entry else {
            panic!("still a tool entry");
        };
        assert_eq!(view.target, "echo hi there");
        assert_eq!(view.detail.as_deref(), Some("red"));
        assert_eq!(view.body, ToolBody::Output(vec!["ab".into(), "c".into()]));
        assert_eq!(view.input, "x");
        assert_eq!(view.output, "ok");
    }

    #[test]
    fn every_entry_is_its_own_block() {
        let theme = Theme::dark();
        let entries = [
            Entry::User("fix it".to_string()),
            Entry::Tool(tool(ToolStatus::Ok, "fs.read", "calc.py", None)),
            Entry::Tool(ToolCallView::command("shell.run", "pytest", 0, "ok\n", "")),
            Entry::Assistant("Done.".to_string()),
        ];
        let lines = layout_entries(entries.iter(), 60, &theme, &Glyphs::UNICODE);
        assert_eq!(
            texts(&lines),
            vec![
                " › fix it",
                "",
                "⏺ Read(calc.py)",
                "",
                "⏺ Bash(pytest)",
                "  ⎿  ok",
                "",
                "⏺ Done.",
            ]
        );
    }

    #[test]
    fn a_shell_command_sits_on_the_human_band_with_its_output_below() {
        let theme = Theme::dark();
        let running = ToolCallView {
            status: ToolStatus::Running,
            ..ToolCallView::new(ToolStatus::Running, "shell", "ls")
        };
        let mut t = Transcript::new();
        t.push(Entry::Shell(running));
        let lines = t.lines(40, &theme, &Glyphs::UNICODE).clone();
        assert_eq!(texts(&lines), vec![" ! ls", "  ⎿  Running…"]);
        assert!(t.finish_shell(ToolCallView::command("shell", "ls", 0, "a\nb\n", "")));
        let lines = t.lines(40, &theme, &Glyphs::UNICODE).clone();
        assert_eq!(texts(&lines), vec![" ! ls", "  ⎿  a", "     b"]);
        assert!(!t.finish_shell(ToolCallView::default()), "nothing running");
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
        assert_eq!(texts(&lines), vec!["⏺ Look:", "   x = 1"]);
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
            Entry::Tool(tool(
                ToolStatus::Failed,
                "shell.run",
                &"d".repeat(300),
                Some(&"e".repeat(300)),
            )),
            Entry::Shell(ToolCallView::command(
                "shell",
                &"f".repeat(300),
                0,
                &"中".repeat(300),
                "",
            )),
        ];
        for width in [8usize, 20, 80] {
            for line in layout_entries(entries.iter(), width, &theme, &Glyphs::UNICODE) {
                assert!(line.width() <= width, "{width}: {:?}", line.text());
            }
        }
    }
}
