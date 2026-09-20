//! [`Notifier`], the seam every notification-decision test in this crate dispatches through, and
//! [`SystemNotifier`], the one real implementation.
//!
//! # Untested by necessity
//! [`SystemNotifier::notify`] is not exercised by this crate's test suite, on purpose: it either
//! calls a real OS notification API, spawns a real `terminal-notifier` process, or writes a raw
//! escape sequence to a real terminal device, all real I/O with real (and, for `terminal-notifier`
//! shelling out, occasionally slow) side effects that a `cargo test` run must never depend on —
//! the exact category `crates/xtask/src/hygiene.rs`'s `check_network_in_tests`-style checks exist
//! to keep out of this codebase's test suite. Everything this module's *decisions* depend on
//! (which event kinds fire, what the message says) lives in [`crate::decision`] instead, tested
//! there against a recording fake of this trait.

use std::io::Write;

/// One rendered notification: a short title and a longer body, both already human-readable text
/// (no further templating expected downstream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The notification's title/summary line.
    pub title: String,
    /// The notification's body text.
    pub body: String,
}

/// Why [`Notifier::notify`] could not deliver a notification. Never fatal to a caller like
/// [`crate::watch::spawn_notification_watcher`]'s poll loop — a failed notification is logged and
/// skipped, never allowed to take down the process it was meant to inform a human about.
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    /// The platform notification backend (a `notify-rust` call, or a `terminal-notifier`
    /// subprocess) reported failure or was unavailable, and the OSC 9 fallback also failed.
    #[error("desktop notification failed: {0}")]
    Backend(String),
    /// The OSC 9 fallback could not write to any terminal device.
    #[error("failed to write OSC 9 fallback: {0}")]
    Io(#[from] std::io::Error),
}

/// A destination for [`Notification`]s. The real fallback chain
/// (`docs/decisions/D-007-desktop-notifications.md`) lives behind [`SystemNotifier`]; every test
/// in this crate that needs to assert *whether*/*what* a notification fired uses its own
/// in-memory recording implementation instead, so no test ever triggers a real OS popup.
pub trait Notifier: Send + Sync {
    /// Deliver `notification`. Implementations should treat failure as non-fatal to their own
    /// caller (see [`NotifyError`]'s docs) — this method's `Result` exists so a caller can log
    /// the failure, not so it can meaningfully recover from one.
    fn notify(&self, notification: &Notification) -> Result<(), NotifyError>;
}

/// The real fallback chain the audit specifies (`docs/audit-2026-09-18-fable.md` M-04): on
/// Linux/Windows, `notify-rust`'s native backend; on macOS, shell out to `terminal-notifier` when
/// it is on `PATH` (see the crate's `Cargo.toml` for why macOS does not use `notify-rust`'s own
/// backend here); if the platform-appropriate mechanism is unavailable or fails, write an OSC 9
/// escape sequence (`\x1b]9;<message>\x1b\\`) to the controlling terminal.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemNotifier;

impl SystemNotifier {
    /// A new [`SystemNotifier`]. Carries no state — every call re-resolves the chain, since
    /// whether `terminal-notifier` is on `PATH` (or a controlling terminal is even attached) can
    /// change between calls in a long-running process like `tm sched run`.
    pub fn new() -> Self {
        SystemNotifier
    }
}

impl Notifier for SystemNotifier {
    fn notify(&self, notification: &Notification) -> Result<(), NotifyError> {
        #[cfg(target_os = "macos")]
        {
            if try_terminal_notifier(notification) {
                return Ok(());
            }
            osc9_fallback(notification)
        }
        #[cfg(not(target_os = "macos"))]
        {
            match notify_rust::Notification::new()
                .summary(&notification.title)
                .body(&notification.body)
                .show()
            {
                Ok(_) => Ok(()),
                Err(e) => {
                    tracing::debug!(error = %e, "notify-rust delivery failed, falling back to OSC 9");
                    osc9_fallback(notification)
                }
            }
        }
    }
}

/// Shell out to `terminal-notifier -title <title> -message <body>`. Returns `false` (never an
/// error) whenever the binary is missing or exits non-zero, since either just means "fall
/// through to OSC 9", not "the process should error".
///
/// `Command`/`Stdio` are imported locally rather than at module scope: this function is the only
/// caller, it is `cfg(target_os = "macos")`-gated, and a module-scope import would be an unused
/// import (a `-D warnings` clippy failure) on every other target.
#[cfg(target_os = "macos")]
fn try_terminal_notifier(notification: &Notification) -> bool {
    use std::process::{Command, Stdio};

    Command::new("terminal-notifier")
        .arg("-title")
        .arg(&notification.title)
        .arg("-message")
        .arg(&notification.body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Write the OSC 9 escape sequence (`\x1b]9;<message>\x1b\\`) to the controlling terminal:
/// `/dev/tty` on Unix, `CONOUT$` on Windows, falling back to stderr when neither can be opened
/// (e.g. no controlling terminal at all — a headless/CI context, which
/// [`crate::decision::notifications_enabled`] should already have filtered out before this is
/// ever reached, but this function stays honest about its own fallback rather than assuming that).
fn osc9_fallback(notification: &Notification) -> Result<(), NotifyError> {
    let message = format!("{}: {}", notification.title, notification.body);
    let sequence = format!("\x1b]9;{message}\x1b\\");

    #[cfg(unix)]
    let tty_path = "/dev/tty";
    #[cfg(windows)]
    let tty_path = "CONOUT$";

    #[cfg(any(unix, windows))]
    {
        if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open(tty_path) {
            tty.write_all(sequence.as_bytes())?;
            return Ok(());
        }
    }

    // No controlling terminal reachable; stderr is the last honest place left to put it.
    eprint!("{sequence}");
    Ok(())
}
