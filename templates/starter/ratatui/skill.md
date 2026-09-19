# Ratatui TUI conventions

This project is a terminal UI built with [Ratatui](https://ratatui.rs) on the `crossterm`
backend. Conventions for working in this codebase:

- **Draw, don't mutate the terminal directly.** All rendering happens inside a single
  `terminal.draw(|frame| ...)` closure per tick. Never call `crossterm` output functions
  (`execute!`, raw `write!` to stdout) mid-frame — Ratatui owns the screen buffer and diffs it
  for you.
- **`Frame::area()`**, not `.size()`. Ratatui 0.30 renamed `Frame::size()` to `Frame::area()`;
  this scaffold is pinned to that version.
- **Raw mode and the alternate screen are entered once, at startup, and left once, on every exit
  path** (including panics — wrap `run()` so `disable_raw_mode`/`LeaveAlternateScreen` always
  run, even on an `Err` return). A TUI that leaves the terminal in raw mode on a crash is a
  common and unpleasant bug class in this stack.
- **Poll, don't block, for input**, with a short timeout (this scaffold uses 250ms), so the loop
  can also redraw on a timer/tick rather than only on keypress.
- **Business logic stays separate from rendering.** Keep anything worth unit-testing (text
  formatting, state transitions) in plain functions that don't take a `Frame`, so `cargo test`
  can exercise them without a real terminal — see `welcome_text()` in `src/main.rs`.
- **Formatting and linting**: `cargo fmt` and `cargo clippy --all-targets -- -D warnings` are
  expected to pass clean; this scaffold's `verify.toml` runs `cargo build`/`cargo test` and
  assumes both already pass.
