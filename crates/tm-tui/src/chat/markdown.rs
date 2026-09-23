//! A lightweight Markdown renderer for assistant replies.
//!
//! Deliberately not CommonMark: a chat reply needs the handful of constructs models actually
//! produce — headings, emphasis, inline code, fenced code, lists, quotes, rules, links — rendered
//! legibly at a terminal's width, not a spec-complete document model. Source line breaks are kept
//! (a model's newline is almost always intentional in chat), and anything unrecognized renders as
//! its literal text rather than disappearing.
//!
//! Input is expected to be already [`crate::chat::sanitize::sanitize`]d.

use ratatui_core::style::{Color, Modifier, Style};

use crate::chat::glyphs::Glyphs;
use crate::chat::lines::{hard_wrap, truncate_spans, wrap, wrap_with_prefix, Line, Span};
use crate::text::display_width;
use crate::theme::Theme;

/// The style pair for a raised block (fenced code): explicit foreground on the theme's surface,
/// or the terminal's defaults when the surface is unavailable at this colour depth.
pub fn block_style(theme: &Theme) -> Style {
    if theme.surface == Color::Reset {
        Style::default()
    } else {
        Style::default().fg(theme.foreground).bg(theme.surface)
    }
}

/// Render `text` as Markdown into rows of at most `width` columns.
pub fn render(text: &str, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let width = width.max(4);
    let mut out: Vec<Line> = Vec::new();
    let source: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < source.len() {
        let line = source[i];
        let trimmed = line.trim_start();

        if let Some(fence) = fence_marker(trimmed) {
            let lang = trimmed[fence.len()..].trim();
            let mut body = Vec::new();
            i += 1;
            while i < source.len() && !source[i].trim_start().starts_with(fence) {
                body.push(source[i]);
                i += 1;
            }
            // Skip the closing fence (absent when the reply was cut off mid-block).
            i += 1;
            render_code_block(&mut out, lang, &body, width, theme);
            continue;
        }

        if trimmed.is_empty() {
            // Collapse runs of blank lines, and never open with one.
            if matches!(out.last(), Some(last) if !(last.spans.is_empty() && last.fill.is_none())) {
                out.push(Line::blank());
            }
            i += 1;
            continue;
        }

        if let Some((level, heading)) = heading(trimmed) {
            let mut style = Style::default().add_modifier(Modifier::BOLD);
            if level <= 2 {
                style = style.fg(theme.accent);
            }
            for row in wrap(&inline(heading, style, theme), width) {
                out.push(Line::from_spans(row));
            }
        } else if is_rule(trimmed) {
            out.push(Line::plain(
                glyphs
                    .rule
                    .repeat(width.min(40) / display_width(glyphs.rule).max(1)),
                Style::default().fg(theme.muted),
            ));
        } else if let Some(quote) = trimmed.strip_prefix('>') {
            let style = Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::ITALIC);
            let gutter = format!("{} ", glyphs.gutter);
            let indent = " ".repeat(display_width(&gutter));
            out.extend(wrap_with_prefix(
                &inline(quote.trim_start(), style, theme),
                width,
                vec![Span::new(gutter, Style::default().fg(theme.muted))],
                &indent,
            ));
        } else if let Some(item) = list_item(line) {
            let depth_indent = "  ".repeat(item.depth.min(6));
            let marker = match item.number {
                Some(n) => format!("{n}. "),
                None => format!("{} ", glyphs.bullet),
            };
            let prefix_text = format!("{depth_indent}{marker}");
            let hang = " ".repeat(display_width(&prefix_text));
            let prefix = vec![
                Span::new(depth_indent, Style::default()),
                Span::new(marker, Style::default().fg(theme.muted)),
            ];
            out.extend(wrap_with_prefix(
                &inline(item.text, Style::default(), theme),
                width,
                prefix,
                &hang,
            ));
        } else if trimmed.starts_with('|') {
            // Tables keep their own column alignment; wrapping would destroy it, so each row is
            // shown verbatim and cut at the edge.
            let style = if is_table_separator(trimmed) {
                Style::default().fg(theme.muted)
            } else {
                Style::default()
            };
            out.push(Line::from_spans(truncate_spans(
                &[Span::new(line.trim_end(), style)],
                width,
                glyphs.ellipsis,
            )));
        } else {
            for row in wrap(&inline(line.trim_end(), Style::default(), theme), width) {
                out.push(Line::from_spans(row));
            }
        }
        i += 1;
    }

    while matches!(out.last(), Some(last) if last.spans.is_empty() && last.fill.is_none()) {
        out.pop();
    }
    out
}

/// A fenced code block: every row filled with [`block_style`], one column of padding each side,
/// hard-wrapped rather than word-wrapped, with the language (if any) as a muted label row.
fn render_code_block(out: &mut Vec<Line>, lang: &str, body: &[&str], width: usize, theme: &Theme) {
    let style = block_style(theme);
    let inner = width.saturating_sub(2).max(1);
    if !lang.is_empty() {
        let label_style = style.fg(theme.muted);
        out.push(Line::plain(format!(" {lang}"), label_style).with_fill(style));
    }
    if body.is_empty() {
        out.push(Line::blank().with_fill(style));
    }
    for source in body {
        for row in hard_wrap(source.trim_end(), inner) {
            out.push(Line::plain(format!(" {row}"), style).with_fill(style));
        }
    }
}

/// The fence string (```` ``` ```` or `~~~`) a line opens a code block with, if it does.
fn fence_marker(trimmed: &str) -> Option<&'static str> {
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// `# Title` → `(1, "Title")`, for levels 1-6 with a space after the hashes.
fn heading(trimmed: &str) -> Option<(usize, &str)> {
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&level) {
        let rest = &trimmed[level..];
        if let Some(text) = rest.strip_prefix(' ') {
            return Some((level, text.trim()));
        }
    }
    None
}

/// `---`, `***`, `___` (optionally spaced), three or more.
fn is_rule(trimmed: &str) -> bool {
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && (compact.chars().all(|c| c == '-')
            || compact.chars().all(|c| c == '*')
            || compact.chars().all(|c| c == '_'))
}

fn is_table_separator(trimmed: &str) -> bool {
    trimmed
        .chars()
        .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '+'))
}

/// One parsed list item.
struct ListItem<'a> {
    depth: usize,
    number: Option<&'a str>,
    text: &'a str,
}

/// `  - item`, `* item`, `+ item`, `3. item`, `3) item`.
fn list_item(line: &str) -> Option<ListItem<'_>> {
    let leading = line.len() - line.trim_start().len();
    let trimmed = &line[leading..];
    let depth = leading / 2;
    for marker in ["- ", "* ", "+ "] {
        if let Some(text) = trimmed.strip_prefix(marker) {
            return Some(ListItem {
                depth,
                number: None,
                text,
            });
        }
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 3 {
        let rest = &trimmed[digits..];
        if let Some(text) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some(ListItem {
                depth,
                number: Some(&trimmed[..digits]),
                text,
            });
        }
    }
    None
}

/// Parse inline Markdown (`**bold**`, `*italic*`, `` `code` ``, `[text](url)`, backslash
/// escapes) into styled spans layered over `base`.
pub fn inline(text: &str, base: Style, theme: &Theme) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let mut bold = false;
    let mut italic = false;
    let code_style = Style::default().fg(theme.accent);

    let style_of = |bold: bool, italic: bool| {
        let mut s = base;
        if bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        if italic {
            s = s.add_modifier(Modifier::ITALIC);
        }
        s
    };
    let flush = |spans: &mut Vec<Span>, buf: &mut String, style: Style| {
        if !buf.is_empty() {
            spans.push(Span::new(std::mem::take(buf), style));
        }
    };

    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let prev = if i == 0 { None } else { Some(chars[i - 1]) };
        let next = chars.get(i + 1).copied();

        // Backslash escape of ASCII punctuation.
        if c == '\\' {
            if let Some(n) = next {
                if n.is_ascii_punctuation() {
                    buf.push(n);
                    i += 2;
                    continue;
                }
            }
        }

        // Code span: a run of N backticks closed by the next run of exactly N.
        if c == '`' {
            let run = chars[i..].iter().take_while(|c| **c == '`').count();
            if let Some(close) = find_backtick_run(&chars, i + run, run) {
                flush(&mut spans, &mut buf, style_of(bold, italic));
                let content: String = chars[i + run..close].iter().collect();
                let content =
                    if content.len() > 2 && content.starts_with(' ') && content.ends_with(' ') {
                        content[1..content.len() - 1].to_string()
                    } else {
                        content
                    };
                spans.push(Span::new(content, code_style));
                i = close + run;
                continue;
            }
            // Unmatched: literal backticks.
            for _ in 0..run {
                buf.push('`');
            }
            i += run;
            continue;
        }

        // Strong emphasis: `**` / `__`.
        if (c == '*' || c == '_') && next == Some(c) {
            let opening_ok = !bold
                && chars.get(i + 2).is_some_and(|n| !n.is_whitespace())
                && (c == '*' || !prev.is_some_and(char::is_alphanumeric))
                && has_closing(&chars, i + 2, &[c, c]);
            let closing_ok = bold && prev.is_some_and(|p| !p.is_whitespace());
            if opening_ok || closing_ok {
                flush(&mut spans, &mut buf, style_of(bold, italic));
                bold = !bold;
                i += 2;
                continue;
            }
        }

        // Emphasis: `*` / `_`.
        if c == '*' || c == '_' {
            let opening_ok = !italic
                && next.is_some_and(|n| !n.is_whitespace() && n != c)
                && (c == '*' || !prev.is_some_and(char::is_alphanumeric))
                && has_closing(&chars, i + 1, &[c]);
            let closing_ok = italic
                && prev.is_some_and(|p| !p.is_whitespace())
                && (c == '*' || !next.is_some_and(char::is_alphanumeric));
            if opening_ok || closing_ok {
                flush(&mut spans, &mut buf, style_of(bold, italic));
                italic = !italic;
                i += 1;
                continue;
            }
        }

        // Link: `[text](url)`.
        if c == '[' {
            if let Some((label, url, end)) = parse_link(&chars, i) {
                flush(&mut spans, &mut buf, style_of(bold, italic));
                spans.push(Span::new(
                    label.clone(),
                    style_of(bold, italic).add_modifier(Modifier::UNDERLINED),
                ));
                if url != label && !url.is_empty() {
                    spans.push(Span::new(
                        format!(" ({url})"),
                        Style::default().fg(theme.muted),
                    ));
                }
                i = end;
                continue;
            }
        }

        buf.push(c);
        i += 1;
    }
    flush(&mut spans, &mut buf, style_of(bold, italic));
    spans
}

/// The index of the next run of exactly `len` backticks at or after `from`.
fn find_backtick_run(chars: &[char], from: usize, len: usize) -> Option<usize> {
    let mut j = from;
    while j < chars.len() {
        if chars[j] == '`' {
            let run = chars[j..].iter().take_while(|c| **c == '`').count();
            if run == len {
                return Some(j);
            }
            j += run;
        } else {
            j += 1;
        }
    }
    None
}

/// Whether `marker` occurs at or after `from` preceded by a non-whitespace character.
fn has_closing(chars: &[char], from: usize, marker: &[char]) -> bool {
    let m = marker.len();
    if chars.len() < m {
        return false;
    }
    (from + 1..=chars.len() - m)
        .any(|j| chars[j..j + m] == *marker && !chars[j - 1].is_whitespace())
}

/// `[label](url)` starting at `start`; returns the label, url, and the index after `)`.
fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let close_label = (start + 1..chars.len()).find(|&j| chars[j] == ']')?;
    if chars.get(close_label + 1) != Some(&'(') {
        return None;
    }
    let close_url = (close_label + 2..chars.len()).find(|&j| chars[j] == ')')?;
    let label: String = chars[start + 1..close_label].iter().collect();
    let url: String = chars[close_label + 2..close_url].iter().collect();
    if label.is_empty() || url.contains(' ') {
        return None;
    }
    Some((label, url, close_url + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::dark()
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::text).collect()
    }

    fn find_span<'a>(lines: &'a [Line], text: &str) -> &'a Span {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.text.contains(text))
            .unwrap_or_else(|| panic!("no span containing {text:?} in {:?}", texts(lines)))
    }

    #[test]
    fn bold_and_inline_code_get_their_own_styles() {
        let t = theme();
        let lines = render(
            "Run **cargo test** then `mise run verify`.",
            80,
            &t,
            &Glyphs::UNICODE,
        );
        assert_eq!(texts(&lines), vec!["Run cargo test then mise run verify."]);
        assert!(find_span(&lines, "cargo test")
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(
            find_span(&lines, "mise run verify").style.fg,
            Some(t.accent)
        );
    }

    #[test]
    fn italic_needs_a_closing_marker_and_ignores_snake_case() {
        let t = theme();
        let lines = render(
            "an *emphasis* and snake_case_name and 2 * 3",
            80,
            &t,
            &Glyphs::UNICODE,
        );
        assert_eq!(
            texts(&lines),
            vec!["an emphasis and snake_case_name and 2 * 3"]
        );
        assert!(find_span(&lines, "emphasis")
            .style
            .add_modifier
            .contains(Modifier::ITALIC));
        assert!(!find_span(&lines, "snake_case_name")
            .style
            .add_modifier
            .contains(Modifier::ITALIC));
    }

    #[test]
    fn fenced_code_is_filled_unwrapped_and_labelled() {
        let t = theme();
        let md = "Here:\n```python\ndef add(a, b):\n    return a + b\n```\nDone.";
        let lines = render(md, 40, &t, &Glyphs::UNICODE);
        let text = texts(&lines);
        assert_eq!(text[0], "Here:");
        assert_eq!(text[1], " python");
        assert_eq!(text[2], " def add(a, b):");
        assert_eq!(text[3], "     return a + b");
        assert_eq!(text[4], "Done.");
        for code in &lines[1..4] {
            assert_eq!(code.fill, Some(block_style(&t)));
        }
        assert_eq!(lines[4].fill, None);
    }

    #[test]
    fn long_code_lines_hard_wrap_inside_the_block() {
        let t = theme();
        let md = format!("```\n{}\n```", "x".repeat(30));
        let lines = render(&md, 12, &t, &Glyphs::UNICODE);
        assert!(lines.len() >= 3);
        for line in &lines {
            assert!(line.width() <= 12);
            assert!(line.fill.is_some());
        }
    }

    #[test]
    fn an_unterminated_fence_still_renders_its_body() {
        let t = theme();
        let lines = render("```\nlet x = 1;", 40, &t, &Glyphs::UNICODE);
        assert_eq!(texts(&lines), vec![" let x = 1;"]);
    }

    #[test]
    fn lists_get_markers_and_hanging_indents() {
        let t = theme();
        let md = "- first item that is long enough to wrap around\n  - nested\n2. numbered";
        let lines = render(md, 24, &t, &Glyphs::UNICODE);
        let text = texts(&lines);
        assert!(text[0].starts_with("• first"));
        assert!(text[1].starts_with("  "), "continuation hangs: {text:?}");
        assert!(text.iter().any(|l| l.starts_with("  • nested")));
        assert!(text.iter().any(|l| l.starts_with("2. numbered")));
        for line in &lines {
            assert!(line.width() <= 24);
        }
    }

    #[test]
    fn headings_are_bold_and_hashes_disappear() {
        let t = theme();
        let lines = render("## Plan\nbody", 40, &t, &Glyphs::UNICODE);
        assert_eq!(texts(&lines), vec!["Plan", "body"]);
        let style = lines[0].spans[0].style;
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(style.fg, Some(t.accent));
    }

    #[test]
    fn quotes_rules_and_links() {
        let t = theme();
        let lines = render(
            "> note this\n---\nsee [docs](https://x.dev)",
            40,
            &t,
            &Glyphs::UNICODE,
        );
        let text = texts(&lines);
        assert_eq!(text[0], "│ note this");
        assert!(text[1].starts_with("───"));
        assert_eq!(text[2], "see docs (https://x.dev)");
        assert!(find_span(&lines, "docs")
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED));
    }

    #[test]
    fn repeated_blank_lines_collapse_and_trailing_blanks_trim() {
        let t = theme();
        let lines = render("a\n\n\n\nb\n\n", 40, &t, &Glyphs::UNICODE);
        assert_eq!(texts(&lines), vec!["a", "", "b"]);
    }

    #[test]
    fn prose_soft_wraps_to_width() {
        let t = theme();
        let lines = render(&"word ".repeat(40), 30, &t, &Glyphs::UNICODE);
        assert!(lines.len() > 5);
        for line in &lines {
            assert!(line.width() <= 30, "{:?}", line.text());
        }
    }

    #[test]
    fn unmatched_markers_render_literally() {
        let t = theme();
        let lines = render("a ` b ** c", 40, &t, &Glyphs::UNICODE);
        assert_eq!(texts(&lines), vec!["a ` b ** c"]);
    }

    #[test]
    fn escapes_suppress_formatting() {
        let t = theme();
        let lines = render(r"\*not italic\*", 40, &t, &Glyphs::UNICODE);
        assert_eq!(texts(&lines), vec!["*not italic*"]);
    }
}
