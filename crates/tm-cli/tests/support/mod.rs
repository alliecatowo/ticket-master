//! A minimal real-pty test harness for `tm-cli`'s own integration tests.
//!
//! `tm-tui::testing::PtyHarness` is `#[cfg(test)]`-gated (see that module's docs) and therefore
//! unreachable from this crate's `tests/*.rs`, which link the *non*-`cfg(test)` build of `tm-tui`
//! even though this whole crate is a `[dev-dependencies]` context. This is the same helper shape
//! (`portable-pty` to spawn, a background reader thread, `vt100::Parser` for a real screen) built
//! directly against the compiled `tm` binary instead, per that module's own suggestion for
//! exactly this situation.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, PtyPair, PtySize};

/// A real terminal session driving a spawned `tm` child process.
pub struct Pty {
    pair: PtyPair,
    child: Box<dyn Child + Send + Sync>,
    output: Arc<Mutex<Vec<u8>>>,
    consumed: usize,
    parser: vt100::Parser,
}

impl Pty {
    /// Spawn `command` inside a `cols`x`rows` pty.
    pub fn spawn(command: CommandBuilder, cols: u16, rows: u16) -> std::io::Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| std::io::Error::other(format!("opening a pty: {e}")))?;

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| std::io::Error::other(format!("spawning the child in the pty: {e}")))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| std::io::Error::other(format!("cloning the pty reader: {e}")))?;

        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        // Detached deliberately, matching `tm_tui::testing::PtyHarness`: the thread ends when
        // the child closes the pty, and a failing test should not also hang joining it.
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

        Ok(Pty {
            pair,
            child,
            output,
            consumed: 0,
            parser: vt100::Parser::new(rows, cols, 0),
        })
    }

    /// The child's rendered screen right now, as plain text lines.
    pub fn screen(&mut self) -> Vec<String> {
        if let Ok(buf) = self.output.lock() {
            if buf.len() > self.consumed {
                self.parser.process(&buf[self.consumed..]);
                self.consumed = buf.len();
            }
        }

        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
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
    pub fn write(&mut self, input: &[u8]) -> std::io::Result<()> {
        let mut writer = self
            .pair
            .master
            .take_writer()
            .map_err(|e| std::io::Error::other(format!("taking the pty writer: {e}")))?;
        writer.write_all(input)?;
        writer.flush()
    }

    /// Wait for the child to exit, returning whether it exited successfully.
    ///
    /// Bounded rather than `Child::wait`'s unconditional block: a regression that leaves the
    /// child hung (this harness exists because of exactly one — see `tui.rs`'s `App`/`runtime.rs`
    /// doc comments on the caps-probe/`EventStream` stdin conflict) must fail this test quickly,
    /// not hang the whole suite (and, on this machine, everything else sharing its memory) for as
    /// long as it takes a human to notice and `kill -9` it by hand.
    pub fn wait(&mut self, timeout: Duration) -> std::io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|e| std::io::Error::other(format!("polling the child: {e}")))?
            {
                return Ok(status.success());
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Block until `screen()` contains `pattern` or `timeout` elapses, returning the screen
    /// either way so a timeout assertion failure shows what was actually displayed.
    pub fn wait_for(&mut self, pattern: &str, timeout: Duration) -> Vec<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self.screen();
            if screen.iter().any(|line| line.contains(pattern)) {
                return screen;
            }
            if Instant::now() >= deadline {
                return screen;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
