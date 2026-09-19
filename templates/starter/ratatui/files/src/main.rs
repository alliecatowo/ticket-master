//! {{description}}
//!
//! Minimal Ratatui scaffold: one bordered paragraph, redrawn on a 250ms tick, exits on `q`.
//! See `skill.md` for this stack's conventions.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

const PROJECT_NAME: &str = "{{project_name}}";

/// The greeting shown at the top of the screen. Kept as a plain function (no `Frame` argument)
/// so it's unit-testable without a real terminal — see `skill.md`'s "business logic stays
/// separate from rendering" convention.
fn welcome_text() -> String {
    format!("Welcome to {PROJECT_NAME} \u{2014} press q to quit.")
}

fn draw(frame: &mut Frame) {
    let area = frame.area();
    let layout = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);
    let title = Paragraph::new(welcome_text()).block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, layout[0]);
}

/// Run the TUI event loop until `q` is pressed. Raw mode and the alternate screen are always
/// left, even on an `Err` return, via the `result` shadowing below.
fn run() -> io::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let result = (|| -> io::Result<()> {
        loop {
            terminal.draw(draw)?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if key.code == KeyCode::Char('q') {
                        return Ok(());
                    }
                }
            }
        }
    })();

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    result
}

fn main() -> io::Result<()> {
    run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_text_includes_the_project_name() {
        assert!(welcome_text().contains(PROJECT_NAME));
    }
}
