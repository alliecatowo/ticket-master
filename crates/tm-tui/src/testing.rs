//! The TUI test harness, in the two layers D-002 lays out.
//!
//! - [`Harness`] drives a [`Component`] against ratatui's in-memory `TestBackend` and renders to
//!   a plain string grid suitable for `insta` snapshotting — fast, no real terminal, one process
//!   per test.
//! - The event-loop and signal tests `TestBackend` cannot reach (D-002: `SIGTERM`/`SIGHUP`
//!   restore, `SIGTSTP`/`SIGCONT`) spawn a real command inside a real pseudo-terminal via
//!   [`tm_pty::PtySession`] — `docs/audit-2026-09-18-fable.md` B-16's "done looks like" for this
//!   crate: this module used to carry its own `PtyHarness` (`portable-pty` + `vt100`, hand-rolled
//!   reader thread and screen parser) as a `cfg(test)`-gated duplicate of the same logic B-16
//!   extracted into `tm-pty` as a real, production-usable crate. `tm-pty` is now the one
//!   implementation; this crate depends on it as a `[dev-dependencies]` entry instead of
//!   re-deriving it. See `crates/tm-pty/src/session.rs`'s module doc for the bounded-memory and
//!   `Clock`-vs-real-wait reasoning that implementation follows.
//!
//! VHS-tape visual-regression gates on the highest-value screens (D-002) are deliberately out of
//! scope for this module: they run out-of-process against a built `tm` binary, not against this
//! crate's Rust test suite.
//!
//! # Why this module is `#[cfg(test)]`-gated
//!
//! `insta` is a `[dev-dependencies]` entry, so Cargo does not link it into the plain library
//! build — only into the `cargo test` unittests binary, where `cfg(test)` is true for the whole
//! crate (see `lib.rs`). That makes [`Harness`] reachable from any sibling module's
//! `#[cfg(test)] mod tests` (e.g. `widgets_data::table`'s own tests), which covers the
//! per-screen `insta` snapshot layer D-002 asks for.
//!
//! It does **not** make the tests below reachable from a separate integration-test binary under
//! `crates/tm-tui/tests/*.rs`: those link against the *non*-`cfg(test)` build of this crate, so a
//! `cfg(test)`-gated module does not exist for them, even though integration tests do get their
//! own access to `[dev-dependencies]` — including `tm-pty` directly, which is exactly how
//! `crates/tm-cli/tests/support/mod.rs` reaches the same real-pty behaviour for `tm-cli`'s own
//! integration tests instead of carrying a fourth copy of this logic.

// Redundant with `#[cfg(test)] pub mod testing;` in lib.rs, but stated here so the fact that this
// whole file is test-only is visible in the file itself.
#![cfg(test)]

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use tm_pty::PtySession;
    use tm_types::SystemClock;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    /// `PATH` alone, resolved from this test process's real environment — `PtySession::spawn`
    /// clears the child's environment and expects an explicit allowlist, so a spawned `echo`
    /// otherwise cannot even be resolved.
    fn default_env() -> Vec<(String, String)> {
        vec![(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        )]
    }

    #[test]
    fn harness_construction_does_not_panic() {
        let _harness = Harness::new(80, 24);
    }

    #[test]
    fn pty_harness_captures_a_child_process_output() {
        let mut pty = PtySession::spawn(
            &argv(&["echo", "hello-from-the-pty"]),
            None,
            &default_env(),
            40,
            10,
            Arc::new(SystemClock),
        )
        .expect("spawning `echo` in a pty");

        let outcome = pty.expect("hello-from-the-pty", Duration::from_secs(5));

        assert!(
            outcome.matched(),
            "child output should appear on the parsed screen, got: {:?}",
            outcome.screen()
        );
    }

    #[test]
    fn pty_screen_has_one_entry_per_row() {
        let mut pty = PtySession::spawn(
            &argv(&["echo", "x"]),
            None,
            &default_env(),
            40,
            10,
            Arc::new(SystemClock),
        )
        .expect("spawning `echo` in a pty");
        let _ = pty.expect("x", Duration::from_secs(5));
        // Blank rows are kept so callers can index by row rather than guessing which were elided.
        assert_eq!(pty.screen().len(), 10);
    }

    #[test]
    fn wait_for_returns_the_screen_on_timeout_rather_than_only_an_error() {
        // A timeout must still show what was on screen — a bare error tells you nothing about
        // why the expected text never appeared.
        let mut pty = PtySession::spawn(
            &argv(&["echo", "present"]),
            None,
            &default_env(),
            40,
            10,
            Arc::new(SystemClock),
        )
        .expect("spawning `echo` in a pty");

        let outcome = pty.expect("never-printed", Duration::from_millis(200));
        assert!(!outcome.matched(), "this pattern was never printed");
        assert_eq!(outcome.screen().len(), 10);
    }
}
