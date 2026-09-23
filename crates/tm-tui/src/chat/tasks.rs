//! The Ctrl+T task checklist: Claude Code's to-do list, shown above the prompt. In tm the tasks
//! are tickets — the attached ticket's subtasks and whatever this conversation handed to the
//! background — so the application supplies them from the event log.

use ratatui_core::style::{Modifier, Style};

use crate::chat::glyphs::Glyphs;
use crate::chat::lines::{truncate_spans, Line, Span};
use crate::theme::Theme;

/// Where a task stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Not started.
    Pending,
    /// Being worked.
    Active,
    /// Done.
    Done,
    /// Failed or cancelled.
    Failed,
}

/// One checklist row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskItem {
    /// Where it stands.
    pub state: TaskState,
    /// What it is (`T-5 Fix the flaky test`).
    pub text: String,
}

/// The most rows the checklist takes.
pub const MAX_ROWS: usize = 8;

/// The checklist laid out for `width` columns, at most [`MAX_ROWS`] rows plus its heading.
pub fn lines(items: &[TaskItem], width: usize, theme: &Theme, glyphs: &Glyphs) -> Vec<Line> {
    let muted = Style::default().fg(theme.muted);
    let mut out = vec![Line::from_spans(vec![
        Span::new(" Tasks", muted.add_modifier(Modifier::BOLD)),
        Span::new(" (ctrl+t to hide)", muted),
    ])];
    if items.is_empty() {
        out.push(Line::plain(
            "   No tasks yet: /bg hands work to a background ticket, and an attached ticket's \
             subtasks show here.",
            muted,
        ));
    }
    let (open, done) = if glyphs.unicode {
        ("☐", "☒")
    } else {
        ("[ ]", "[x]")
    };
    for item in items.iter().take(MAX_ROWS) {
        let (mark, mark_style, text_style) = match item.state {
            TaskState::Pending => (open, muted, Style::default()),
            TaskState::Active => (
                open,
                Style::default().fg(theme.accent),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            TaskState::Done => (
                done,
                Style::default().fg(theme.success),
                muted.add_modifier(Modifier::CROSSED_OUT),
            ),
            TaskState::Failed => (
                done,
                Style::default().fg(theme.danger),
                Style::default().fg(theme.danger),
            ),
        };
        out.push(Line::from_spans(vec![
            Span::new("   ", Style::default()),
            Span::new(mark, mark_style),
            Span::new(" ", Style::default()),
            Span::new(item.text.clone(), text_style),
        ]));
    }
    if items.len() > MAX_ROWS {
        out.push(Line::plain(
            format!("   {} +{} more", glyphs.ellipsis, items.len() - MAX_ROWS),
            muted,
        ));
    }
    for line in &mut out {
        line.spans = truncate_spans(&line.spans, width, glyphs.ellipsis);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_get_their_marks_and_the_list_is_capped() {
        let theme = Theme::dark();
        let items: Vec<TaskItem> = (0..10)
            .map(|i| TaskItem {
                state: if i == 0 {
                    TaskState::Done
                } else {
                    TaskState::Pending
                },
                text: format!("T-{i} task"),
            })
            .collect();
        let rows = lines(&items, 60, &theme, &Glyphs::UNICODE);
        assert_eq!(rows.len(), 1 + MAX_ROWS + 1);
        assert_eq!(rows[1].text(), "   ☒ T-0 task");
        assert_eq!(rows[2].text(), "   ☐ T-1 task");
        assert!(rows.last().is_some_and(|r| r.text().contains("+2 more")));
        let empty = lines(&[], 40, &theme, &Glyphs::ASCII);
        assert!(empty[1].text().starts_with("   No tasks yet"));
        assert!(empty.iter().all(|l| l.width() <= 40));
    }
}
