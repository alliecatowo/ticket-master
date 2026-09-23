//! Proves D-003's scope split end to end against the real compiled `tm` binary: bare `tm` in a
//! directory with no `.tm/` anywhere above it must create its state entirely under `$TM_HOME`,
//! outside the workspace, and print a scope line saying so — never write a `.tm/` into the
//! workspace, and never assimilate a git repository the way the old bare-`tm` auto-bootstrap
//! (`bare_bootstrap.rs`, replaced by this file) used to. Every test here sets `TM_HOME` to its own
//! tempdir so none can ever touch a real developer's `~/.tm`.
//!
//! Runs the real binary (the same `Command::new(env!("CARGO_BIN_EXE_tm"))` +
//! piped-stdio-then-close-stdin convention `tests/tui_launch.rs` established) rather than driving
//! `dispatch` in-process, because past opening the project this path falls into the plain
//! interactive loop, which blocks on a real stdin read that an in-process `#[tokio::test]` cannot
//! control the way a spawned subprocess with its own piped stdio can.

use std::path::Path;
use std::process::{Command, Stdio};

/// `git init` a repository at `root` with one real commit, so it has committed history that the
/// *old* bare-`tm` auto-bootstrap would have assimilated — proving the new one doesn't is the
/// whole point of [`bare_tm_in_a_git_repo_with_a_commit_creates_no_tm_dir_and_assimilates_nothing`].
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

/// Run the real `tm` binary with `args` in `dir`, `TM_HOME` set to `tm_home`, and stdin closed
/// immediately (the same EOF-as-ctrl-d convention `tests/tui_launch.rs` uses) so a bare/non-tty
/// invocation falls into the plain loop, prints its prompt once, and exits cleanly instead of
/// blocking forever.
fn run_tm_in(dir: &Path, tm_home: &Path, args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tm"))
        .args(args)
        .current_dir(dir)
        .env("TM_HOME", tm_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `tm` with piped stdio");
    drop(child.stdin.take());
    child
        .wait_with_output()
        .expect("`tm` must run to completion on a piped invocation")
}

/// The single `$TM_HOME/projects/<key>/` entry under `tm_home`, panicking with full context if
/// there isn't exactly one — every case in this file creates at most one global project.
fn the_one_global_project_dir(tm_home: &Path) -> std::path::PathBuf {
    let projects_dir = tm_home.join("projects");
    let entries: Vec<_> = std::fs::read_dir(&projects_dir)
        .unwrap_or_else(|e| panic!("{} should exist: {e}", projects_dir.display()))
        .map(|e| e.expect("dir entry").path())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one global project dir under {}, got {entries:?}",
        projects_dir.display()
    );
    entries.into_iter().next().unwrap()
}

/// Case 1: bare `tm` in a genuinely empty directory.
#[test]
fn bare_tm_in_an_empty_directory_creates_only_a_global_project_under_tm_home() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().canonicalize().expect("canonicalize");

    let output = run_tm_in(tmp.path(), tm_home.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "bare `tm` in an empty directory must succeed, got status {:?}, stdout {stdout:?}, \
         stderr {stderr:?}",
        output.status
    );
    assert!(
        stdout.contains("tm> "),
        "a piped/non-tty bare `tm` must still reach the plain loop's prompt, got stdout \
         {stdout:?}"
    );
    assert!(
        stdout.contains("global"),
        "the D-003 scope line printed before the first prompt must say this session is \
         global-scoped, got stdout {stdout:?}"
    );

    let entries_in_dir: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("read the workspace dir")
        .collect();
    assert!(
        entries_in_dir.is_empty(),
        "bare `tm` must never write into the workspace when it falls back to global scope, got \
         entries {entries_in_dir:?}"
    );

    let state_dir = the_one_global_project_dir(tm_home.path());
    assert!(
        state_dir.join("project.db").is_file(),
        "the global project dir must contain project.db, got {state_dir:?}"
    );
    let workspace_json = state_dir.join("workspace.json");
    let raw = std::fs::read_to_string(&workspace_json)
        .unwrap_or_else(|e| panic!("{} should exist: {e}", workspace_json.display()));
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("workspace.json must be JSON");
    assert_eq!(
        doc.get("workspace").and_then(|v| v.as_str()),
        Some(canonical.to_string_lossy().as_ref()),
        "workspace.json's workspace field must equal the canonical workspace path, got {doc:?}"
    );
}

/// Case 2: bare `tm` in a git repo with one real commit — the exact directory shape the old
/// bare-`tm` auto-bootstrap used to assimilate (creating `.tm/index.db`, a `T-001` investigation
/// ticket, and so on). D-003 removes that entirely: bare `tm` here must behave identically to the
/// empty-directory case, leaving the repository untouched.
#[test]
fn bare_tm_in_a_git_repo_with_a_commit_creates_no_tm_dir_and_assimilates_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");
    init_git_repo_with_a_commit(tmp.path());

    let output = run_tm_in(tmp.path(), tm_home.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "bare `tm` in a git repo must succeed, got status {:?}, stdout {stdout:?}, stderr \
         {stderr:?}",
        output.status
    );
    assert!(stdout.contains("tm> "), "got stdout {stdout:?}");
    assert!(stdout.contains("global"), "got stdout {stdout:?}");

    assert!(
        !tmp.path().join(".tm").exists(),
        "bare `tm` must never create a .tm/ directory in the workspace anymore (D-003 removed \
         bare-tm assimilation)"
    );

    let git_status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(tmp.path())
        .output()
        .expect("git status should run");
    assert!(
        String::from_utf8_lossy(&git_status.stdout)
            .trim()
            .is_empty(),
        "the repository must be left exactly as it was, got `git status --porcelain`: {:?}",
        String::from_utf8_lossy(&git_status.stdout)
    );

    // Confirm no assimilation happened by asking the same global project (same TM_HOME, same
    // workspace) for its ticket list: an assimilated project would have a `T-001` investigation
    // ticket (`tm_genesis::attach::attach_repository`'s documented behavior).
    let output = run_tm_in(tmp.path(), tm_home.path(), &["ticket", "list", "--json"]);
    assert!(
        output.status.success(),
        "tm ticket list --json must succeed against the just-created global project, got \
         status {:?}, stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let tickets: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ticket list --json must be JSON");
    let tickets = tickets.as_array().expect("ticket list --json is an array");
    assert!(
        tickets.is_empty(),
        "no assimilation must have happened: expected zero tickets, got {tickets:?}"
    );
}

/// Case 3: `tm status --json` in the same directory/`TM_HOME` bare `tm` just created a global
/// project for reports `scope.kind == "global"`.
#[test]
fn status_json_in_a_global_scoped_directory_reports_global_scope() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");

    // Bare `tm` first, to create the global project (subcommands never create state on their
    // own — see case 4).
    let bootstrap = run_tm_in(tmp.path(), tm_home.path(), &[]);
    assert!(bootstrap.status.success(), "bootstrap run must succeed");

    let output = run_tm_in(tmp.path(), tm_home.path(), &["status", "--json"]);
    assert!(
        output.status.success(),
        "tm status --json must succeed once a global project exists, got status {:?}, stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("status --json must be JSON");
    assert_eq!(
        report
            .get("scope")
            .and_then(|s| s.get("kind"))
            .and_then(|k| k.as_str()),
        Some("global"),
        "got report {report:?}"
    );
}

/// Case 4: an explicit subcommand (`tm status`) in a fresh directory with an empty/unused
/// `TM_HOME` must error `NotFound` rather than silently creating anything — only bare `tm` is
/// allowed to create global state.
#[test]
fn status_in_a_fresh_directory_with_no_project_anywhere_errors_not_found() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");

    let output = run_tm_in(tmp.path(), tm_home.path(), &["status"]);

    assert!(
        !output.status.success(),
        "tm status with no project in either scope must fail, got status {:?}, stdout {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("not found"),
        "expected a NotFound-shaped error, got stderr {stderr:?}"
    );
    assert!(
        stderr.contains("tm init"),
        "the error must say how to get a project, got stderr {stderr:?}"
    );
    let projects_dir = tm_home.path().join("projects");
    assert!(
        !projects_dir.exists() || std::fs::read_dir(&projects_dir).unwrap().next().is_none(),
        "a subcommand must never create global state, got entries under {projects_dir:?}"
    );
}

/// Case 5 (regression check): `tm --project <dir that already has .tm>` still resolves repo scope
/// correctly, unaffected by the global-scope fallback this track adds.
#[test]
fn explicit_project_flag_still_resolves_repo_scope() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");
    tm_core::Store::open(tmp.path()).expect("create a real .tm project");

    let output = run_tm_in(
        tmp.path(),
        tm_home.path(),
        &[
            "--project",
            tmp.path().to_str().unwrap(),
            "status",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "got status {:?}, stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("status --json must be JSON");
    assert_eq!(
        report
            .get("scope")
            .and_then(|s| s.get("kind"))
            .and_then(|k| k.as_str()),
        Some("repo"),
        "got report {report:?}"
    );
    // An explicit `--project` must never touch `$TM_HOME`.
    assert!(!tm_home.path().join("projects").exists());
}

/// Case 6: `tm project list` after bare `tm` created a global project lists it.
#[test]
fn project_list_shows_a_global_project_created_by_bare_tm() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().canonicalize().expect("canonicalize");

    let bootstrap = run_tm_in(tmp.path(), tm_home.path(), &[]);
    assert!(bootstrap.status.success(), "bootstrap run must succeed");

    let output = run_tm_in(tmp.path(), tm_home.path(), &["project", "list", "--json"]);
    assert!(
        output.status.success(),
        "tm project list --json must succeed, got status {:?}, stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let entries: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("project list --json must be JSON");
    let entries = entries.as_array().expect("project list --json is an array");
    assert_eq!(entries.len(), 1, "got entries {entries:?}");
    assert_eq!(
        entries[0].get("workspace").and_then(|v| v.as_str()),
        Some(canonical.to_string_lossy().as_ref()),
        "got entries {entries:?}"
    );
}

/// Case 7: `tm project show` in a fresh directory with no project anywhere reports global scope,
/// `exists: false`, and — the actual point of the test — creates nothing at all: `show` resolves
/// scope directly (`resolve_scope`) rather than going through `open_for_command`/`open_bare`, so
/// it must behave identically whether or not a project ever ends up existing here.
#[test]
fn project_show_in_a_fresh_directory_reports_global_scope_and_creates_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");
    let canonical = tmp.path().canonicalize().expect("canonicalize");

    let output = run_tm_in(tmp.path(), tm_home.path(), &["project", "show", "--json"]);
    assert!(
        output.status.success(),
        "tm project show must succeed even with no project anywhere, got status {:?}, stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("project show --json must be JSON");
    assert_eq!(report.get("kind").and_then(|v| v.as_str()), Some("global"));
    assert_eq!(
        report.get("workspace").and_then(|v| v.as_str()),
        Some(canonical.to_string_lossy().as_ref())
    );
    assert!(
        report.get("state_dir").and_then(|v| v.as_str()).is_some(),
        "got report {report:?}"
    );
    assert_eq!(report.get("exists").and_then(|v| v.as_bool()), Some(false));

    assert!(
        !tm_home.path().join("projects").exists(),
        "tm project show must never create global state"
    );
    assert_eq!(
        std::fs::read_dir(tmp.path())
            .expect("read the workspace dir")
            .count(),
        0,
        "tm project show must never write into the workspace either"
    );
}

/// Case 7b: `tm project show --project <dir with .tm>` reports repo scope (the other branch of
/// `show`), reusing case 5's setup.
#[test]
fn project_show_with_explicit_project_flag_reports_repo_scope() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");
    tm_core::Store::open(tmp.path()).expect("create a real .tm project");

    let output = run_tm_in(
        tmp.path(),
        tm_home.path(),
        &[
            "--project",
            tmp.path().to_str().unwrap(),
            "project",
            "show",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "got status {:?}, stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("project show --json must be JSON");
    assert_eq!(report.get("kind").and_then(|v| v.as_str()), Some("repo"));
    assert_eq!(report.get("exists").and_then(|v| v.as_bool()), Some(true));
}

/// `tm ticket new` in a fresh directory is an explicit request to have a project: it starts one in
/// global scope the way bare `tm` does, writing nothing into the workspace (D-003), and the ticket
/// is then listable.
#[test]
fn ticket_new_in_a_fresh_directory_starts_a_global_project() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");

    let created = run_tm_in(
        tmp.path(),
        tm_home.path(),
        &["ticket", "new", "write the docs"],
    );
    assert!(
        created.status.success(),
        "tm ticket new must succeed in a fresh directory, got stderr {:?}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(
        !tmp.path().join(".tm").exists(),
        "nothing is written into the workspace"
    );

    let listed = run_tm_in(tmp.path(), tm_home.path(), &["ticket", "list", "--json"]);
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("json");
    assert_eq!(
        listed.as_array().map(Vec::len),
        Some(1),
        "the new ticket is listed: {listed:?}"
    );
    let id = listed[0]["id"].as_str().expect("ticket id").to_string();
    assert_eq!(listed[0]["state"], "draft");

    let activated = run_tm_in(tmp.path(), tm_home.path(), &["ticket", "activate", &id]);
    assert!(
        activated.status.success(),
        "tm ticket activate must succeed, got stderr {:?}",
        String::from_utf8_lossy(&activated.stderr)
    );
    let listed = run_tm_in(tmp.path(), tm_home.path(), &["ticket", "list", "--json"]);
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("json");
    assert_eq!(
        listed[0]["state"], "ready",
        "activate makes it runnable: {listed:?}"
    );
}
