//! Proves the TUI's navigation shell actually navigates and returns, end to end against the real
//! compiled `tm` binary — not just that each screen renders correctly in isolation (`tm-tui`'s
//! own per-screen unit tests, e.g. `screens::kanban`'s), and not just that the pure routing
//! helpers (`tui.rs`'s `is_tickets_chord`/`is_back_chord`) return the right `bool` in isolation.
//! `crate::component::FocusTree` only ever reaches a screen that is actually wired into
//! `App::focusable_children`'s current branch, so the one thing a unit test cannot observe —
//! whether a key sequence typed into a real terminal actually lands on the right screen at each
//! step — is exactly what this test drives for real, mirroring `tests/tui_launch.rs`'s and
//! `tests/tui_turn.rs`'s own real-pty approach.
//!
//! The scenario is `App`'s own back-stack ask taken literally: `Home -> Kanban -> Detail`, then
//! back twice, landing on `Kanban` (not `Home`) after the first `Esc` and `Home` after the
//! second — proving the stack, not just "Esc always goes home".

mod support;

use std::time::Duration;

use portable_pty::CommandBuilder;
use tm_core::{ExecutorRequirements, RetryPolicy, Store, TicketKind, VerificationPolicy};
use tm_types::{Authority, Budget, ParticipantId, Role, Tolerance};

/// Create a fresh project directory with exactly one real ticket already in it (`Draft`, the
/// state every newly created ticket starts in — `tm_core::ticket`'s own machine docs), so the
/// Kanban board this test navigates into has a real card to select and drill into, not an empty
/// board. Named distinctively enough (`"kanban nav probe"`) that it cannot be confused with
/// anything a mock provider or another test fixture might also print. Returns the ticket's own
/// assigned id alongside the project directory rather than assuming a literal `"T-1"` — this test
/// does not own (and must not hardcode) `Store`'s id-allocation scheme.
fn init_project_with_one_ticket() -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open (and thereby create) a fresh .tm project");
    let actor = ParticipantId::new("human:tester").expect("human:tester is a valid ParticipantId");
    let events = store
        .create_ticket(
            TicketKind::Investigation,
            "kanban nav probe".to_string(),
            None,
            None,
            Authority::root(),
            Vec::new(),
            ExecutorRequirements {
                role: Role::CoderFast,
                human_required: false,
                min_capability: Tolerance::Preferred,
            },
            Vec::new(),
            Vec::new(),
            VerificationPolicy::None,
            Budget::unlimited(),
            RetryPolicy {
                max_attempts: 1,
                base_delay_seconds: 0,
                backoff_multiplier: 1.0,
                max_delay_seconds: 0,
            },
            0,
            actor,
        )
        .expect("create_ticket");
    let ticket_id = events
        .iter()
        .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.to_string()))
        .expect("create_ticket emits ticket.created");
    (tmp, ticket_id)
}

/// `Ctrl+T`, byte-for-byte: terminals send a control letter as `letter - '@'`
/// (`'T'` is `0x54`, `'@'` is `0x40`, so `Ctrl+T` is `0x14`) — the same encoding a real terminal
/// emulator would produce for this chord, which is what `tui.rs`'s `is_tickets_chord` matches
/// against (`crossterm` decodes this back into `KeyCode::Char('t')` + `KeyModifiers::CONTROL`).
const CTRL_T: u8 = 0x14;

#[test]
fn home_to_kanban_to_detail_and_back_twice_lands_on_kanban_then_home() {
    let (project, ticket_id) = init_project_with_one_ticket();
    let detail_heading = format!("Ticket {ticket_id}");
    // Repo scope via `--project` never reads `$TM_HOME`, but every test spawning the real binary
    // sets it to a tempdir regardless so none can ever accidentally touch a real developer's
    // `~/.tm` (D-003) — mirrors `tui_launch.rs`/`tui_turn.rs`.
    let tm_home = tempfile::tempdir().expect("tempdir");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tm"));
    cmd.arg("--project");
    cmd.arg(project.path());
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TM_HOME", tm_home.path());

    let mut pty = support::Pty::spawn(cmd, 100, 30).expect("spawn `tm` inside a pty");

    // 1. Home is the default screen, exactly as before this task: the dashboard and chat input
    // must both be on screen with zero navigation keys pressed yet.
    let screen = pty.wait_for("Tickets", Duration::from_secs(10));
    assert!(
        screen.iter().any(|line| line.contains("Tickets")),
        "bare `tm` must still open on the Home screen's dashboard by default, got: {screen:?}"
    );
    assert!(
        screen.iter().any(|line| line.contains("tm\u{203a}")),
        "the chat input's label must be on screen at launch, got: {screen:?}"
    );

    // 2. Ctrl+T enters the Kanban board.
    pty.write(&[CTRL_T]).expect("send Ctrl+T");
    let screen = pty.wait_for("Left/Right: columns", Duration::from_secs(10));
    assert!(
        screen
            .iter()
            .any(|line| line.contains("Left/Right: columns")),
        "Ctrl+T from Home must open the Kanban board, got: {screen:?}"
    );
    assert!(
        screen.iter().any(|line| line.contains("Draft")),
        "the Kanban board must show a column named after the real TicketState, got: {screen:?}"
    );
    assert!(
        screen
            .iter()
            .any(|line| line.contains(&ticket_id) && line.contains("kanban nav probe")),
        "the real ticket's id and objective must both be on screen as a card, got: {screen:?}"
    );

    // 3. Enter drills into that card's ticket detail screen. Waiting on `detail_heading`
    // specifically (not e.g. the ticket's objective text, which is already on screen as part of
    // the Kanban card and would make `wait_for` return before Enter is even processed) is what
    // makes this an actual wait for the navigation to happen, not a same-frame false positive.
    pty.write(b"\r")
        .expect("press enter to drill into the card");
    let screen = pty.wait_for(&detail_heading, Duration::from_secs(10));
    assert!(
        screen.iter().any(|line| line.contains(&detail_heading)),
        "Enter on the Kanban card must open {ticket_id}'s detail screen, got: {screen:?}"
    );
    assert!(
        screen.iter().any(|line| line.contains("kanban nav probe")),
        "the detail screen must show the real ticket's objective, got: {screen:?}"
    );
    assert!(
        !screen
            .iter()
            .any(|line| line.contains("Left/Right: columns")),
        "the Kanban board must no longer be the screen on screen once drilled into detail, \
         got: {screen:?}"
    );

    // 4. First Esc: back to Kanban, NOT all the way home — the real back-stack assertion this
    // whole test exists for.
    pty.write(&[0x1b]).expect("press Esc");
    let screen = pty.wait_for("Left/Right: columns", Duration::from_secs(10));
    assert!(
        screen
            .iter()
            .any(|line| line.contains("Left/Right: columns")),
        "the first Esc from Detail must return to the Kanban board, got: {screen:?}"
    );
    assert!(
        !screen.iter().any(|line| line.contains(&detail_heading)),
        "the detail screen must no longer be on screen after backing out of it, got: {screen:?}"
    );
    assert!(
        !screen.iter().any(|line| line.contains("tm\u{203a}")),
        "one Esc from two levels deep must land on Kanban, not jump straight past it to Home, \
         got: {screen:?}"
    );

    // 5. Second Esc: back to Home.
    pty.write(&[0x1b]).expect("press Esc again");
    let screen = pty.wait_for("tm\u{203a}", Duration::from_secs(10));
    assert!(
        screen.iter().any(|line| line.contains("tm\u{203a}")),
        "the second Esc must return all the way to Home's chat input, got: {screen:?}"
    );
    assert!(
        screen.iter().any(|line| line.contains("Tickets")),
        "Home's own dashboard must be back on screen too, got: {screen:?}"
    );
    assert!(
        !screen
            .iter()
            .any(|line| line.contains("Left/Right: columns")),
        "the Kanban board must no longer be on screen once all the way back home, got: {screen:?}"
    );

    pty.write(b"q").expect("send the quit key");
    let exited_cleanly = pty
        .wait(Duration::from_secs(10))
        .expect("the tm process must exit after `q`");
    assert!(exited_cleanly, "`tm` must exit 0 after the quit keybinding");
}

#[test]
fn a_bare_t_while_home_is_focused_types_into_the_chat_input_instead_of_navigating() {
    let (project, _ticket_id) = init_project_with_one_ticket();
    let tm_home = tempfile::tempdir().expect("tempdir");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tm"));
    cmd.arg("--project");
    cmd.arg(project.path());
    cmd.env("TERM", "xterm-256color");
    cmd.env("TM_HOME", tm_home.path());

    let mut pty = support::Pty::spawn(cmd, 100, 30).expect("spawn `tm` inside a pty");
    let _ = pty.wait_for("Tickets", Duration::from_secs(10));

    // A bare 't' (no Ctrl) must be swallowed by the always-focused chat input as a typed
    // character -- exactly the reasoning `tui.rs`'s `is_tickets_chord` doc comment gives for
    // requiring a modifier chord instead of a bare letter.
    pty.write(b"test").expect("type a word starting with 't'");
    let screen = pty.wait_for("test", Duration::from_secs(5));
    assert!(
        screen.iter().any(|line| line.contains("test")),
        "typing a word starting with 't' must land in the chat input, not open Kanban, \
         got: {screen:?}"
    );
    assert!(
        !screen
            .iter()
            .any(|line| line.contains("Left/Right: columns")),
        "a bare 't' must never open the Kanban board, got: {screen:?}"
    );

    // `q` is this app's global quit chord regardless of what is focused (`tui.rs`'s
    // `App::is_quit`, an existing tested convention `tests/tui_turn.rs` already relies on), so it
    // ends the process here too even mid-typed-word.
    pty.write(b"q").expect("send the quit key");
    let _ = pty.wait(Duration::from_secs(10));
}

#[test]
fn typing_slash_tickets_and_enter_opens_kanban_without_spawning_a_turn() {
    let (project, _ticket_id) = init_project_with_one_ticket();
    let tm_home = tempfile::tempdir().expect("tempdir");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tm"));
    cmd.arg("--project");
    cmd.arg(project.path());
    cmd.env("TERM", "xterm-256color");
    cmd.env("TM_HOME", tm_home.path());
    // Deliberately no `TM_TEST_MOCK_PROVIDER`: if `is_tickets_command` ever failed to intercept
    // this submission before `App::spawn_turn`, this test would hang on a real network call
    // instead of passing, which is the point -- a mock provider here would hide that failure mode.

    let mut pty = support::Pty::spawn(cmd, 100, 30).expect("spawn `tm` inside a pty");
    let _ = pty.wait_for("Tickets", Duration::from_secs(10));

    // Typed, not a chord: the same chat input every prompt goes through.
    pty.write(b"/tickets").expect("type the tickets command");
    pty.write(b"\r").expect("submit with Enter");

    let screen = pty.wait_for("Left/Right: columns", Duration::from_secs(10));
    assert!(
        screen
            .iter()
            .any(|line| line.contains("Left/Right: columns")),
        "submitting `/tickets` must open the Kanban board, got: {screen:?}"
    );
    assert!(
        !screen.iter().any(|line| line.contains("/tickets")),
        "the command text must not remain visible as a leftover chat submission, got: {screen:?}"
    );

    pty.write(b"q").expect("send the quit key");
    let _ = pty.wait(Duration::from_secs(10));
}
