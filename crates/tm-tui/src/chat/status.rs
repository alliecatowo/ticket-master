//! The chat status bar: a single row of prioritized segments that degrades by dropping the least
//! important ones first when the terminal is narrow, rather than wrapping or clipping mid-word.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};

use crate::chat::glyphs::Glyphs;
use crate::chat::lines::{draw_spans, truncate_spans, Span};
use crate::text::display_width;
use crate::theme::Theme;

/// What the session is doing right now, shown as the bar's leading glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TurnState {
    /// Waiting for input; the last turn (if any) ended normally.
    #[default]
    Idle,
    /// A turn is running.
    Running,
    /// The last turn ended in an error or failure.
    Failed,
}

/// The facts the status bar shows, supplied by the application.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusInfo {
    /// The model that most recently answered, else the configured one (`provider/model`).
    pub model: String,
    /// The working directory, already shortened for display (`~/src/app`).
    pub cwd: String,
    /// The checked-out git branch, when the directory is a repository.
    pub branch: Option<String>,
    /// Set when the project's state lives outside the repo (D-003 global scope).
    pub global_scope: bool,
    /// The attached ticket's id, if any.
    pub ticket: Option<String>,
    /// How many tickets are not closed or cancelled.
    pub open_tickets: usize,
    /// Tokens spent by this session so far.
    pub tokens: u64,
}

/// One status-bar segment.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// Higher survives longer when space runs out.
    pub priority: u8,
    /// The segment's content.
    pub spans: Vec<Span>,
    /// Whether this segment may be truncated (with an ellipsis) instead of dropped.
    pub shrinkable: bool,
}

/// `12`, `1.2k`, `3.4M` — compact token counts.
pub fn format_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => format!("{tokens}"),
        1_000..=999_999 => format!("{:.1}k", tokens as f64 / 1_000.0),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.0),
    }
}

/// Build the bar's segments, most important first in display order.
pub fn segments(
    info: &StatusInfo,
    state: TurnState,
    spinner_frame: &str,
    theme: &Theme,
    glyphs: &Glyphs,
) -> Vec<Segment> {
    let muted = Style::default().fg(theme.muted);
    let (glyph, glyph_style) = match state {
        TurnState::Idle => (glyphs.dot, Style::default().fg(theme.success)),
        TurnState::Running => (spinner_frame, Style::default().fg(theme.accent)),
        TurnState::Failed => (glyphs.dot, Style::default().fg(theme.danger)),
    };
    let mut out = vec![Segment {
        priority: 100,
        spans: vec![
            Span::new(glyph, glyph_style),
            Span::new(" ", Style::default()),
            Span::new(info.model.clone(), Style::default().fg(theme.accent)),
        ],
        shrinkable: true,
    }];

    let mut place = vec![Span::new(info.cwd.clone(), muted)];
    if let Some(branch) = &info.branch {
        place.push(Span::new(format!(" ({branch})"), muted));
    }
    out.push(Segment {
        priority: 60,
        spans: place,
        shrinkable: true,
    });

    if info.global_scope {
        out.push(Segment {
            priority: 20,
            spans: vec![Span::new(
                "global scope",
                Style::default().fg(theme.warning),
            )],
            shrinkable: false,
        });
    }

    out.push(Segment {
        priority: 80,
        spans: vec![match &info.ticket {
            Some(ticket) => Span::new(
                ticket.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            None => Span::new("no ticket", muted),
        }],
        shrinkable: false,
    });

    if info.open_tickets > 0 {
        out.push(Segment {
            priority: 30,
            spans: vec![Span::new(format!("{} open", info.open_tickets), muted)],
            shrinkable: false,
        });
    }

    if info.tokens > 0 {
        out.push(Segment {
            priority: 40,
            spans: vec![Span::new(
                format!("{} tokens", format_tokens(info.tokens)),
                muted,
            )],
            shrinkable: false,
        });
    }
    out
}

fn segment_width(segment: &Segment) -> usize {
    segment.spans.iter().map(|s| display_width(&s.text)).sum()
}

/// Fit `segments` into `width` columns: drop the lowest-priority segments first, then truncate
/// shrinkable ones, joining survivors with `sep`. Never exceeds `width`.
pub fn layout(segments: &[Segment], width: usize, sep: &Span, ellipsis: &str) -> Vec<Span> {
    let sep_width = display_width(&sep.text);
    let mut kept: Vec<Segment> = segments.to_vec();
    let total = |kept: &[Segment]| -> usize {
        kept.iter().map(segment_width).sum::<usize>() + sep_width * kept.len().saturating_sub(1)
    };

    // The segment a squeeze lands on is always the least important one still shown (never the
    // first, which carries the turn state and model). A shrinkable one is cut to fit if that
    // leaves it at least `MIN_SHRUNK` columns (`~/src/…` beats no cwd at all); otherwise it goes.
    const MIN_SHRUNK: usize = 10;
    while total(&kept) > width && kept.len() > 1 {
        let Some((victim, _)) = kept
            .iter()
            .enumerate()
            .skip(1)
            .min_by_key(|(_, s)| s.priority)
        else {
            break;
        };
        let over = total(&kept) - width;
        let current = segment_width(&kept[victim]);
        if kept[victim].shrinkable && current >= over + MIN_SHRUNK {
            kept[victim].spans = truncate_spans(&kept[victim].spans, current - over, ellipsis);
            break;
        }
        kept.remove(victim);
    }

    let mut out = Vec::new();
    for (i, segment) in kept.iter().enumerate() {
        if i > 0 {
            out.push(sep.clone());
        }
        out.extend(segment.spans.iter().cloned());
    }
    truncate_spans(&out, width, ellipsis)
}

/// How far tm may go without asking, as the chat shows it. Shift+Tab cycles auto → plan → ask
/// (D-019); the application maps this onto the agent session's own mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionMode {
    /// The default: tm acts on its own. No indicator.
    #[default]
    Auto,
    /// Read-only: tm investigates and proposes a plan.
    Plan,
    /// tm asks before every edit, command, git operation, or pty keystroke.
    Ask,
}

impl PermissionMode {
    /// The next mode in the Shift+Tab cycle.
    pub fn next(self) -> Self {
        match self {
            PermissionMode::Auto => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::Ask,
            PermissionMode::Ask => PermissionMode::Auto,
        }
    }

    /// Lowercase name (`auto`, `plan`, `ask`).
    pub fn label(self) -> &'static str {
        match self {
            PermissionMode::Auto => "auto",
            PermissionMode::Plan => "plan",
            PermissionMode::Ask => "ask",
        }
    }

    /// The status-line indicator, Claude Code style (`⏸ plan mode on (shift+tab to cycle)`), or
    /// `None` in the default mode.
    pub fn indicator(self, theme: &Theme, glyphs: &Glyphs) -> Option<Vec<Span>> {
        let (glyph, text, color) = match self {
            PermissionMode::Auto => return None,
            PermissionMode::Plan => (glyphs.pause, "plan mode on", theme.accent),
            PermissionMode::Ask => (glyphs.play, "ask mode on", theme.warning),
        };
        Some(vec![
            Span::new(
                format!("{glyph} {text}"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::new(" (shift+tab to cycle)", Style::default().fg(theme.muted)),
        ])
    }
}

/// Draw the whole bar into the one-row `area`: `left` (the key hint, mode indicator, or a
/// transient message) first, then as many of `segments` as fit, right-aligned. The segments yield
/// space before `left` does.
pub fn render(
    buf: &mut Buffer,
    area: Rect,
    left: &[Span],
    segments: &[Segment],
    theme: &Theme,
    glyphs: &Glyphs,
) {
    if area.width < 2 || area.height == 0 {
        return;
    }
    let inner_x = area.x + 1;
    let inner_width = (area.width - 2) as usize;
    let sep = Span::new(glyphs.sep, Style::default().fg(theme.muted));

    let left = truncate_spans(left, inner_width, glyphs.ellipsis);
    let left_width: usize = left.iter().map(|s| display_width(&s.text)).sum();
    draw_spans(buf, inner_x, area.y, inner_width as u16, &left);

    let gap = if left_width == 0 { 0 } else { 3 };
    let room = inner_width.saturating_sub(left_width + gap);
    // Below this the model name would be cut to a stub; better to show nothing on the right.
    const MIN_RIGHT: usize = 12;
    if room < MIN_RIGHT {
        return;
    }
    let right = layout(segments, room, &sep, glyphs.ellipsis);
    let right_width: usize = right.iter().map(|s| display_width(&s.text)).sum();
    let x = inner_x + (inner_width - right_width) as u16;
    draw_spans(buf, x, area.y, right_width as u16, &right);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> StatusInfo {
        StatusInfo {
            model: "devpass/muse-spark-1.3".to_string(),
            cwd: "~/src/ticket-master".to_string(),
            branch: Some("main".to_string()),
            global_scope: false,
            ticket: None,
            open_tickets: 3,
            tokens: 12_345,
        }
    }

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn laid_out(width: usize) -> String {
        let theme = Theme::dark();
        let segs = segments(&info(), TurnState::Idle, "", &theme, &Glyphs::UNICODE);
        text(&layout(
            &segs,
            width,
            &Span::new(" · ", Style::default()),
            "…",
        ))
    }

    #[test]
    fn wide_bars_show_every_segment() {
        let s = laid_out(200);
        assert_eq!(
            s,
            "● devpass/muse-spark-1.3 · ~/src/ticket-master (main) · no ticket · 3 open · 12.3k tokens"
        );
    }

    #[test]
    fn narrow_bars_drop_the_least_important_segments_first() {
        let s = laid_out(70);
        assert!(display_width(&s) <= 70, "{s}");
        assert!(s.contains("devpass/muse-spark-1.3"));
        assert!(s.contains("no ticket"));
        assert!(!s.contains("3 open"), "open count is the first to go: {s}");
    }

    #[test]
    fn very_narrow_bars_keep_the_model() {
        for width in [10usize, 24, 30, 40] {
            let s = laid_out(width);
            assert!(display_width(&s) <= width, "width {width}: {s}");
            assert!(s.starts_with('●'), "width {width}: {s}");
        }
        assert!(laid_out(40).contains("devpass"));
    }

    #[test]
    fn the_model_segment_never_disappears_even_when_truncated() {
        let s = laid_out(12);
        assert!(s.starts_with("● devpass"), "{s}");
    }

    #[test]
    fn tokens_format_compactly() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_234), "1.2k");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn zero_counts_are_omitted_and_an_attached_ticket_is_shown() {
        let theme = Theme::dark();
        let mut i = info();
        i.open_tickets = 0;
        i.tokens = 0;
        i.ticket = Some("T-7".to_string());
        let segs = segments(&i, TurnState::Idle, "", &theme, &Glyphs::UNICODE);
        let s = text(&layout(
            &segs,
            200,
            &Span::new(" · ", Style::default()),
            "…",
        ));
        assert!(s.contains("T-7"));
        assert!(!s.contains("open") && !s.contains("tokens"), "{s}");
    }

    fn row(left: &str, width: u16) -> String {
        let theme = Theme::dark();
        let segs = segments(&info(), TurnState::Idle, "", &theme, &Glyphs::UNICODE);
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        render(
            &mut buf,
            area,
            &[Span::new(left, Style::default())],
            &segs,
            &theme,
            &Glyphs::UNICODE,
        );
        (0..width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect::<String>()
    }

    #[test]
    fn the_hint_is_on_the_left_and_segments_yield_first() {
        let wide = row("? for shortcuts", 160);
        assert!(wide.starts_with(" ? for shortcuts"), "{wide}");
        assert!(wide.trim_end().ends_with("12.3k tokens"), "{wide}");
        let narrow = row("? for shortcuts", 50);
        assert!(narrow.contains("? for shortcuts"), "{narrow}");
        assert!(narrow.contains("devpass"), "{narrow}");
        assert!(!narrow.contains("3 open"), "{narrow}");
        let tiny = row("Press Ctrl+C again to exit", 34);
        assert!(tiny.contains("Press Ctrl+C again to exit"), "{tiny}");
        assert!(!tiny.contains("devpass"), "no room for a stub: {tiny}");
    }

    #[test]
    fn modes_show_claude_code_indicators() {
        let theme = Theme::dark();
        assert_eq!(PermissionMode::Auto.indicator(&theme, &Glyphs::UNICODE), None);
        let text = |mode: PermissionMode| -> String {
            mode.indicator(&theme, &Glyphs::UNICODE)
                .map(|spans| spans.iter().map(|s| s.text.as_str()).collect())
                .unwrap_or_default()
        };
        assert_eq!(
            text(PermissionMode::Plan),
            "⏸ plan mode on (shift+tab to cycle)"
        );
        assert_eq!(
            text(PermissionMode::Ask),
            "⏵ ask mode on (shift+tab to cycle)"
        );
        assert_eq!(PermissionMode::Auto.next(), PermissionMode::Plan);
        assert_eq!(PermissionMode::Plan.next(), PermissionMode::Ask);
        assert_eq!(PermissionMode::Ask.next(), PermissionMode::Auto);
    }
}
