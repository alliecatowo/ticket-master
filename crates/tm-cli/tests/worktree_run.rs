//! `tm run <ticket> --worktree` against the real compiled `tm` binary
//! (`docs/decisions/D-012-run-worktree-isolation.md`), following the `Command::new(env!(
//! "CARGO_BIN_EXE_tm"))` + tempdir-isolated `--project` convention `tests/ticket_fork.rs` and
//! `tests/auth_redaction.rs` already use — never the primary checkout, always a fresh tempdir.
//!
//! `TM_TEST_MOCK_PROVIDER=1` (`crates/tm-cli/src/agent.rs`) swaps the real model provider for a
//! deterministic scripted one that always ends a run's single turn without submitting — enough
//! to prove the CLI verb is wired end to end (a real `git worktree add` happens, on a fresh
//! branch off `HEAD`, and it is *kept*, not silently deleted, since the run never reaches a
//! forward-progress state) without a network call or a real API key. `crates/tm-cli/src/
//! worktree.rs`'s own unit tests separately prove the git-level isolation guarantee itself in
//! depth (a commit made inside the worktree never reaching the main branch); this file's job is
//! narrower — prove `--worktree` reaches `crate::worktree::create` from real argument parsing.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

/// `tm ticket new` leaves a ticket in `Draft` (no CLI verb drives `Trigger::Activate` today —
/// production activation is normally an upstream `tm genesis`/scheduler concern this file has no
/// reason to depend on), and `tm run` requires `Ready`. Opens the same `.tm` store the `tm`
/// subprocess itself will open, calls the real `Store::activate`, and drops the handle before
/// returning so the subprocess never contends with it for the sqlite file.
fn activate_ticket(root: &Path, ticket: &str) {
    let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::SystemClock);
    let ids: Arc<dyn tm_types::IdSource> = Arc::new(tm_types::CounterIds::new());
    let store =
        tm_core::Store::open_with(root, clock, ids).expect("open store to activate the ticket");
    let actor = tm_types::ParticipantId::new("human:test").expect("actor id");
    let ticket_id = tm_types::TicketId::new(ticket).expect("ticket id");
    store.activate(&ticket_id, actor).expect("activate ticket");
}

/// A real tempdir git repo with one commit and `.tm/` gitignored, matching what a real `tm init`
/// project inside a git repo looks like.
fn init_git_repo(root: &Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    std::fs::write(root.join("README.md"), "hello\n").expect("write file");
    std::fs::write(root.join(".gitignore"), ".tm/\n").expect("write .gitignore");
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "initial"]);
}

/// Run the real `tm` binary in `dir`, scoped to `dir` itself via `--project` (repo scope at
/// exactly that path, regardless of whether `dir` is a git repo), with the deterministic mock
/// provider and desktop notifications both switched on/off exactly as this file needs.
fn run_tm(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut full_args = vec!["--project", dir.to_str().expect("utf8 tempdir path")];
    full_args.extend_from_slice(args);
    Command::new(env!("CARGO_BIN_EXE_tm"))
        .args(&full_args)
        .env("TM_TEST_MOCK_PROVIDER", "1")
        .env("TM_NOTIFY", "0")
        .stdin(Stdio::null())
        .output()
        .expect("`tm` should run to completion")
}

fn run_tm_ok(dir: &Path, args: &[&str]) -> std::process::Output {
    let output = run_tm(dir, args);
    assert!(
        output.status.success(),
        "tm {args:?} should succeed, got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn run_with_worktree_creates_a_real_worktree_and_keeps_it_when_the_run_does_not_submit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_git_repo(root);

    let created = run_tm_ok(root, &["--json", "ticket", "new", "delegate this"]);
    let ticket_id: String = serde_json::from_slice(&created.stdout).expect("ticket id json");
    activate_ticket(root, &ticket_id);

    let output = run_tm_ok(root, &["run", &ticket_id, "--worktree"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    let worktrees_dir = root.join(".tm").join("worktrees");
    let entries: Vec<_> = std::fs::read_dir(&worktrees_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}\nstdout: {stdout}", worktrees_dir.display()))
        .collect::<Result<Vec<_>, _>>()
        .expect("read worktrees dir entries");
    assert_eq!(
        entries.len(),
        1,
        "exactly one worktree directory should exist\nstdout: {stdout}"
    );
    let wt_path = entries[0].path();
    assert!(
        wt_path
            .file_name()
            .expect("worktree dir has a name")
            .to_string_lossy()
            .starts_with(&ticket_id),
        "the worktree directory should be named after the ticket: {}",
        wt_path.display()
    );
    assert!(
        wt_path.join("README.md").exists(),
        "the worktree must be a real checkout off HEAD"
    );

    let list = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(root)
        .output()
        .expect("git worktree list");
    assert!(
        String::from_utf8_lossy(&list.stdout).contains(&wt_path.to_string_lossy().into_owned()),
        "git itself must know about the new worktree"
    );

    // The scripted mock provider ends the turn without submitting (see this file's own module
    // doc comment), so the run never reaches a forward-progress state — per
    // `docs/decisions/D-012-run-worktree-isolation.md`'s policy the worktree must be *kept*, not
    // silently removed, since there is real state here worth inspecting.
    assert!(
        wt_path.exists(),
        "a non-success run's worktree must be kept for inspection"
    );
    assert!(
        stdout.contains("Kept worktree"),
        "stdout should explain the worktree was kept: {stdout}"
    );
}

#[test]
fn run_with_worktree_outside_a_git_repository_is_a_clear_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    // Deliberately no `git init` — `--project root` still makes this a valid repo-scoped `tm`
    // project (D-003), just not one backed by a real git repository.

    let created = run_tm_ok(root, &["--json", "ticket", "new", "no repo here"]);
    let ticket_id: String = serde_json::from_slice(&created.stdout).expect("ticket id json");

    let output = run_tm(root, &["run", &ticket_id, "--worktree"]);
    assert!(
        !output.status.success(),
        "--worktree outside a git repository must fail, not silently run against the main tree"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("git repository"),
        "the error should name the real problem plainly: {stderr}"
    );
    assert!(
        !root.join(".tm").join("worktrees").exists(),
        "nothing should be created for a run that never gets past the git-repo precondition"
    );
}
