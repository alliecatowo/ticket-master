//! [`PtySession`]: a real pseudo-terminal session driving a child process, extracted and
//! generalized from `crates/tm-tui/src/testing.rs`'s `cfg(test)`-gated `PtyHarness`
//! (`docs/audit-2026-09-18-fable.md` B-16, `SPEC.md` §22).
//!
//! # Why a pipe is not enough
//!
//! `SPEC.md` §22.1: programs behave differently when stdout is not a tty (colour/progress bars/
//! prompts disabled, sometimes refusing outright), and a TUI's escape sequences are meaningless
//! as a raw byte log. [`PtySession::screen`] runs the child's output through a real terminal
//! emulator (`vt100`, a VT100/xterm parser) so what a caller sees is a rendered grid, the same
//! thing a human would see — not ANSI soup.
//!
//! # Bounded memory, not bounded fidelity
//!
//! Two different things are kept bounded, for two different reasons:
//!
//! - The *rendered* terminal state ([`vt100::Parser`]) is O(rows × cols) by construction: it is
//!   constructed with zero scrollback ([`vt100::Parser::new`]'s third argument), so it never
//!   grows regardless of how long the session runs or how much output the child produces.
//! - The *raw* bytes a background reader thread pulls off the pty are buffered only until the
//!   next [`PtySession::screen`]/[`PtySession::diff`]/[`PtySession::expect`] call drains them
//!   into the parser above — but a caller that never polls (or a child that outputs faster than
//!   it is polled) could otherwise grow that buffer without limit, which is exactly the
//!   "unbounded interactive session's scrollback" `SPEC.md` §22.4 asks not to allow into memory.
//!   [`MAX_BUFFERED_BYTES`] hard-caps it: once exceeded, the oldest unconsumed bytes are dropped
//!   from the front rather than the buffer growing further. This never corrupts `vt100`'s state
//!   (dropped bytes are simply never fed to the parser, the same as a screen scrolling off
//!   terminal history a human never looked at) — the visible consequence is only that the
//!   *rendered* screen may skip some of what a very fast, very unpolled child printed, which is
//!   an honest trade documented here rather than a silent one.
//!
//! The asciicast recording ([`PtySession::to_asciicast`]/[`PtySession::record`]) is bounded
//! separately by [`MAX_RECORDING_BYTES`]: once a session's raw output recording would exceed it,
//! further frames are dropped and [`PtySession::recording_truncated`] reports `true` — the
//! evidence artifact stays honest about covering only a prefix of the session rather than
//! silently claiming completeness (`SPEC.md` §22.3, §22.4's "output is bounded ... truncated
//! into an artifact rather than into a context window").
//!
//! # Time source
//!
//! [`PtySession::expect`]'s poll loop needs two different kinds of "time," and only one of them
//! goes through [`tm_types::Clock`]:
//!
//! - The **deadline arithmetic** ("has `timeout` elapsed since I started waiting") reads
//!   [`tm_types::Clock::now`], the injected clock this crate's `Cargo.toml`-level dependency on
//!   `tm-types` exists to make possible — never `std::time::Instant::now`/`SystemTime::now`
//!   directly, which the workspace hygiene check (`crates/xtask/src/hygiene.rs`) forbids outside
//!   `tm-types/src/clock.rs` for exactly the determinism reason `SPEC.md` §2.2/§16.14 give: a
//!   fixed clock can drive replay deterministically in a test.
//! - The **inter-poll delay** (`thread::sleep(POLL_INTERVAL)` between checks) is a real wait on
//!   a real external process: a REPL that takes 300ms to print its prompt takes 300ms regardless
//!   of what clock a test injects, the same way `crates/tm-browser`'s CDP round-trips are
//!   genuinely I/O-bound. This is not a replay hazard the hygiene check's determinism rule is
//!   trying to prevent, and `thread::sleep` is not in its forbidden-needle list. Production
//!   callers pass [`tm_types::SystemClock`] (real time flows, so the deadline is eventually
//!   met); a test that wants `expect` to time out deterministically passes a
//!   [`tm_types::FixedClock`] instead — but note that with a `FixedClock` that is never advanced
//!   from another thread, the deadline never arrives on its own, so such a test must either
//!   advance the clock concurrently or rely on the loop's match condition firing before the
//!   first `now()` re-check.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use portable_pty::{native_pty_system, Child, CommandBuilder, PtyPair, PtySize};
use serde::Serialize;
use tm_types::{Clock, Result, Timestamp, TmError};

/// Poll interval used by [`PtySession::expect`]. Mirrors
/// `crates/tm-browser/src/session.rs`'s `POLL_INTERVAL` for the same kind of wait.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Hard cap on unconsumed raw bytes buffered between the reader thread and the next
/// `screen()`/`diff()`/`expect()` call. See this module's doc comment.
const MAX_BUFFERED_BYTES: usize = 8 * 1024 * 1024;

/// Hard cap on total bytes captured into the asciicast recording. See this module's doc comment.
const MAX_RECORDING_BYTES: usize = 4 * 1024 * 1024;

/// State shared between a [`PtySession`] and its background reader thread.
struct SharedState {
    /// Bytes read from the child but not yet fed to the `vt100` parser. Bounded by
    /// [`MAX_BUFFERED_BYTES`] (oldest bytes evicted first) so a session nobody polls cannot grow
    /// this without limit.
    unconsumed: Vec<u8>,
    /// Every chunk read from the child, each stamped with seconds since the session started, for
    /// [`PtySession::to_asciicast`]. Independent of `unconsumed` (draining `unconsumed` into the
    /// parser must not also erase the recording), bounded by [`MAX_RECORDING_BYTES`].
    recording: Vec<AsciicastFrame>,
    recorded_bytes: usize,
    /// Set once `recorded_bytes` would exceed [`MAX_RECORDING_BYTES`]: the recording covers only
    /// a prefix of the session's real output from that point on.
    truncated: bool,
}

impl SharedState {
    fn new() -> Self {
        SharedState {
            unconsumed: Vec::new(),
            recording: Vec::new(),
            recorded_bytes: 0,
            truncated: false,
        }
    }

    /// Append a chunk just read from the child: buffer it for the parser (bounded eviction) and
    /// record it for the asciicast (bounded truncation).
    fn on_chunk(&mut self, chunk: &[u8], elapsed_secs: f64) {
        self.unconsumed.extend_from_slice(chunk);
        if self.unconsumed.len() > MAX_BUFFERED_BYTES {
            let excess = self.unconsumed.len() - MAX_BUFFERED_BYTES;
            self.unconsumed.drain(0..excess);
        }

        if !self.truncated {
            if self.recorded_bytes + chunk.len() > MAX_RECORDING_BYTES {
                self.truncated = true;
            } else {
                self.recorded_bytes += chunk.len();
                self.recording.push(AsciicastFrame {
                    time: elapsed_secs,
                    data: chunk.to_vec(),
                });
            }
        }
    }
}

/// One captured output frame for the asciicast recording: `time` is seconds since the session
/// started (asciicast v2's convention), `data` is the raw bytes read from the pty in that chunk.
struct AsciicastFrame {
    time: f64,
    data: Vec<u8>,
}

/// Where [`PtySession::record`] writes the asciicast recording. Mirrors
/// `crates/tm-browser/src/session.rs`'s `ArtifactSink` trait exactly (same
/// store-bytes-get-an-id shape) — duplicated here rather than depended on, since `tm-pty` cannot
/// depend on `tm-browser` and the trait is genuinely provider-agnostic, not browser-specific.
pub trait ArtifactSink: Send + Sync {
    /// Persist `bytes` (of the given MIME `content_type`) and return its new artifact id.
    fn store(&self, bytes: &[u8], content_type: &str) -> Result<tm_types::ArtifactId>;
}

/// The MIME type [`PtySession::record`] stores the recording as. Not registered with IANA; this
/// is the same informal `application/x-asciicast` convention asciinema's own tooling uses.
pub const ASCIICAST_CONTENT_TYPE: &str = "application/x-asciicast";

/// What [`PtySession::expect`] found: either `pattern` matched (with the screen at that moment),
/// or `timeout` elapsed first — carrying the screen either way, per `SPEC.md` §22.2: "a timeout
/// returns the *current screen* rather than a bare error, because 'what was on screen when it
/// hung' is the whole diagnosis."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyExpectOutcome {
    /// `pattern` was found in some line of the screen at this point.
    Matched(Vec<String>),
    /// `timeout` elapsed with no match; this is the screen at that moment.
    TimedOut(Vec<String>),
}

impl PtyExpectOutcome {
    /// The screen captured at the moment this outcome was produced, regardless of which variant.
    pub fn screen(&self) -> &[String] {
        match self {
            PtyExpectOutcome::Matched(s) | PtyExpectOutcome::TimedOut(s) => s,
        }
    }

    /// `true` for [`PtyExpectOutcome::Matched`].
    pub fn matched(&self) -> bool {
        matches!(self, PtyExpectOutcome::Matched(_))
    }
}

/// A real terminal session driving a child process: `SPEC.md` §22's `pty.*` surface, minus the
/// authority gate and tool-schema plumbing [`crate::capability::PtyCapability`] adds on top.
///
/// Extracted from `crates/tm-tui/src/testing.rs`'s `PtyHarness` (`docs/audit-2026-09-18-fable.md`
/// B-16): `portable_pty::native_pty_system()` gives a `PtySystem` whose `openpty` returns a
/// `PtyPair` plus a `Box<dyn Child + Send + Sync>` from `pair.slave.spawn_command(command)`.
/// Reading the child's output and writing input both go through `PtyPair::master`'s
/// `try_clone_reader`/`take_writer`.
pub struct PtySession {
    pair: PtyPair,
    child: Box<dyn Child + Send + Sync>,
    state: Arc<Mutex<SharedState>>,
    parser: vt100::Parser,
    /// The screen as of the last [`PtySession::diff`] call, for computing the next diff against.
    last_diff_screen: Option<Vec<String>>,
    clock: Arc<dyn Clock>,
    started_at: Timestamp,
    cols: u16,
    rows: u16,
}

impl PtySession {
    /// Spawn `argv[0] argv[1..]` inside a `cols`x`rows` pty, with `cwd` and an explicit `env`
    /// (matching `shell.run`'s own discipline — `crates/tm-agent/src/agent_loop.rs`'s
    /// `env_clear()` plus an explicit allowlist — rather than inheriting the parent's
    /// environment wholesale, since a pty session can run arbitrary commands just as `shell.run`
    /// can and should get no more ambient environment than one).
    ///
    /// `clock` is the session's injected time source (see this module's doc comment on why one
    /// `Arc<dyn Clock>` is threaded through both `expect`'s deadline arithmetic and the
    /// recording's frame timestamps, matching `crates/tm-browser/src/session.rs`'s
    /// `BrowserSession::clock` field).
    pub fn spawn(
        argv: &[String],
        cwd: Option<&Path>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
        clock: Arc<dyn Clock>,
    ) -> Result<PtySession> {
        let Some(program) = argv.first() else {
            return Err(TmError::parse("pty.spawn requires a non-empty argv"));
        };

        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| io::Error::other(format!("opening a pty: {e}")))?;

        let mut command = CommandBuilder::new(program);
        command.args(&argv[1..]);
        if let Some(cwd) = cwd {
            command.cwd(cwd);
        }
        command.env_clear();
        for (k, v) in env {
            command.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| io::Error::other(format!("spawning the child in the pty: {e}")))?;

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| io::Error::other(format!("cloning the pty reader: {e}")))?;

        let state = Arc::new(Mutex::new(SharedState::new()));
        let started_at = clock.now();
        let reader_state = Arc::clone(&state);
        let reader_clock = Arc::clone(&clock);
        // Detached deliberately, mirroring `PtyHarness`'s original reasoning: the thread ends
        // when the child closes the pty, and a caller that stops polling should not also hang
        // waiting to join it.
        thread::spawn(move || {
            let mut reader = reader;
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let elapsed_secs =
                            reader_clock.now().millis_since(started_at).max(0) as f64 / 1000.0;
                        if let Ok(mut s) = reader_state.lock() {
                            s.on_chunk(&chunk[..n], elapsed_secs);
                        } else {
                            break;
                        }
                    }
                }
            }
        });

        Ok(PtySession {
            pair,
            child,
            state,
            parser: vt100::Parser::new(rows, cols, 0),
            last_diff_screen: None,
            clock,
            started_at,
            cols,
            rows,
        })
    }

    /// Drain unconsumed bytes into the parser. Called by every method that needs a fresh
    /// screen — `screen`/`diff`/`expect` all funnel through this rather than each re-locking.
    fn sync_parser(&mut self) {
        let chunk = {
            let Ok(mut s) = self.state.lock() else {
                return;
            };
            std::mem::take(&mut s.unconsumed)
        };
        if !chunk.is_empty() {
            self.parser.process(&chunk);
        }
    }

    /// The child's rendered screen right now, as plain text lines — one entry per row, always
    /// (including blank ones), so a caller can index by row and an assertion failure shows the
    /// real geometry rather than a collapsed list. Parses with `vt100` (a real VT100/xterm
    /// emulator), not a hand-rolled subset, per `SPEC.md` §22.1/§22.2: "give the agent the
    /// rendered screen, not the raw byte stream."
    pub fn screen(&mut self) -> Vec<String> {
        self.sync_parser();
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

    /// What changed on screen since the last [`PtySession::diff`] call (or since the session
    /// started, for the first call): the rows whose rendered content differs now, as
    /// `(row_index, new_content)` pairs. `SPEC.md` §22.2: "how an agent learns what its keystroke
    /// did, cheaply" — a caller does not have to re-read every row to find the one that changed.
    pub fn diff(&mut self) -> Vec<(usize, String)> {
        let current = self.screen();
        let previous = self.last_diff_screen.replace(current.clone());
        let previous = previous.unwrap_or_default();
        current
            .into_iter()
            .enumerate()
            .filter(|(i, line)| previous.get(*i) != Some(line))
            .collect()
    }

    /// Write raw bytes to the child's terminal, as if typed.
    pub fn write(&mut self, input: &[u8]) -> Result<()> {
        let mut writer = self
            .pair
            .master
            .take_writer()
            .map_err(|e| io::Error::other(format!("taking the pty writer: {e}")))?;
        writer.write_all(input)?;
        writer.flush()?;
        Ok(())
    }

    /// Write `text` to the child's terminal, as if typed. A thin wrapper over
    /// [`PtySession::write`] for the common literal-text case.
    pub fn send(&mut self, text: &str) -> Result<()> {
        self.write(text.as_bytes())
    }

    /// Press a named key or chord, e.g. `"enter"`, `"ctrl+c"`, `"down"`. Only the small,
    /// terminal-relevant vocabulary `tm`'s own TUI needs is supported; an unrecognized name is a
    /// parse error rather than a silent no-op, so a typo in a test/tool call fails loudly instead
    /// of hanging on a prompt that never advances.
    pub fn key(&mut self, chord: &str) -> Result<()> {
        let bytes = key_chord_bytes(chord)?;
        self.write(&bytes)
    }

    /// Resize the pty and the parser together — exercising reflow, which `SPEC.md` §22.2 notes
    /// "is where TUIs break."
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.pair
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| io::Error::other(format!("resizing the pty: {e}")))?;
        self.parser.screen_mut().set_size(rows, cols);
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    /// Block until `screen()` contains `pattern` or `timeout` elapses, returning the matched (or
    /// last-seen) screen either way. See this module's doc comment for the deadline/poll time
    /// source split.
    pub fn expect(&mut self, pattern: &str, timeout: Duration) -> PtyExpectOutcome {
        let start = self.clock.now();
        loop {
            let screen = self.screen();
            if screen.iter().any(|line| line.contains(pattern)) {
                return PtyExpectOutcome::Matched(screen);
            }
            let elapsed_ms = self.clock.now().millis_since(start).max(0) as u128;
            if elapsed_ms >= timeout.as_millis() {
                return PtyExpectOutcome::TimedOut(screen);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Non-blocking: has the child already exited?
    pub fn try_wait(&mut self) -> Result<Option<portable_pty::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    /// Block until the child exits, returning its exit status. **Unbounded**: a hung child parks
    /// the calling thread forever. Fine for a caller that has already decided to wait as long as
    /// it takes (a direct, synchronous consumer of this crate); wrong for anything called from
    /// inside an async runtime's worker thread, which [`PtySession::wait_exit_bounded`] exists
    /// for — [`crate::capability::PtyCapability`]'s `pty.wait_exit` tool uses that one, not this
    /// one, for exactly this reason.
    pub fn wait_exit(&mut self) -> Result<portable_pty::ExitStatus> {
        Ok(self.child.wait()?)
    }

    /// Block until the child exits or `timeout` elapses, returning `Ok(None)` on timeout rather
    /// than blocking forever. Polls [`PtySession::try_wait`] (non-blocking) against a
    /// [`tm_types::Clock`]-sourced deadline, the same split [`PtySession::expect`]'s doc comment
    /// describes: the deadline arithmetic reads the injected clock, the inter-poll delay is a
    /// real `thread::sleep`.
    pub fn wait_exit_bounded(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<portable_pty::ExitStatus>> {
        let start = self.clock.now();
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(Some(status));
            }
            let elapsed_ms = self.clock.now().millis_since(start).max(0) as u128;
            if elapsed_ms >= timeout.as_millis() {
                return Ok(None);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Send `signal` (a POSIX signal number) to the child process. The entry point for
    /// `SIGTERM`/`SIGHUP`/`SIGTSTP` restore behaviour, carried over unchanged from `PtyHarness`
    /// (`crates/tm-tui/src/testing.rs`), including its reasoning: shells out to `kill(1)` rather
    /// than calling `libc::kill`, because this crate is `#![forbid(unsafe_code)]` with no
    /// `libc`/`nix` dependency.
    pub fn signal(&mut self, signal: i32) -> Result<()> {
        let Some(pid) = self.child.process_id() else {
            return Err(TmError::invariant("child has already exited"));
        };
        let status = std::process::Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(pid.to_string())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(TmError::invariant(format!("kill -{signal} {pid} failed")))
        }
    }

    /// Kill the whole process group the child heads, not just the child itself — `SPEC.md`
    /// §22.4: "Sessions carry the lease's TTL and are killed on expiry — process group and all —
    /// so a crashed agent cannot leave an orphaned interactive shell holding a lock." `portable-pty`
    /// calls `setsid()` before exec on Unix (see its `unix.rs`), making the child a new session
    /// and process-group leader, so `kill(1)`'s negative-pid form (`kill -SIG -PID`) reaches the
    /// child and every process it spawned. See [`PtySession::signal`]'s doc comment for why this
    /// shells out to `kill(1)` rather than calling `libc::kill`/`killpg` directly.
    ///
    /// This is the *explicit* teardown this crate provides. What it is not: a subscriber wired to
    /// real lease expiry. `docs/audit-2026-09-18-fable.md` B-16's brief and `crates/tm-browser`/
    /// `crates/tm-computer`'s own B-02 reports agree that nothing in this workspace yet fires a
    /// callback when `tm-core::Store::expire_leases` appends `ticket.lease_expired` — that
    /// subscriber hook does not exist anywhere in this workspace today, not merely unwired here —
    /// so calling this method at the right moment (a lease's actual expiry) is the owning
    /// executor's responsibility, the same gap `BrowserCapability`/`ComputerCapability` document
    /// rather than paper over. [`crate::capability::SessionRegistry::close`]/`close_all` are
    /// this crate's equivalent of `SessionRegistry::close_all` on those two tracks: real teardown,
    /// called on every ordinary dispatch return, not yet triggered by lease expiry itself.
    pub fn kill_process_group(&mut self) -> Result<()> {
        let Some(pid) = self.child.process_id() else {
            // Already exited: nothing to kill, and not an error — teardown of a session whose
            // child is already gone is the common, harmless case.
            return Ok(());
        };
        // pid 0 and 1 would make `-PID` mean "my own group" or "every process I may signal".
        if pid <= 1 {
            return Err(TmError::invariant(format!(
                "refusing to signal process group {pid}"
            )));
        }
        // `--` is load-bearing: procps-ng's `kill` (Ubuntu's /bin/kill) otherwise parses `-PID`
        // as an option and keeps only its first digit, so `-12345` became `kill(-1, SIGTERM)`,
        // which signalled every process this user owns and killed the CI runner.
        let status = std::process::Command::new("kill")
            .args(["-s", "TERM", "--"])
            .arg(format!("-{pid}"))
            .status()?;
        // `kill` exits nonzero when the target no longer exists (already reaped between
        // `process_id()` and this call) — a race, not a failure this method should propagate.
        let _ = status;
        Ok(())
    }

    /// This session's current `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// `true` once the asciicast recording has dropped frames because it exceeded
    /// [`MAX_RECORDING_BYTES`] — the recording covers only a prefix of the session's real output.
    pub fn recording_truncated(&self) -> bool {
        self.state.lock().map(|s| s.truncated).unwrap_or(false)
    }

    /// Render this session's captured output as an asciicast v2 recording: a JSON header line
    /// followed by one JSON-array line per output frame (`[time, "o", data]`), asciinema's own
    /// on-disk format. Deliberately not driven through a general asciinema-compatible player
    /// dependency — `SPEC.md` §22.3/§16 ask only for "a replayable recording attached to a
    /// ticket," and asciicast v2 is simple enough (JSON Lines, two record shapes) to emit
    /// directly with `serde_json` rather than pulling in a writer crate for it.
    pub fn to_asciicast(&self) -> String {
        #[derive(Serialize)]
        struct Header {
            version: u8,
            width: u16,
            height: u16,
            timestamp: i64,
        }

        let header = Header {
            version: 2,
            width: self.cols,
            height: self.rows,
            timestamp: self.started_at.unix_seconds(),
        };
        let mut out = serde_json::to_string(&header).unwrap_or_default();
        out.push('\n');

        if let Ok(s) = self.state.lock() {
            for frame in &s.recording {
                // `String::from_utf8_lossy` rather than a hard UTF-8 requirement: terminal output
                // is not guaranteed valid UTF-8 mid-escape-sequence-split, and a recording that
                // refused to serialize a session with one bad byte would be worse than one that
                // renders it as U+FFFD.
                let data = String::from_utf8_lossy(&frame.data);
                if let Ok(line) = serde_json::to_string(&(frame.time, "o", data)) {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
        }
        out
    }

    /// Write this session's asciicast recording through `sink` and return its artifact id —
    /// `SPEC.md` §22.3: "Every session records an asciicast artifact, so 'the TUI works' is a
    /// replayable recording attached to a ticket, not a claim."
    pub fn record(&self, sink: &dyn ArtifactSink) -> Result<tm_types::ArtifactId> {
        let cast = self.to_asciicast();
        sink.store(cast.as_bytes(), ASCIICAST_CONTENT_TYPE)
    }
}

/// Translate a small, terminal-relevant chord vocabulary into the bytes a pty expects. Supports
/// single named keys (`"enter"`, `"tab"`, `"esc"`/`"escape"`, `"backspace"`, `"up"`/`"down"`/
/// `"left"`/`"right"`, `"space"`) and `ctrl+<letter>` chords (`"ctrl+c"`, `"ctrl+d"`, ...), plus a
/// space-separated sequence of any of the above (`"down down enter"`, matching `SPEC.md` §22.2's
/// own example). An unrecognized token is a parse error rather than silently producing no bytes.
fn key_chord_bytes(chord: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for token in chord.split_whitespace() {
        out.extend(single_key_bytes(token)?);
    }
    if out.is_empty() {
        return Err(TmError::parse(format!("empty key chord: `{chord}`")));
    }
    Ok(out)
}

fn single_key_bytes(token: &str) -> Result<Vec<u8>> {
    if let Some(letter) = token.to_ascii_lowercase().strip_prefix("ctrl+") {
        let mut chars = letter.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return Err(TmError::parse(format!("bad ctrl chord: `{token}`")));
        };
        if !c.is_ascii_alphabetic() {
            return Err(TmError::parse(format!("bad ctrl chord: `{token}`")));
        }
        // Ctrl+<letter> maps to the letter's position in the alphabet (A=1, ..., Z=26).
        let code = (c.to_ascii_uppercase() as u8) - b'A' + 1;
        return Ok(vec![code]);
    }

    Ok(match token.to_ascii_lowercase().as_str() {
        "enter" | "return" => vec![b'\r'],
        "tab" => vec![b'\t'],
        "esc" | "escape" => vec![0x1b],
        "backspace" => vec![0x7f],
        "space" => vec![b' '],
        "up" => vec![0x1b, b'[', b'A'],
        "down" => vec![0x1b, b'[', b'B'],
        "right" => vec![0x1b, b'[', b'C'],
        "left" => vec![0x1b, b'[', b'D'],
        other => return Err(TmError::parse(format!("unrecognized key: `{other}`"))),
    })
}

/// An in-memory [`ArtifactSink`] for tests: stores bytes under a counter-derived id and lets a
/// test assert on what was recorded, mirroring how `crates/tm-browser`'s own tests fake this
/// trait rather than touching real storage.
#[cfg(test)]
pub(crate) struct FakeArtifactSink {
    pub(crate) stored: Mutex<std::collections::HashMap<String, (Vec<u8>, String)>>,
}

#[cfg(test)]
impl FakeArtifactSink {
    pub(crate) fn new() -> Self {
        FakeArtifactSink {
            stored: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[cfg(test)]
impl ArtifactSink for FakeArtifactSink {
    fn store(&self, bytes: &[u8], content_type: &str) -> Result<tm_types::ArtifactId> {
        let mut stored = self.stored.lock().unwrap();
        let id = format!("ART-{:012x}", stored.len() + 1);
        stored.insert(id.clone(), (bytes.to_vec(), content_type.to_string()));
        tm_types::ArtifactId::new(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::SystemClock;

    fn clock() -> Arc<dyn Clock> {
        Arc::new(SystemClock)
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    /// `PATH` alone, resolved from this test process's real environment — `PtySession::spawn`'s
    /// `env_clear()` discipline means a spawned child otherwise cannot resolve a bare program
    /// name like `"echo"` at all (`portable_pty::CommandBuilder` does its own `PATH` search
    /// rather than falling back to a libc default), so every test that needs to actually run a
    /// program passes this explicitly.
    fn default_env() -> Vec<(String, String)> {
        vec![(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        )]
    }

    #[test]
    fn spawn_rejects_an_empty_argv() {
        // `PtySession` intentionally does not derive `Debug` (it holds trait objects and a raw
        // pty pair with no useful debug representation), so `unwrap_err()`'s `T: Debug` bound
        // does not apply here — match instead.
        let err = match PtySession::spawn(&[], None, &default_env(), 40, 10, clock()) {
            Ok(_) => panic!("expected an error for an empty argv"),
            Err(e) => e,
        };
        assert!(matches!(err, TmError::Parse(_)));
    }

    #[test]
    fn screen_captures_child_output() {
        let mut pty = PtySession::spawn(
            &argv(&["echo", "hello-from-the-pty"]),
            None,
            &default_env(),
            40,
            10,
            clock(),
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
    fn screen_has_one_entry_per_row() {
        let mut pty =
            PtySession::spawn(&argv(&["echo", "x"]), None, &default_env(), 40, 10, clock())
                .unwrap();
        let _ = pty.expect("x", Duration::from_secs(5));
        assert_eq!(pty.screen().len(), 10);
    }

    #[test]
    fn expect_returns_the_screen_on_timeout_rather_than_only_an_error() {
        let mut pty = PtySession::spawn(
            &argv(&["echo", "present"]),
            None,
            &default_env(),
            40,
            10,
            clock(),
        )
        .unwrap();
        let outcome = pty.expect("absolutely-never-printed", Duration::from_millis(200));
        assert!(!outcome.matched());
        assert!(!outcome.screen().is_empty());
    }

    #[test]
    fn diff_reports_only_changed_rows() {
        let mut pty = PtySession::spawn(
            &argv(&["echo", "line-one"]),
            None,
            &default_env(),
            40,
            10,
            clock(),
        )
        .unwrap();
        let _ = pty.expect("line-one", Duration::from_secs(5));
        let first_diff = pty.diff();
        assert!(first_diff.iter().any(|(_, line)| line.contains("line-one")));

        // A second diff with nothing new should report no changes (the child has already
        // exited and produced no further output).
        thread::sleep(Duration::from_millis(50));
        let second_diff = pty.diff();
        assert!(second_diff.is_empty(), "unexpected diff: {second_diff:?}");
    }

    #[test]
    fn resize_updates_the_reported_size_and_reflows_the_parser() {
        let mut pty =
            PtySession::spawn(&argv(&["cat"]), None, &default_env(), 40, 10, clock()).unwrap();
        assert_eq!(pty.size(), (40, 10));
        pty.resize(80, 24).unwrap();
        assert_eq!(pty.size(), (80, 24));
        assert_eq!(pty.screen().len(), 24);
        let _ = pty.kill_process_group();
    }

    #[test]
    fn wait_exit_reports_success_for_a_clean_exit() {
        let mut pty =
            PtySession::spawn(&argv(&["true"]), None, &default_env(), 40, 10, clock()).unwrap();
        let status = pty.wait_exit().unwrap();
        assert!(status.success());
    }

    #[test]
    fn wait_exit_bounded_reports_the_status_once_the_child_exits() {
        let mut pty =
            PtySession::spawn(&argv(&["true"]), None, &default_env(), 40, 10, clock()).unwrap();
        let status = pty
            .wait_exit_bounded(Duration::from_secs(5))
            .unwrap()
            .expect("the child should have exited within 5 seconds");
        assert!(status.success());
    }

    #[test]
    fn wait_exit_bounded_returns_none_on_timeout_rather_than_blocking() {
        let mut pty = PtySession::spawn(
            &argv(&["sh", "-c", "sleep 5"]),
            None,
            &default_env(),
            40,
            10,
            clock(),
        )
        .unwrap();
        let outcome = pty.wait_exit_bounded(Duration::from_millis(200)).unwrap();
        assert!(outcome.is_none(), "the child is still sleeping, not exited");
        let _ = pty.kill_process_group();
    }

    #[test]
    fn kill_process_group_terminates_a_sleeping_child_and_its_descendants() {
        // `sh -c 'sleep 100 & wait'` spawns a grandchild; killing only the direct child would
        // leave `sleep` orphaned. This is the discriminating check for the process-group claim.
        let mut pty = PtySession::spawn(
            &argv(&["sh", "-c", "sleep 100 & wait"]),
            None,
            &default_env(),
            40,
            10,
            clock(),
        )
        .unwrap();
        // Give the shell a moment to actually fork the background `sleep`.
        thread::sleep(Duration::from_millis(200));
        pty.kill_process_group().unwrap();
        let status = pty.wait_exit().unwrap();
        assert!(
            !status.success(),
            "a killed session should not exit successfully"
        );
    }

    #[test]
    fn key_chords_translate_to_the_expected_bytes() {
        assert_eq!(key_chord_bytes("enter").unwrap(), vec![b'\r']);
        assert_eq!(key_chord_bytes("ctrl+c").unwrap(), vec![3]);
        assert_eq!(
            key_chord_bytes("down down enter").unwrap(),
            vec![0x1b, b'[', b'B', 0x1b, b'[', b'B', b'\r']
        );
        assert!(key_chord_bytes("not-a-real-key").is_err());
    }

    #[test]
    fn to_asciicast_emits_a_header_and_at_least_one_frame() {
        let mut pty = PtySession::spawn(
            &argv(&["echo", "recorded-output"]),
            None,
            &default_env(),
            40,
            10,
            clock(),
        )
        .unwrap();
        let _ = pty.expect("recorded-output", Duration::from_secs(5));

        let cast = pty.to_asciicast();
        let mut lines = cast.lines();
        let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["version"], 2);
        assert_eq!(header["width"], 40);
        assert_eq!(header["height"], 10);

        let frame_lines: Vec<&str> = lines.collect();
        assert!(
            !frame_lines.is_empty(),
            "expected at least one recorded frame"
        );
        let frame: serde_json::Value = serde_json::from_str(frame_lines[0]).unwrap();
        assert!(frame.as_array().unwrap()[2]
            .as_str()
            .unwrap()
            .contains("recorded-output"));
        assert!(!pty.recording_truncated());
    }

    #[test]
    fn record_stores_the_recording_through_the_sink() {
        let mut pty = PtySession::spawn(
            &argv(&["echo", "evidence"]),
            None,
            &default_env(),
            40,
            10,
            clock(),
        )
        .unwrap();
        let _ = pty.expect("evidence", Duration::from_secs(5));

        let sink = FakeArtifactSink::new();
        let id = pty.record(&sink).unwrap();
        let stored = sink.stored.lock().unwrap();
        let (bytes, content_type) = stored.get(id.as_str()).expect("artifact was stored");
        assert_eq!(content_type, ASCIICAST_CONTENT_TYPE);
        assert!(String::from_utf8_lossy(bytes).contains("evidence"));
    }

    #[test]
    fn env_is_cleared_not_inherited() {
        // `shell.run`'s own discipline (env_clear + explicit allowlist): a variable set in this
        // test process's environment must not leak into the child unless explicitly passed.
        std::env::set_var("TM_PTY_TEST_LEAK_CANARY", "should-not-appear");
        let mut pty = PtySession::spawn(
            &argv(&["sh", "-c", "echo canary=$TM_PTY_TEST_LEAK_CANARY;echo done"]),
            None,
            &[(
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            )],
            80,
            10,
            clock(),
        )
        .unwrap();
        let outcome = pty.expect("done", Duration::from_secs(5));
        std::env::remove_var("TM_PTY_TEST_LEAK_CANARY");
        assert!(
            outcome.screen().iter().any(|l| l.contains("canary=")),
            "expected the echoed line, got: {:?}",
            outcome.screen()
        );
        assert!(
            !outcome
                .screen()
                .iter()
                .any(|l| l.contains("should-not-appear")),
            "ambient environment leaked into the pty child: {:?}",
            outcome.screen()
        );
    }
}
