//! Proves the TUI's chat input is a real chat surface end to end, against the real compiled `tm`
//! binary: type a prompt into the home screen the moment it launches (no keybinding needed to
//! "enter chat mode" first), press Enter, and observe that the turn's real output reached the
//! screen (the stream pane) — not just that the input widget accepted keystrokes — and that
//! chatting created no ticket (a session is not a ticket, `docs/decisions/D-017-session-ticket-
//! executor-model.md`).
//!
//! Runs against `TM_TEST_MOCK_PROVIDER=1` (see `crates/tm-cli/src/agent.rs`'s
//! `build_mock_fabric`), a deterministic `tm_provider::MockProvider`-backed fabric baked into the
//! compiled binary for exactly this situation: an out-of-process pty test has no way to inject a
//! Rust closure or a pre-built `Fabric` into the spawned child, so a real network call or API key
//! is the only alternative without this hook.

mod support;

use std::time::Duration;

use portable_pty::CommandBuilder;

/// Create a fresh project directory the `tm` binary can open, the same way `project::open`
/// expects (`.tm/project.db` present) — mirrors `tests/tui_launch.rs`'s own setup.
fn init_project() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    tm_core::Store::open(tmp.path()).expect("open (and thereby create) a fresh .tm project");
    tmp
}

#[test]
fn typing_a_prompt_into_the_tui_shows_turn_output_without_creating_a_ticket() {
    let project = init_project();
    // Repo scope via `--project` never reads `$TM_HOME`, but every test spawning the real binary
    // sets it to a tempdir regardless so none can ever accidentally touch a real developer's
    // `~/.tm` (D-003).
    let tm_home = tempfile::tempdir().expect("tempdir");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tm"));
    cmd.arg("--project");
    cmd.arg(project.path());
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TM_TEST_MOCK_PROVIDER", "1");
    cmd.env("TM_HOME", tm_home.path());

    let mut pty = support::Pty::spawn(cmd, 100, 30).expect("spawn `tm` inside a pty");

    // The home screen must be up (dashboard + chat input) before typing means anything.
    let screen = pty.wait_for("Tickets", Duration::from_secs(10));
    assert!(
        screen.iter().any(|line| line.contains("Tickets")),
        "the home screen's dashboard must be on screen at launch, got: {screen:?}"
    );
    assert!(
        screen.iter().any(|line| line.contains("tm\u{203a}")),
        "the chat input's label must be on screen at launch with zero extra keystrokes, \
         got: {screen:?}"
    );

    // No slash command, no keybinding to "enter chat mode" — just type, the way the project
    // owner asked for. Avoids the letter 'q' deliberately: this app's quit chord is a bare `q`
    // regardless of what is focused (see `tui.rs`'s `App::handle_event`), an existing, tested
    // convention (`tui_launch.rs`) this test does not change.
    let prompt = "fix the flaky test";
    pty.write(prompt.as_bytes()).expect("type the prompt");

    let screen = pty.wait_for(prompt, Duration::from_secs(5));
    assert!(
        screen.iter().any(|line| line.contains(prompt)),
        "the typed text must be visible in the chat input before Enter is pressed, got: {screen:?}"
    );

    pty.write(b"\r").expect("press enter to submit the prompt");

    // The mock fabric's scripted reply (`build_mock_fabric` in `agent.rs`) ends the turn as a
    // plain reply (`AgentOutcome::Replied`) after one step, whose text is this literal string —
    // real agent output, driven through the exact same `AgentSession::run_turn_streaming` the
    // plain `tm -p` loop uses, reaching the screen via `AppMessage::StreamChunk`.
    let screen = pty.wait_for("mock provider:", Duration::from_secs(15));
    assert!(
        screen.iter().any(|line| line.contains("mock provider:")),
        "the turn's real output must reach the stream pane, got: {screen:?}"
    );

    pty.write(b"q").expect("send the quit key");
    let exited_cleanly = pty
        .wait(Duration::from_secs(10))
        .expect("the tm process must exit after `q`");
    assert!(exited_cleanly, "`tm` must exit 0 after the quit keybinding");

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
