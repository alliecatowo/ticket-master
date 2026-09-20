//! `tm ticket fork <TICKET> --at <SEQ>` against the real compiled `tm` binary
//! (`docs/decisions/D-008-ticket-checkpoint-fork.md`), following the `Command::new(env!(
//! "CARGO_BIN_EXE_tm"))` + tempdir-isolated `--project` convention `tests/auth_redaction.rs` and
//! `tests/bare_scope.rs` already use — never the primary checkout, always a fresh tempdir.
//!
//! `tm-core::store`'s own tests already prove the seq-bounded-replay semantics in depth (state as
//! of `seq` vs. as of HEAD, goal reconstruction, hash-chain integrity); this file's job is
//! narrower — prove the CLI verb itself is wired end to end: it produces a real new ticket id,
//! that ticket's state genuinely reflects the source as of `--at` rather than the source's
//! current state, and an out-of-range `--at` is a real CLI error, not a silent no-op.

use std::path::Path;
use std::process::{Command, Stdio};

/// Run the real `tm` binary in `dir`, scoped to `dir` itself via `--project` (always repo scope
/// at exactly that path, per `GlobalOpts::project`'s own doc comment), with stdin closed — every
/// command this file drives is a one-shot subcommand, never the interactive loop.
fn run_tm(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut full_args = vec!["--project", dir.to_str().expect("utf8 tempdir path")];
    full_args.extend_from_slice(args);
    Command::new(env!("CARGO_BIN_EXE_tm"))
        .args(&full_args)
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

/// The `seq` of the first event of `kind` recorded against `subject` in `dir`'s project log —
/// read directly off `project.db`, the same "legitimate way to inspect what actually got
/// written" this repo's own `CLAUDE.md` documents, since no `tm events` verb exposes a stable,
/// scriptable "seq for this ticket's creation" lookup today.
fn first_seq_for(dir: &Path, kind: &str, subject: &str) -> u64 {
    let db_path = dir.join(".tm").join("project.db");
    let conn =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap_or_else(|e| panic!("open {}: {e}", db_path.display()));
    conn.query_row(
        "SELECT MIN(seq) FROM events WHERE kind = ?1 AND subject = ?2",
        rusqlite::params![kind, subject],
        |row| row.get(0),
    )
    .unwrap_or_else(|e| panic!("query seq for {kind} {subject}: {e}"))
}

#[test]
fn fork_at_seq_reflects_source_state_as_of_seq_not_current_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let created = run_tm_ok(root, &["--json", "ticket", "new", "first objective"]);
    let source_id: String = serde_json::from_slice(&created.stdout).expect("ticket id json");

    let created_seq = first_seq_for(root, "ticket.created", &source_id);

    run_tm_ok(
        root,
        &[
            "ticket",
            "edit",
            &source_id,
            "--objective",
            "changed after the fork point",
        ],
    );

    let forked = run_tm_ok(
        root,
        &[
            "--json",
            "ticket",
            "fork",
            &source_id,
            "--at",
            &created_seq.to_string(),
        ],
    );
    let forked_json: serde_json::Value =
        serde_json::from_slice(&forked.stdout).expect("fork json output");
    let forked_id = forked_json["ticket"]
        .as_str()
        .expect("forked ticket id")
        .to_string();
    assert_ne!(
        forked_id, source_id,
        "fork must produce a brand-new ticket id"
    );
    assert_eq!(forked_json["forked_from"], source_id);
    assert_eq!(forked_json["at_seq"], created_seq);

    let shown = run_tm_ok(root, &["--json", "ticket", "show", &forked_id]);
    let ticket: serde_json::Value = serde_json::from_slice(&shown.stdout).expect("ticket json");
    assert_eq!(
        ticket["objective"], "first objective",
        "the fork must carry the source's objective as of --at, not its later edit"
    );
    assert_eq!(ticket["state"], "draft", "a fresh fork always starts Draft");

    // The source ticket's own current state must be completely unaffected by having been forked.
    let source_shown = run_tm_ok(root, &["--json", "ticket", "show", &source_id]);
    let source_ticket: serde_json::Value =
        serde_json::from_slice(&source_shown.stdout).expect("ticket json");
    assert_eq!(source_ticket["objective"], "changed after the fork point");
}

#[test]
fn fork_at_a_seq_beyond_the_log_head_is_a_real_error_not_a_silent_head_fork() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let created = run_tm_ok(root, &["--json", "ticket", "new", "only objective"]);
    let source_id: String = serde_json::from_slice(&created.stdout).expect("ticket id json");

    let output = run_tm(root, &["ticket", "fork", &source_id, "--at", "999999999"]);
    assert!(
        !output.status.success(),
        "forking at a seq past the log's head must fail, not silently fork from HEAD"
    );
}

#[test]
fn fork_of_an_unknown_ticket_is_a_real_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // Establish a project (and thus a project.db with at least one event) without ever creating
    // the ticket this test then tries to fork.
    run_tm_ok(root, &["--json", "ticket", "new", "unrelated"]);

    let output = run_tm(root, &["ticket", "fork", "T-999", "--at", "1"]);
    assert!(
        !output.status.success(),
        "forking a ticket id that was never created must fail"
    );
}
