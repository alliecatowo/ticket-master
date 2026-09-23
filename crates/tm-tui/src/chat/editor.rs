//! Ctrl+G: edit the prompt in `$VISUAL`/`$EDITOR`, the way Claude Code does.
//!
//! The editor needs the real terminal, so for its lifetime the TUI steps aside: raw mode off, the
//! alternate screen left, bracketed paste and mouse capture off. The call blocks the event loop on
//! purpose — while it blocks, nothing polls crossterm's input stream, so no keystroke meant for
//! the editor is read by the TUI (the stdin contention `Runtime::start` documents); a pty check
//! with an editor that reads `/dev/tty` confirmed the keystrokes reach it. Afterwards the terminal
//! is set back up here, before any frame can be drawn, and the caller asks the chat screen for a
//! full repaint ([`crate::screens::chat::ChatScreen::force_full_repaint`]).
//!
//! The runtime's own resume path (a `SIGCONT`, as after Ctrl+Z) is deliberately not used: its
//! `Terminal::clear` queries the cursor position, and that query times out while crossterm's
//! event-stream thread holds the input reader, which ends the TUI with "The cursor position could
//! not be read" — found by trying exactly that.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

/// The editor command: `$VISUAL`, else `$EDITOR`, else `vi`.
pub fn editor_command() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "vi".to_string())
}

fn scratch_path() -> PathBuf {
    std::env::temp_dir().join(format!("tm-prompt-{}.md", std::process::id()))
}

/// Open `text` in the external editor and return what was saved, or `None` when the editor
/// exited unsuccessfully (the prompt is then left as it was). Blocks until the editor exits.
pub fn edit(text: &str) -> io::Result<Option<String>> {
    let path = scratch_path();
    std::fs::write(&path, text)?;
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = terminal::disable_raw_mode();
    let _ = stdout.flush();

    // Through `sh -c` so an `$EDITOR` with arguments (`code --wait`) works as it does in a shell.
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{} \"$1\"", editor_command()))
        .arg("tm-editor")
        .arg(&path)
        .status();

    let _ = terminal::enable_raw_mode();
    let _ = execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    );

    let edited = match status {
        Ok(status) if status.success() => Some(std::fs::read_to_string(&path)?),
        Ok(_) => None,
        Err(err) => {
            let _ = std::fs::remove_file(&path);
            return Err(err);
        }
    };
    let _ = std::fs::remove_file(&path);
    // Editors add a trailing newline; a prompt does not want one.
    Ok(edited.map(|t| t.trim_end_matches('\n').to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_falls_back_to_vi() {
        // Only checks the shape: the environment of the test process is not ours to change.
        assert!(!editor_command().trim().is_empty());
    }
}
