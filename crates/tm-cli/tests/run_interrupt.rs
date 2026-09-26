//! `tm run <ticket>` on `SIGINT`/`SIGTERM` (u1-run-sigterm-releases-lease): a killed foreground
//! run used to leave the ticket permanently `Leased`/`Running` (`tm ticket list` showed
//! `T-2 work active` forever after the process that was driving it was gone) — there was no
//! signal handling in `run_ticket`'s poll loop at all, so the process just died and the lease it
//! held never got released. `crates/tm-cli/src/sched.rs`'s `InterruptWatcher`/
//! `handle_run_interrupt` fix that by calling the real `Store::record_failure` on the first
//! `SIGINT`/`SIGTERM`, which both releases the lease and drives the same retry-vs-escalate
//! decision any other failed attempt goes through.
//!
//! Follows the `Command::new(env!("CARGO_BIN_EXE_tm"))` + tempdir-isolated `--project` convention
//! `tests/worktree_run.rs`/`tests/sched_run.rs` already use — never the primary checkout, always
//! a fresh tempdir. Sends a real `SIGTERM` via the `kill` binary rather than a new `libc`/`nix`
//! dev-dependency, since this workspace has no existing precedent for sending signals from a test
//! and shelling out to `kill` needs nothing new.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A fresh `.tm` project the `tm` subprocess can open, following `tests/worktree_run.rs`'s own
/// helper naming — opens the same store type the CLI uses rather than shelling out to `tm init`.
fn init_project(root: &Path) {
    tm_core::Store::open(root).expect("open (and thereby create) a fresh .tm project");
}

/// `tm ticket new` leaves a ticket in `Draft`; `tm run` requires `Ready`. Opens the same store
/// the subprocess will open, activates it directly, and drops the handle before returning so the
/// subprocess never contends with it for the sqlite file — mirrors `tests/worktree_run.rs`'s
/// `activate_ticket`.
fn activate_ticket(root: &Path, ticket: &str) {
    let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::SystemClock);
    let ids: Arc<dyn tm_types::IdSource> = Arc::new(tm_types::CounterIds::new());
    let store =
        tm_core::Store::open_with(root, clock, ids).expect("open store to activate the ticket");
    let actor = tm_types::ParticipantId::new("human:test").expect("actor id");
    let ticket_id = tm_types::TicketId::new(ticket).expect("ticket id");
    store.activate(&ticket_id, actor).expect("activate ticket");
}

/// Poll the real on-disk store (a fresh handle each time, so it always sees the subprocess's
/// latest committed state) until `ticket` leaves `Ready`, meaning the subprocess has leased it
/// and a `tm run` attempt is genuinely in flight -- the point at which sending a signal actually
/// exercises `handle_run_interrupt` rather than racing against a run that hasn't started yet.
fn wait_until_in_flight(root: &Path, ticket: &str, timeout: Duration) -> tm_core::TicketState {
    let ticket_id = tm_types::TicketId::new(ticket).expect("ticket id");
    let deadline = Instant::now() + timeout;
    loop {
        let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::SystemClock);
        let ids: Arc<dyn tm_types::IdSource> = Arc::new(tm_types::CounterIds::new());
        let store = tm_core::Store::open_with(root, clock, ids).expect("reopen store to poll");
        let view = store.view().expect("project view");
        let state = view.tickets.get(&ticket_id).expect("ticket exists").state;
        if state != tm_core::TicketState::Ready {
            return state;
        }
        if Instant::now() >= deadline {
            panic!("ticket never left Ready within {timeout:?} (still {state:?})");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn sigterm_during_a_run_releases_the_lease_and_records_an_interrupted_attempt() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_project(root);
    let tm_home = tempfile::tempdir().expect("tempdir");

    let created = Command::new(env!("CARGO_BIN_EXE_tm"))
        .args([
            "--project",
            root.to_str().expect("utf8 tempdir"),
            "--json",
            "ticket",
            "new",
            "interrupt me",
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

    // `TM_TEST_MOCK_PROVIDER_BLOCK=1` makes the scripted provider's completion never resolve, so
    // the run stays genuinely in-flight (leased, mid-attempt) long enough for this test to land
    // a real signal on it instead of racing a turn that would otherwise finish in milliseconds.
    let child = Command::new(env!("CARGO_BIN_EXE_tm"))
        .args([
            "--project",
            root.to_str().expect("utf8 tempdir"),
            "run",
            &ticket_id,
        ])
        .env("TM_HOME", tm_home.path())
        .env("TM_NOTIFY", "0")
        .env("TM_TEST_MOCK_PROVIDER", "1")
        .env("TM_TEST_MOCK_PROVIDER_BLOCK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `tm run`");

    let state = wait_until_in_flight(root, &ticket_id, Duration::from_secs(10));
    assert!(
        matches!(
            state,
            tm_core::TicketState::Leased | tm_core::TicketState::Running
        ),
        "ticket should be Leased or Running (attempt in flight, blocked on the never-resolving \
         mock completion) at the moment the signal is sent, got {state:?}"
    );

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
        "a SIGTERM'd `tm run` should exit 130 (the conventional killed-by-signal code); \
         stderr: {stderr}"
    );

    let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::SystemClock);
    let ids: Arc<dyn tm_types::IdSource> = Arc::new(tm_types::CounterIds::new());
    let store = tm_core::Store::open_with(root, clock, ids).expect("reopen store after the run");
    let view = store.view().expect("project view");
    let ticket_id_typed = tm_types::TicketId::new(&ticket_id).expect("ticket id");
    let ticket = view
        .tickets
        .get(&ticket_id_typed)
        .expect("ticket still exists after being interrupted");

    assert_eq!(
        ticket.state,
        tm_core::TicketState::Ready,
        "an interrupted attempt should release the lease and return the ticket to Ready via \
         the normal retry path, not leave it stuck Leased/Running forever \
         (the exact dogfood symptom this test guards against)"
    );
    assert!(
        ticket
            .failures
            .iter()
            .any(|f| f.detail.contains("interrupted by user")),
        "the ticket's failure history should record the interruption; failures: {:?}",
        ticket.failures
    );
}
