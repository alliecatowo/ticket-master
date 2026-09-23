//! Proves the TUI's chat is a real chat surface end to end, against the real compiled `tm`
//! binary: type a prompt the moment it launches, press Enter, and observe the prompt echoed in
//! the transcript and the turn's real reply rendered beneath it — and that chatting created no
//! ticket (a session is not a ticket, `docs/decisions/D-017-session-ticket-executor-model.md`).
//!
//! The prompt deliberately contains the letter `q`: bare `q` used to be a global quit key that
//! fired even while typing (the showstopper `docs/decisions/D-018-tui-chat-first-shell.md`
//! fixes), so this test is also the regression test for "typing never triggers a shortcut".
//!
//! Runs against `TM_TEST_MOCK_PROVIDER=1` (see `crates/tm-cli/src/agent.rs`'s
//! `build_mock_fabric`), a deterministic `tm_provider::MockProvider`-backed fabric baked into the
//! compiled binary for exactly this situation: an out-of-process pty test cannot inject a fabric.

mod support;

use std::time::Duration;

use portable_pty::CommandBuilder;

/// Create a fresh project directory the `tm` binary can open (`.tm/project.db` present).
fn init_project() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    tm_core::Store::open(tmp.path()).expect("open (and thereby create) a fresh .tm project");
    tmp
}

#[test]
fn typing_a_prompt_with_q_echoes_it_shows_the_reply_and_creates_no_ticket() {
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

    let screen = pty.wait_for("Ask tm anything", Duration::from_secs(10));
    assert!(
        screen.iter().any(|line| line.contains("Ask tm anything")),
        "the chat's prompt box must be up at launch, got: {screen:?}"
    );
    let status = screen.last().cloned().unwrap_or_default();
    assert!(
        status.contains("mock/m1") && status.contains("no ticket"),
        "the bottom row is the status bar, naming the model and the (absent) ticket, got: \
         {status:?}"
    );

    // `q` twice, `quit` spelled out: none of it may do anything but type.
    let prompt = "quick question: is the queue quiet?";
    pty.write(prompt.as_bytes()).expect("type the prompt");
    let screen = pty.wait_for(prompt, Duration::from_secs(5));
    assert!(
        screen.iter().any(|line| line.contains(prompt)),
        "the typed text must be visible in the prompt box, got: {screen:?}"
    );
    assert!(pty.is_running(), "typing `q` must never quit");

    pty.write(b"\r").expect("press enter to submit the prompt");

    // The mock fabric's scripted reply ends the turn as a plain reply (`AgentOutcome::Replied`);
    // its text reaches the transcript through `AppMessage::Turn`.
    let screen = pty.wait_for("mock provider:", Duration::from_secs(15));
    assert!(
        screen.iter().any(|line| line.contains("mock provider:")),
        "the turn's real reply must reach the transcript, got: {screen:?}"
    );
    assert!(
        screen
            .iter()
            .any(|line| line.contains(&format!("> {prompt}"))
                || line.contains(&format!("› {prompt}"))),
        "the submitted prompt must be echoed in the transcript with its marker, got: {screen:?}"
    );
    assert!(
        !screen
            .iter()
            .any(|line| line.contains("Ask tm anything") && line.contains(prompt)),
        "the prompt box must be cleared after sending, got: {screen:?}"
    );

    // Ctrl+D on the (now empty) prompt quits too.
    pty.write(&[0x04]).expect("ctrl-d on an empty prompt");
    let exited_cleanly = pty
        .wait(Duration::from_secs(10))
        .expect("the tm process must exit after ctrl-d");
    assert!(
        exited_cleanly,
        "`tm` must exit 0 after ctrl-d on an empty prompt"
    );

    let view = tm_core::Store::open(project.path())
        .expect("reopen the project the TUI wrote to")
        .view()
        .expect("materialized view");
    assert!(
        view.tickets.is_empty(),
        "chatting in the TUI must not create a ticket, found {:?}",
        view.tickets.keys().collect::<Vec<_>>()
    );
}
