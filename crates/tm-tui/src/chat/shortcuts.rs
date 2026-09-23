//! The `?` shortcuts panel: Claude Code's grid of the keys that matter, shown under the prompt
//! when `?` is pressed on an empty prompt (and by `/help`). Every key listed here is one the chat
//! actually handles.

use ratatui_core::style::Style;

use crate::chat::glyphs::Glyphs;
use crate::chat::lines::{Line, Span};
use crate::text::display_width;
use crate::theme::Theme;

/// The panel's entries, in reading order (down the first column, then the next).
fn entries(glyphs: &Glyphs) -> Vec<String> {
    let enter = if glyphs.unicode { "⏎" } else { "enter" };
    vec![
        "! for bash mode".to_string(),
        "/ for commands".to_string(),
        "@ for file paths".to_string(),
        format!("{} for tickets", glyphs.left),
        "esc to interrupt".to_string(),
        "double tap esc to clear input".to_string(),
        "shift + tab to cycle modes".to_string(),
        "ctrl + o for transcript".to_string(),
        "ctrl + r to search history".to_string(),
        format!("shift + {enter} for newline"),
        "ctrl + _ to undo".to_string(),
        "ctrl + g for $EDITOR".to_string(),
        "ctrl + y to yank".to_string(),
        "ctrl + c twice to exit".to_string(),
    ]
}

/// The widest each column is when `items` are laid out `rows` deep.
fn column_widths(items: &[String], rows: usize) -> Vec<usize> {
    items
        .chunks(rows.max(1))
        .map(|column| column.iter().map(|s| display_width(s)).max().unwrap_or(0))
        .collect()
}

/// The panel laid out for `width` columns: as many columns as fit (up to three), each entry
/// muted, indented two cells like Claude Code's.
pub fn lines(width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let items = entries(glyphs);
    let muted = Style::default().fg(theme.muted);
    let gap = 3;
    let usable = width.saturating_sub(2);
    let columns = (1..=3)
        .rev()
        .find(|&c| {
            let widths = column_widths(&items, items.len().div_ceil(c));
            widths.iter().sum::<usize>() + gap * (widths.len().saturating_sub(1)) <= usable
        })
        .unwrap_or(1);
    let rows = items.len().div_ceil(columns);
    let widths = column_widths(&items, rows);
    let column_width = |c: usize| widths.get(c).copied().unwrap_or(0);
    let mut out = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut spans = vec![Span::new("  ", Style::default())];
        for column in 0..columns {
            let Some(item) = items.get(column * rows + row) else {
                continue;
            };
            spans.push(Span::new(item.clone(), muted));
            if column + 1 < columns {
                let pad = column_width(column).saturating_sub(display_width(item)) + gap;
                spans.push(Span::new(" ".repeat(pad), Style::default()));
            }
        }
        out.push(Line::from_spans(spans));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_terminals_get_three_columns_and_narrow_ones_fewer() {
        let theme = Theme::dark();
        let wide = lines(120, &theme, &Glyphs::UNICODE);
        assert_eq!(wide.len(), 5);
        assert!(wide[0].text().contains("! for bash mode"));
        assert!(wide[0].text().contains("double tap esc to clear input"));
        assert!(wide[1].text().contains("shift + tab to cycle modes"));
        assert_eq!(lines(80, &theme, &Glyphs::UNICODE).len(), 5, "3 columns at 80");
        let narrow = lines(40, &theme, &Glyphs::UNICODE);
        assert_eq!(narrow.len(), 14);
        for width in [30usize, 60, 80, 100, 200] {
            for line in lines(width, &theme, &Glyphs::ASCII) {
                assert!(line.text().is_ascii());
            }
        }
    }

    #[test]
    fn at_eighty_columns_it_fits() {
        let theme = Theme::dark();
        for line in lines(80, &theme, &Glyphs::UNICODE) {
            assert!(line.width() <= 80, "{:?}", line.text());
        }
    }
}
