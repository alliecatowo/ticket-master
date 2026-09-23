//! The chat screen's discoverability surfaces, end to end against the real compiled `tm` binary
//! (`docs/decisions/D-018-tui-chat-first-shell.md`): `/` on an empty prompt opens the command
//! popup, typing filters it, Enter runs the selected command; `?` on an empty prompt opens help.

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
fn slash_popup_filters_and_enter_runs_the_command_and_question_mark_opens_help() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tm"));
    cmd.arg("--project");
    cmd.arg(project.path());
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TM_TEST_MOCK_PROVIDER", "1");
    cmd.env("TM_HOME", tm_home.path());
    let mut pty = support::Pty::spawn(cmd, 100, 30).expect("spawn `tm` inside a pty");
    let _ = pty.wait_for("Ask tm anything", Duration::from_secs(10));

    // `/` opens the popup listing every command with its description.
    pty.write(b"/").expect("type /");
    let screen = pty.wait_for("/attach <ticket>", Duration::from_secs(5));
    assert!(
        has(&screen, "/attach <ticket>") && has(&screen, "/help") && has(&screen, "/clear"),
        "`/` must open the command popup, got: {screen:?}"
    );
    assert!(
        has(&screen, "Attach this conversation to a ticket"),
        "each command carries a one-line description, got: {screen:?}"
    );

    // Typing filters it.
    pty.write(b"cle").expect("filter");
    let screen = pty.wait_until_gone("/attach <ticket>", Duration::from_secs(5));
    assert!(
        has(&screen, "/clear"),
        "the filter keeps the match, got: {screen:?}"
    );
    assert!(
        !has(&screen, "/attach <ticket>"),
        "the filter drops non-matches, got: {screen:?}"
    );

    // Enter runs the selected command: /clear starts a fresh conversation and says so.
    pty.write(b"\r").expect("Enter");
    let screen = pty.wait_for("Fresh conversation", Duration::from_secs(5));
    assert!(
        has(&screen, "Fresh conversation"),
        "Enter in the popup must run the command, got: {screen:?}"
    );
    assert!(
        !has(&screen, "/attach <ticket>"),
        "the popup closes, got: {screen:?}"
    );

    // `?` on the empty prompt opens help; Esc closes it.
    pty.write(b"?").expect("type ?");
    let screen = pty.wait_for("tm help", Duration::from_secs(5));
    assert!(
        has(&screen, "tm help"),
        "`?` must open help, got: {screen:?}"
    );
    assert!(
        has(&screen, "Sessions & tickets") && has(&screen, "ctrl+c twice"),
        "help explains keys and how sessions relate to tickets, got: {screen:?}"
    );
    pty.write(&[0x1b]).expect("Esc");
    let screen = pty.wait_until_gone("tm help", Duration::from_secs(5));
    assert!(!has(&screen, "tm help"), "Esc closes help, got: {screen:?}");

    // `?` inside a message is just a character.
    pty.write(b"why?").expect("type a question");
    let screen = pty.wait_for("why?", Duration::from_secs(5));
    assert!(
        has(&screen, "why?") && !has(&screen, "tm help"),
        "got: {screen:?}"
    );

    pty.write(&[0x03, 0x03]).expect("ctrl-c twice");
    let exited = pty.wait(Duration::from_secs(10)).expect("wait for exit");
    assert!(exited, "`tm` must exit 0 after Ctrl+C twice");
}
