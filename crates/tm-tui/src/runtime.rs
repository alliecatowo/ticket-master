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

/// How long [`Runtime::run`] waits after the *last* `Event::Resize` before flushing it as one
/// redraw. A drag-resize fires a burst of `Event::Resize`s in quick succession; debouncing (reset
/// on every new resize, fire once the burst goes quiet) coalesces that burst into a single redraw
/// instead of one per intermediate size, per D-002 ("Resize ... handled without a redraw storm").
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(60);

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

/// The pure coalescing policy behind [`RESIZE_DEBOUNCE`]: remembers only the *latest* pending
/// resize, so a burst of `Event::Resize`s collapses to the one size that mattered by the time the
/// debounce timer (owned by `Runtime::run`, not this struct) fires. Kept separate from the
/// `tokio::time::Sleep` that drives it so the coalescing policy itself is unit-testable without a
/// runtime.
#[derive(Debug, Default, PartialEq, Eq)]
struct ResizeCoalescer {
    pending: Option<(u16, u16)>,
}

impl ResizeCoalescer {
    /// Record a resize, overwriting whatever was pending — only the most recent size in a burst
    /// is worth redrawing to.
    fn on_resize(&mut self, width: u16, height: u16) {
        self.pending = Some((width, height));
    }

    /// True while a resize is waiting to be flushed, i.e. whether `Runtime::run`'s debounce timer
    /// should be polled at all this iteration.
    fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Take the pending resize, if any, clearing it.
    fn take(&mut self) -> Option<(u16, u16)> {
        self.pending.take()
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

    /// A clone of this runtime's shutdown notifier, for application code (e.g. a quit
    /// keybinding on the root [`ComponentParent`]) that wants to end [`Runtime::run`]'s event
    /// loop the same way a `SIGTERM`/`SIGHUP` does, rather than the runtime needing to know about
    /// any particular key.
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.shutdown)
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
        // `caps::probe` is *not* called here, deliberately: it needs to read the OSC 11/kitty/DA1
        // reply off the real stdin, and the only `AsyncRead` available for that is
        // `tokio::io::stdin()`. On Unix, `tokio::io::stdin()` is backed by a dedicated blocking
        // OS thread that keeps issuing `read()` on fd 0 for the rest of the process, even after
        // the future using it is dropped (e.g. on `caps::PROBE_TIMEOUT`, which is the *expected*
        // outcome whenever nothing answers the probe — a plain terminal, tmux, or, as here, no
        // terminal at all). That background thread then wins every future race for stdin bytes
        // against the `EventStream` this same `Runtime` constructs in `run` below, so any key the
        // human ever presses is silently swallowed instead of reaching the event loop — confirmed
        // with a minimal repro (`tokio::io::stdin()` read, dropped on timeout, then
        // `EventStream::next()` never resolves for a real keypress even 20s later) before this
        // was fixed. This is not new in this change: it predates `tm-cli` ever driving `Runtime`
        // with real keyboard input, which is exactly why no earlier test caught it — only the
        // signal-handling tests exercised this runtime before `tm-cli`'s `tui_launch` PTY tests.
        //
        // `caps::probe` itself is untouched and still fully tested against a `tokio::io::duplex`
        // pair (see `caps.rs`'s own tests) — the bug was this call site's choice of reader, not
        // the function. Until there is a way to read the reply through the same internal reader
        // `EventStream` uses (crossterm keeps that reader `pub(crate)`; its own
        // `supports_keyboard_enhancement()` is the one probe it exposes publicly, and it has no
        // equivalent for the OSC 11 truecolor query `caps::probe` also sends), detection here is
        // env-only via `caps::detect`. D-002's "truecolor over SSH/tmux" bar is met exactly as far
        // as `COLORTERM`/`TERM` get forwarded, same as every env-only fallback path already
        // covered by `caps.rs`'s own tests — not the stronger guarantee an actual round trip would
        // give, which is why this is called out here rather than silently downgraded.
        let caps = caps::detect(&env, synchronized_output, mouse_enabled);
        // Degrade once, here, rather than leaving every component to call `Theme::degraded`
        // itself: `NO_COLOR`/`TERM=dumb` (folded into `caps.color` by `caps::detect`) must
        // hold end to end, not just be detected and then ignored by whatever `FrameContext` hands
        // down. `FrameContext::theme` is always this already-degraded theme from here on.
        let theme = theme.degraded(&caps);

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
        // Compute tab order/focus before the first frame: without this, `self.focus.state()`
        // stays empty forever (nothing ever calls `rebuild`), so every widget's
        // `ctx.focus.is_focused(self.id)` check is always false and the whole tree is inert to
        // input.
        self.focus.rebuild(&*root);

        let mut input = EventStream::new();
        let mut ticker = tokio::time::interval(self.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; that is not a real elapsed interval and would just
        // force a redundant redraw before anything has happened.
        ticker.tick().await;

        // See `ResizeCoalescer`'s docs: `resize_timer` is rebuilt (not `.reset()`) on every new
        // `Event::Resize`, so it always fires `RESIZE_DEBOUNCE` after the *last* one in a burst.
        // Its guard (`if resize.is_pending()`) keeps this idle placeholder from ever being polled
        // before the first real resize.
        let mut resize = ResizeCoalescer::default();
        let mut resize_timer = Box::pin(tokio::time::sleep(Duration::from_secs(0)));

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

                // The debounce window since the last `Event::Resize` elapsed with nothing newer
                // arriving: flush the one redraw a whole drag-resize burst earned, instead of one
                // per intermediate size (D-002, "Resize ... handled without a redraw storm").
                // Guarded so this branch (and therefore `resize_timer`) is never polled while no
                // resize is pending.
                _ = &mut resize_timer, if resize.is_pending() => {
                    if resize.take().is_some() {
                        needs_redraw = true;
                    }
                }

                maybe_event = input.next() => {
                    match maybe_event {
                        Some(Ok(ct_event)) => {
                            let event = translate(ct_event);
                            if let Event::Resize { width, height } = event {
                                // Debounced above rather than dispatched/redrawn immediately:
                                // ratatui's own `Terminal::draw` autoresizes against the real
                                // backend size on the next draw regardless, so this coalescing
                                // only controls *how many* redraws a resize burst costs, not
                                // whether the final frame is drawn at the right size.
                                resize.on_resize(width, height);
                                resize_timer = Box::pin(tokio::time::sleep(RESIZE_DEBOUNCE));
                            } else {
                                let ctx = FrameContext {
                                    theme: &self.theme,
                                    caps: &self.caps,
                                    clock: self.clock.as_ref(),
                                    focus: self.focus.state(),
                                };
                                let propagation = self.focus.dispatch(root, &event, &ctx);
                                needs_redraw = propagation.is_consumed();
                            }
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
                // `notify_one`, not `notify_waiters`: `Runtime::run`'s select loop reconstructs
                // its `shutdown.notified()` listener fresh every iteration, so a `notify_waiters`
                // fired between iterations (nobody currently registered) would be silently lost.
                // `notify_one` stores a permit in exactly that case, so the next listener
                // consumes it immediately instead of blocking forever — see the same reasoning on
                // `tm-cli`'s `App::handle_event` quit-key path, which hits this race
                // deterministically rather than just theoretically.
                shutdown.notify_one();
            }
            _ = sighup.recv() => {
                teardown_terminal(mouse_enabled);
                shutdown.notify_one();
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
                // Same `notify_one` reasoning as `shutdown` above: `resume`'s listener is also
                // rebuilt every loop iteration.
                resume.notify_one();
            }
        }
    }
}

#[cfg(test)]
mod resize_coalescer_tests {
    use super::ResizeCoalescer;

    #[test]
    fn fresh_coalescer_has_nothing_pending() {
        let coalescer = ResizeCoalescer::default();
        assert!(!coalescer.is_pending());
    }

    #[test]
    fn on_resize_makes_it_pending_and_take_clears_it() {
        let mut coalescer = ResizeCoalescer::default();
        coalescer.on_resize(80, 24);
        assert!(coalescer.is_pending());
        assert_eq!(coalescer.take(), Some((80, 24)));
        assert!(!coalescer.is_pending());
        assert_eq!(coalescer.take(), None);
    }

    #[test]
    fn a_burst_of_resizes_coalesces_to_only_the_last_one() {
        let mut coalescer = ResizeCoalescer::default();
        coalescer.on_resize(80, 24);
        coalescer.on_resize(81, 24);
        coalescer.on_resize(82, 25);
        // A drag-resize fires many intermediate sizes; only the final one is worth a redraw.
        assert_eq!(coalescer.take(), Some((82, 25)));
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
