//! Proves the TUI's navigation shell actually navigates and returns, end to end against the real
//! compiled `tm` binary (`docs/decisions/D-018-tui-chat-first-shell.md`, and D-019 §2 for the
//! tickets screen that replaced D-018's home): the chat is the root, `←` `←`/Ctrl+T/`/tickets`
//! reach the tickets screen, the Kanban board and ticket detail hang off it, and Esc walks back
//! one level at a time — the back-stack, not "Esc always goes to chat".

mod support;

use std::time::Duration;

use portable_pty::CommandBuilder;
use tm_core::{ExecutorRequirements, RetryPolicy, Store, TicketKind, VerificationPolicy};
use tm_types::{Authority, Budget, ParticipantId, Role, Tolerance};

/// A fresh project with exactly one real ticket (`Draft`), named distinctively enough that it
/// cannot be confused with anything else on screen. Returns the ticket's own assigned id rather
/// than assuming a literal `T-1`.
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

/// Spawn `tm` on `project` in a 100x30 pty. `mock` sets `TM_TEST_MOCK_PROVIDER`.
fn spawn(project: &std::path::Path, tm_home: &std::path::Path, mock: bool) -> support::Pty {
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tm"));
    cmd.arg("--project");
    cmd.arg(project);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TM_HOME", tm_home);
    if mock {
        cmd.env("TM_TEST_MOCK_PROVIDER", "1");
    }
    support::Pty::spawn(cmd, 100, 30).expect("spawn `tm` inside a pty")
}

/// `Ctrl+T`: a control letter is `letter - '@'` (`'T'` is 0x54, so 0x14).
const CTRL_T: u8 = 0x14;
const ESC: u8 = 0x1b;
/// The Left arrow's escape sequence (CSI D).
const LEFT: &[u8] = b"\x1b[D";

/// Text only the tickets screen shows (its dispatch input's placeholder).
const TICKETS_MARK: &str = "Describe a task for a background worker";
/// Text only the chat shows.
const CHAT_MARK: &str = "Ask tm anything";
/// Text only the Kanban board shows.
const BOARD_MARK: &str = "Left/Right: columns";

fn has(screen: &[String], needle: &str) -> bool {
    screen.iter().any(|line| line.contains(needle))
}

fn quit(pty: &mut support::Pty) {
    pty.write(&[0x03]).expect("ctrl-c");
    pty.write(&[0x03]).expect("ctrl-c");
    let exited = pty.wait(Duration::from_secs(10)).expect("wait for exit");
    assert!(exited, "`tm` must exit 0 after Ctrl+C twice");
}

#[test]
fn chat_to_tickets_to_board_to_detail_and_back_one_level_at_a_time() {
    let (project, ticket_id) = init_project_with_one_ticket();
    let detail_heading = format!("Ticket {ticket_id}");
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn(project.path(), tm_home.path(), true);

    // 1. The chat is the default screen.
    let screen = pty.wait_for(CHAT_MARK, Duration::from_secs(10));
    assert!(
        has(&screen, CHAT_MARK),
        "bare `tm` opens the chat, got: {screen:?}"
    );
    assert!(
        !has(&screen, TICKETS_MARK),
        "tickets is not the default, got: {screen:?}"
    );

    // 2. Ctrl+T opens tickets, which lists the real ticket under its group, and no sessions:
    //    conversations are not rows here (D-019 §3).
    pty.write(&[CTRL_T]).expect("send Ctrl+T");
    let screen = pty.wait_for(TICKETS_MARK, Duration::from_secs(10));
    assert!(
        has(&screen, TICKETS_MARK),
        "Ctrl+T must open tickets, got: {screen:?}"
    );
    assert!(
        screen
            .iter()
            .any(|l| l.contains(&ticket_id) && l.contains("kanban nav probe")),
        "tickets must list the real ticket, got: {screen:?}"
    );
    assert!(has(&screen, "Queued"), "a draft is queued, got: {screen:?}");
    assert!(
        !has(&screen, "Sessions") && !has(&screen, "this conversation"),
        "sessions are not listed on the tickets screen, got: {screen:?}"
    );

    // 3. `b` opens the board.
    pty.write(b"b").expect("press b");
    let screen = pty.wait_for(BOARD_MARK, Duration::from_secs(10));
    assert!(
        has(&screen, BOARD_MARK),
        "`b` on tickets opens the board, got: {screen:?}"
    );
    assert!(
        has(&screen, "Draft"),
        "the board names the real states, got: {screen:?}"
    );

    // 4. Enter on the card drills into the detail.
    pty.write(b"\r").expect("enter on the card");
    let screen = pty.wait_for(&detail_heading, Duration::from_secs(10));
    assert!(
        has(&screen, &detail_heading),
        "Enter on the Kanban card must open {ticket_id}'s detail, got: {screen:?}"
    );

    // 5. Esc: back to the board, not further.
    pty.write(&[ESC]).expect("Esc");
    let screen = pty.wait_for(BOARD_MARK, Duration::from_secs(10));
    assert!(
        has(&screen, BOARD_MARK),
        "first Esc returns to the board, got: {screen:?}"
    );
    assert!(!has(&screen, &detail_heading));

    // 6. Esc: back to tickets.
    pty.write(&[ESC]).expect("Esc");
    let screen = pty.wait_for(TICKETS_MARK, Duration::from_secs(10));
    assert!(
        has(&screen, TICKETS_MARK),
        "second Esc returns to tickets, got: {screen:?}"
    );
    assert!(!has(&screen, BOARD_MARK));

    // 7. Esc: back to the chat.
    pty.write(&[ESC]).expect("Esc");
    let screen = pty.wait_for(CHAT_MARK, Duration::from_secs(10));
    assert!(
        has(&screen, CHAT_MARK),
        "third Esc returns to the chat, got: {screen:?}"
    );
    assert!(!has(&screen, TICKETS_MARK));

    quit(&mut pty);
}

#[test]
fn left_twice_on_an_empty_prompt_opens_tickets_and_esc_comes_back() {
    let (project, _ticket_id) = init_project_with_one_ticket();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn(project.path(), tm_home.path(), true);
    let _ = pty.wait_for(CHAT_MARK, Duration::from_secs(10));

    // The first ← only says what a second one does, as Claude Code does.
    pty.write(LEFT).expect("Left");
    let screen = pty.wait_for("Press ← again to open tickets", Duration::from_secs(10));
    assert!(
        has(&screen, "Press ← again to open tickets"),
        "the first ← shows the hint, got: {screen:?}"
    );
    assert!(!has(&screen, TICKETS_MARK), "one ← does not navigate yet");

    pty.write(LEFT).expect("Left again");
    let screen = pty.wait_for(TICKETS_MARK, Duration::from_secs(10));
    assert!(
        has(&screen, TICKETS_MARK),
        "a second ← opens tickets, got: {screen:?}"
    );

    pty.write(&[ESC]).expect("Esc");
    let screen = pty.wait_for(CHAT_MARK, Duration::from_secs(10));
    assert!(
        has(&screen, CHAT_MARK),
        "Esc on tickets returns to the chat, got: {screen:?}"
    );

    // With text in the prompt, ← is just cursor movement.
    pty.write(b"ab").expect("type");
    let _ = pty.wait_for("ab", Duration::from_secs(5));
    pty.write(LEFT).expect("Left");
    pty.write(LEFT).expect("Left");
    let screen = pty.settle(Duration::from_millis(400), Duration::from_millis(600));
    assert!(
        !has(&screen, TICKETS_MARK),
        "← with text in the prompt must not navigate, got: {screen:?}"
    );

    quit(&mut pty);
}

#[test]
fn typing_letters_that_used_to_be_shortcuts_only_types() {
    let (project, _ticket_id) = init_project_with_one_ticket();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn(project.path(), tm_home.path(), true);
    let _ = pty.wait_for(CHAT_MARK, Duration::from_secs(10));

    // `q` was a global quit key, `t` a navigation key in an earlier design; `b` is a key on the
    // tickets screen. In the prompt, all of them are just text.
    pty.write(b"qtb q").expect("type former shortcut letters");
    let screen = pty.wait_for("qtb q", Duration::from_secs(5));
    assert!(
        has(&screen, "qtb q"),
        "the letters must land in the prompt, got: {screen:?}"
    );
    assert!(pty.is_running(), "typing q must not quit");
    assert!(!has(&screen, TICKETS_MARK) && !has(&screen, BOARD_MARK));

    quit(&mut pty);
}

#[test]
fn slash_tickets_opens_tickets_without_spawning_a_turn() {
    let (project, _ticket_id) = init_project_with_one_ticket();
    let tm_home = tempfile::tempdir().expect("tempdir");
    // Deliberately no `TM_TEST_MOCK_PROVIDER`: if the command ever fell through to a turn, this
    // would attempt a real provider call and show an error instead of the tickets screen.
    let mut pty = spawn(project.path(), tm_home.path(), false);
    let _ = pty.wait_for(CHAT_MARK, Duration::from_secs(10));

    pty.write(b"/tickets").expect("type the command");
    let _ = pty.wait_for("/tickets", Duration::from_secs(5));
    pty.write(b"\r").expect("Enter");
    let screen = pty.wait_for(TICKETS_MARK, Duration::from_secs(10));
    assert!(
        has(&screen, TICKETS_MARK),
        "`/tickets` must open the tickets screen, got: {screen:?}"
    );

    pty.write(&[ESC]).expect("Esc");
    let screen = pty.wait_for(CHAT_MARK, Duration::from_secs(10));
    assert!(
        !has(&screen, "/tickets") && !has(&screen, "could not run"),
        "the command neither stays in the prompt nor runs a turn, got: {screen:?}"
    );

    quit(&mut pty);
}
