//! An owned, styled line and the small drawing helpers every chat view shares.
//!
//! Transcript rendering computes lines once (and caches them) and draws them many times, so the
//! line type owns its text rather than borrowing it, unlike [`crate::text::StyledSpan`].

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::Style;

use crate::chat::glyphs::Border;
use crate::text::{display_width, graphemes, truncate, wrap_spans, StyledSpan};

/// One run of text sharing a style.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    /// The run's text. Never contains a newline.
    pub text: String,
    /// The run's style.
    pub style: Style,
}

impl Span {
    /// A span of `text` in `style`.
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Span {
            text: text.into(),
            style,
        }
    }
}

/// One display row: a sequence of spans, plus an optional style painted across the row (from
/// `fill_start` to the right edge) before the spans — a code block's background band.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Line {
    /// The row's content, left to right.
    pub spans: Vec<Span>,
    /// When set, the row is filled with this style first.
    pub fill: Option<Style>,
    /// The column (relative to the row's start) the fill begins at, so a band can sit inside an
    /// indent instead of bleeding under it.
    pub fill_start: u16,
}

impl Line {
    /// An empty row.
    pub fn blank() -> Self {
        Line::default()
    }

    /// A row of spans with no fill.
    pub fn from_spans(spans: Vec<Span>) -> Self {
        Line {
            spans,
            fill: None,
            fill_start: 0,
        }
    }

    /// This row with `prefix` spans prepended; a fill shifts right with the content.
    pub fn prefixed(mut self, prefix: Vec<Span>) -> Self {
        let width: usize = prefix.iter().map(|s| display_width(&s.text)).sum();
        self.fill_start = self.fill_start.saturating_add(width as u16);
        let mut spans = prefix;
        spans.append(&mut self.spans);
        self.spans = spans;
        self
    }

    /// True for a row with no content and no fill.
    pub fn is_blank(&self) -> bool {
        self.fill.is_none() && self.spans.iter().all(|s| s.text.is_empty())
    }

    /// A row holding one span.
    pub fn plain(text: impl Into<String>, style: Style) -> Self {
        Line::from_spans(vec![Span::new(text, style)])
    }

    /// This row with `fill` painted across its full width.
    pub fn with_fill(mut self, fill: Style) -> Self {
        self.fill = Some(fill);
        self
    }

    /// The row's text with styles discarded, for tests and plain-text assertions.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// The row's rendered width in columns.
    pub fn width(&self) -> usize {
        self.spans.iter().map(|s| display_width(&s.text)).sum()
    }
}

/// Draw `line` at `(x, y)`, clipped to `width` columns. Returns the column after the last cell
/// written.
pub fn draw_line(buf: &mut Buffer, x: u16, y: u16, width: u16, line: &Line) -> u16 {
    if width == 0 {
        return x;
    }
    if let Some(fill) = line.fill {
        let start = line.fill_start.min(width);
        buf.set_style(Rect::new(x + start, y, width - start, 1), fill);
    }
    draw_spans(buf, x, y, width, &line.spans)
}

/// Draw `spans` left to right from `(x, y)`, never writing past `x + width`.
pub fn draw_spans(buf: &mut Buffer, x: u16, y: u16, width: u16, spans: &[Span]) -> u16 {
    let end = x.saturating_add(width);
    let mut col = x;
    for span in spans {
        if col >= end {
            break;
        }
        let remaining = (end - col) as usize;
        let (next, _) = buf.set_stringn(col, y, &span.text, remaining, span.style);
        col = next;
    }
    col
}

/// Word-wrap `spans` to `width` columns, keeping each fragment's style.
pub fn wrap(spans: &[Span], width: usize) -> Vec<Vec<Span>> {
    let width = width.max(1);
    let borrowed: Vec<StyledSpan<'_>> = spans
        .iter()
        .map(|s| StyledSpan::new(s.text.as_str(), s.style))
        .collect();
    wrap_spans(&borrowed, width)
        .into_iter()
        .map(|line| {
            let mut out: Vec<Span> = Vec::new();
            for fragment in line {
                match out.last_mut() {
                    Some(last) if last.style == fragment.style => last.text.push_str(fragment.text),
                    _ => out.push(Span::new(fragment.text, fragment.style)),
                }
            }
            out
        })
        .collect()
}

/// Wrap `spans` to `width` with a first-line prefix and a hanging indent for every following line.
/// `prefix` and `indent` must have the same display width.
pub fn wrap_with_prefix(
    spans: &[Span],
    width: usize,
    prefix: Vec<Span>,
    indent: &str,
) -> Vec<Line> {
    let prefix_width: usize = prefix.iter().map(|s| display_width(&s.text)).sum();
    let body_width = width.saturating_sub(prefix_width).max(1);
    let wrapped = wrap(spans, body_width);
    if wrapped.is_empty() {
        return vec![Line::from_spans(prefix)];
    }
    wrapped
        .into_iter()
        .enumerate()
        .map(|(i, body)| {
            let mut row = if i == 0 {
                prefix.clone()
            } else {
                vec![Span::new(indent, Style::default())]
            };
            row.extend(body);
            Line::from_spans(row)
        })
        .collect()
}

/// Break `text` into rows of at most `width` columns at grapheme boundaries (no word wrapping),
/// for code where every character matters.
pub fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut row_width = 0;
    for g in graphemes(text) {
        if row_width > 0 && row_width + g.width > width {
            rows.push(std::mem::take(&mut row));
            row_width = 0;
        }
        row.push_str(g.text);
        row_width += g.width;
    }
    rows.push(row);
    rows
}

/// Truncate a line's spans to `width` columns, ending in `ellipsis` when anything was cut.
pub fn truncate_spans(spans: &[Span], width: usize, ellipsis: &str) -> Vec<Span> {
    let total: usize = spans.iter().map(|s| display_width(&s.text)).sum();
    if total <= width {
        return spans.to_vec();
    }
    let budget = width.saturating_sub(display_width(ellipsis));
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let w = display_width(&span.text);
        if used + w <= budget {
            out.push(span.clone());
            used += w;
            continue;
        }
        let room = budget - used;
        let cut = truncate(&span.text, room, "");
        let style = span.style;
        if !cut.is_empty() {
            out.push(Span::new(cut, style));
        }
        out.push(Span::new(ellipsis, style));
        return out;
    }
    out
}

/// Draw a one-cell box border around `area` in `style`.
pub fn draw_box(buf: &mut Buffer, area: Rect, border: &Border, style: Style) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let right = area.x + area.width - 1;
    let bottom = area.y + area.height - 1;
    buf.set_string(area.x, area.y, border.top_left, style);
    buf.set_string(right, area.y, border.top_right, style);
    buf.set_string(area.x, bottom, border.bottom_left, style);
    buf.set_string(right, bottom, border.bottom_right, style);
    for x in (area.x + 1)..right {
        buf.set_string(x, area.y, border.horizontal, style);
        buf.set_string(x, bottom, border.horizontal, style);
    }
    for y in (area.y + 1)..bottom {
        buf.set_string(area.x, y, border.vertical, style);
        buf.set_string(right, y, border.vertical, style);
    }
}

/// Blank every cell in `area` (an overlay clearing what is beneath it before drawing).
pub fn clear(buf: &mut Buffer, area: Rect) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_core::style::{Color, Modifier};

    #[test]
    fn wrap_merges_adjacent_same_style_fragments() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let rows = wrap(
            &[
                Span::new("one two ", Style::default()),
                Span::new("three", bold),
            ],
            40,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 2, "one run per style, not one per word");
    }

    #[test]
    fn wrap_with_prefix_hangs_continuation_lines() {
        let rows = wrap_with_prefix(
            &[Span::new("alpha beta gamma delta", Style::default())],
            12,
            vec![Span::new("- ", Style::default())],
            "  ",
        );
        assert!(rows.len() > 1);
        assert!(rows[0].text().starts_with("- "));
        for row in &rows[1..] {
            assert!(row.text().starts_with("  "));
        }
        for row in &rows {
            assert!(row.width() <= 12, "{:?} is wider than 12", row.text());
        }
    }

    #[test]
    fn hard_wrap_breaks_mid_word() {
        assert_eq!(hard_wrap("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(hard_wrap("", 4), vec![""]);
    }

    #[test]
    fn truncate_spans_respects_width_and_marks_the_cut() {
        let red = Style::default().fg(Color::Red);
        let spans = vec![
            Span::new("hello ", Style::default()),
            Span::new("world", red),
        ];
        let cut = truncate_spans(&spans, 8, "…");
        let text: String = cut.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(text, "hello w…");
        assert!(display_width(&text) <= 8);
        let untouched = truncate_spans(&spans, 40, "…");
        assert_eq!(untouched, spans);
    }

    #[test]
    fn draw_line_clips_to_width() {
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(area);
        draw_line(&mut buf, 0, 0, 3, &Line::plain("abcdef", Style::default()));
        let row: String = (0..5).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "abc  ");
    }
}
