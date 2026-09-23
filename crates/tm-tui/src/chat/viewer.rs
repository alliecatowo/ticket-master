//! The transcript viewer (Ctrl+O): the whole conversation full screen, with every tool call's
//! real name, full input and full output — what the compact transcript summarizes as
//! `… +N lines (ctrl+o to expand)`. Scrolls with ↑/↓/PgUp/PgDn/g/G; `q`, Esc, Ctrl+C or Ctrl+O
//! close it.

use std::cell::{Cell, RefCell};

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};

use crate::chat::glyphs::Glyphs;
use crate::chat::lines::{clear, draw_line, hard_wrap, truncate_spans, Line, Span};
use crate::chat::transcript::{
    diff_rows_capped, entry_lines, tool_label, Entry, ToolBody, ToolCallView,
};
use crate::theme::Theme;

/// The viewer's state: where it is scrolled to, and its layout cache.
#[derive(Debug, Default)]
pub struct Viewer {
    /// The first visible row; `None` pins the view to the bottom.
    top: Cell<Option<usize>>,
    /// `(width, version, lines)` of the last layout.
    cache: RefCell<Option<(usize, u64, Vec<Line>)>>,
    /// `(total rows, visible rows)` at the last render.
    last_view: Cell<(usize, usize)>,
}

impl Viewer {
    /// A viewer opened at the newest content.
    pub fn new() -> Self {
        Viewer::default()
    }

    fn current_top(&self) -> usize {
        let (total, height) = self.last_view.get();
        let bottom = total.saturating_sub(height);
        self.top.get().map_or(bottom, |t| t.min(bottom))
    }

    /// Scroll by `delta` rows (negative is up).
    pub fn scroll(&self, delta: isize) {
        let (total, height) = self.last_view.get();
        let bottom = total.saturating_sub(height);
        let top = (self.current_top() as isize + delta).clamp(0, bottom as isize) as usize;
        self.top.set(if top >= bottom { None } else { Some(top) });
    }

    /// Scroll by a page (`1` down, `-1` up).
    pub fn page(&self, direction: isize) {
        let (_, height) = self.last_view.get();
        self.scroll(direction * height.saturating_sub(2).max(1) as isize);
    }

    /// Jump to the top.
    pub fn home(&self) {
        self.top.set(Some(0));
    }

    /// Jump to the bottom.
    pub fn end(&self) {
        self.top.set(None);
    }

    /// Draw the viewer over all of `area`. `version` must change whenever `entries` does.
    pub fn render<'a>(
        &self,
        area: Rect,
        buf: &mut Buffer,
        entries: impl Iterator<Item = &'a Entry>,
        version: u64,
        theme: &Theme,
        glyphs: &Glyphs,
    ) {
        if area.width < 10 || area.height < 3 {
            return;
        }
        clear(buf, area);
        let muted = Style::default().fg(theme.muted);
        let body = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height - 2,
        );
        let width = body.width as usize;
        let stale = !matches!(&*self.cache.borrow(), Some((w, v, _)) if *w == width && *v == version);
        if stale {
            *self.cache.borrow_mut() = Some((
                width,
                version,
                viewer_lines(entries, width, theme, glyphs),
            ));
        }
        let cache = self.cache.borrow();
        let lines: &[Line] = cache.as_ref().map_or(&[], |(_, _, l)| l.as_slice());
        self.last_view.set((lines.len(), body.height as usize));
        let top = self.current_top();
        for (row, line) in lines.iter().skip(top).take(body.height as usize).enumerate() {
            draw_line(buf, body.x, body.y + row as u16, body.width, line);
        }

        let title = Line::from_spans(vec![
            Span::new(
                " Transcript ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::new(
                format!(
                    "{}{} of {} rows",
                    glyphs.sep.trim_start(),
                    (top + body.height as usize).min(lines.len()),
                    lines.len()
                ),
                muted,
            ),
        ]);
        draw_line(buf, area.x, area.y, area.width, &title);
        let footer = format!(
            " {} scroll{}pgup/pgdn page{}g/G top/bottom{}q/esc close ",
            glyphs.updown, glyphs.sep, glyphs.sep, glyphs.sep
        );
        let footer = Line::from_spans(truncate_spans(
            &[Span::new(footer, muted)],
            area.width as usize,
            glyphs.ellipsis,
        ));
        draw_line(buf, area.x, area.y + area.height - 1, area.width, &footer);
    }
}

/// Every entry in full, for the viewer.
pub fn viewer_lines<'a>(
    entries: impl Iterator<Item = &'a Entry>,
    width: usize,
    theme: &Theme,
    glyphs: &Glyphs,
) -> Vec<Line> {
    let mut out = Vec::new();
    for (i, entry) in entries.enumerate() {
        if i > 0 {
            out.push(Line::blank());
        }
        match entry {
            Entry::Tool(view) | Entry::Shell(view) => {
                out.extend(full_tool_lines(view, width, theme, glyphs))
            }
            other => out.extend(entry_lines(other, width, theme, glyphs)),
        }
    }
    if out.is_empty() {
        out.push(Line::plain(
            "Nothing yet.",
            Style::default().fg(theme.muted),
        ));
    }
    out
}

fn full_tool_lines(view: &ToolCallView, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let muted = Style::default().fg(theme.muted);
    let heading = Style::default()
        .fg(theme.muted)
        .add_modifier(Modifier::BOLD);
    let status = crate::chat::transcript::tool_lines(
        &ToolCallView {
            body: ToolBody::None,
            detail: None,
            ..view.clone()
        },
        width,
        theme,
        glyphs,
    );
    let mut out = Vec::new();
    // The header (`⏺ Bash(cmd)`), then the real tool name so nothing is hidden by the label.
    out.extend(status.into_iter().take(3));
    if tool_label(&view.name) != view.name && view.name != "shell" {
        out.push(Line::plain(format!("  {}", view.name), muted));
    }
    if let Some(detail) = &view.detail {
        for row in hard_wrap(detail, width.saturating_sub(2)) {
            out.push(Line::plain(format!("  {row}"), muted));
        }
    }
    let block = |out: &mut Vec<Line>, title: &str, text: &str| {
        if text.trim().is_empty() {
            return;
        }
        out.push(Line::plain(format!("  {title}"), heading));
        for source in text.lines() {
            for row in hard_wrap(source, width.saturating_sub(4).max(1)) {
                out.push(Line::plain(format!("    {row}"), Style::default()));
            }
        }
    };
    if let ToolBody::Diff { summary, lines } = &view.body {
        out.push(Line::plain(format!("  {summary}"), heading));
        for line in diff_rows_capped(lines, width.saturating_sub(4), theme, glyphs, None) {
            out.push(line.prefixed(vec![Span::new("    ", Style::default())]));
        }
    }
    block(&mut out, "Input", &view.input);
    block(&mut out, "Output", &view.output);
    for line in &mut out {
        if line.width() > width {
            line.spans = truncate_spans(&line.spans, width, glyphs.ellipsis);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::transcript::ToolStatus;

    fn render(viewer: &Viewer, entries: &[Entry], width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        viewer.render(
            area,
            &mut buf,
            entries.iter(),
            1,
            &Theme::dark(),
            &Glyphs::UNICODE,
        );
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
    fn it_shows_every_line_of_output_and_the_real_tool_name() {
        let stdout: String = (1..=30).map(|i| format!("out {i}\n")).collect();
        let mut view = ToolCallView::command("shell.run", "cargo test", 0, &stdout, "");
        view.input = "{\n  \"command\": \"cargo test\"\n}".to_string();
        let entries = vec![Entry::User("go".into()), Entry::Tool(view)];
        let viewer = Viewer::new();
        viewer.home();
        let text = render(&viewer, &entries, 60, 60).join("\n");
        assert!(text.contains("⏺ Bash(cargo test)"), "{text}");
        assert!(text.contains("shell.run"), "{text}");
        assert!(text.contains("\"command\": \"cargo test\""), "{text}");
        assert!(text.contains("out 1\n") || text.contains("out 1"), "{text}");
        assert!(text.contains("out 25"), "{text}");
        assert!(text.contains("q/esc close"), "{text}");
    }

    #[test]
    fn it_opens_at_the_bottom_and_scrolls() {
        let entries: Vec<Entry> = (0..40)
            .map(|i| Entry::Assistant(format!("message {i}")))
            .collect();
        let viewer = Viewer::new();
        let rows = render(&viewer, &entries, 40, 12);
        assert!(rows.iter().any(|r| r.contains("message 39")), "{rows:?}");
        viewer.home();
        let rows = render(&viewer, &entries, 40, 12);
        assert!(rows.iter().any(|r| r.contains("message 0")), "{rows:?}");
        viewer.page(1);
        let rows = render(&viewer, &entries, 40, 12);
        assert!(!rows.iter().any(|r| r.contains("message 0 ")));
        viewer.end();
        let rows = render(&viewer, &entries, 40, 12);
        assert!(rows.iter().any(|r| r.contains("message 39")));
    }

    #[test]
    fn nothing_overflows_even_with_wide_characters() {
        let view = ToolCallView {
            output: "中".repeat(400),
            input: "x".repeat(300),
            detail: Some("y".repeat(300)),
            ..ToolCallView::new(ToolStatus::Failed, "shell.run", "z".repeat(200))
        };
        for width in [10usize, 33, 80] {
            for line in viewer_lines(
                [Entry::Tool(view.clone())].iter(),
                width,
                &Theme::dark(),
                &Glyphs::UNICODE,
            ) {
                assert!(line.width() <= width, "{width}: {:?}", line.text());
            }
        }
    }
}
