//! The chat screen's discoverability surfaces, end to end against the real compiled `tm` binary
//! (`docs/decisions/D-018-tui-chat-first-shell.md`, `docs/decisions/D-019-claude-code-parity-
//! shell.md`): `/` on an empty prompt opens the command popup, typing filters it, Enter runs the
//! selected command; `?` on an empty prompt opens the shortcuts panel.

mod support;

use std::time::Duration;

use portable_pty::CommandBuilder;

fn init_project() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    tm_core::Store::open(tmp.path()).expect("open (and thereby create) a fresh .tm project");
    tmp
}

fn has(screen: &[String], needle: &str) -> bool {
    screen.iter().any(|line| line.contains(needle))
}

#[test]
fn slash_popup_filters_and_enter_runs_the_command_and_question_mark_opens_shortcuts() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tm"));
    cmd.arg("--project");
    cmd.arg(project.path());
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TM_TEST_MOCK_PROVIDER", "1");
    cmd.env("TM_HOME", tm_home.path());
    cmd.env("TM_NOTIFY", "0");
    let mut pty = support::Pty::spawn(cmd, 100, 30).expect("spawn `tm` inside a pty");
    let _ = pty.wait_for("Ask tm anything", Duration::from_secs(10));

    // `/` opens the popup listing the commands with their descriptions.
    pty.write(b"/").expect("type /");
    let screen = pty.wait_for("/resume", Duration::from_secs(5));
    assert!(
        has(&screen, "/help") && has(&screen, "/clear") && has(&screen, "/resume"),
        "`/` must open the command popup, got: {screen:?}"
    );
    assert!(
        has(&screen, "Start a fresh conversation"),
        "each command carries a one-line description, got: {screen:?}"
    );

    // Typing filters it.
    pty.write(b"cle").expect("filter");
    let screen = pty.wait_until_gone("/resume", Duration::from_secs(5));
    assert!(
        has(&screen, "/clear"),
        "the filter keeps the match, got: {screen:?}"
    );
    assert!(
        !has(&screen, "/resume"),
        "the filter drops non-matches, got: {screen:?}"
    );

    // Enter runs the selected command: /clear starts a fresh conversation and says so.
    pty.write(b"\r").expect("Enter");
    let screen = pty.wait_for("Fresh conversation", Duration::from_secs(5));
    assert!(
        has(&screen, "Fresh conversation"),
        "Enter in the popup must run the command, got: {screen:?}"
    );

    // `?` on the empty prompt opens the shortcuts panel; `?` again closes it.
    pty.write(b"?").expect("type ?");
    let screen = pty.wait_for("! for bash mode", Duration::from_secs(5));
    assert!(
        has(&screen, "! for bash mode")
            && has(&screen, "@ for file paths")
            && has(&screen, "double tap esc to clear input")
            && has(&screen, "shift + tab to cycle modes"),
        "`?` must open the shortcuts panel, got: {screen:?}"
    );
    pty.write(b"?").expect("type ? again");
    let screen = pty.wait_until_gone("! for bash mode", Duration::from_secs(5));
    assert!(
        !has(&screen, "! for bash mode"),
        "? closes the panel, got: {screen:?}"
    );

    // `?` inside a message is just a character.
    pty.write(b"why?").expect("type a question");
    let screen = pty.wait_for("why?", Duration::from_secs(5));
    assert!(
        has(&screen, "why?") && !has(&screen, "! for bash mode"),
        "got: {screen:?}"
    );

    pty.write(&[0x03, 0x03]).expect("ctrl-c twice");
    let exited = pty.wait(Duration::from_secs(10)).expect("wait for exit");
    assert!(exited, "`tm` must exit 0 after Ctrl+C twice");
}
