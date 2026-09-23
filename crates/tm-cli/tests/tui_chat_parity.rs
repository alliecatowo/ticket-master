//! Claude Code parity for the chat, end to end against the real compiled `tm` binary in a real
//! pty (`docs/decisions/D-019-claude-code-parity-shell.md` §1): `!` shell mode, `@` file
//! completion, history (↑, Ctrl+R, persisted across runs), collapsed pastes, Ctrl+C clear-then-
//! exit, the Shift+Tab mode indicator, the `?` panel, the Ctrl+O viewer, queued messages, Esc
//! interrupting a running command, and `/status`.
//!
//! The mock provider (`TM_TEST_MOCK_PROVIDER=1`) answers instantly with text only, so the tests
//! that need a *running* turn use a `!sleep` shell command instead: it marks the chat busy exactly
//! as a model turn does.

mod support;

use std::path::Path;
use std::time::Duration;

use portable_pty::CommandBuilder;

const WAIT: Duration = Duration::from_secs(10);

/// A project with a couple of files to complete against.
fn init_project() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    tm_core::Store::open(tmp.path()).expect("open (and thereby create) a fresh .tm project");
    std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir src");
    std::fs::write(tmp.path().join("src/zebra_module.rs"), "fn main() {}\n").expect("write");
    std::fs::write(tmp.path().join("notes.md"), "# notes\n").expect("write");
    tmp
}

fn spawn(project: &Path, tm_home: &Path) -> support::Pty {
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tm"));
    cmd.arg("--project");
    cmd.arg(project);
    cmd.cwd(project);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TM_TEST_MOCK_PROVIDER", "1");
    cmd.env("TM_HOME", tm_home);
    cmd.env("TM_NOTIFY", "0");
    let mut pty = support::Pty::spawn(cmd, 100, 30).expect("spawn `tm` inside a pty");
    let screen = pty.wait_for("Ask tm anything", WAIT);
    assert!(has(&screen, "Ask tm anything"), "tm came up: {screen:?}");
    pty
}

fn has(screen: &[String], needle: &str) -> bool {
    screen.iter().any(|line| line.contains(needle))
}

/// The prompt box's text row (the line with the `›`/`!` marker inside the box).
fn prompt_row(screen: &[String]) -> String {
    screen
        .iter()
        .rev()
        .find(|l| l.starts_with("│ › ") || l.starts_with("│ ! "))
        .cloned()
        .unwrap_or_default()
}

fn quit(pty: &mut support::Pty) {
    pty.write(&[0x15]).expect("ctrl-u");
    pty.write(&[0x04]).expect("ctrl-d");
    assert!(
        pty.wait(WAIT).expect("wait for exit"),
        "tm exits 0 on ctrl-d"
    );
}

#[test]
fn bang_runs_a_shell_command_and_shows_it_as_a_bash_block_then_ctrl_o_expands_it() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn(project.path(), tm_home.path());

    pty.write(b"!").expect("!");
    let screen = pty.wait_for("! for bash mode", WAIT);
    assert!(has(&screen, "! for bash mode"), "{screen:?}");
    pty.write(b"seq 1 30").expect("type command");
    pty.write(b"\r").expect("enter");
    let screen = pty.wait_for("+26 lines (ctrl+o to expand)", WAIT);
    assert!(has(&screen, "! seq 1 30"), "the command is echoed: {screen:?}");
    assert!(has(&screen, "⎿  1"), "output hangs under ⎿: {screen:?}");
    assert!(
        has(&screen, "… +26 lines (ctrl+o to expand)"),
        "long output collapses: {screen:?}"
    );

    // Ctrl+O shows every line; q closes it without typing a q.
    pty.write(&[0x0f]).expect("ctrl-o");
    let screen = pty.wait_for("Transcript", WAIT);
    assert!(has(&screen, "q/esc close"), "{screen:?}");
    pty.write(b"G").expect("bottom");
    let screen = pty.wait_for("    30", WAIT);
    assert!(has(&screen, "    30"), "the viewer has the full output: {screen:?}");
    pty.write(b"q").expect("q");
    let screen = pty.wait_until_gone("q/esc close", WAIT);
    assert!(!prompt_row(&screen).contains('q'), "{screen:?}");

    // The output went into the conversation: the next turn runs normally after it.
    pty.write(b"what did it print?\r").expect("send");
    let screen = pty.wait_for("mock provider:", Duration::from_secs(15));
    assert!(has(&screen, "mock provider:"), "{screen:?}");
    quit(&mut pty);
}

#[test]
fn at_completes_a_file_path_and_tab_inserts_it() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn(project.path(), tm_home.path());

    pty.write(b"look at @zebra").expect("type mention");
    let screen = pty.wait_for("zebra_module.rs", WAIT);
    assert!(has(&screen, "src/"), "the popup shows the directory: {screen:?}");
    pty.write(b"\t").expect("tab");
    let screen = pty.wait_for("@src/zebra_module.rs", WAIT);
    assert!(
        prompt_row(&screen).contains("look at @src/zebra_module.rs"),
        "{screen:?}"
    );
    quit(&mut pty);
}

#[test]
fn history_recalls_with_up_searches_with_ctrl_r_and_survives_a_restart() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn(project.path(), tm_home.path());

    pty.write(b"first prompt alpha\r").expect("send");
    let _ = pty.wait_for("mock provider:", Duration::from_secs(15));
    pty.write(b"second prompt beta\r").expect("send");
    let _ = pty.settle(Duration::from_millis(600), Duration::from_secs(15));

    pty.write(b"\x1b[A").expect("up");
    let screen = pty.wait_for("│ › second prompt beta", WAIT);
    assert!(prompt_row(&screen).contains("second prompt beta"), "{screen:?}");
    pty.write(&[0x15]).expect("ctrl-u");

    pty.write(&[0x12]).expect("ctrl-r");
    pty.write(b"alp").expect("query");
    let screen = pty.wait_for("search history: alp", WAIT);
    assert!(prompt_row(&screen).contains("first prompt alpha"), "{screen:?}");
    pty.write(b"\t").expect("accept");
    let screen = pty.wait_until_gone("search history", WAIT);
    assert!(prompt_row(&screen).contains("first prompt alpha"), "{screen:?}");
    quit(&mut pty);

    // A new run of tm in the same project remembers.
    let mut pty = spawn(project.path(), tm_home.path());
    pty.write(b"\x1b[A").expect("up");
    let screen = pty.wait_for("│ › second prompt beta", WAIT);
    assert!(prompt_row(&screen).contains("second prompt beta"), "{screen:?}");
    quit(&mut pty);
}

#[test]
fn a_long_paste_collapses_to_a_placeholder() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn(project.path(), tm_home.path());

    let pasted: String = (1..=10).map(|i| format!("pasted line {i}\n")).collect();
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend_from_slice(pasted.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    pty.write(&bytes).expect("paste");
    let screen = pty.wait_for("[Pasted text #1 +9 lines]", WAIT);
    assert!(
        prompt_row(&screen).contains("[Pasted text #1 +9 lines]"),
        "{screen:?}"
    );
    assert!(!has(&screen, "pasted line 5"), "{screen:?}");
    quit(&mut pty);
}

#[test]
fn ctrl_c_clears_first_and_exits_on_the_second_press() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn(project.path(), tm_home.path());

    pty.write(b"half a thought").expect("type");
    let _ = pty.wait_for("half a thought", WAIT);
    pty.write(&[0x03]).expect("ctrl-c");
    let screen = pty.wait_for("Press Ctrl+C again to quit", WAIT);
    assert!(!prompt_row(&screen).contains("half a thought"), "{screen:?}");
    assert!(pty.is_running(), "one Ctrl+C with text only clears it");
    pty.write(&[0x03]).expect("ctrl-c again");
    assert!(pty.wait(WAIT).expect("wait"), "the second press exits");
}

#[test]
fn shift_tab_cycles_the_mode_indicator_and_question_mark_shows_shortcuts() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn(project.path(), tm_home.path());

    pty.write(b"\x1b[Z").expect("shift-tab");
    let screen = pty.wait_for("plan mode on (shift+tab to cycle)", WAIT);
    assert!(has(&screen, "⏸ plan mode on"), "{screen:?}");
    pty.write(b"\x1b[Z").expect("shift-tab");
    let screen = pty.wait_for("ask mode on", WAIT);
    assert!(has(&screen, "⏵ ask mode on (shift+tab to cycle)"), "{screen:?}");
    pty.write(b"\x1b[Z").expect("shift-tab");
    let screen = pty.wait_until_gone("ask mode on", WAIT);
    assert!(has(&screen, "? for shortcuts"), "{screen:?}");

    pty.write(b"?").expect("?");
    let screen = pty.wait_for("! for bash mode", WAIT);
    assert!(
        has(&screen, "ctrl + o for transcript") && has(&screen, "ctrl + r to search history"),
        "{screen:?}"
    );
    pty.write(b"?").expect("?");
    let _ = pty.wait_until_gone("! for bash mode", WAIT);

    pty.write(b"/status\r").expect("/status");
    let screen = pty.wait_for("Model: mock/m1", WAIT);
    assert!(
        has(&screen, "Mode: auto") && has(&screen, "Provider: mock"),
        "{screen:?}"
    );
    quit(&mut pty);
}

#[test]
fn a_message_typed_while_busy_is_queued_then_sent_and_esc_interrupts() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn(project.path(), tm_home.path());

    pty.write(b"!sleep 2\r").expect("run a slow command");
    let _ = pty.wait_for("Running", WAIT);
    pty.write(b"queued hello\r").expect("send while busy");
    let screen = pty.wait_for("(queued)", WAIT);
    assert!(has(&screen, "queued hello  (queued)"), "{screen:?}");
    // When the command finishes the queued message goes out on its own.
    let screen = pty.wait_for("mock provider:", Duration::from_secs(20));
    assert!(has(&screen, "› queued hello"), "{screen:?}");
    assert!(!has(&screen, "(queued)"), "{screen:?}");

    // Esc stops a running command.
    pty.write(b"!sleep 30\r").expect("run a long command");
    let _ = pty.wait_for("Running", WAIT);
    pty.write(b"\x1b").expect("esc");
    let screen = pty.wait_for("Interrupted", WAIT);
    assert!(has(&screen, "Error: Interrupted"), "{screen:?}");
    quit(&mut pty);
}
