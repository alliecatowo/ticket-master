//! `tm run <ticket> --record <path>` on `SIGTERM` mid-run (u1-record-flush-incremental): before
//! this fix, `--record`'s real path (`RecordingProviderWrapper`/`CassetteSink` in
//! `crates/tm-cli/src/agent.rs`, wired up in `crates/tm-cli/src/sched.rs`) only ever wrote the
//! whole cassette once, via `Cassette::write_jsonl`, after the run finished -- a kill mid-run left
//! no file at all (`ls <path>` gave "No such file or directory" even after a completed provider
//! call). The fix appends each entry incrementally to a `tm_provider::cassette::CassetteWriter`
//! (fsynced immediately) as it's recorded, so a completed call survives a kill of the process that
//! made it.
//!
//! Follows the `Command::new(env!("CARGO_BIN_EXE_tm"))` + tempdir-isolated `--project` convention
//! `tests/run_interrupt.rs`/`tests/worktree_run.rs` already use.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn init_project(root: &Path) {
    tm_core::Store::open(root).expect("open (and thereby create) a fresh .tm project");
}

fn activate_ticket(root: &Path, ticket: &str) {
    let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::SystemClock);
    let ids: Arc<dyn tm_types::IdSource> = Arc::new(tm_types::CounterIds::new());
    let store =
        tm_core::Store::open_with(root, clock, ids).expect("open store to activate the ticket");
    let actor = tm_types::ParticipantId::new("human:test").expect("actor id");
    let ticket_id = tm_types::TicketId::new(ticket).expect("ticket id");
    store.activate(&ticket_id, actor).expect("activate ticket");
}

/// Poll for `path` to exist and contain at least `min_bytes` -- proof the incremental writer's
/// header (and, once a call has completed, its first entry) has actually landed on disk, not
/// just that the process is still alive.
fn wait_for_file_at_least(path: &Path, min_bytes: u64, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() >= min_bytes {
                return;
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "{} never reached {min_bytes} bytes within {timeout:?} (currently: {:?})",
                path.display(),
                std::fs::metadata(path).map(|m| m.len())
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn sigterm_after_one_recorded_call_leaves_a_valid_partial_cassette() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_project(root);
    let tm_home = tempfile::tempdir().expect("tempdir");
    let cassette_path = tmp.path().join("recording.cassette");

    let created = Command::new(env!("CARGO_BIN_EXE_tm"))
        .args([
            "--project",
            root.to_str().expect("utf8 tempdir"),
            "--json",
            "ticket",
            "new",
            "record then interrupt me",
        ])
        .env("TM_HOME", tm_home.path())
        .env("TM_NOTIFY", "0")
        .env("TM_TEST_MOCK_PROVIDER", "1")
        .output()
        .expect("`tm ticket new` should run");
    assert!(
        created.status.success(),
        "ticket new failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let ticket_id: String = serde_json::from_slice(&created.stdout).expect("ticket id json");
    activate_ticket(root, &ticket_id);

    // The scripted worker's first call (artifact.store, per its own doc comment) is served
    // normally; the second (ticket.submit) blocks forever -- so by the time this test sends its
    // signal, exactly one completion has already been recorded, and this proves that specific
    // completed entry survives rather than only the empty header.
    let mut child = Command::new(env!("CARGO_BIN_EXE_tm"))
        .args([
            "--project",
            root.to_str().expect("utf8 tempdir"),
            "run",
            &ticket_id,
            "--record",
        ])
        .arg(&cassette_path)
        .env("TM_HOME", tm_home.path())
        .env("TM_NOTIFY", "0")
        .env("TM_TEST_MOCK_PROVIDER", "1")
        .env("TM_TEST_MOCK_PROVIDER_BLOCK_AFTER", "2")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `tm run --record`");

    // A header-only cassette is a handful of bytes; require enough that at least one real entry
    // (which embeds a whole `CompletionRequest`/`Completion`) must have landed too, not just the
    // header written before dispatch even started.
    wait_for_file_at_least(&cassette_path, 200, Duration::from_secs(10));

    let pid = child.id();
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("send SIGTERM via `kill`");
    assert!(status.success(), "`kill -TERM {pid}` itself failed to run");

    let output = child
        .wait_with_output()
        .expect("`tm run` should exit after SIGTERM, not hang");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(130),
        "a SIGTERM'd `tm run` should exit 130; stderr: {stderr}"
    );

    let cassette = tm_provider::Cassette::read_jsonl(&cassette_path).unwrap_or_else(|e| {
        panic!(
            "the cassette at {} must still be a validly readable file after the kill \
             (this is the exact bug u1-record-flush-incremental fixes -- before it, no file \
             existed at all): {e}",
            cassette_path.display()
        )
    });
    assert_eq!(
        cassette.entries.len(),
        1,
        "exactly the one completed call before the block should have been durably recorded; \
         entries: {:?}",
        cassette.entries
    );
}
