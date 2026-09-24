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

/// `git init` a repository at `root` with one real commit — `tm-codeintel`'s own test project
/// helper (`new_project` in `crates/tm-codeintel/src/api.rs`) always does this too, and
/// `search_exact` finds nothing in a directory that isn't a real git repository. Mirrors
/// `tests/promotion.rs`'s identical helper.
fn init_git_repo_with_a_commit(root: &std::path::Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git should run in test environment");
        assert!(status.success(), "git {:?} failed", args);
    };
    run(&["init", "--quiet", "--initial-branch=main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    std::fs::write(root.join("needle.txt"), "foo bar\n").expect("write needle.txt");
    run(&["add", "needle.txt"]);
    run(&["commit", "--quiet", "-m", "init"]);
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

/// Run the real `tm` binary non-interactively (piped stdio, `TM_HOME` isolated to a tempdir) and
/// wait for it to exit. Mirrors `tests/promotion.rs`'s `run_tm_in` convention; these three tests
/// don't need a pty, only a spawned process and its captured output.
fn run_tm(dir: &std::path::Path, tm_home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tm"))
        .args(args)
        .current_dir(dir)
        .env("TM_HOME", tm_home)
        .env("TM_NOTIFY", "0")
        .env("TM_TEST_MOCK_PROVIDER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `tm`")
        .wait_with_output()
        .expect("`tm` must run to completion")
}

/// s1-cli-tree-regroup: `tm --help` groups daily verbs first, then planning, then serving, folds
/// everything else (the explicit plumbing verbs plus the rest of the top-level tree) out of the
/// listing into an `after_help` "More commands" note, and still runs every hidden verb (see the
/// next two tests). Only the `Commands:` section is checked for the grouping/hiding assertions
/// (not the whole `--help` text, so the "More commands" note naming a hidden verb in prose
/// doesn't make this test see it as listed there).
#[test]
fn tm_help_groups_daily_commands_and_hides_plumbing_verbs() {
    let tm_home = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_tm"))
        .arg("--help")
        .env("TM_HOME", tm_home.path())
        .output()
        .expect("`tm --help` must run");
    assert!(output.status.success(), "`tm --help` must exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let commands_start = stdout
        .find("Commands:")
        .unwrap_or_else(|| panic!("`tm --help` must have a `Commands:` section, got: {stdout}"));
    let commands_section = match stdout[commands_start..].find("\nOptions:") {
        Some(options_offset) => &stdout[commands_start..commands_start + options_offset],
        None => &stdout[commands_start..],
    };

    // Daily verbs (`search`/`run`/`doctor`) must precede planning verbs (`milestone`) must
    // precede serving verbs (`serve`) — today's declaration order puts `milestone` first, so
    // this only passes once the regroup lands.
    let pos = |needle: &str| {
        commands_section.find(needle).unwrap_or_else(|| {
            panic!("`tm --help`'s Commands: section must list {needle:?}, got: {commands_section}")
        })
    };
    let search_pos = pos("\n  search");
    let run_pos = pos("\n  run");
    let doctor_pos = pos("\n  doctor");
    let milestone_pos = pos("\n  milestone");
    let serve_pos = pos("\n  serve");
    assert!(
        search_pos < milestone_pos && run_pos < milestone_pos && doctor_pos < milestone_pos,
        "daily verbs (search/run/doctor) must be listed before planning verbs (milestone), \
         got: {commands_section}"
    );
    assert!(
        milestone_pos < serve_pos,
        "planning verbs (milestone) must be listed before serving verbs (serve), \
         got: {commands_section}"
    );

    // Plumbing verbs — the explicit hide list plus the "more" group folded into `after_help`
    // (`attach`, `genesis`, `sched`, `history`, `docs`, `workflow`, `mirror`, `templates`,
    // `events`, `project`, `wiki`) — are hidden from the listing itself.
    for hidden in [
        "lease",
        "harness",
        "bench",
        "browser",
        "computer",
        "attach",
        "genesis",
        "sched",
        "history",
        "docs",
        "workflow",
        "mirror",
        "templates",
        "events",
        "project",
        "wiki",
    ] {
        let line_prefix = format!("\n  {hidden} ");
        assert!(
            !commands_section.contains(&line_prefix),
            "`{hidden}` must be hidden from `tm --help`'s Commands: section, \
             got: {commands_section}"
        );
    }

    // ~16 commands, grouped: 8 daily + 3 planning + 2 serving + `provider` + `auth` (left
    // untouched per the task) + the auto-generated `help` entry.
    let listed = commands_section
        .lines()
        .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
        .count();
    assert!(
        listed <= 17,
        "the regrouped listing should be about 16 commands, got {listed} lines: \
         {commands_section}"
    );

    // The verbs folded out of the listing above must still be discoverable somewhere in
    // `--help`, via the `after_help` "More commands" note.
    assert!(
        stdout.contains("More commands"),
        "hidden verbs must still be named in a `More commands` note, got: {stdout}"
    );
}

/// s1-cli-tree-regroup acceptance: hidden plumbing subcommands must still run.
#[test]
fn hidden_plumbing_subcommands_still_run() {
    let project = init_project();
    let tm_home = tempfile::tempdir().expect("tempdir");

    let lease_list = run_tm(
        project.path(),
        tm_home.path(),
        &[
            "--project",
            project.path().to_str().expect("utf8 path"),
            "lease",
            "list",
            "--json",
        ],
    );
    assert!(
        lease_list.status.success(),
        "`tm lease list` must still work though hidden from --help, stderr: {}",
        String::from_utf8_lossy(&lease_list.stderr)
    );

    let sched_tick = run_tm(
        project.path(),
        tm_home.path(),
        &[
            "--project",
            project.path().to_str().expect("utf8 path"),
            "sched",
            "tick",
            "--json",
        ],
    );
    assert!(
        sched_tick.status.success(),
        "`tm sched tick` must still work though hidden from --help, stderr: {}",
        String::from_utf8_lossy(&sched_tick.stderr)
    );
}

/// s1-cli-tree-regroup acceptance: `tm search --exact foo` must behave exactly like
/// `tm search --mode exact foo` (`--mode` stays a working, now-hidden alias).
#[test]
fn search_exact_flag_matches_mode_exact_flag() {
    let project = init_project();
    // `search_exact` doesn't need a prebuilt index (see `tm-codeintel/src/api.rs`'s
    // `search_exact_finds_a_literal_substring_without_indexing_first`), but it does need a real
    // git repository to scan (`CodeIntel`'s own tests always set one up); a real hit — not just
    // matching-empty-output on both sides — is what actually proves `--exact` and `--mode exact`
    // take the same code path.
    init_git_repo_with_a_commit(project.path());
    let tm_home = tempfile::tempdir().expect("tempdir");
    let project_arg = project.path().to_str().expect("utf8 path").to_string();

    let via_flag = run_tm(
        project.path(),
        tm_home.path(),
        &[
            "--project",
            &project_arg,
            "search",
            "--exact",
            "foo",
            "--json",
        ],
    );
    let via_mode = run_tm(
        project.path(),
        tm_home.path(),
        &[
            "--project",
            &project_arg,
            "search",
            "--mode",
            "exact",
            "foo",
            "--json",
        ],
    );

    assert!(
        via_flag.status.success() && via_mode.status.success(),
        "both `--exact` and `--mode exact` must succeed, stderr: {} / {}",
        String::from_utf8_lossy(&via_flag.stderr),
        String::from_utf8_lossy(&via_mode.stderr)
    );
    let stdout = String::from_utf8_lossy(&via_flag.stdout);
    assert!(
        stdout.contains("needle.txt"),
        "`tm search --exact foo` must actually find the literal match, got: {stdout}"
    );
    assert_eq!(
        via_flag.stdout, via_mode.stdout,
        "`tm search --exact foo` must produce the same output as `tm search --mode exact foo`"
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
