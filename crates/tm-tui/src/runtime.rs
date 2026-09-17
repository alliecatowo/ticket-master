//! The runtime: terminal lifecycle, the event loop, and the signal handling ratatui's panic hook
//! does not give us for free.
//!
//! ratatui's panic hook restores the terminal on `panic!` only. A `kill -TERM`, a closed SSH
//! session (`SIGHUP`), or a `^Z`/`fg` (`SIGTSTP`/`SIGCONT`) bypass unwinding entirely and, left
//! unhandled, leave the tty stuck in raw/alt-screen mode for whatever uses it next (D-002, "What
//! this costs, stated plainly"). This module is the only place allowed to touch raw mode, the
//! alternate screen, or process signals; every other module only ever sees a [`Buffer`] and a
//! [`Rect`](ratatui_core::layout::Rect) to render into.
//!
//! [`Buffer`]: ratatui_core::buffer::Buffer

use std::io;
use std::sync::Arc;
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tm_types::Clock;
use tokio::sync::{mpsc, Notify};

use crate::caps::Capabilities;
use crate::component::{Component, FocusTree};
use crate::event::AppMessage;
use crate::theme::Theme;

/// Everything that can go wrong standing up or driving the terminal.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// A terminal control operation (raw mode, alternate screen, drawing, ...) failed.
    #[error("terminal I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The application-message channel closed while the runtime was still expecting messages —
    /// every `MessageSender` clone was dropped without the runtime shutting down first.
    #[error("the application message channel closed unexpectedly")]
    ChannelClosed,
}

/// A handle application code uses to push [`AppMessage`]s into the running event loop from a
/// background task: a streaming agent session, an event-stream subscription, a timer.
///
/// Cheap to clone (an `mpsc::UnboundedSender` under the hood) so every background task that
/// needs to talk to the UI can hold its own copy.
#[derive(Debug, Clone)]
pub struct MessageSender {
    tx: mpsc::UnboundedSender<AppMessage>,
}

impl MessageSender {
    /// Enqueue `message` for the next event-loop iteration.
    ///
    /// The channel is unbounded: backpressure on UI messages is the producer's job (coalescing
    /// stream chunks, for instance), not something that should block a background task on the
    /// terminal's redraw rate. A closed channel (the runtime already shut down) is treated as a
    /// no-op rather than an error — callers should not need to special-case shutdown races.
    pub fn send(&self, message: AppMessage) {
        let _ = self.tx.send(message);
    }
}

/// Owns the terminal for the lifetime of the TUI.
///
/// Enters raw mode and the alternate screen on construction ([`Runtime::start`]), restores both
/// on drop *and* on `SIGTERM`/`SIGHUP`/`SIGTSTP` (`install_signal_handlers`, below — a killed
/// process or a closed SSH session bypasses `Drop` entirely, which is exactly the gap D-002 calls
/// out), and drives the event loop that turns crossterm input, resizes, ticks, and
/// [`AppMessage`]s into [`crate::event::Event`]s for the root [`Component`].
pub struct Runtime {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    caps: Capabilities,
    theme: Theme,
    clock: Arc<dyn Clock>,
    focus: FocusTree,
    messages: mpsc::UnboundedReceiver<AppMessage>,
    /// Signalled by `install_signal_handlers`'s task on `SIGTERM`/`SIGHUP`; `run` selects on
    /// this to exit the event loop and let `main` return non-zero.
    shutdown: Arc<Notify>,
    /// How often `run` emits `Event::Tick`, e.g. for animation.
    tick_interval: Duration,
}

impl Runtime {
    /// This runtime's detected terminal capabilities.
    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// This runtime's active theme.
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Enter raw mode and the alternate screen, install the signal handlers below, and return a
    /// `Runtime` plus a [`MessageSender`] background tasks can clone freely.
    ///
    /// IMPL:
    /// 1. `crossterm::terminal::enable_raw_mode()?`, then
    ///    `crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?`.
    /// 2. Probe capabilities: `caps::probe_synchronized_output(&env)`, a mouse-capture probe
    ///    (`crossterm::execute!(.., crossterm::event::EnableMouseCapture)` and record whether it
    ///    returned `Ok`), then `caps::detect(&env, synchronized_output, mouse)`. `env` comes from
    ///    `std::env::vars().collect()` — this is the one legitimate call site for reading the
    ///    real environment; `caps::detect` itself stays a pure function of its input for tests.
    /// 3. Build `Terminal::new(CrosstermBackend::new(io::stdout()))?`.
    /// 4. Build the `(MessageSender, mpsc::UnboundedReceiver<AppMessage>)` pair
    ///    (`mpsc::unbounded_channel()`), and a `shutdown: Arc<Notify>` shared with the task
    ///    `install_signal_handlers` spawns — `tokio::spawn` that task here, cloning `shutdown`
    ///    and whatever this method needs to call the same restore path `Runtime::drop` uses (do
    ///    not duplicate the raw-mode/alt-screen teardown in two places; factor it so both the
    ///    signal task and `Drop` call one function).
    /// 5. Return `(Runtime { .. }, MessageSender { tx })`.
    pub async fn start(clock: Arc<dyn Clock>, theme: Theme) -> Result<(Runtime, MessageSender), RuntimeError> {
        let _ = (clock, theme);
        todo!("stand up the terminal and signal handlers per the IMPL note above")
    }

    /// Run the event loop until a termination signal or `root` requests exit, redrawing after
    /// every event `root` consumed and on every tick.
    ///
    /// IMPL: `tokio::select!` over:
    /// - `crossterm::event::EventStream` (requires the `event-stream` crossterm feature, already
    ///   enabled) — map `crossterm::event::Event::{Key,Mouse,Paste,FocusGained,FocusLost}` to
    ///   `Event::Input(InputEvent::..)` and `crossterm::event::Event::Resize` to `Event::Resize`.
    /// - `tokio::time::interval(self.tick_interval)` — emit
    ///   `Event::Tick { at: self.clock.now() }`. Note: `self.clock.now()`, never
    ///   `std::time::Instant::now()` — the workspace hygiene check forbids the latter outside
    ///   `tm-types::clock`, and replay depends on every timestamp coming from the injected clock.
    /// - `self.messages.recv()` — emit `Event::App(msg)`; a `None` here means every
    ///   `MessageSender` was dropped, which is `RuntimeError::ChannelClosed`.
    /// - `self.shutdown.notified()` — exit the loop (a termination signal fired).
    ///
    /// Dispatch each `Event` via `self.focus.dispatch(root, &event, &ctx)` (`component.rs`'s
    /// stub), where `ctx` is a fresh `FrameContext` built from `self.theme`/`self.caps`/
    /// `self.clock`/`self.focus.state()` for that iteration. Redraw with
    /// `self.terminal.draw(|frame| root.render(frame.area(), frame.buffer_mut(), &ctx))?` after
    /// any iteration that changed something (a consumed event, a tick, a resize) — not
    /// unconditionally, so an unhandled key does not force a redraw.
    pub async fn run(&mut self, root: &mut dyn Component) -> Result<(), RuntimeError> {
        let _ = root;
        todo!("drive the event loop per the IMPL note above")
    }

    /// Leave the alternate screen and disable raw mode. Idempotent and infallible-in-effect: any
    /// underlying I/O error is logged, not propagated, since this runs during shutdown —
    /// including from the signal-handling path — where panicking is exactly the bug D-002 calls
    /// out ("a killed process or closed SSH session bypasses unwinding").
    ///
    /// IMPL: `crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)`, then
    /// `crossterm::terminal::disable_raw_mode()`; if mouse capture was enabled in `start`, disable
    /// it first. Share this function between `Drop::drop` and the signal-handling task rather
    /// than duplicating the sequence.
    fn restore_terminal(&mut self) {
        todo!("reverse the raw-mode/alt-screen setup per the IMPL note above")
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.restore_terminal();
    }
}

/// Install handlers for `SIGTERM`, `SIGHUP`, `SIGTSTP`, and `SIGCONT`, notifying `shutdown` when
/// a termination signal (`SIGTERM`/`SIGHUP`) requests the event loop stop.
///
/// IMPL:
/// - `SIGTERM`/`SIGHUP`: `tokio::signal::unix::signal(SignalKind::terminate())` and
///   `signal(SignalKind::hangup())` (both named constructors exist on `tokio::signal::unix::
///   SignalKind`, no extra dependency needed). On either firing: run the same restore path
///   `Runtime::restore_terminal` uses, then `shutdown.notify_waiters()` so `Runtime::run` exits.
/// - `SIGTSTP`/`SIGCONT` are *not* named on `tokio::signal::unix::SignalKind` — only
///   `SignalKind::from_raw(i32)` reaches them, and this crate deliberately has no `libc`
///   dependency for the platform constant. `SIGTSTP` is `20` and `SIGCONT` is `18` on Linux,
///   `20` and `19` on macOS (Darwin) respectively; gate the literal on `cfg(target_os)` rather
///   than hard-coding one platform, and flag rather than guess if a third Unix target this
///   workspace supports needs a third value. On `SIGTSTP`: run the restore path, then re-raise
///   `SIGTSTP` on this process with the default disposition (`signal(SIGTSTP, SIG_DFL)` then
///   `raise(SIGTSTP)` in C terms) so the process actually stops — a caught signal does not stop
///   the process by itself, and this crate has no `libc::raise` either, so this step may need to
///   go back to whoever owns this crate's dependency list rather than being solved silently. On
///   `SIGCONT`: redo `start`'s raw-mode/alternate-screen setup and force a full redraw, since the
///   terminal's contents are undefined after a suspend.
/// - This function is Unix-only (`cfg(unix)`); `Runtime::start` needs a no-op fallback for other
///   targets (there is no signal-based teardown hazard on Windows the way there is on Unix).
#[cfg(unix)]
pub async fn install_signal_handlers(shutdown: Arc<Notify>) -> Result<(), RuntimeError> {
    let _ = shutdown;
    todo!("install SIGTERM/SIGHUP/SIGTSTP/SIGCONT handlers per the IMPL note above")
}
