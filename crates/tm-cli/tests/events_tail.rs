//! Event command behavior against the compiled CLI, using only fresh temporary project state.

use std::path::Path;
use std::process::{Command, Stdio};

fn run_tm(dir: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    let mut full_args = vec!["--project", dir.to_str().expect("utf8 tempdir path")];
    full_args.extend_from_slice(args);
    Command::new(env!("CARGO_BIN_EXE_tm"))
        .args(full_args)
        .env("TM_HOME", home)
        .env("TM_NOTIFY", "0")
        .stdin(Stdio::null())
        .output()
        .expect("tm should finish")
}

#[test]
fn events_tail_bare_events_prints_snapshot_and_follow_is_opt_in() {
    let project = tempfile::tempdir().expect("project tempdir");
    let home = tempfile::tempdir().expect("TM_HOME tempdir");

    let created = run_tm(
        project.path(),
        home.path(),
        &["ticket", "new", "snapshot marker"],
    );
    assert!(
        created.status.success(),
        "ticket creation failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    let snapshot = run_tm(project.path(), home.path(), &["events"]);
    assert!(
        snapshot.status.success(),
        "bare events failed: {}",
        String::from_utf8_lossy(&snapshot.stderr)
    );
    let text = String::from_utf8_lossy(&snapshot.stdout);
    assert!(
        text.contains("ticket.created"),
        "missing event in snapshot: {text}"
    );
    assert!(text.contains("T-1"), "missing ticket in snapshot: {text}");

    let tail_snapshot = run_tm(
        project.path(),
        home.path(),
        &["events", "tail", "--from", "1"],
    );
    assert!(
        tail_snapshot.status.success(),
        "tail snapshot should exit without --follow: {}",
        String::from_utf8_lossy(&tail_snapshot.stderr)
    );
    assert!(String::from_utf8_lossy(&tail_snapshot.stdout).contains("ticket.created"));

    let mut follower = Command::new(env!("CARGO_BIN_EXE_tm"))
        .args([
            "--project",
            project.path().to_str().expect("utf8 tempdir path"),
            "events",
            "tail",
            "--from",
            "1",
            "--follow",
        ])
        .env("TM_HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("follow command should start");
    std::thread::sleep(std::time::Duration::from_millis(100));
    follower.kill().expect("stop live follower");
    let followed = follower
        .wait_with_output()
        .expect("collect follower output");
    assert!(
        String::from_utf8_lossy(&followed.stdout).contains("ticket.created"),
        "live follow should print the existing backlog"
    );

    let conflict = run_tm(
        project.path(),
        home.path(),
        &["events", "tail", "--follow", "--no-follow"],
    );
    assert!(
        !conflict.status.success(),
        "conflicting follow options must fail"
    );
}
