//! Proves D-003 Phase 1-C's promotion seam end to end against the real compiled `tm` binary:
//! a global chat session created by bare `tm` (Phase 1-B, `tests/bare_scope.rs`) survives being
//! adopted into a real repo-scoped project by `tm init`, with its ticket history intact and the
//! resulting project passing `tm doctor` clean. This is the seam that makes "chat first, decide
//! to track it as a real project later" feel like one product instead of two disconnected modes.
//!
//! Runs the real binary (the same `Command::new(env!("CARGO_BIN_EXE_tm"))` +
//! piped-stdio-then-close-stdin convention `tests/bare_scope.rs` already established) rather than
//! driving `dispatch` in-process, for the same reason that file gives: past opening the project,
//! the bare (no subcommand) path falls into the plain interactive loop, which blocks on a real
//! stdin read an in-process test can't control the way a spawned subprocess with piped, explicitly
//! -closed stdin can. Every test here sets `TM_HOME` to its own tempdir, so none can ever touch a
//! real developer's `~/.tm`.

use std::path::Path;
use std::process::{Command, Stdio};

/// `git init` a repository at `root` with one real commit, so `tm doctor`'s `index-health` check
/// (which depends on `tm-codeintel` finding a real git repository to ingest history from) passes
/// cleanly rather than failing for a reason that has nothing to do with promotion. Mirrors
/// `tests/bare_scope.rs`'s identical helper.
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

/// Run the real `tm` binary with `args` in `dir`, `TM_HOME` set to `tm_home`, and stdin fed
/// `stdin_data` then closed — the same convention `tests/bare_scope.rs`'s `run_tm_in` uses, plus
/// an input payload for the bare (mock-provider) turn this file's first case needs to drive.
fn run_tm_in(
    dir: &Path,
    tm_home: &Path,
    args: &[&str],
    mock_provider: bool,
    stdin_data: &str,
) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tm"));
    cmd.args(args)
        .current_dir(dir)
        .env("TM_HOME", tm_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if mock_provider {
        cmd.env("TM_TEST_MOCK_PROVIDER", "1");
    }
    let mut child = cmd.spawn().expect("spawn `tm` with piped stdio");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(stdin_data.as_bytes())
            .expect("write to tm's stdin");
        // Dropping `stdin` here closes it (EOF), same as `bare_scope.rs`'s `drop(child.stdin.take())`.
    }
    child
        .wait_with_output()
        .expect("`tm` must run to completion on a piped invocation")
}

/// The full promotion story: a global session gains a ticket via bare `tm`, `tm init` promotes
/// it into a real repo-scoped project without losing that ticket, and the resulting project is
/// healthy per `tm doctor`.
#[test]
fn tm_init_promotes_a_global_session_into_a_repo_scoped_project_with_its_ticket_intact() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");
    init_git_repo_with_a_commit(tmp.path());

    // Step 1 (Phase 1-B behavior, re-proven here as this test's starting premise): bare `tm`
    // with a piped mock-provider turn creates only a *global* session — the workspace itself
    // stays empty except for the git repo this test just created.
    let bare = run_tm_in(tmp.path(), tm_home.path(), &[], true, "say hello\n");
    assert!(
        bare.status.success(),
        "bare `tm` must succeed, got status {:?}, stdout {:?}, stderr {:?}",
        bare.status,
        String::from_utf8_lossy(&bare.stdout),
        String::from_utf8_lossy(&bare.stderr)
    );
    let bare_stdout = String::from_utf8_lossy(&bare.stdout);
    assert!(
        bare_stdout.contains("mock provider:"),
        "expected the scripted mock-provider reply, got stdout {bare_stdout:?}"
    );
    assert!(
        !tmp.path().join(".tm").exists(),
        "bare `tm` must still never write a .tm/ into the workspace (D-003), got a .tm/ after \
         only the bare turn"
    );

    // Step 2: `tm init` in the same directory/`TM_HOME` must promote that global session rather
    // than starting fresh.
    let init = run_tm_in(tmp.path(), tm_home.path(), &["init"], false, "");
    assert!(
        init.status.success(),
        "tm init must succeed, got status {:?}, stdout {:?}, stderr {:?}",
        init.status,
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    let init_stdout = String::from_utf8_lossy(&init.stdout);
    assert!(
        init_stdout.contains("promoted"),
        "tm init must report that it promoted the global session, got stdout {init_stdout:?}"
    );
    assert!(
        tmp.path().join(".tm").join("project.db").is_file(),
        "tm init must create a real repo-scoped project.db"
    );

    // Step 3: `tm ticket list --json` now resolves repo scope (Phase 1-B's resolution order:
    // a located `.tm/` always wins) and shows exactly the one ticket the mock-provider turn
    // created, objective intact.
    let tickets = run_tm_in(
        tmp.path(),
        tm_home.path(),
        &["ticket", "list", "--json"],
        false,
        "",
    );
    assert!(
        tickets.status.success(),
        "tm ticket list --json must succeed against the promoted project, got status {:?}, \
         stderr {:?}",
        tickets.status,
        String::from_utf8_lossy(&tickets.stderr)
    );
    let tickets: serde_json::Value =
        serde_json::from_slice(&tickets.stdout).expect("ticket list --json must be JSON");
    let tickets = tickets.as_array().expect("ticket list --json is an array");
    assert_eq!(
        tickets.len(),
        1,
        "expected exactly one promoted ticket, got {tickets:?}"
    );
    assert_eq!(
        tickets[0].get("objective").and_then(|v| v.as_str()),
        Some("say hello"),
        "got ticket {:?}",
        tickets[0]
    );

    // Step 4: the promoted project is healthy — `tm doctor` exits 0, with both the hash chain
    // and invariants checks passing.
    let doctor = run_tm_in(
        tmp.path(),
        tm_home.path(),
        &["doctor", "--skip-computer-probe", "--json"],
        false,
        "",
    );
    assert!(
        doctor.status.success(),
        "tm doctor must exit 0 on the promoted project, got status {:?}, stdout {:?}, stderr {:?}",
        doctor.status,
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&doctor.stdout).expect("doctor --json must be JSON");
    let checks = report
        .get("checks")
        .and_then(|v| v.as_array())
        .expect("doctor report has a checks array");
    let check_ok = |name: &str| {
        checks
            .iter()
            .find(|c| c.get("name").and_then(|n| n.as_str()) == Some(name))
            .and_then(|c| c.get("ok"))
            .and_then(|v| v.as_bool())
    };
    assert_eq!(check_ok("hash-chain"), Some(true), "got checks {checks:?}");
    assert_eq!(check_ok("invariants"), Some(true), "got checks {checks:?}");

    // And `tm project list` still has a row for the now-emptied global session, carrying a
    // `promoted` marker rather than disappearing outright.
    let list = run_tm_in(
        tmp.path(),
        tm_home.path(),
        &["project", "list", "--json"],
        false,
        "",
    );
    assert!(list.status.success(), "tm project list must succeed");
    let entries: serde_json::Value =
        serde_json::from_slice(&list.stdout).expect("project list --json must be JSON");
    let entries = entries.as_array().expect("project list --json is an array");
    assert_eq!(entries.len(), 1, "got entries {entries:?}");
    assert!(
        entries[0].get("promoted").is_some_and(|v| !v.is_null()),
        "the promoted global session's list entry must carry a promoted marker, got {:?}",
        entries[0]
    );
}

/// `tm init --fresh` on a workspace with an existing global session must leave both intact and
/// separate: the global session keeps its ticket, and the fresh repo-scoped project starts empty.
#[test]
fn tm_init_fresh_ignores_an_existing_global_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");
    init_git_repo_with_a_commit(tmp.path());

    let bare = run_tm_in(tmp.path(), tm_home.path(), &[], true, "say hello\n");
    assert!(bare.status.success(), "bare `tm` must succeed");

    let init = run_tm_in(tmp.path(), tm_home.path(), &["init", "--fresh"], false, "");
    assert!(
        init.status.success(),
        "tm init --fresh must succeed, got status {:?}, stderr {:?}",
        init.status,
        String::from_utf8_lossy(&init.stderr)
    );
    let init_stdout = String::from_utf8_lossy(&init.stdout);
    assert!(
        !init_stdout.contains("promoted"),
        "tm init --fresh must never promote, got stdout {init_stdout:?}"
    );

    let tickets = run_tm_in(
        tmp.path(),
        tm_home.path(),
        &["ticket", "list", "--json"],
        false,
        "",
    );
    assert!(tickets.status.success());
    let tickets: serde_json::Value =
        serde_json::from_slice(&tickets.stdout).expect("ticket list --json must be JSON");
    assert!(
        tickets.as_array().expect("array").is_empty(),
        "the fresh repo-scoped project must start with no tickets, got {tickets:?}"
    );

    // The global session is untouched: `tm ticket list --project` doesn't reach it directly, but
    // its `project.db` must still exist and never have been cleared/promoted.
    let projects_dir = tm_home.path().join("projects");
    let entries: Vec<_> = std::fs::read_dir(&projects_dir)
        .expect("projects dir exists")
        .map(|e| e.expect("dir entry").path())
        .collect();
    assert_eq!(entries.len(), 1, "got entries {entries:?}");
    assert!(
        entries[0].join("project.db").is_file(),
        "--fresh must leave the global session's project.db untouched"
    );
    assert!(
        !entries[0].join("promoted.json").exists(),
        "--fresh must never write a promoted.json marker"
    );
}
