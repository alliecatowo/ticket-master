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

/// A fresh private scratch directory (mode 0700, unpredictable name) and the prompt file inside
/// it. A predictable `/tmp/tm-prompt-<pid>.md` could be pre-created as a symlink by another user
/// on a shared host, and was world-readable.
fn scratch_file() -> io::Result<(tempfile::TempDir, PathBuf)> {
    let dir = tempfile::Builder::new().prefix("tm-prompt-").tempdir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    let path = dir.path().join("prompt.md");
    Ok((dir, path))
}

/// Write `text` to `path`, creating it new and owner-only.
fn write_private(path: &std::path::Path, text: &str) -> io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(text.as_bytes())
}

/// Open `text` in the external editor and return what was saved, or `None` when the editor
/// exited unsuccessfully (the prompt is then left as it was). Blocks until the editor exits.
pub fn edit(text: &str) -> io::Result<Option<String>> {
    let (_dir, path) = scratch_file()?;
    write_private(&path, text)?;
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

    #[cfg(unix)]
    #[test]
    fn scratch_file_is_private_and_not_predictable() {
        use std::os::unix::fs::PermissionsExt;
        let (dir_a, path_a) = scratch_file().expect("scratch");
        let (dir_b, path_b) = scratch_file().expect("scratch");
        assert_ne!(path_a, path_b);
        assert!(!path_a
            .to_string_lossy()
            .contains(&std::process::id().to_string()));
        write_private(&path_a, "secret prompt").expect("write");
        let dir_mode = std::fs::metadata(dir_a.path())
            .expect("meta")
            .permissions()
            .mode();
        let file_mode = std::fs::metadata(&path_a)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o077, 0, "dir must be owner-only");
        assert_eq!(file_mode & 0o077, 0, "file must be owner-only");
        // create_new refuses to follow or reuse an existing path (e.g. a planted symlink).
        assert!(write_private(&path_a, "again").is_err());
        drop(dir_b);
    }
}
