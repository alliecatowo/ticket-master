//! Regression test for the `tm sched run` nested-tokio-runtime panic.
//!
//! `sched::dispatch_sched`'s `SchedCommand::Run` arm used to build a *second*
//! `tokio::runtime::Runtime` and `block_on` it from inside `main.rs`'s `#[tokio::main]` runtime --
//! entering a fresh multi-threaded runtime from a thread that is already driving one panics
//! (tokio's own "Cannot start a runtime from within a runtime" guard), so every real invocation of
//! `tm sched run` crashed with exit code 101 before doing any work at all. The fix threads
//! `.await` straight through (`dispatch_sched` and `main.rs`'s `dispatch` are already `async fn`s
//! all the way up to `main()`), so this proves the real compiled binary survives long enough to
//! reach the scheduler's tick/ctrl-c loop instead of panicking on startup -- following the
//! `Command::new(env!("CARGO_BIN_EXE_tm"))` + tempdir-isolated `--project` convention
//! `tests/worktree_run.rs` and `tests/ticket_fork.rs` already use, never the primary checkout,
//! always a fresh tempdir.

use std::process::{Command, Stdio};
use std::time::Duration;

/// Create a fresh project directory the `tm` binary can open, the same way `project::open`
/// expects (`.tm/project.db` present) -- mirrors `tests/tui_launch.rs`'s `init_project` rather
/// than shelling out to `tm init` first, since this crate already depends on `tm-core` directly.
fn init_project() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    tm_core::Store::open(tmp.path()).expect("open (and thereby create) a fresh .tm project");
    tmp
}

#[test]
fn sched_run_does_not_panic_on_a_nested_tokio_runtime() {
    let project = init_project();
    // Repo scope via `--project` never reads `$TM_HOME`, but every test spawning the real binary
    // sets it to a tempdir regardless so none can ever accidentally touch a real developer's
    // `~/.tm` -- same convention as `tests/tui_launch.rs`/`tests/worktree_run.rs`.
    let tm_home = tempfile::tempdir().expect("tempdir");

    // A long tick interval: this test only cares whether the process survives past the startup
    // panic, not about observing a real tick, so nothing should fire during the sleep below.
    let mut child = Command::new(env!("CARGO_BIN_EXE_tm"))
        .arg("--project")
        .arg(project.path())
        .arg("sched")
        .arg("run")
        .arg("--interval-secs")
        .arg("3600")
        .env("TM_HOME", tm_home.path())
        .env("TM_NOTIFY", "0")
        .env("TM_TEST_MOCK_PROVIDER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `tm sched run`");

    // The nested-runtime bug panicked immediately, before the scheduler loop was ever entered --
    // give it a generous window to prove it does NOT exit on its own (it should be parked in
    // `tokio::select!` on the tick/ctrl-c branches instead).
    std::thread::sleep(Duration::from_millis(1500));
    let exited_early = child
        .try_wait()
        .expect("poll `tm sched run`'s status")
        .is_some();

    // Whether or not it already exited, stop it: `kill` on an already-exited child is a harmless
    // no-op error, and `wait_with_output` below still reports the real (pre-kill) exit status.
    let _ = child.kill();
    let output = child
        .wait_with_output()
        .expect("collect `tm sched run`'s output");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !exited_early,
        "`tm sched run` must not exit on its own within 1.5s -- it should be blocked inside the \
         scheduler loop's tick/ctrl-c select, not panicking on startup; exit status {:?}, \
         stderr: {stderr}",
        output.status
    );
    assert_ne!(
        output.status.code(),
        Some(101),
        "`tm sched run` must not panic (101 is Rust's default panic-unwind exit code); \
         stderr: {stderr}"
    );
    assert!(
        !stderr.contains("Cannot start a runtime from within a runtime"),
        "`tm sched run` must not construct a nested tokio runtime; stderr: {stderr}"
    );
}

/// A second, independent proof using a *short* interval instead of a long one: where the test
/// above proves the process survives past the startup panic with nothing left to do, this one
/// proves it survives into a real, executing scheduler loop (`--interval-secs 1` against an
/// otherwise-empty project, long enough for several real ticks) rather than merely not-yet-having-
/// panicked. Doesn't assert on stdout content -- `Renderer::note` uses a bare `println!`, which is
/// block-buffered (not line-buffered) once stdout is a pipe rather than a tty, so anything printed
/// may still be sitting in that buffer, never flushed, at the moment `kill()` lands; asserting on
/// exit status/stderr only avoids depending on that buffering behavior.
#[test]
fn sched_run_survives_several_real_ticks() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_tm"))
        .arg("--project")
        .arg(project.path())
        .arg("sched")
        .arg("run")
        .arg("--interval-secs")
        .arg("1")
        .env("TM_HOME", tm_home.path())
        .env("TM_NOTIFY", "0")
        .env("TM_TEST_MOCK_PROVIDER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `tm sched run`");

    // Empty project, one-second ticks: give it enough time for several real ticks.
    std::thread::sleep(Duration::from_millis(3500));
    let exited_early = child
        .try_wait()
        .expect("poll `tm sched run`'s status")
        .is_some();

    let _ = child.kill();
    let output = child
        .wait_with_output()
        .expect("collect `tm sched run`'s output");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !exited_early,
        "`tm sched run` must still be ticking after 3.5s against an empty project, not exited on \
         its own; exit status {:?}, stderr: {stderr}",
        output.status
    );
    assert_ne!(
        output.status.code(),
        Some(101),
        "`tm sched run` must not panic; stderr: {stderr}"
    );
}
