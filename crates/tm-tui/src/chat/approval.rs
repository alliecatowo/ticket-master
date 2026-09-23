//! The permission prompt: Claude Code's numbered question, shown in place of the input box while
//! a turn waits for the human.
//!
//! ```text
//! ╭────────────────────────────────────────────────────╮
//! │ Bash command                                       │
//! │                                                    │
//! │   git push origin main                             │
//! │   network access needs approval                    │
//! │                                                    │
//! │ Do you want to proceed?                            │
//! │ › 1. Yes                                           │
//! │   2. Yes, and don't ask again this session         │
//! │   3. No, and tell tm what to do differently (esc)  │
//! ╰────────────────────────────────────────────────────╯
//! ```
//!
//! Number keys pick directly; ↑/↓ and Enter pick the highlighted one; Esc is "No"; Shift+Tab is
//! "Yes, and don't ask again", as in Claude Code.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};

use crate::chat::glyphs::Glyphs;
use crate::chat::lines::{clear, draw_box, draw_spans, truncate_spans, wrap_with_prefix, Line, Span};
use crate::chat::transcript::tool_label;
use crate::theme::Theme;

/// What the human chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalChoice {
    /// "Yes".
    Yes,
    /// "Yes, and don't ask again this session".
    YesForSession,
    /// "No, and tell tm what to do differently": the turn stops so the human can say what instead.
    No,
}

/// A tool call waiting for approval, already reduced to text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// The tool's real name (`shell.run`).
    pub tool: String,
    /// What it would do: the command, or the path.
    pub target: String,
    /// Why it needs asking.
    pub reason: String,
}

/// The prompt's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPrompt {
    request: ApprovalRequest,
    selected: usize,
}

const CHOICES: [ApprovalChoice; 3] = [
    ApprovalChoice::Yes,
    ApprovalChoice::YesForSession,
    ApprovalChoice::No,
];

impl ApprovalPrompt {
    /// A prompt for `request`, "Yes" highlighted.
    pub fn new(request: ApprovalRequest) -> Self {
        ApprovalPrompt {
            request,
            selected: 0,
        }
    }

    /// The request being asked about.
    pub fn request(&self) -> &ApprovalRequest {
        &self.request
    }

    /// Handle a key. Returns the choice once one is made.
    pub fn handle_key(&mut self, key: &KeyEvent) -> Option<ApprovalChoice> {
        match key.code {
            KeyCode::Char('1') | KeyCode::Char('y') => Some(ApprovalChoice::Yes),
            KeyCode::Char('2') => Some(ApprovalChoice::YesForSession),
            KeyCode::Char('3') | KeyCode::Char('n') | KeyCode::Esc => Some(ApprovalChoice::No),
            KeyCode::BackTab => Some(ApprovalChoice::YesForSession),
            KeyCode::Up => {
                self.selected = (self.selected + CHOICES.len() - 1) % CHOICES.len();
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1) % CHOICES.len();
                None
            }
            KeyCode::Enter => Some(CHOICES[self.selected]),
            _ => None,
        }
    }

    fn title(&self) -> String {
        match tool_label(&self.request.tool).as_str() {
            "Bash" => "Bash command".to_string(),
            "Update" | "Write" | "Delete" => "Edit file".to_string(),
            label => format!("Tool use: {label}"),
        }
    }

    fn question(&self) -> String {
        match tool_label(&self.request.tool).as_str() {
            "Bash" => "Do you want to proceed?".to_string(),
            "Update" | "Write" | "Delete" if !self.request.target.is_empty() => {
                format!("Do you want to make this edit to {}?", self.request.target)
            }
            _ => format!("Do you want to allow {}?", self.request.tool),
        }
    }

    fn body(&self, width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let muted = Style::default().fg(theme.muted);
        let mut out = vec![Line::plain(self.title(), bold.fg(theme.warning)), Line::blank()];
        if !self.request.target.is_empty() {
            let mut rows = wrap_with_prefix(
                &[Span::new(self.request.target.clone(), Style::default())],
                width,
                vec![Span::new("  ", Style::default())],
                "  ",
            );
            rows.truncate(3);
            out.extend(rows);
        }
        if !self.request.reason.is_empty() {
            let mut rows = wrap_with_prefix(
                &[Span::new(self.request.reason.clone(), muted)],
                width,
                vec![Span::new("  ", Style::default())],
                "  ",
            );
            rows.truncate(2);
            out.extend(rows);
        }
        out.push(Line::blank());
        out.push(Line::plain(self.question(), Style::default()));
        let labels = [
            "Yes".to_string(),
            "Yes, and don't ask again this session".to_string(),
            "No, and tell tm what to do differently (esc)".to_string(),
        ];
        for (i, label) in labels.iter().enumerate() {
            let selected = i == self.selected;
            let style = if selected {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            out.push(Line::from_spans(vec![
                Span::new(
                    if selected { glyphs.pointer } else { " " },
                    Style::default().fg(theme.accent),
                ),
                Span::new(format!(" {}. {label}", i + 1), style),
            ]));
        }
        out
    }

    /// The prompt's height (borders included) at `width` columns.
    pub fn height(&self, width: u16, theme: &Theme, glyphs: &Glyphs) -> u16 {
        self.body((width as usize).saturating_sub(4), theme, glyphs)
            .len() as u16
            + 2
    }

    /// Draw the prompt into `area`.
    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme, glyphs: &Glyphs) {
        if area.width < 8 || area.height < 3 {
            return;
        }
        clear(buf, area);
        draw_box(
            buf,
            area,
            &glyphs.border,
            Style::default().fg(theme.warning),
        );
        let inner = (area.width as usize).saturating_sub(4);
        let lines = self.body(inner, theme, glyphs);
        // When the box is squeezed, keep the question and the choices: drop from the top.
        let visible = (area.height - 2) as usize;
        let skip = lines.len().saturating_sub(visible);
        for (row, line) in lines.iter().skip(skip).enumerate() {
            draw_spans(
                buf,
                area.x + 2,
                area.y + 1 + row as u16,
                inner as u16,
                &truncate_spans(&line.spans, inner, glyphs.ellipsis),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn prompt() -> ApprovalPrompt {
        ApprovalPrompt::new(ApprovalRequest {
            tool: "shell.run".to_string(),
            target: "git push origin main".to_string(),
            reason: "network access needs approval".to_string(),
        })
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn numbers_pick_and_arrows_move() {
        let mut p = prompt();
        assert_eq!(p.handle_key(&key(KeyCode::Char('2'))), Some(ApprovalChoice::YesForSession));
        assert_eq!(p.handle_key(&key(KeyCode::Esc)), Some(ApprovalChoice::No));
        assert_eq!(p.handle_key(&key(KeyCode::Down)), None);
        assert_eq!(p.handle_key(&key(KeyCode::Down)), None);
        assert_eq!(p.handle_key(&key(KeyCode::Enter)), Some(ApprovalChoice::No));
        assert_eq!(p.handle_key(&key(KeyCode::Up)), None);
        assert_eq!(p.handle_key(&key(KeyCode::Enter)), Some(ApprovalChoice::YesForSession));
        assert_eq!(p.handle_key(&key(KeyCode::Char('x'))), None, "typing is ignored");
    }

    #[test]
    fn it_reads_like_claude_code() {
        let p = prompt();
        let theme = Theme::dark();
        let height = p.height(80, &theme, &Glyphs::UNICODE);
        let area = Rect::new(0, 0, 80, height);
        let mut buf = Buffer::empty(area);
        p.render(area, &mut buf, &theme, &Glyphs::UNICODE);
        let text: Vec<String> = (0..height)
            .map(|y| {
                (0..80)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        let text = text.join("\n");
        for needle in [
            "Bash command",
            "git push origin main",
            "Do you want to proceed?",
            "› 1. Yes",
            "2. Yes, and don't ask again this session",
            "3. No, and tell tm what to do differently (esc)",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from\n{text}");
        }
    }

    #[test]
    fn a_squeezed_box_keeps_the_choices() {
        let p = prompt();
        let theme = Theme::dark();
        let area = Rect::new(0, 0, 40, 6);
        let mut buf = Buffer::empty(area);
        p.render(area, &mut buf, &theme, &Glyphs::ASCII);
        let bottom: String = (0..40).map(|x| buf[(x, 4)].symbol().to_string()).collect();
        assert!(bottom.contains("3. No"), "{bottom}");
    }
}
