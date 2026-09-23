//! The tickets screen (`tm tickets`, D-019 §2: Claude Code's agent view with tickets as rows),
//! driven end to end through the real compiled `tm` binary in a real pty, against a project whose
//! tickets were put into real states through `tm_core::Store` itself. Plus `tm tickets --json`.

mod support;

use std::path::Path;
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;
use tm_core::{
    ArtifactKind, ExecutorRequirements, FailureClass, RetryPolicy, Store, TicketKind, TicketState,
    Trigger, VerificationPolicy,
};
use tm_types::{ArtifactId, Authority, Budget, ParticipantId, Role, TicketId, Tolerance};

const ESC: u8 = 0x1b;
const CTRL_X: u8 = 0x18;
const DOWN: &[u8] = b"\x1b[B";
/// Only the tickets screen shows this (its dispatch input's placeholder).
const TICKETS_MARK: &str = "Describe a task for a background worker";
const CHAT_MARK: &str = "Ask tm anything";

fn human() -> ParticipantId {
    ParticipantId::new("human:tester").expect("valid participant")
}

fn worker() -> ParticipantId {
    ParticipantId::new("agent:builtin/tester").expect("valid participant")
}

fn create(store: &Store, objective: &str, max_attempts: u32) -> TicketId {
    let events = store
        .create_ticket(
            TicketKind::Work,
            objective.to_string(),
            None,
            None,
            Authority::worker(),
            Vec::new(),
            ExecutorRequirements {
                role: Role::CoderFast,
                human_required: false,
                min_capability: Tolerance::Preferred,
            },
            Vec::new(),
            Vec::new(),
            VerificationPolicy::Single,
            Budget::unlimited(),
            RetryPolicy {
                max_attempts,
                base_delay_seconds: 0,
                backoff_multiplier: 1.0,
                max_delay_seconds: 0,
            },
            0,
            human(),
        )
        .expect("create_ticket");
    events
        .iter()
        .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.clone()))
        .expect("ticket.created")
}

/// A ticket a worker leased and started: `Running`.
fn running(store: &Store, objective: &str, max_attempts: u32) -> TicketId {
    let id = create(store, objective, max_attempts);
    store.activate(&id, human()).expect("activate");
    store
        .acquire_lease(&id, worker(), Authority::none(), vec![], 600, human())
        .expect("lease");
    store
        .transition(&id, Trigger::WorkStarted, worker())
        .expect("work started");
    id
}

/// A ticket whose worker submitted it for review: `Submitted`, with a summary and evidence.
fn submitted(store: &Store, objective: &str, summary: &str) -> TicketId {
    let id = running(store, objective, 3);
    let events = store
        .store_artifact(
            ArtifactKind::File,
            "text/plain".into(),
            b"tests pass".to_vec(),
            serde_json::json!({}),
            Some(id.clone()),
            worker(),
        )
        .expect("artifact");
    let artifact = ArtifactId::new(events[0].subject.as_str()).expect("artifact id");
    store
        .submit(&id, summary.to_string(), vec![artifact], worker())
        .expect("submit");
    id
}

struct Seeded {
    dir: tempfile::TempDir,
    escalated: TicketId,
    review: TicketId,
    second_review: TicketId,
    draft: TicketId,
    stopped: TicketId,
}

/// One ticket per group: escalated (Needs input), two submitted (Ready for review), a draft
/// (Queued), and a cancelled one (Completed, as stopped).
fn seed() -> Seeded {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open store");
    let stopped = create(&store, "delta stopped probe", 3);
    store
        .cancel(&stopped, Some("not needed".into()), human())
        .expect("cancel");
    let draft = create(&store, "gamma queued probe", 3);
    let review = submitted(&store, "alpha review probe", "all alpha tests pass");
    let second_review = submitted(&store, "omega review probe", "omega done");
    let escalated = running(&store, "beta escalation probe", 1);
    store
        .record_failure(
            &escalated,
            FailureClass::Other,
            "beta needs a human decision".into(),
            worker(),
        )
        .expect("record failure");
    Seeded {
        dir,
        escalated,
        review,
        second_review,
        draft,
        stopped,
    }
}

fn state_of(project: &Path, id: &TicketId) -> TicketState {
    let store = Store::open(project).expect("reopen store");
    store.view().expect("view").tickets[id].state
}

/// Poll the store until `id` reaches a state `pred` accepts (the TUI writes asynchronously to
/// this test), or fail after a bound.
fn wait_state(project: &Path, id: &TicketId, pred: impl Fn(TicketState) -> bool) -> TicketState {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = state_of(project, id);
        if pred(state) || Instant::now() >= deadline {
            return state;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `tm tickets` on `project` in a 110x34 pty. `mock` sets `TM_TEST_MOCK_PROVIDER`; every test
/// here sets it, because the TUI's in-process scheduler works any ticket that becomes ready (a
/// rejected one does) and must never reach a real provider from a test.
fn spawn_tickets(project: &Path, tm_home: &Path, mock: bool) -> support::Pty {
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_tm"));
    cmd.arg("--project");
    cmd.arg(project);
    cmd.arg("tickets");
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TM_HOME", tm_home);
    cmd.env("TM_NOTIFY", "0");
    if mock {
        cmd.env("TM_TEST_MOCK_PROVIDER", "1");
    }
    support::Pty::spawn(cmd, 110, 34).expect("spawn `tm tickets` inside a pty")
}

fn has(screen: &[String], needle: &str) -> bool {
    screen.iter().any(|line| line.contains(needle))
}

fn line_of(screen: &[String], needle: &str) -> usize {
    screen
        .iter()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not on screen: {screen:#?}"))
}

fn quit(pty: &mut support::Pty) {
    pty.write(&[0x03]).expect("ctrl-c");
    pty.write(&[0x03]).expect("ctrl-c");
    let exited = pty.wait(Duration::from_secs(10)).expect("wait for exit");
    assert!(exited, "`tm` must exit 0 after Ctrl+C twice");
}

#[test]
fn tm_tickets_opens_on_the_tickets_screen_grouped_like_the_agent_view() {
    let s = seed();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn_tickets(s.dir.path(), tm_home.path(), true);
    let screen = pty.wait_for("delta stopped probe", Duration::from_secs(15));
    assert!(has(&screen, TICKETS_MARK), "{screen:#?}");

    // Groups in the agent view's order, each ticket under its own group.
    let needs = line_of(&screen, "Needs input");
    let review = line_of(&screen, "Ready for review");
    let queued = line_of(&screen, "Queued");
    let completed = line_of(&screen, "Completed");
    assert!(
        needs < review && review < queued && queued < completed,
        "{screen:#?}"
    );
    let beta = line_of(&screen, "beta escalation probe");
    let alpha = line_of(&screen, "alpha review probe");
    let gamma = line_of(&screen, "gamma queued probe");
    let delta = line_of(&screen, "delta stopped probe");
    assert!(needs < beta && beta < review, "{screen:#?}");
    assert!(review < alpha && alpha < queued, "{screen:#?}");
    assert!(queued < gamma && gamma < completed, "{screen:#?}");
    assert!(completed < delta, "{screen:#?}");

    // Rows carry the id, a summary, and the header counts them.
    assert!(screen[beta].contains(s.escalated.as_str()));
    assert!(
        screen[beta].contains("beta needs a human decision"),
        "{screen:#?}"
    );
    assert!(
        screen[alpha].contains("all alpha tests pass"),
        "{screen:#?}"
    );
    assert!(screen[delta].contains("stopped: not needed"), "{screen:#?}");
    assert!(screen[gamma].contains(s.draft.as_str()));
    assert!(
        has(
            &screen,
            "1 needs input · 2 ready for review · 1 queued · 1 completed"
        ),
        "{screen:#?}"
    );

    // Esc goes to a fresh chat.
    pty.write(&[ESC]).expect("Esc");
    let screen = pty.wait_for(CHAT_MARK, Duration::from_secs(10));
    assert!(has(&screen, CHAT_MARK), "{screen:#?}");
    quit(&mut pty);
    assert_eq!(state_of(s.dir.path(), &s.stopped), TicketState::Cancelled);
}

#[test]
fn dispatching_from_the_input_creates_and_queues_a_ticket() {
    let s = seed();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn_tickets(s.dir.path(), tm_home.path(), true);
    let _ = pty.wait_for(TICKETS_MARK, Duration::from_secs(15));

    pty.write(b"epsilon dispatch probe").expect("type a task");
    let _ = pty.wait_for("epsilon dispatch probe", Duration::from_secs(5));
    pty.write(b"\r").expect("Enter");
    let screen = pty.wait_for("to a background worker", Duration::from_secs(10));
    assert!(
        has(&screen, "Dispatched T-") || has(&screen, "Queued T-"),
        "{screen:#?}"
    );
    assert!(
        has(&screen, "epsilon dispatch probe"),
        "the new row is listed: {screen:#?}"
    );

    // It really exists, with the same defaults `tm ticket new` uses, and it was activated (the
    // in-process scheduler may already have picked it up, so only "no longer a draft" is stable).
    let store = Store::open(s.dir.path()).expect("reopen");
    let view = store.view().expect("view");
    let ticket = view
        .tickets
        .values()
        .find(|t| t.objective == "epsilon dispatch probe")
        .expect("the dispatched ticket exists");
    assert_eq!(ticket.kind, TicketKind::Work);
    assert_eq!(ticket.authority, Authority::worker());
    assert_eq!(ticket.budget, Budget::unlimited());
    let id = ticket.id.clone();
    drop(view);
    let state = wait_state(s.dir.path(), &id, |st| st != TicketState::Draft);
    assert_ne!(
        state,
        TicketState::Draft,
        "dispatch must activate the ticket"
    );
    quit(&mut pty);
}

#[test]
fn space_peeks_at_the_selected_ticket() {
    let s = seed();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn_tickets(s.dir.path(), tm_home.path(), true);
    let _ = pty.wait_for("beta escalation probe", Duration::from_secs(15));

    // The first row (Needs input) is selected; Space opens its peek panel.
    pty.write(b" ").expect("Space");
    let screen = pty.wait_for("attempt 1  Other: beta needs", Duration::from_secs(10));
    assert!(
        has(&screen, "objective  beta escalation probe"),
        "{screen:#?}"
    );
    assert!(has(&screen, "since it escalated"), "{screen:#?}");
    assert!(has(&screen, "space to close"), "{screen:#?}");

    // ↓ peeks at the next ticket without closing the panel.
    pty.write(DOWN).expect("Down");
    let screen = pty.wait_for("submitted  ", Duration::from_secs(10));
    assert!(
        has(&screen, "evidence"),
        "the submission's evidence shows: {screen:#?}"
    );

    // Esc closes the panel first, not the screen.
    pty.write(&[ESC]).expect("Esc");
    let screen = pty.wait_until_gone("space to close", Duration::from_secs(10));
    assert!(has(&screen, TICKETS_MARK), "{screen:#?}");
    quit(&mut pty);
    let _ = s.review;
}

#[test]
fn ctrl_x_twice_cancels_the_selected_ticket() {
    let s = seed();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn_tickets(s.dir.path(), tm_home.path(), true);
    let _ = pty.wait_for("beta escalation probe", Duration::from_secs(15));

    pty.write(&[CTRL_X]).expect("Ctrl+X");
    let screen = pty.wait_for("Press ctrl+x again to cancel", Duration::from_secs(10));
    assert!(
        has(
            &screen,
            &format!("Press ctrl+x again to cancel {}", s.escalated)
        ),
        "{screen:#?}"
    );
    assert_eq!(
        state_of(s.dir.path(), &s.escalated),
        TicketState::Escalated,
        "one press does not cancel"
    );
    pty.write(&[CTRL_X]).expect("Ctrl+X again");
    let state = wait_state(s.dir.path(), &s.escalated, |st| {
        st == TicketState::Cancelled
    });
    assert_eq!(state, TicketState::Cancelled);
    let screen = pty.wait_for("Cancelled T-", Duration::from_secs(10));
    assert!(
        has(&screen, "stopped"),
        "it moves to Completed as stopped: {screen:#?}"
    );
    quit(&mut pty);
}

#[test]
fn a_accepts_and_r_rejects_a_ready_for_review_ticket() {
    let s = seed();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn_tickets(s.dir.path(), tm_home.path(), true);
    let _ = pty.wait_for("alpha review probe", Duration::from_secs(15));

    // Needs input row -> "Ready for review" heading -> the first review row (newest first:
    // omega), then the next (alpha).
    pty.write(DOWN).expect("Down");
    pty.write(DOWN).expect("Down");
    let screen = pty.wait_for("a to accept", Duration::from_secs(10));
    assert!(has(&screen, "r to reject"), "{screen:#?}");
    pty.write(b"a").expect("a");
    let state = wait_state(s.dir.path(), &s.second_review, |st| {
        st == TicketState::Closed
    });
    assert_eq!(
        state,
        TicketState::Closed,
        "`a` accepts the selected submission"
    );
    let _ = pty.wait_for("Accepted T-", Duration::from_secs(10));

    // The selection stays on omega, now under Completed; walk back up to alpha.
    let screen = pty.settle(Duration::from_millis(300), Duration::from_secs(2));
    let alpha = line_of(&screen, "alpha review probe");
    let omega = line_of(&screen, "omega review probe");
    assert!(
        alpha < omega,
        "omega moved below, into Completed: {screen:#?}"
    );
    let mut on_review = false;
    for _ in 0..10 {
        pty.write(b"\x1b[A").expect("Up");
        let screen = pty.settle(Duration::from_millis(200), Duration::from_secs(2));
        if has(&screen, "a to accept") {
            on_review = true;
            break;
        }
    }
    assert!(on_review, "walking up reaches the remaining review row");
    pty.write(b"r").expect("r");
    let screen = pty.wait_for("Why reject", Duration::from_secs(10));
    assert!(
        has(&screen, &format!("Why reject {}?", s.review)),
        "{screen:#?}"
    );
    pty.write(b"tests skip the flaky path").expect("reason");
    let _ = pty.wait_for("tests skip the flaky path", Duration::from_secs(5));
    pty.write(b"\r").expect("Enter");
    let state = wait_state(s.dir.path(), &s.review, |st| st != TicketState::Submitted);
    assert_ne!(state, TicketState::Submitted, "`r` + reason rejects it");
    let store = Store::open(s.dir.path()).expect("reopen");
    let ticket = store.view().expect("view").tickets[&s.review].clone();
    assert!(
        ticket
            .failures
            .iter()
            .any(|f| f.detail.contains("tests skip the flaky path")),
        "the reason joins the failure history the next attempt sees: {:?}",
        ticket.failures
    );
    quit(&mut pty);
}

#[test]
fn enter_attaches_the_chat_to_the_ticket_with_a_recap() {
    let s = seed();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let mut pty = spawn_tickets(s.dir.path(), tm_home.path(), true);
    let _ = pty.wait_for("beta escalation probe", Duration::from_secs(15));
    pty.write(b"\r").expect("Enter");
    let screen = pty.wait_for("Recap of", Duration::from_secs(10));
    assert!(
        has(&screen, &format!("Attached to {}", s.escalated)),
        "{screen:#?}"
    );
    assert!(
        has(&screen, &format!("Recap of {}: escalated", s.escalated)),
        "{screen:#?}"
    );
    quit(&mut pty);
}

/// Run the real `tm` binary to completion with piped (non-tty) stdio.
fn tm(args: &[&str], cwd: &Path, tm_home: &Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_tm"))
        .args(args)
        .current_dir(cwd)
        .env("TM_HOME", tm_home)
        .env("TM_NOTIFY", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run tm")
}

#[test]
fn tm_tickets_json_lists_open_tickets_and_all_adds_completed() {
    let s = seed();
    let tm_home = tempfile::tempdir().expect("tempdir");
    let project = s.dir.path().to_str().expect("utf-8 path");
    let out = tm(
        &["--project", project, "tickets", "--json"],
        s.dir.path(),
        tm_home.path(),
    );
    assert!(out.status.success(), "{out:?}");
    let rows: serde_json::Value = serde_json::from_slice(&out.stdout).expect("a JSON array");
    let rows = rows.as_array().expect("array");
    let ids: Vec<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(rows.len(), 4, "open tickets only: {ids:?}");
    assert!(!ids.contains(&s.stopped.as_str()));
    let beta = rows
        .iter()
        .find(|r| r["id"] == s.escalated.as_str())
        .expect("the escalated ticket");
    assert_eq!(beta["group"], "needs_input");
    assert_eq!(beta["state"], "escalated");
    assert_eq!(beta["waiting_for"], "escalation");
    assert!(beta["summary"]
        .as_str()
        .is_some_and(|s| s.contains("beta needs a human decision")));
    let alpha = rows
        .iter()
        .find(|r| r["id"] == s.review.as_str())
        .expect("the review ticket");
    assert_eq!(alpha["group"], "review");
    assert_eq!(alpha["submission"], "all alpha tests pass");

    let out = tm(
        &["--project", project, "tickets", "--json", "--all"],
        s.dir.path(),
        tm_home.path(),
    );
    let rows: serde_json::Value = serde_json::from_slice(&out.stdout).expect("a JSON array");
    let stopped = rows
        .as_array()
        .expect("array")
        .iter()
        .find(|r| r["id"] == s.stopped.as_str())
        .expect("--all includes the cancelled ticket");
    assert_eq!(stopped["group"], "completed");
    assert_eq!(stopped["summary"], "stopped: not needed");
}

#[test]
fn tm_tickets_json_without_a_project_is_empty_and_creates_nothing() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let tm_home = tempfile::tempdir().expect("tempdir");
    let out = tm(&["tickets", "--json"], cwd.path(), tm_home.path());
    assert!(out.status.success(), "{out:?}");
    let rows: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(rows, serde_json::json!([]));
    assert!(!cwd.path().join(".tm").exists());
    assert!(
        std::fs::read_dir(tm_home.path())
            .map(|mut d| d.next().is_none())
            .unwrap_or(true),
        "listing must not create a global project"
    );
}
