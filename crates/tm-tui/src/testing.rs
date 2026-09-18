//! The TUI test harness, in the two layers D-002 lays out.
//!
//! - [`Harness`] drives a [`Component`] against ratatui's in-memory `TestBackend` and renders to
//!   a plain string grid suitable for `insta` snapshotting — fast, no real terminal, one process
//!   per test.
//! - [`PtyHarness`] spawns a real command inside a pseudo-terminal via `portable-pty`, for the
//!   event-loop and signal behaviour (`SIGTERM`/`SIGHUP` restore, `SIGTSTP`/`SIGCONT`) that only
//!   a real tty triggers and `TestBackend` cannot reach.
//!
//! VHS-tape visual-regression gates on the highest-value screens (D-002) are deliberately out of
//! scope for this module: they run out-of-process against a built `tm` binary, not against this
//! crate's Rust test suite.
//!
//! # Why this module is `#[cfg(test)]`-gated
//!
//! `insta`, `portable-pty` and `vt100` are `[dev-dependencies]`, so Cargo does not link them into the
//! plain library build — only into the `cargo test` unittests binary, where `cfg(test)` is true
//! for the whole crate (see `lib.rs`). That makes [`Harness`] reachable from any sibling module's
//! `#[cfg(test)] mod tests` (e.g. `widgets_data::table`'s own tests), which covers the
//! per-screen `insta` snapshot layer D-002 asks for.
//!
//! It does **not** make [`PtyHarness`] reachable from a separate integration-test binary under
//! `crates/tm-tui/tests/*.rs`: those link against the *non*-`cfg(test)` build of this crate, so a
//! `cfg(test)`-gated module does not exist for them, even though integration tests do get their
//! own access to `[dev-dependencies]`. If the event-loop/signal tests need to live there (spawning
//! a real compiled binary is usually cleaner as an integration test than a unit test), write the
//! pty-driving helper directly under `tests/support/` instead of trying to import it from here —
//! or, if sharing this exact code is worth it, promote `portable-pty`/`vt100`/`insta` to optional
//! regular dependencies behind a feature.
//!
//! [`PtyHarness::screen`] parses the child's output with `vt100` rather than a hand-rolled subset
//! parser: this harness gates the `SIGTERM`/`SIGHUP`/`SIGTSTP` behaviour of D-002, and a parser
//! that mishandled an escape sequence would make those tests flaky in precisely the cases they
//! exist to catch.

// Redundant with `#[cfg(test)] pub mod testing;` in lib.rs, but stated here so the fact that this
// whole file is test-only is visible in the file itself — and so `xtask verify`'s hygiene scan,
// which reads one file at a time and cannot see lib.rs's gate, treats `PtyHarness::wait_for`'s
// wall-clock timeout as the test code it is rather than a non-deterministic production clock read.
#![cfg(test)]

use std::io;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, PtyPair, PtySize};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use crate::component::{Component, FrameContext};
use crate::event::Event;

/// Renders a [`Component`] into an in-memory buffer for `insta` snapshotting, without touching a
/// real terminal.
pub struct Harness {
    terminal: Terminal<TestBackend>,
}

impl Harness {
    /// A harness with a `width`x`height` virtual terminal.
    pub fn new(width: u16, height: u16) -> Self {
        Harness {
            terminal: Terminal::new(TestBackend::new(width, height))
                .expect("TestBackend construction over an in-memory buffer cannot fail"),
        }
    }

    /// Render one frame of `component` and return it as plain text lines (styling discarded).
    ///
    /// IMPL: `self.terminal.draw(|frame| component.render(frame.area(), frame.buffer_mut(),
    /// ctx))?`, then read `self.terminal.backend().buffer()` and flatten it: for each row, join
    /// each cell's `.symbol()` in column order into one `String`, trimming trailing blank cells
    /// only if the widget under test does not care about trailing whitespace (most snapshot
    /// comparisons read better without it, but note the tradeoff rather than silently deciding
    /// it). Panic (via `.expect` with a real message, not a bare `.unwrap()`) on a draw error —
    /// `TestBackend` failing to draw is a bug in the test, not a recoverable condition.
    pub fn render_lines(
        &mut self,
        component: &dyn Component,
        ctx: &FrameContext<'_>,
    ) -> Vec<String> {
        self.draw(component, ctx);

        let buffer = self.terminal.backend().buffer();
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                let mut row = String::new();
                for x in 0..area.width {
                    row.push_str(buffer[(area.x + x, area.y + y)].symbol());
                }
                // Trailing blanks are padding to the virtual terminal's width, not content, and
                // they make every snapshot a rectangle of trailing spaces. The tradeoff: a test
                // that genuinely cares about trailing whitespace must use `render_styled`, which
                // keeps every cell.
                row.trim_end().to_string()
            })
            .collect()
    }

    /// Draw one frame of `component` into the in-memory terminal.
    fn draw(&mut self, component: &dyn Component, ctx: &FrameContext<'_>) {
        self.terminal
            .draw(|frame| {
                let area = frame.area();
                component.render(area, frame.buffer_mut(), ctx);
            })
            .expect("drawing into TestBackend's in-memory buffer cannot fail");
    }

    /// A second rendering that keeps per-cell style, for tests that assert on colour/attributes
    /// rather than only text — e.g. confirming `caps::degrade_color` actually changed what a
    /// widget drew.
    ///
    /// IMPL: same draw as `render_lines`, but return each row as
    /// `Vec<(String, ratatui_core::style::Style)>` — one entry per *run* of same-styled cells
    /// (coalesce adjacent cells sharing a style into one string+style pair) rather than one entry
    /// per cell, so snapshots stay readable.
    pub fn render_styled(
        &mut self,
        component: &dyn Component,
        ctx: &FrameContext<'_>,
    ) -> Vec<Vec<(String, ratatui_core::style::Style)>> {
        self.draw(component, ctx);

        let buffer = self.terminal.backend().buffer();
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                let mut runs: Vec<(String, ratatui_core::style::Style)> = Vec::new();
                for x in 0..area.width {
                    let cell = &buffer[(area.x + x, area.y + y)];
                    let style = cell.style();
                    // Coalesce adjacent cells sharing a style so a snapshot reads as
                    // ("hello", red) rather than five separate single-character entries.
                    match runs.last_mut() {
                        Some((text, run_style)) if *run_style == style => {
                            text.push_str(cell.symbol())
                        }
                        _ => runs.push((cell.symbol().to_string(), style)),
                    }
                }
                runs
            })
            .collect()
    }

    /// Feed `event` to `component` and immediately render — the common "press a key, see what
    /// changed" test shape.
    ///
    /// IMPL: `component.handle_event(event, ctx)` (the return value is available to assert on
    /// separately if a test needs it — consider adding a variant that returns
    /// `(Propagation, Vec<String>)` if `render_lines`-only proves insufficient), then
    /// `self.render_lines(component, ctx)`.
    pub fn send_and_render(
        &mut self,
        component: &mut dyn Component,
        event: &Event,
        ctx: &FrameContext<'_>,
    ) -> Vec<String> {
        let _propagation = component.handle_event(event, ctx);
        self.render_lines(&*component, ctx)
    }

    /// Like [`Harness::send_and_render`], but also returns whether the component consumed the
    /// event — for tests asserting on routing (e.g. that an unfocused widget propagates).
    pub fn send_and_render_with_propagation(
        &mut self,
        component: &mut dyn Component,
        event: &Event,
        ctx: &FrameContext<'_>,
    ) -> (crate::event::Propagation, Vec<String>) {
        let propagation = component.handle_event(event, ctx);
        (propagation, self.render_lines(&*component, ctx))
    }
}

/// A real terminal session driving a child process, for the event-loop and signal tests
/// `TestBackend` cannot reach (D-002: `SIGTERM`/`SIGHUP` restore, `SIGTSTP`/`SIGCONT`).
///
/// IMPL, at the struct level: `portable_pty::native_pty_system()` gives a `PtySystem` whose
/// `openpty` returns this `pair` plus a `Box<dyn Child + Send + Sync>` from
/// `pair.slave.spawn_command(command)`. Reading `pair.master`'s output and writing input both go
/// through `PtyPair::master`'s `try_clone_reader`/`take_writer`.
pub struct PtyHarness {
    pair: PtyPair,
    child: Box<dyn Child + Send + Sync>,
    /// Everything the child has written so far. A reader thread owns the pty's read side and
    /// appends here, because a pty read blocks until the child writes — polling it inline from
    /// `screen()` would deadlock a test whose child is idle and waiting for input.
    output: Arc<Mutex<Vec<u8>>>,
    /// Bytes already fed to `parser`, so each `screen()` only processes what is new.
    consumed: usize,
    parser: vt100::Parser,
}

impl PtyHarness {
    /// Spawn `command` inside a `cols`x`rows` pty.
    ///
    /// IMPL: `portable_pty::native_pty_system().openpty(PtySize { rows, cols, .. })`, then
    /// `pair.slave.spawn_command(command)`. Keep `pair.master`'s reader/writer accessible for
    /// `screen()`/input helpers to be added alongside this stub as needed.
    pub fn spawn(command: CommandBuilder, cols: u16, rows: u16) -> io::Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| io::Error::other(format!("opening a pty: {e}")))?;

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| io::Error::other(format!("spawning the child in the pty: {e}")))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| io::Error::other(format!("cloning the pty reader: {e}")))?;

        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        // Detached deliberately: the thread ends when the child closes the pty, and a test that
        // fails should not also hang waiting to join it.
        thread::spawn(move || {
            let mut reader = reader;
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut buf) = sink.lock() {
                            buf.extend_from_slice(&chunk[..n]);
                        } else {
                            break;
                        }
                    }
                }
            }
        });

        Ok(PtyHarness {
            pair,
            child,
            output,
            consumed: 0,
            parser: vt100::Parser::new(rows, cols, 0),
        })
    }

    /// The child's rendered screen right now, as plain text lines.
    ///
    /// IMPL: this needs a VT100-ish interpreter over the pty's raw output (cursor moves, erases,
    /// at minimum) — `portable-pty` hands back bytes, not a parsed screen, and this crate has no
    /// terminal-emulator dependency to lean on (SPEC.md §19's `pty.screen()` sandbox tool solves
    /// the same problem for a different crate; check whether it can be shared before writing a
    /// second parser). If no shared parser is available, implement the minimal subset `tm`'s own
    /// output actually uses rather than a general VT100 emulator, and say so in a doc comment —
    /// do not silently claim full VT100 fidelity.
    pub fn screen(&mut self) -> Vec<String> {
        // Feed only the bytes that arrived since the last call: vt100::Parser is stateful, so
        // re-processing the whole buffer would replay every escape sequence from the start.
        if let Ok(buf) = self.output.lock() {
            if buf.len() > self.consumed {
                self.parser.process(&buf[self.consumed..]);
                self.consumed = buf.len();
            }
        }

        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        // One entry per screen row, always — including blank ones — so a caller can index by row
        // and so an assertion failure shows the real geometry rather than a collapsed list.
        (0..rows)
            .map(|row| {
                screen
                    .contents_between(row, 0, row, cols)
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Write `input` to the child's terminal, as if typed.
    pub fn write(&mut self, input: &[u8]) -> io::Result<()> {
        let mut writer = self
            .pair
            .master
            .take_writer()
            .map_err(|e| io::Error::other(format!("taking the pty writer: {e}")))?;
        writer.write_all(input)?;
        writer.flush()
    }

    /// Send `signal` to the child process — the entry point for the `SIGTERM`/`SIGHUP`/`SIGTSTP`
    /// restore tests D-002 requires, which only a real process in a real pty can exercise.
    ///
    /// Shells out to `kill(1)` rather than calling `libc::kill`, because this crate is
    /// `#![forbid(unsafe_code)]` with no `libc`/`nix` dependency (see `runtime.rs`, which makes
    /// the same choice for the same reason). Delivering a real signal is the point of this
    /// harness, so the signal must be real rather than a simulated in-process notification.
    pub fn signal(&mut self, signal: i32) -> io::Result<()> {
        let Some(pid) = self.child.process_id() else {
            return Err(io::Error::other("child has already exited"));
        };
        let status = std::process::Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(pid.to_string())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("kill -{signal} {pid} failed")))
        }
    }

    /// Wait for the child to exit, returning whether it exited successfully.
    pub fn wait(&mut self) -> io::Result<bool> {
        self.child
            .wait()
            .map(|status| status.success())
            .map_err(|e| io::Error::other(format!("waiting on the child: {e}")))
    }

    /// Block until `screen()` contains `pattern` or `timeout` elapses, returning the screen
    /// either way so a timeout assertion failure shows what was actually on screen (this is the
    /// same "diagnosis over bare error" shape SPEC.md §19 asks of `pty.expect`).
    ///
    /// IMPL: poll `screen()` on a short interval (e.g. every 20ms) until it contains `pattern` or
    /// `timeout` elapses; this is one of the few places in this crate where a direct
    /// `std::time::Instant` read is legitimate (a test's own wall-clock timeout, not production
    /// replay state) — confirm against the hygiene check's test-region carve-out rather than
    /// assuming.
    pub fn wait_for(&mut self, pattern: &str, timeout: Duration) -> io::Result<Vec<String>> {
        // A test's own wall-clock timeout, not replay state — the hygiene check's test-region
        // carve-out is what makes a direct `Instant` read legitimate here.
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self.screen();
            if screen.iter().any(|line| line.contains(pattern)) {
                return Ok(screen);
            }
            if Instant::now() >= deadline {
                // Return the screen rather than a bare timeout error, so the assertion failure
                // shows what was actually displayed instead of only that nothing matched.
                return Ok(screen);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_construction_does_not_panic() {
        let _harness = Harness::new(80, 24);
    }

    #[test]
    fn pty_harness_captures_a_child_process_output() {
        let mut command = CommandBuilder::new("echo");
        command.arg("hello-from-the-pty");
        let mut pty = PtyHarness::spawn(command, 40, 10).expect("spawning `echo` in a pty");

        let screen = pty
            .wait_for("hello-from-the-pty", Duration::from_secs(5))
            .expect("polling the pty screen");

        assert!(
            screen.iter().any(|l| l.contains("hello-from-the-pty")),
            "child output should appear on the parsed screen, got: {screen:?}"
        );
    }

    #[test]
    fn pty_screen_has_one_entry_per_row() {
        let mut command = CommandBuilder::new("echo");
        command.arg("x");
        let mut pty = PtyHarness::spawn(command, 40, 10).expect("spawning `echo` in a pty");
        let _ = pty.wait_for("x", Duration::from_secs(5));
        // Blank rows are kept so callers can index by row rather than guessing which were elided.
        assert_eq!(pty.screen().len(), 10);
    }

    #[test]
    fn wait_for_returns_the_screen_on_timeout_rather_than_only_an_error() {
        // A timeout must still show what was on screen — a bare error tells you nothing about
        // why the expected text never appeared.
        let mut command = CommandBuilder::new("echo");
        command.arg("present");
        let mut pty = PtyHarness::spawn(command, 40, 10).expect("spawning `echo` in a pty");

        let screen = pty
            .wait_for("never-printed", Duration::from_millis(200))
            .expect("a timeout is not an error, it returns the screen");
        assert_eq!(screen.len(), 10);
    }
}
