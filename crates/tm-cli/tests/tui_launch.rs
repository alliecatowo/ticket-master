//! Proves the bare-`tm` dispatch gate in `main.rs`/`tui::should_launch` actually routes to the
//! right loop end to end, against the real compiled `tm` binary — not just the pure
//! `should_launch` unit tests in `tui.rs`, which cannot observe what the binary actually prints.

mod support;

use std::process::{Command, Stdio};
use std::time::Duration;

use portable_pty::CommandBuilder;

/// Create a fresh project directory the `tm` binary can open, the same way `project::open`
/// expects (`.tm/project.db` present) — mirrors `tm-core`'s own test setup rather than shelling
/// out to `tm init` first, since this crate already depends on `tm-core` directly.
fn init_project() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    tm_core::Store::open(tmp.path()).expect("open (and thereby create) a fresh .tm project");
    tmp
}

#[test]
fn bare_tm_on_a_real_tty_launches_the_chat_not_the_plain_loop() {
    let project = init_project();
    // Repo scope via `--project` never reads `$TM_HOME` (D-003's `resolve_scope` returns before
    // consulting it), but every test spawning the real binary sets it to a tempdir regardless so
    // none can ever accidentally touch a real developer's `~/.tm`.
    let tm_home = tempfile::tempdir().expect("tempdir");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tm"));
    cmd.arg("--project");
    cmd.arg(project.path());
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TM_HOME", tm_home.path());
    cmd.env("TM_TEST_MOCK_PROVIDER", "1");

    let mut pty = support::Pty::spawn(cmd, 80, 24).expect("spawn `tm` inside a pty");

    // The chat is the default screen (D-018): the prompt box's placeholder and the status bar's
    // model segment, with zero keys pressed.
    let screen = pty.wait_for("Ask tm anything", Duration::from_secs(10));
    assert!(
        screen.iter().any(|line| line.contains("Ask tm anything")),
        "a bare `tm` on a real tty must open the chat's prompt box, got: {screen:?}"
    );
    assert!(
        screen.iter().any(|line| line.contains("mock/m1")),
        "the status bar must name the model, got: {screen:?}"
    );
    assert!(
        !screen.iter().any(|line| line.trim() == "tm>"),
        "a real tty must not fall through to the plain agent loop's prompt, got: {screen:?}"
    );
    assert!(
        pty.alternate_screen(),
        "the TUI runs in the alternate screen"
    );

    // Ctrl+C twice (within the quit window) is the quit chord; exit 0 and the shell's own screen
    // restored.
    pty.write(&[0x03]).expect("first ctrl-c");
    let screen = pty.wait_for("Press Ctrl+C again", Duration::from_secs(5));
    assert!(
        screen
            .iter()
            .any(|line| line.contains("Press Ctrl+C again to quit")),
        "the first Ctrl+C must explain how to quit, not quit, got: {screen:?}"
    );
    assert!(pty.is_running(), "one Ctrl+C must not quit");
    pty.write(&[0x03]).expect("second ctrl-c");
    let exited_cleanly = pty
        .wait(Duration::from_secs(10))
        .expect("the tm process must exit after Ctrl+C twice");
    assert!(exited_cleanly, "`tm` must exit 0 after Ctrl+C twice");
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !pty.alternate_screen(),
        "exiting must leave the alternate screen (terminal restored)"
    );
}

#[test]
fn bare_tm_with_piped_stdout_uses_the_plain_loop_not_the_tui() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_tm"))
        .arg("--project")
        .arg(project.path())
        .env("TM_HOME", tm_home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `tm` with piped stdio");

    // Close stdin immediately: an EOF is `run_interactive`'s normal exit path (the same as
    // ctrl-d), so the plain loop prints its prompt once and then exits cleanly rather than
    // blocking forever on a read that will never come.
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .expect("`tm` must run to completion on a piped invocation");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("tm> "),
        "a piped/non-tty bare `tm` must still print the plain loop's prompt \
         (agent.rs::run_interactive), got stdout: {stdout:?}"
    );
    assert!(
        output.status.success(),
        "the plain loop must exit 0 on stdin EOF, got status {:?} stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn bare_tm_with_plain_flag_uses_the_plain_loop_even_on_a_real_tty() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tm"));
    cmd.arg("--project");
    cmd.arg(project.path());
    cmd.arg("--plain");
    cmd.env("TERM", "xterm-256color");
    cmd.env("TM_HOME", tm_home.path());

    // `Pty::screen()` trims trailing whitespace per line (see its docs), so the prompt's
    // trailing space never survives into a line to match on; match the trimmed "tm>" instead.
    let mut pty = support::Pty::spawn(cmd, 80, 24).expect("spawn `tm --plain` inside a pty");
    let screen = pty.wait_for("tm>", Duration::from_secs(10));
    assert!(
        screen.iter().any(|line| line.trim() == "tm>"),
        "--plain must force the plain loop's prompt even on a real tty, got: {screen:?}"
    );

    pty.write(&[0x04]).expect("send ctrl-d (EOF)"); // ends `run_interactive`'s readline loop
    let exited_cleanly = pty
        .wait(Duration::from_secs(10))
        .expect("the tm process must exit after ctrl-d");
    assert!(exited_cleanly, "`tm --plain` must exit 0 on ctrl-d");
}
