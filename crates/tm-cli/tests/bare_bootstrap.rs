//! Proves the bare-`tm` auto-bootstrap end to end against the real compiled `tm` binary: a bare
//! invocation in a directory with no `.tm/` anywhere above it must "just work" (create one and
//! keep going) instead of erroring out the way it used to, the same "no setup required" bar
//! `claude`/`codex` already clear. Runs the real binary (the same `Command::new(env!(
//! "CARGO_BIN_EXE_tm"))` + piped-stdio-then-close-stdin convention `tests/tui_launch.rs` already
//! established) rather than driving `dispatch` in-process, because past the bootstrap this path
//! falls into the plain interactive loop, which blocks on a real stdin read that an in-process
//! `#[tokio::test]` cannot control the way a spawned subprocess with its own piped stdio can.

use std::process::{Command, Stdio};

/// `git init` a repository at `root` with one real commit, so it has committed history worth
/// assimilating (mirrors `crates/tm-cli/src/project.rs`'s own `init_git_repo` test helper, which
/// is `cfg(test)`-private to that crate and so not reachable from this integration test binary).
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
    std::fs::write(root.join("README.md"), "# hi\n").expect("write README");
    run(&["add", "README.md"]);
    run(&["commit", "--quiet", "-m", "init"]);
}

/// Run the real `tm` binary bare (no args, no `--project`) in `dir`, with stdin closed
/// immediately (the same EOF-as-ctrl-d convention `tests/tui_launch.rs`'s
/// `bare_tm_with_piped_stdout_uses_the_plain_loop_not_the_tui` test uses) so a non-tty run falls
/// into the plain loop, prints its prompt once, and exits cleanly instead of blocking forever.
fn run_bare_tm_in(dir: &std::path::Path) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tm"))
        .current_dir(dir)
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

#[test]
fn bare_tm_bootstraps_a_project_in_a_genuinely_empty_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let output = run_bare_tm_in(tmp.path());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "bare `tm` in an empty directory must succeed instead of erroring, got status {:?}, \
         stdout {stdout:?}, stderr {stderr:?}",
        output.status
    );
    assert!(
        tmp.path().join(".tm").is_dir(),
        "bare `tm` must leave a real .tm/ project behind, got stdout {stdout:?}"
    );
    assert!(
        stdout.contains("No Ticketmaster project here yet"),
        "bare `tm`'s auto-bootstrap must announce the side effect before proceeding, got \
         stdout {stdout:?}"
    );
    assert!(
        stdout.contains("tm> "),
        "after bootstrapping, a non-tty bare `tm` must still reach the plain loop's prompt, \
         got stdout {stdout:?}"
    );
    assert!(
        !tmp.path().join(".tm").join("index.db").exists(),
        "a genuinely empty directory has no git history to assimilate, so the bootstrap must \
         take the plain `init`-equivalent path, not `attach`'s (which would have created \
         .tm/index.db)"
    );
}

#[test]
fn bare_tm_assimilates_an_existing_git_repository_with_commits() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_git_repo_with_a_commit(tmp.path());

    let output = run_bare_tm_in(tmp.path());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "bare `tm` in an existing git repository must succeed, got status {:?}, stdout \
         {stdout:?}, stderr {stderr:?}",
        output.status
    );
    assert!(
        tmp.path().join(".tm").is_dir(),
        "bare `tm` must leave a real .tm/ project behind, got stdout {stdout:?}"
    );
    assert!(
        stdout.contains("No Ticketmaster project here yet"),
        "bare `tm`'s auto-bootstrap must announce the side effect before proceeding, got \
         stdout {stdout:?}"
    );
    assert!(
        tmp.path().join(".tm").join("index.db").is_file(),
        "an existing git repository with commits must be assimilated the same way `tm attach` \
         would (indexed via tm-codeintel, not left as an empty shell that ignores history), got \
         stdout {stdout:?}"
    );
}
