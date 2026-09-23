//! The `/resume` picker: past conversations, newest first, each with its first message, age and
//! turn count. ↑/↓ move, Enter resumes, Esc closes. Typing filters by message or id.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};

use crate::chat::glyphs::Glyphs;
use crate::chat::lines::{clear, draw_box, draw_spans, truncate_spans, Span};
use crate::text::display_width;
use crate::theme::Theme;

/// One past conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationRow {
    /// Its session id (`S-12`).
    pub id: String,
    /// The first thing the human said.
    pub first_message: String,
    /// How long ago it was last active (`5m ago`).
    pub age: String,
    /// How many turns it has.
    pub turns: usize,
}

/// What a key did to the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    /// Still open.
    Open,
    /// Closed without choosing.
    Cancelled,
    /// This conversation was chosen.
    Chosen(String),
}

/// The picker's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePicker {
    rows: Vec<ConversationRow>,
    filter: String,
    selected: usize,
}

impl ResumePicker {
    /// A picker over `rows` (newest first).
    pub fn new(rows: Vec<ConversationRow>) -> Self {
        ResumePicker {
            rows,
            filter: String::new(),
            selected: 0,
        }
    }

    fn visible(&self) -> Vec<&ConversationRow> {
        let needle = self.filter.to_lowercase();
        self.rows
            .iter()
            .filter(|r| {
                needle.is_empty()
                    || r.first_message.to_lowercase().contains(&needle)
                    || r.id.to_lowercase().contains(&needle)
            })
            .collect()
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> PickerOutcome {
        let count = self.visible().len();
        match key.code {
            KeyCode::Esc => return PickerOutcome::Cancelled,
            KeyCode::Up if count > 0 => self.selected = (self.selected + count - 1) % count,
            KeyCode::Down if count > 0 => self.selected = (self.selected + 1) % count,
            KeyCode::Enter => {
                if let Some(row) = self.visible().get(self.selected.min(count.saturating_sub(1)))
                {
                    return PickerOutcome::Chosen(row.id.clone());
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.filter.push(c);
                self.selected = 0;
            }
            _ => {}
        }
        PickerOutcome::Open
    }

    /// Draw the picker centred in `area`.
    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme, glyphs: &Glyphs) {
        let width = area.width.saturating_sub(4).min(100);
        let rows = self.visible();
        let list_height = rows.len().clamp(1, 12) as u16;
        let height = (list_height + 4).min(area.height);
        if width < 20 || height < 5 {
            return;
        }
        let outer = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        clear(buf, outer);
        draw_box(buf, outer, &glyphs.border, Style::default().fg(theme.accent));
        let accent = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let muted = Style::default().fg(theme.muted);
        let title = if self.filter.is_empty() {
            " Resume a conversation ".to_string()
        } else {
            format!(" Resume a conversation {} {} ", glyphs.sep.trim(), self.filter)
        };
        buf.set_stringn(
            outer.x + 2,
            outer.y,
            &title,
            (width as usize).saturating_sub(4),
            accent,
        );
        let footer = format!(
            " {} select{}enter resume{}esc close ",
            glyphs.updown, glyphs.sep, glyphs.sep
        );
        let footer_width = display_width(&footer) as u16;
        if footer_width + 4 < width {
            buf.set_string(
                outer.x + width - footer_width - 2,
                outer.y + height - 1,
                &footer,
                muted,
            );
        }
        let inner = (width as usize).saturating_sub(4);
        if rows.is_empty() {
            let text = if self.rows.is_empty() {
                "No saved conversations in this project yet."
            } else {
                "Nothing matches."
            };
            buf.set_stringn(outer.x + 2, outer.y + 2, text, inner, muted);
            return;
        }
        let shown = (height - 4) as usize;
        let selected = self.selected.min(rows.len() - 1);
        let first = selected.saturating_sub(shown.saturating_sub(1));
        for (row, (i, r)) in rows.iter().enumerate().skip(first).take(shown).enumerate() {
            let is_selected = i == selected;
            let meta = format!(
                "  {} {} {} turn{}",
                r.age,
                glyphs.sep.trim(),
                r.turns,
                if r.turns == 1 { "" } else { "s" }
            );
            let spans = vec![
                Span::new(
                    if is_selected { glyphs.pointer } else { " " },
                    Style::default().fg(theme.accent),
                ),
                Span::new(format!(" {:<6} ", r.id), muted),
                Span::new(
                    r.first_message.clone(),
                    if is_selected { accent } else { Style::default() },
                ),
            ];
            let meta_width = display_width(&meta);
            let spans = truncate_spans(&spans, inner.saturating_sub(meta_width), glyphs.ellipsis);
            let y = outer.y + 2 + row as u16;
            draw_spans(buf, outer.x + 2, y, inner as u16, &spans);
            if meta_width < inner {
                buf.set_stringn(
                    outer.x + 2 + (inner - meta_width) as u16,
                    y,
                    &meta,
                    meta_width,
                    muted,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> ResumePicker {
        ResumePicker::new(vec![
            ConversationRow {
                id: "S-3".into(),
                first_message: "fix the flaky test".into(),
                age: "5m ago".into(),
                turns: 3,
            },
            ConversationRow {
                id: "S-1".into(),
                first_message: "explain the scheduler".into(),
                age: "2d ago".into(),
                turns: 1,
            },
        ])
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn arrows_and_enter_choose_and_typing_filters() {
        let mut p = picker();
        assert_eq!(p.handle_key(&key(KeyCode::Down)), PickerOutcome::Open);
        assert_eq!(
            p.handle_key(&key(KeyCode::Enter)),
            PickerOutcome::Chosen("S-1".into())
        );
        let mut p = picker();
        for c in "flaky".chars() {
            p.handle_key(&key(KeyCode::Char(c)));
        }
        assert_eq!(
            p.handle_key(&key(KeyCode::Enter)),
            PickerOutcome::Chosen("S-3".into())
        );
        assert_eq!(p.handle_key(&key(KeyCode::Esc)), PickerOutcome::Cancelled);
    }

    #[test]
    fn it_lists_first_message_age_and_turns() {
        let p = picker();
        let area = Rect::new(0, 0, 80, 12);
        let mut buf = Buffer::empty(area);
        p.render(area, &mut buf, &Theme::dark(), &Glyphs::UNICODE);
        let text: String = (0..12)
            .map(|y| {
                (0..80)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(text.contains("Resume a conversation"), "{text}");
        assert!(text.contains("fix the flaky test"), "{text}");
        assert!(text.contains("5m ago · 3 turns"), "{text}");
        assert!(text.contains("2d ago · 1 turn"), "{text}");
    }
}
