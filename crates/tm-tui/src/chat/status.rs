//! The chat status bar: a single row of prioritized segments that degrades by dropping the least
//! important ones first when the terminal is narrow, rather than wrapping or clipping mid-word.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};

use crate::chat::glyphs::Glyphs;
use crate::chat::lines::{draw_spans, truncate_spans, Span};
use crate::text::{display_width, truncate};
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

/// A right-aligned message for the status bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Hint {
    /// The message.
    pub span: Span,
    /// An urgent hint ("press Ctrl+C again to quit") claims its space by squeezing the segments;
    /// an ordinary one ("? for shortcuts") only appears when everything else already fits.
    pub urgent: bool,
}

/// Draw the whole bar into the one-row `area`: laid-out segments on the left, `hint`
/// right-aligned.
pub fn render(
    buf: &mut Buffer,
    area: Rect,
    segments: &[Segment],
    hint: Option<&Hint>,
    theme: &Theme,
    glyphs: &Glyphs,
) {
    if area.width < 2 || area.height == 0 {
        return;
    }
    let inner_x = area.x + 1;
    let inner_width = (area.width - 2) as usize;
    let sep = Span::new(glyphs.sep, Style::default().fg(theme.muted));

    let hint_width = hint.map(|h| display_width(&h.span.text)).unwrap_or(0);
    let full = layout(segments, inner_width, &sep, glyphs.ellipsis);
    let full_width: usize = full.iter().map(|s| display_width(&s.text)).sum();

    let (left, show_hint) = match hint {
        Some(_) if full_width + 2 + hint_width <= inner_width => (full, true),
        Some(h) if h.urgent && hint_width + 2 < inner_width => (
            layout(
                segments,
                inner_width - hint_width - 2,
                &sep,
                glyphs.ellipsis,
            ),
            true,
        ),
        _ => (full, false),
    };
    draw_spans(buf, inner_x, area.y, inner_width as u16, &left);

    if let (Some(hint), true) = (hint, show_hint) {
        let width = hint_width.min(inner_width);
        let x = inner_x + (inner_width - width) as u16;
        let text = truncate(&hint.span.text, width, glyphs.ellipsis);
        buf.set_stringn(x, area.y, text, width, hint.span.style);
    }
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

    #[test]
    fn render_places_the_hint_only_when_it_fits() {
        let theme = Theme::dark();
        let segs = segments(&info(), TurnState::Idle, "", &theme, &Glyphs::UNICODE);
        let row_with = |width: u16, urgent: bool| {
            let hint = Hint {
                span: Span::new(
                    if urgent {
                        "Press Ctrl+C again to quit"
                    } else {
                        "? for shortcuts"
                    },
                    Style::default(),
                ),
                urgent,
            };
            let area = Rect::new(0, 0, width, 1);
            let mut buf = Buffer::empty(area);
            render(&mut buf, area, &segs, Some(&hint), &theme, &Glyphs::UNICODE);
            (0..width)
                .map(|x| buf[(x, 0)].symbol().to_string())
                .collect::<String>()
        };
        let row = |width: u16| row_with(width, false);
        assert!(row(160).trim_end().ends_with("? for shortcuts"));
        let narrow = row(50);
        assert!(!narrow.contains("? for shortcuts"), "{narrow}");
        assert!(narrow.contains("devpass"));
        let urgent = row_with(60, true);
        assert!(urgent.contains("Press Ctrl+C again to quit"), "{urgent}");
        assert!(urgent.contains("devpass"), "{urgent}");
    }
}
