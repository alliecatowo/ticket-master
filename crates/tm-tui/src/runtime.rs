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
//! # The `SIGTSTP` compromise (read before touching the signal task)
//!
//! The textbook way to make `^Z` actually stop a process that has its own `SIGTSTP` handler
//! installed is: reset the handler to `SIG_DFL`, then `raise(SIGTSTP)` — the kernel's default
//! disposition for `SIGTSTP` is "stop the process", and a caught signal never triggers that
//! default action by itself. Both of those calls are raw libc FFI (`signal(2)`/`raise(3)`); this
//! crate has `#![forbid(unsafe_code)]` and no `libc`/`nix` dependency (hard rule: this file may
//! not add one). Re-sending a *caught* `SIGTSTP` to ourselves would just re-enter our own signal
//! stream — the kernel would not stop us.
//!
//! Instead, on `SIGTSTP` this module tears the terminal down and sends **`SIGSTOP`** to itself
//! via a real child `kill` process (`std::process::Command`, no FFI). `SIGSTOP` cannot be
//! caught, blocked or ignored — the kernel *always* stops the process on receipt, which is the
//! actual property we need ("the shell takes over"). The shell's `fg`/`bg` job control reacts to
//! *any* stopped process the same way regardless of which stop signal put it there, and the
//! kernel raises a real `SIGCONT` on resume either way, which this module's `SIGCONT` handler
//! picks up to redraw. The only externally-visible difference from the textbook approach is
//! cosmetic (some `ps`/`jobs` output distinguishes the stop signal); the job-control behavior a
//! user experiences is identical. If a real re-raised `SIGTSTP` is ever required, that needs
//! `libc::{signal, raise}` added to this crate's dependencies — flag it to whoever owns
//! `Cargo.toml` rather than reaching for `unsafe` here.
//!
//! [`Buffer`]: ratatui_core::buffer::Buffer

use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture, EventStream};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, terminal};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tm_types::Clock;
use tokio::sync::{mpsc, Notify};

use crate::caps::{self, Capabilities};
use crate::component::{ComponentParent, FocusTree, FrameContext};
use crate::event::{AppMessage, Event, InputEvent};
use crate::theme::Theme;

/// The DEC private-mode 2026 "begin synchronized update" sequence. Wrapping a frame flush in
/// this (paired with [`SYNC_END`]) tells a terminal that honours mode 2026 to buffer the whole
/// update and paint it atomically, so a frame is never shown half-written.
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
/// The matching "end synchronized update" sequence for [`SYNC_BEGIN`].
const SYNC_END: &[u8] = b"\x1b[?2026l";

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
    /// The crossterm input stream ended (stdin closed) while the runtime was still running.
    #[error("the terminal input stream ended unexpectedly")]
    InputClosed,
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

/// Leave the alternate screen, disable mouse capture (if it was enabled) and raw mode.
///
/// Infallible-in-effect: every step is best-effort and logs rather than propagates, because this
/// runs from contexts where returning a `Result` has nowhere useful to go — `Drop::drop`, and the
/// signal-handling task, both of which may be racing process teardown itself. Idempotent: calling
/// this twice (e.g. once from a signal handler, once from `Drop` on the way out) is harmless —
/// `disable_raw_mode`/`LeaveAlternateScreen` on an already-restored terminal are no-ops in
/// practice on every backend crossterm supports.
fn teardown_terminal(mouse_enabled: bool) {
    let mut stdout = io::stdout();
    if mouse_enabled {
        if let Err(err) = execute!(stdout, DisableMouseCapture) {
            eprintln!("tm-tui: failed to disable mouse capture during teardown: {err}");
        }
    }
    if let Err(err) = execute!(stdout, LeaveAlternateScreen) {
        eprintln!("tm-tui: failed to leave alternate screen during teardown: {err}");
    }
    if let Err(err) = terminal::disable_raw_mode() {
        eprintln!("tm-tui: failed to disable raw mode during teardown: {err}");
    }
}

/// Enter raw mode, the alternate screen, and (if requested) mouse capture — the inverse of
/// [`teardown_terminal`]. Used both by [`Runtime::start`] and by the `SIGCONT` path, which must
/// redo this setup since the terminal's contents and mode are undefined after a suspend.
fn setup_terminal(want_mouse: bool) -> io::Result<bool> {
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mouse_enabled = if want_mouse {
        execute!(io::stdout(), EnableMouseCapture).is_ok()
    } else {
        false
    };
    Ok(mouse_enabled)
}

/// Translate one crossterm terminal event into this crate's [`Event`] currency.
///
/// `Event::Resize` is lifted out of `InputEvent` at exactly this boundary (see `event.rs`'s
/// module docs) — every other crossterm event kind maps straight into `InputEvent`.
fn translate(event: crossterm::event::Event) -> Event {
    match event {
        crossterm::event::Event::Key(key) => Event::Input(InputEvent::Key(key)),
        crossterm::event::Event::Mouse(mouse) => Event::Input(InputEvent::Mouse(mouse)),
        crossterm::event::Event::Paste(text) => Event::Input(InputEvent::Paste(text)),
        crossterm::event::Event::FocusGained => Event::Input(InputEvent::FocusGained),
        crossterm::event::Event::FocusLost => Event::Input(InputEvent::FocusLost),
        crossterm::event::Event::Resize(width, height) => Event::Resize { width, height },
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
    /// Signalled by `install_signal_handlers`'s task on `SIGCONT`; `run` selects on this to
    /// clear the diff buffer and force a full redraw, since the terminal's contents are
    /// undefined after a suspend.
    resume: Arc<Notify>,
    /// Whether mouse capture was actually enabled (the terminal confirmed the escape sequence),
    /// so suspend/resume and final teardown toggle exactly what was toggled on at startup.
    mouse_enabled: bool,
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
    pub async fn start(
        clock: Arc<dyn Clock>,
        theme: Theme,
    ) -> Result<(Runtime, MessageSender), RuntimeError> {
        let env: caps::Environment = std::env::vars().collect();
        let synchronized_output = caps::probe_synchronized_output(&env);

        let mouse_enabled = setup_terminal(true)?;
        // The OSC 11 / kitty-keyboard / DA1 round trip needs raw mode already active (no line
        // buffering/echo swallowing the reply) and stdin/stdout, which is why this happens here
        // rather than before `setup_terminal` above.
        let probe_reply = {
            let mut stdin = tokio::io::stdin();
            let mut stdout = tokio::io::stdout();
            caps::probe(&mut stdin, &mut stdout, caps::PROBE_TIMEOUT).await
        };
        let caps = caps::detect_with_probe(&env, probe_reply, synchronized_output, mouse_enabled);

        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

        let (tx, messages) = mpsc::unbounded_channel();
        let shutdown = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());

        #[cfg(unix)]
        {
            let shutdown = Arc::clone(&shutdown);
            let resume = Arc::clone(&resume);
            tokio::spawn(async move {
                if let Err(err) = install_signal_handlers(shutdown, resume, mouse_enabled).await {
                    eprintln!("tm-tui: signal handler task exited: {err}");
                }
            });
        }

        let runtime = Runtime {
            terminal,
            caps,
            theme,
            clock,
            focus: FocusTree::new(),
            messages,
            shutdown,
            resume,
            mouse_enabled,
            tick_interval: Duration::from_millis(100),
        };

        Ok((runtime, MessageSender { tx }))
    }

    /// Run the event loop until a termination signal or `root` requests exit, redrawing after
    /// every event `root` consumed and on every tick.
    pub async fn run(&mut self, root: &mut dyn ComponentParent) -> Result<(), RuntimeError> {
        let mut input = EventStream::new();
        let mut ticker = tokio::time::interval(self.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; that is not a real elapsed interval and would just
        // force a redundant redraw before anything has happened.
        ticker.tick().await;

        loop {
            // The `shutdown` branch below breaks out of the loop without ever reading this,
            // which is exactly the point (no point drawing a frame we are about to tear down
            // for) but reads as dead-store to the lint.
            #[allow(unused_assignments)]
            let mut needs_redraw = false;

            tokio::select! {
                biased;

                // A termination signal fired: stop driving the loop so `main` can return.
                _ = self.shutdown.notified() => {
                    break;
                }

                // Resumed from a suspend: the alt-screen contents are undefined now, so drop
                // ratatui's diff buffer and repaint everything from scratch.
                _ = self.resume.notified() => {
                    self.terminal.clear()?;
                    needs_redraw = true;
                }

                maybe_event = input.next() => {
                    match maybe_event {
                        Some(Ok(ct_event)) => {
                            let event = translate(ct_event);
                            let ctx = FrameContext {
                                theme: &self.theme,
                                caps: &self.caps,
                                clock: self.clock.as_ref(),
                                focus: self.focus.state(),
                            };
                            let propagation = self.focus.dispatch(root, &event, &ctx);
                            needs_redraw = propagation.is_consumed();
                        }
                        Some(Err(err)) => return Err(RuntimeError::Io(err)),
                        None => return Err(RuntimeError::InputClosed),
                    }
                }

                _ = ticker.tick() => {
                    let at = self.clock.now();
                    let ctx = FrameContext {
                        theme: &self.theme,
                        caps: &self.caps,
                        clock: self.clock.as_ref(),
                        focus: self.focus.state(),
                    };
                    // A tick's consumption is not what gates a redraw — animation frequently
                    // wants to repaint whether or not any component claimed the event.
                    let _ = self.focus.dispatch(root, &Event::Tick { at }, &ctx);
                    needs_redraw = true;
                }

                message = self.messages.recv() => {
                    match message {
                        Some(app_message) => {
                            let ctx = FrameContext {
                                theme: &self.theme,
                                caps: &self.caps,
                                clock: self.clock.as_ref(),
                                focus: self.focus.state(),
                            };
                            let propagation = self.focus.dispatch(root, &Event::App(app_message), &ctx);
                            needs_redraw = propagation.is_consumed();
                        }
                        None => return Err(RuntimeError::ChannelClosed),
                    }
                }
            }

            if needs_redraw {
                self.draw(root)?;
            }
        }

        Ok(())
    }

    /// Render one frame, wrapped in DEC 2026 synchronized output when `self.caps` confirms the
    /// terminal honours it, so a resize or a fast stream never shows a half-painted frame.
    fn draw(&mut self, root: &mut dyn ComponentParent) -> Result<(), RuntimeError> {
        let ctx = FrameContext {
            theme: &self.theme,
            caps: &self.caps,
            clock: self.clock.as_ref(),
            focus: self.focus.state(),
        };

        if self.caps.synchronized_output {
            io::stdout().write_all(SYNC_BEGIN)?;
        }
        self.terminal
            .draw(|frame| root.render(frame.area(), frame.buffer_mut(), &ctx))?;
        if self.caps.synchronized_output {
            let mut stdout = io::stdout();
            stdout.write_all(SYNC_END)?;
            stdout.flush()?;
        }
        Ok(())
    }

    /// Leave the alternate screen and disable raw mode. Idempotent and infallible-in-effect: any
    /// underlying I/O error is logged, not propagated, since this runs during shutdown —
    /// including from the signal-handling path — where panicking is exactly the bug D-002 calls
    /// out ("a killed process or closed SSH session bypasses unwinding").
    fn restore_terminal(&mut self) {
        teardown_terminal(self.mouse_enabled);
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.restore_terminal();
    }
}

/// `SIGTSTP`'s raw signal number, which `tokio::signal::unix::SignalKind` has no named
/// constructor for. See the module-level doc comment for why this module sends `SIGSTOP` rather
/// than re-raising this one.
#[cfg(target_os = "linux")]
const SIGTSTP: i32 = 20;
#[cfg(target_os = "macos")]
const SIGTSTP: i32 = 18;

/// `SIGCONT`'s raw signal number, which `tokio::signal::unix::SignalKind` has no named
/// constructor for.
#[cfg(target_os = "linux")]
const SIGCONT: i32 = 18;
#[cfg(target_os = "macos")]
const SIGCONT: i32 = 19;

/// `SIGSTOP`'s raw signal number.
///
/// This **does** vary by target, despite being easy to assume otherwise: Linux orders these
/// `CHLD=17, CONT=18, STOP=19, TSTP=20` while macOS orders them `STOP=17, TSTP=18, CONT=19,
/// CHLD=20`. Hardcoding Linux's `19` here sends macOS a `SIGCONT` — the exact opposite of
/// stopping — so this is a named per-target constant rather than a literal at the call site.
#[cfg(target_os = "linux")]
const SIGSTOP: i32 = 19;
#[cfg(target_os = "macos")]
const SIGSTOP: i32 = 17;

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
compile_error!(
    "tm-tui's SIGTSTP/SIGCONT raw signal numbers are only known for linux and macos; add this \
     target's values in runtime.rs rather than guessing"
);

/// Send `signal` (a raw signal number) to this process via a real child `kill` process.
///
/// This crate has `#![forbid(unsafe_code)]` and no `libc`/`nix` dependency, so there is no
/// `raise(3)` available; shelling out to `kill` is the only signal-delivery path open to safe
/// Rust here. `kill` is part of POSIX and present on every Unix target this module compiles for.
fn send_signal_to_self(signal: i32) -> io::Result<()> {
    let pid = std::process::id();
    let status = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "kill -{signal} {pid} exited with {status}"
        )));
    }
    Ok(())
}

/// Install handlers for `SIGTERM`, `SIGHUP`, `SIGTSTP`, and `SIGCONT`, notifying `shutdown` when
/// a termination signal (`SIGTERM`/`SIGHUP`) requests the event loop stop, and `resume` when the
/// terminal has been re-entered after a suspend.
///
/// Runs as its own long-lived task (spawned by `Runtime::start`) for the lifetime of the
/// process, independent of whether `Runtime::run`'s event loop is currently polling anything —
/// this is what makes teardown happen even if `run` is blocked or has not been called yet.
#[cfg(unix)]
pub async fn install_signal_handlers(
    shutdown: Arc<Notify>,
    resume: Arc<Notify>,
    mouse_enabled: bool,
) -> Result<(), RuntimeError> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigtstp = signal(SignalKind::from_raw(SIGTSTP))?;
    let mut sigcont = signal(SignalKind::from_raw(SIGCONT))?;

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                teardown_terminal(mouse_enabled);
                shutdown.notify_waiters();
            }
            _ = sighup.recv() => {
                teardown_terminal(mouse_enabled);
                shutdown.notify_waiters();
            }
            _ = sigtstp.recv() => {
                teardown_terminal(mouse_enabled);
                // Blocks this task (and, once the kernel delivers SIGSTOP, the whole process)
                // until a SIGCONT resumes us — see the module doc comment for why SIGSTOP
                // rather than a re-raised SIGTSTP. The kernel always raises a real SIGCONT on
                // resume, which the `sigcont` branch below picks up to redraw; there is
                // deliberately no redraw/setup logic here to avoid doing it twice.
                if let Err(err) = send_signal_to_self(SIGSTOP) {
                    eprintln!("tm-tui: failed to stop the process for SIGTSTP: {err}");
                }
            }
            _ = sigcont.recv() => {
                if let Err(err) = setup_terminal(mouse_enabled) {
                    eprintln!("tm-tui: failed to re-enter the terminal after SIGCONT: {err}");
                }
                resume.notify_waiters();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration as StdDuration;
    use tokio::time::timeout;

    fn send(signal: i32, pid: u32) {
        let status = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(pid.to_string())
            .status()
            .expect("the `kill` binary must be available to deliver a real signal in this test");
        assert!(status.success(), "kill -{signal} {pid} failed");
    }

    #[tokio::test]
    async fn sigterm_notifies_shutdown_via_a_real_signal() {
        let shutdown = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        tokio::spawn(install_signal_handlers(
            Arc::clone(&shutdown),
            Arc::clone(&resume),
            false,
        ));

        // Let the handler task actually register before the signal is sent.
        tokio::task::yield_now().await;
        send(15 /* SIGTERM */, std::process::id());

        timeout(StdDuration::from_secs(5), shutdown.notified())
            .await
            .expect("a real SIGTERM must wake `shutdown.notified()` within the timeout");
    }

    #[tokio::test]
    async fn sighup_notifies_shutdown_via_a_real_signal() {
        let shutdown = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        tokio::spawn(install_signal_handlers(
            Arc::clone(&shutdown),
            Arc::clone(&resume),
            false,
        ));

        tokio::task::yield_now().await;
        send(1 /* SIGHUP */, std::process::id());

        timeout(StdDuration::from_secs(5), shutdown.notified())
            .await
            .expect("a real SIGHUP must wake `shutdown.notified()` within the timeout");
    }

    #[tokio::test]
    async fn sigcont_notifies_resume_via_a_real_signal() {
        let shutdown = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        tokio::spawn(install_signal_handlers(
            Arc::clone(&shutdown),
            Arc::clone(&resume),
            false,
        ));

        tokio::task::yield_now().await;
        // A bare SIGCONT with no prior stop is exactly the case the handler must still redraw
        // for (e.g. a supervisor sending it defensively).
        send(SIGCONT, std::process::id());

        timeout(StdDuration::from_secs(5), resume.notified())
            .await
            .expect("a real SIGCONT must wake `resume.notified()` within the timeout");
    }

    /// The env var this test binary re-execs itself with to switch a single test function from
    /// "drive the signal path" (parent) to "be the thing the signal path is driven against"
    /// (child).
    const CHILD_MODE_ENV: &str = "TM_TUI_RUNTIME_SIGTSTP_TEST_CHILD";
    /// The line the child prints once its own signal handlers are installed and it is safe for
    /// the parent to start sending it signals.
    const CHILD_READY_LINE: &str = "tm-tui-test-child-ready";

    /// Drives the real `SIGTSTP`→`SIGSTOP`→(stopped)→`SIGCONT`→(resumed) path against a
    /// *separate real process* — not the test binary's own process, which sending SIGTSTP to
    /// would stop the whole test run. That separate process is this same test binary, re-exec'd
    /// with [`CHILD_MODE_ENV`] set so this same test function takes the child branch below
    /// instead of the parent (driving) branch.
    ///
    /// Sending the child a plain `SIGTSTP` and observing it actually reach the kernel's stopped
    /// state is exactly what earlier manual testing showed does *not* reliably happen from a
    /// naive child in this kind of sandboxed/non-interactive shell environment (job-control
    /// signals a background job inherits as ignored). Driving it through this crate's own
    /// `install_signal_handlers` — which converts a caught `SIGTSTP` into an explicit `SIGSTOP`
    /// precisely because that disposition cannot be inherited-as-ignored — is what makes this
    /// assertion hold regardless of that.
    #[test]
    fn sigtstp_then_sigcont_stop_and_resume_a_real_child_running_our_handler() {
        if std::env::var_os(CHILD_MODE_ENV).is_some() {
            child_main();
            return;
        }

        let exe =
            std::env::current_exe().expect("the running test binary's own path must be resolvable");
        let mut child = Command::new(exe)
            .arg("runtime::tests::sigtstp_then_sigcont_stop_and_resume_a_real_child_running_our_handler")
            .arg("--exact")
            .arg("--nocapture")
            .env(CHILD_MODE_ENV, "1")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawning a real child re-exec of this test binary must succeed");
        let pid = child.id();

        let mut reader = std::io::BufReader::new(
            child
                .stdout
                .take()
                .expect("the child's stdout must be captured"),
        );
        // `--nocapture` still shares stdout with the child test binary's own harness preamble
        // (e.g. "running 1 test"), so scan forward for our specific line rather than assuming
        // it is the first one.
        let mut saw_ready = false;
        for _ in 0..20 {
            let mut line = String::new();
            let bytes_read = std::io::BufRead::read_line(&mut reader, &mut line)
                .expect("reading the child's stdout must succeed");
            if bytes_read == 0 {
                break;
            }
            if line.trim() == CHILD_READY_LINE {
                saw_ready = true;
                break;
            }
        }
        assert!(
            saw_ready,
            "child must print its readiness line before the parent signals it"
        );

        // Use the per-target constant, not a literal: the literal `20` is SIGTSTP on Linux but
        // SIGCHLD on macOS, which the child ignores — so the test would fail on macOS while the
        // production bug it was meant to catch stayed invisible.
        send(SIGTSTP, pid);
        assert!(
            wait_until(StdDuration::from_secs(5), || is_stopped(pid)),
            "child must reach the kernel's stopped state after this crate's SIGTSTP handler runs"
        );

        send(SIGCONT, pid);
        assert!(
            wait_until(StdDuration::from_secs(5), || !is_stopped(pid)),
            "child must leave the stopped state after SIGCONT"
        );

        // Let the child's own SIGTERM handler (also under test above) shut it down cleanly
        // rather than killing it, so a failure here would show up as a non-zero exit rather
        // than being masked by a forced kill.
        send(15 /* SIGTERM */, pid);
        let status = child
            .wait()
            .expect("waiting for the child to exit after SIGTERM must succeed");
        assert!(status.success(), "child must exit cleanly: {status}");
    }

    /// The child half of [`sigtstp_then_sigcont_stop_and_resume_a_real_child_running_our_handler`]:
    /// install the real signal handlers, announce readiness, and block on `shutdown` exactly like
    /// production code would, so the parent is testing this module's actual behavior rather than
    /// a stand-in.
    fn child_main() {
        let runtime = tokio::runtime::Runtime::new()
            .expect("building a tokio runtime in the child process must succeed");
        runtime.block_on(async {
            let shutdown = Arc::new(Notify::new());
            let resume = Arc::new(Notify::new());
            tokio::spawn(install_signal_handlers(
                Arc::clone(&shutdown),
                Arc::clone(&resume),
                false,
            ));
            tokio::task::yield_now().await;

            println!("{CHILD_READY_LINE}");
            io::stdout().flush().ok();

            shutdown.notified().await;
        });
    }

    /// Poll `predicate` every 20ms until it is true or `timeout` elapses, returning whether it
    /// ever became true.
    fn wait_until(timeout: StdDuration, mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if predicate() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(StdDuration::from_millis(20));
        }
    }

    /// Read the child's process state from `ps` (`T` = stopped by a job-control signal on both
    /// Linux and macOS) — the simplest portable way to observe kernel process state from safe
    /// Rust without a `libc`/`procfs` dependency this crate does not have.
    fn is_stopped(pid: u32) -> bool {
        let output = Command::new("ps")
            .arg("-o")
            .arg("state=")
            .arg("-p")
            .arg(pid.to_string())
            .output()
            .expect("`ps` must be available to observe process state in this test");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .starts_with('T')
    }
}
