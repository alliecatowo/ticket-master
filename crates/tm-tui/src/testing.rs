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

use std::io;
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, PtyPair};
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
    pub fn render_lines(&mut self, component: &dyn Component, ctx: &FrameContext<'_>) -> Vec<String> {
        let _ = (component, ctx);
        todo!("draw `component` into self.terminal and flatten the buffer per the IMPL note above")
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
        let _ = (component, ctx);
        todo!("draw `component` and coalesce same-styled runs per the IMPL note above")
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
        let _ = (component, event, ctx);
        todo!("dispatch `event` then render per the IMPL note above")
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
}

impl PtyHarness {
    /// Spawn `command` inside a `cols`x`rows` pty.
    ///
    /// IMPL: `portable_pty::native_pty_system().openpty(PtySize { rows, cols, .. })`, then
    /// `pair.slave.spawn_command(command)`. Keep `pair.master`'s reader/writer accessible for
    /// `screen()`/input helpers to be added alongside this stub as needed.
    pub fn spawn(command: CommandBuilder, cols: u16, rows: u16) -> io::Result<Self> {
        let _ = (command, cols, rows);
        todo!("open a pty and spawn `command` per the IMPL note above")
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
        todo!("read and interpret the pty's output per the IMPL note above")
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
        let _ = (pattern, timeout);
        todo!("poll `screen()` for `pattern` per the IMPL note above")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_construction_does_not_panic() {
        let _harness = Harness::new(80, 24);
    }
}
