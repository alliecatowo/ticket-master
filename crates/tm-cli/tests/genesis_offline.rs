//! `genesis-cli-offline-integration-test`: proves `tm genesis` runs offline, end to end, through
//! the real compiled `tm` binary rather than the in-process `GenesisDriver` — the same
//! subprocess convention `tests/promotion.rs` uses (`Command::new(env!("CARGO_BIN_EXE_tm"))` in a
//! fresh git-initialized tempdir, with its own `TM_HOME`). `crates/tm-e2e/tests/genesis_e2e.rs`'s
//! `seed_to_steady_state_offline_and_deterministic` already drives `GenesisDriver` in-process
//! offline, which proves the driver itself is deterministic and network-free, but it never goes
//! through the CLI's own provider resolution (`project::resolve_genesis_provider`) — the exact
//! path that shipped for real with no mock-provider check at all until this task's dependency,
//! `genesis-cli-wire-mock-provider`, landed. This test is the regression guard for that CLI-level
//! path specifically: it runs the real binary under `TM_TEST_MOCK_PROVIDER=1` with a wall-clock
//! timeout guard, so a future regression fails this test instead of hanging or falling back to a
//! real network call.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// How long a single `tm genesis` subprocess run may take before this test fails it outright,
/// rather than hanging the test suite. Offline + mock-provider genesis stops for work at the
/// first unclosed milestone after a handful of in-process stage advances, so a few seconds is
/// generous; anything longer means the offline path stopped being offline.
const GENESIS_TIMEOUT: Duration = Duration::from_secs(60);

/// `git init` a repository at `root` with one real commit, so any `tm doctor`-shaped check that
/// wants a real git history to ingest has one. Mirrors `tests/promotion.rs`'s identical helper.
fn init_git_repo_with_a_commit(root: &Path) {
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
    std::fs::write(root.join("README.md"), "# hi\n").expect("write README");
    run(&["add", "README.md"]);
    run(&["commit", "--quiet", "-m", "init"]);
}

/// Run the real `tm` binary with `args` in `dir`, `TM_HOME` set to `tm_home`,
/// `TM_TEST_MOCK_PROVIDER=1` and `TM_NOTIFY=0` (no desktop notifications, no real provider),
/// waiting up to [`GENESIS_TIMEOUT`] rather than blocking the test suite forever on a hang.
fn run_tm_offline(dir: &Path, tm_home: &Path, args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tm"))
        .args(args)
        .current_dir(dir)
        .env("TM_HOME", tm_home)
        .env("TM_TEST_MOCK_PROVIDER", "1")
        .env("TM_NOTIFY", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `tm` with piped stdio");

    let start = std::time::Instant::now();
    loop {
        if let Some(_status) = child.try_wait().expect("poll child status") {
            return child
                .wait_with_output()
                .expect("`tm` must run to completion");
        }
        if start.elapsed() > GENESIS_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "`tm {:?}` did not finish within {:?} — offline genesis must never hang",
                args, GENESIS_TIMEOUT
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The end-to-end offline story: `tm genesis --prompt ... --plain` under
/// `TM_TEST_MOCK_PROVIDER=1` runs to completion (a stop-for-work policy, not a hang or a network
/// error), reports the committed tickets and the next step in plain human-readable stdout, and
/// `tm tickets --json --all` shows those tickets as real Ready tickets in the project it just
/// created.
#[test]
fn tm_genesis_runs_offline_end_to_end_through_the_real_binary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");
    init_git_repo_with_a_commit(tmp.path());

    let genesis = run_tm_offline(
        tmp.path(),
        tm_home.path(),
        &["genesis", "--prompt", "a Python CLI todo app", "--plain"],
    );
    assert!(
        genesis.status.success(),
        "tm genesis must exit 0 offline, got status {:?}, stdout {:?}, stderr {:?}",
        genesis.status,
        String::from_utf8_lossy(&genesis.stdout),
        String::from_utf8_lossy(&genesis.stderr)
    );
    let genesis_stdout = String::from_utf8_lossy(&genesis.stdout);
    assert!(
        genesis_stdout.contains("ticket") && genesis_stdout.contains("committed"),
        "expected stdout to report the committed tickets, got {genesis_stdout:?}"
    );
    assert!(
        genesis_stdout.contains("tm sched run") || genesis_stdout.contains("tm run"),
        "expected stdout to name the next step to work the committed tickets, got \
         {genesis_stdout:?}"
    );

    let tickets = run_tm_offline(tmp.path(), tm_home.path(), &["tickets", "--json", "--all"]);
    assert!(
        tickets.status.success(),
        "tm tickets --json --all must succeed, got stderr {:?}",
        String::from_utf8_lossy(&tickets.stderr)
    );
    let tickets: serde_json::Value =
        serde_json::from_slice(&tickets.stdout).expect("tickets --json --all must be JSON");
    let tickets = tickets
        .as_array()
        .expect("tickets --json --all is an array");
    assert!(
        !tickets.is_empty(),
        "genesis must have committed at least one ticket, got {tickets:?}"
    );
    assert!(
        tickets
            .iter()
            .all(|t| t.get("state").and_then(|s| s.as_str()) == Some("ready")),
        "genesis's committed tickets must be Ready so the scheduler can work them, got {tickets:?}"
    );
}

/// `genesis-cli-resume-flag`: re-running `tm genesis --resume` after it stopped for work
/// continues from the persisted `GenesisState` snapshot instead of starting a fresh `Seed` —
/// proven the same way the offline end-to-end test above proves the fresh-start path, but
/// asserting no duplicate `GraphCompilation` artifact and no duplicate ticket commit happen on
/// the second run.
#[test]
fn tm_genesis_resume_continues_from_the_persisted_stage_without_double_committing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");
    init_git_repo_with_a_commit(tmp.path());

    let first = run_tm_offline(
        tmp.path(),
        tm_home.path(),
        &["genesis", "--prompt", "a Python CLI todo app", "--plain"],
    );
    assert!(
        first.status.success(),
        "first `tm genesis` must exit 0 offline, got status {:?}, stdout {:?}, stderr {:?}",
        first.status,
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let tickets_after_first =
        run_tm_offline(tmp.path(), tm_home.path(), &["tickets", "--json", "--all"]);
    let tickets_after_first: serde_json::Value =
        serde_json::from_slice(&tickets_after_first.stdout).expect("tickets --json --all is JSON");
    let count_after_first = tickets_after_first
        .as_array()
        .expect("tickets --json --all is an array")
        .len();
    assert!(
        count_after_first > 0,
        "the first run must have committed at least one ticket"
    );

    // No `--prompt` this time: `--resume` (and the auto-detect it shares with an omitted
    // `--prompt`) must find the snapshot the first run left behind rather than requiring a
    // prompt again.
    let second = run_tm_offline(
        tmp.path(),
        tm_home.path(),
        &["genesis", "--resume", "--plain"],
    );
    assert!(
        second.status.success(),
        "second `tm genesis --resume` must exit 0 offline, got status {:?}, stdout {:?}, stderr \
         {:?}",
        second.status,
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    let tickets_after_second =
        run_tm_offline(tmp.path(), tm_home.path(), &["tickets", "--json", "--all"]);
    let tickets_after_second: serde_json::Value =
        serde_json::from_slice(&tickets_after_second.stdout).expect("tickets --json --all is JSON");
    let count_after_second = tickets_after_second
        .as_array()
        .expect("tickets --json --all is an array")
        .len();
    assert_eq!(
        count_after_second, count_after_first,
        "resuming a run that already committed its ticket graph must not commit a second, \
         duplicate graph: before {count_after_first}, after {count_after_second}"
    );
}
