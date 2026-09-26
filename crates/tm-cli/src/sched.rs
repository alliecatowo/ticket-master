//! The `sched`, `lease`, and `run` command groups: everything that drives the scheduler
//! ([`tm_scheduler`]) or executes a ticket through [`tm_agent`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tm_core::ArtifactKind;
use tm_provider::{Cassette, CassetteHeader, CASSETTE_FORMAT_VERSION};
use tm_types::{Authority, LeaseId, ParticipantId, TicketId};

use crate::args::{
    LeaseAcquireArgs, LeaseCommand, LeaseListArgs, LeaseRefArgs, RunArgs, SchedCommand,
    SchedRunArgs,
};
use crate::project::Project;
use crate::render::{Renderer, Table};

/// A serializable representation of a scheduler action for JSON output.
#[derive(Debug, Clone, Serialize)]
struct ActionSummary {
    /// The kind of action (e.g., "Lease", "MarkReady", "Escalate").
    kind: String,
    /// The primary target (ticket or lease id).
    target: String,
}

/// A serializable representation of a scheduler loop event for JSON output.
#[derive(Debug, Clone, Serialize)]
struct EventSummary {
    /// The event kind (e.g., "Ticked", "Applied", "Failed").
    kind: String,
    /// Descriptive detail about the event.
    detail: String,
}

/// A serializable representation of a lease for JSON output.
#[derive(Debug, Clone, Serialize)]
struct LeaseSummary {
    /// The lease id.
    id: String,
    /// The ticket being held.
    ticket: String,
    /// Who holds the lease.
    holder: String,
    /// When it was acquired (ISO 8601 timestamp).
    acquired: String,
    /// When it expires (ISO 8601 timestamp).
    expires: String,
}

/// Dispatch one [`SchedCommand`].
///
/// `async` (unlike the sibling `dispatch_lease`) because [`SchedCommand::Run`] must reach
/// [`sched_run`] via a plain `.await` rather than spinning up a second `tokio::runtime::Runtime`
/// and blocking on it -- this function's only caller, `main.rs`'s `dispatch`, already runs on a
/// worker thread of the one runtime `#[tokio::main]` installed, and a second `Runtime::new()`
/// entered from *that* thread panics (tokio's own "Cannot start a runtime from within a runtime"
/// guard). There was never a deliberate reason for the second runtime -- git history shows the
/// `#[tokio::main]` `main` and this nested `Runtime::new()?.block_on(..)` were introduced in the
/// same commit, so every `tm sched run` invocation has always hit this panic.
pub async fn dispatch_sched(
    cmd: &SchedCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        SchedCommand::Plan => sched_plan(project, renderer),
        SchedCommand::Tick => sched_tick(project, renderer),
        SchedCommand::Run(args) => sched_run(args, project, renderer).await,
        SchedCommand::Pause => sched_pause(project, renderer),
        SchedCommand::Resume => sched_resume(project, renderer),
    }
}

/// `tm sched plan`: print planned actions without applying them.
pub fn sched_plan(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let view = project.store.scheduler_view()?;
    let now = project.clock.now();
    let policy = tm_scheduler::SchedulingPolicy::conservative_default();
    let actions = tm_scheduler::plan::plan(&view, now, &policy);

    let summaries: Vec<ActionSummary> = actions.iter().map(action_to_summary).collect();

    let human = summaries
        .iter()
        .map(|s| format!("{}: {}", s.kind, s.target))
        .collect::<Vec<_>>()
        .join("\n");
    let human = if human.is_empty() {
        "No scheduler actions planned. Tickets may be absent, blocked, or awaiting a worker/provider.".to_string()
    } else {
        human
    };

    if !renderer.is_quiet() {
        renderer.emit(&summaries, &human)?;
    }
    Ok(())
}

/// Who the scheduler acts as: the leases it grants, the attempts it starts, the retries and
/// escalations it drives. It is deterministic machinery, so it records as
/// [`ParticipantId::system`], never as the human who happened to start the process (`tm sched
/// run`, `tm serve`, the TUI) or activate the ticket. `tm run <ticket>` is different: a person
/// asked for that one run, so it stays attributed to them.
pub(crate) fn scheduler_actor() -> ParticipantId {
    ParticipantId::system()
}

/// `tm sched tick`: run one scheduler tick, applying its actions.
pub fn sched_tick(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let policy = tm_scheduler::SchedulingPolicy::conservative_default();
    let loop_driver =
        tm_scheduler::SchedulerLoop::new(&project.store, project.clock.clone(), policy);
    let events = loop_driver.tick(scheduler_actor())?;

    let summaries: Vec<EventSummary> = events.iter().map(event_to_summary).collect();

    let human = summaries
        .iter()
        .map(|s| format!("{}: {}", s.kind, s.detail))
        .collect::<Vec<_>>()
        .join("\n");

    if !renderer.is_quiet() {
        renderer.emit(&summaries, &human)?;
    }
    Ok(())
}

/// Start the desktop-notification watcher for `project`'s event log unless `TM_NOTIFY` opts out
/// (`crates/tm-notify`, `docs/decisions/D-007-desktop-notifications.md`,
/// `docs/audit-2026-09-18-fable.md` M-04). Fire-and-forget, matching `tm-server`'s identical
/// `AppState::spawn_broadcast_poller` convention: the returned `JoinHandle` is discarded rather
/// than held, since the watcher is meant to run for the lifetime of this process, not be joined
/// or explicitly stopped.
///
/// `tm sched run` and `tm run` are this codebase's two long-running/interactive surfaces that can
/// append `approval.requested` (via `tm-scheduler`'s dispatcher spawning `tm-agent`'s loop as a
/// background task, `crates/tm-scheduler/src/dispatch.rs`) or `ticket.escalated` (`tm sched run`'s
/// own tick, via `SchedulerAction::Escalate`) from *within this same process* — see
/// `tm_notify::watch`'s module docs for why polling the log is the right seam even though a
/// notification-worthy event's own append can happen several call frames away from wherever this
/// function was invoked.
fn start_notification_watcher(project: &Project) {
    if !tm_notify::notifications_enabled_from_env() {
        tracing::debug!("desktop notifications disabled via TM_NOTIFY");
        return;
    }
    match tm_notify::spawn_notification_watcher(
        &project.state_dir,
        Arc::new(tm_notify::SystemNotifier::new()),
        tm_notify::DEFAULT_POLL_INTERVAL,
    ) {
        Ok(_handle) => {}
        Err(e) => {
            tracing::warn!(error = %e, "failed to start desktop-notification watcher");
        }
    }
}

/// `tm sched run`: run the scheduler loop continuously until interrupted (ctrl-c), leasing
/// `Ready` tickets to real executors via the same [`crate::dispatch::build_dispatcher`] `tm run`
/// uses (`SPEC.md` §24, audit B-01/B-04).
pub async fn sched_run(
    args: &SchedRunArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    start_notification_watcher(project);

    let mut policy = tm_scheduler::SchedulingPolicy::conservative_default();
    // `conservative_default` starts with no roles available (a safe default for a caller that
    // never attaches an executor at all). A dispatcher is attached below, so every role now has
    // a real executor behind it — `BuiltinExecutor` today, more adapters later — and can be
    // marked available.
    policy.available_roles = tm_types::Role::ALL.iter().copied().collect();
    let tick_interval_secs = args
        .interval_secs
        .unwrap_or(u64::from(policy.tick_interval_seconds));
    let interval = Duration::from_secs(tick_interval_secs);

    let dispatcher =
        crate::dispatch::build_dispatcher(project, tokio::runtime::Handle::current(), None, None)?;
    let loop_driver =
        tm_scheduler::SchedulerLoop::new(&project.store, project.clock.clone(), policy)
            .with_dispatcher(dispatcher);
    let mut interval_timer = tokio::time::interval(interval);

    loop {
        tokio::select! {
            _ = interval_timer.tick() => {
                if !is_scheduler_paused(project)? {
                    match loop_driver.tick(scheduler_actor()) {
                        Ok(events) => {
                            if !renderer.is_quiet() {
                                for event in &events {
                                    let summary = event_to_summary(event);
                                    renderer.note(&format!("{}: {}", summary.kind, summary.detail));
                                }
                            }
                        }
                        Err(e) => {
                            renderer.error(&e);
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                renderer.note("Scheduler stopped.");
                break;
            }
        }
    }

    Ok(())
}

/// Run the scheduler inside this process, the way `tm`'s TUI does while it is open, so tickets
/// dispatched from it actually get worked without anyone starting `tm sched run` (D-019; Claude
/// Code's agent view has a supervisor daemon for the same reason). It ticks every `interval`,
/// honors `tm sched pause`, and logs through `tracing` only, never the terminal a TUI is drawing
/// on. Leases keep it from double-working a ticket a separate `tm sched run` or `tm serve` is
/// also running. Stops when the returned handle is aborted or the runtime shuts down. It acts
/// as [`scheduler_actor`], not as `project.actor`.
///
/// # Errors
/// Fails up front if the dispatcher can't be built (typically: no provider configured).
pub fn spawn_background_runner(
    project: std::sync::Arc<Project>,
    interval: Duration,
) -> tm_types::Result<tokio::task::JoinHandle<()>> {
    let dispatcher =
        crate::dispatch::build_dispatcher(&project, tokio::runtime::Handle::current(), None, None)?;
    Ok(spawn_runner_loop(project, interval, dispatcher))
}

/// [`spawn_background_runner`] for a process whose stdin/stdout aren't a human's terminal (`tm
/// mcp`, where both carry JSON-RPC): a `human_required` ticket fails its attempt through
/// [`crate::dispatch::HeadlessApprovalSink`] instead of prompting on stdout and reading stdin.
///
/// # Errors
/// As [`spawn_background_runner`].
pub fn spawn_headless_background_runner(
    project: std::sync::Arc<Project>,
    interval: Duration,
) -> tm_types::Result<tokio::task::JoinHandle<()>> {
    let dispatcher = crate::dispatch::build_dispatcher_with_human(
        &project,
        tokio::runtime::Handle::current(),
        None,
        None,
        std::sync::Arc::new(crate::dispatch::HeadlessApprovalSink),
    )?;
    Ok(spawn_runner_loop(project, interval, dispatcher))
}

fn spawn_runner_loop(
    project: std::sync::Arc<Project>,
    interval: Duration,
    dispatcher: std::sync::Arc<tm_scheduler::dispatch::ExecutorDispatcher>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut policy = tm_scheduler::SchedulingPolicy::conservative_default();
        policy.available_roles = tm_types::Role::ALL.iter().copied().collect();
        let loop_driver =
            tm_scheduler::SchedulerLoop::new(&project.store, project.clock.clone(), policy)
                .with_dispatcher(dispatcher);
        let mut timer = tokio::time::interval(interval);
        loop {
            timer.tick().await;
            match is_scheduler_paused(&project) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "background runner could not read the pause flag");
                    continue;
                }
            }
            match loop_driver.tick(scheduler_actor()) {
                Ok(events) if !events.is_empty() => {
                    tracing::debug!(events = events.len(), "background runner tick");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "background runner tick failed"),
            }
        }
    })
}

/// `tm sched pause`: stop granting new leases; admitted work continues.
pub fn sched_pause(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let paused_path = pause_flag_path(project);
    std::fs::create_dir_all(paused_path.parent().ok_or_else(|| {
        tm_types::TmError::InvalidTransition("Cannot determine parent directory".to_string())
    })?)?;
    std::fs::write(&paused_path, b"paused")?;
    renderer.status("Scheduler paused.");
    Ok(())
}

/// `tm sched resume`: undo [`sched_pause`].
pub fn sched_resume(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let paused_path = pause_flag_path(project);
    if paused_path.exists() {
        std::fs::remove_file(&paused_path)?;
    }
    renderer.status("Scheduler resumed.");
    Ok(())
}

/// Dispatch one [`LeaseCommand`].
pub fn dispatch_lease(
    cmd: &LeaseCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        LeaseCommand::List(args) => lease_list(args, project, renderer),
        LeaseCommand::Acquire(args) => lease_acquire(args, project, renderer),
        LeaseCommand::Release(args) => lease_release(args, project, renderer),
        LeaseCommand::Expire => lease_expire(project, renderer),
    }
}

/// `tm lease list`
pub fn lease_list(
    args: &LeaseListArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let view = project.store.view()?;
    let now = project.clock.now();

    let leases: Vec<LeaseSummary> = view
        .leases
        .values()
        .filter(|lease| !args.live_only || !lease.is_expired(now))
        .map(|lease| LeaseSummary {
            id: lease.id.to_string(),
            ticket: lease.ticket.to_string(),
            holder: lease.holder.to_string(),
            acquired: lease.acquired.to_string(),
            expires: lease
                .acquired
                .plus_seconds(i64::from(lease.ttl_seconds))
                .to_string(),
        })
        .collect();

    let table = Table::new(
        vec![
            "ID".to_string(),
            "Ticket".to_string(),
            "Holder".to_string(),
            "Acquired".to_string(),
            "Expires".to_string(),
        ],
        leases
            .iter()
            .map(|l| {
                vec![
                    l.id.clone(),
                    l.ticket.clone(),
                    l.holder.clone(),
                    l.acquired.clone(),
                    l.expires.clone(),
                ]
            })
            .collect(),
    );

    renderer.emit(&leases, &table.render_colored(renderer.color_enabled()))
}

/// `tm lease acquire`
pub fn lease_acquire(
    args: &LeaseAcquireArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket = TicketId::new(&args.ticket)?;
    let actor = ParticipantId::new(&args.actor)?;

    let _events = project.store.acquire_lease(
        &ticket,
        actor,
        Authority::none(),
        Vec::new(),
        60,
        project.actor.clone(),
    )?;

    renderer.status(&format!("Lease acquired for {}", ticket));
    Ok(())
}

/// `tm lease release`
pub fn lease_release(
    args: &LeaseRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let lease_id = LeaseId::new(&args.lease)?;

    let _events = project.store.release(&lease_id, project.actor.clone())?;

    renderer.status(&format!("Lease {} released.", lease_id));
    Ok(())
}

/// `tm lease expire`
pub fn lease_expire(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let _events = project.store.expire_leases()?;

    renderer.status("Expired leases swept.");
    Ok(())
}

/// How often [`run_ticket`] polls the ticket's state while its dispatched run is in flight.
const RUN_TICKET_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Ceiling on a `tm run` lease's TTL: long enough for real work, short enough that a crashed
/// executor's lease still expires and the ticket reverts to `Ready` per `tm_core::lease`'s
/// "a dead worker cannot block the project" invariant, rather than sitting `Running` forever. A
/// ticket whose own `budget.wall_seconds` is smaller still governs (via `.min` below); one whose
/// budget is `Budget::unlimited()` (`wall_seconds` near `u64::MAX`) is clamped to this instead of
/// overflowing into a multi-decade lease.
const RUN_TICKET_MAX_TTL_SECONDS: u32 = 3600;

/// How long [`run_ticket`] polls before giving up and detaching (the dispatched run keeps going
/// in the background; only the foreground CLI process stops waiting on it).
const RUN_TICKET_MAX_WAIT: Duration = Duration::from_secs(1800);

/// How long [`run_ticket`]'s `--worktree` cleanup waits for the spawned run's own completion
/// signal once the ticket has visibly left `Leased`/`Running` — see
/// [`tm_scheduler::dispatch::ExecutorDispatcher::dispatch_with_completion`]'s doc comment for why
/// that visible state change is not itself proof the run (and, specifically, everything that
/// still reads the worktree after it) is actually done. Generous but bounded: none of the work
/// this waits on (patch/artifact storage, `git stash create`, session teardown) does network I/O.
const WORKTREE_COMPLETION_GRACE: Duration = Duration::from_secs(30);

/// The exit code [`run_ticket`] uses when a foreground run is interrupted by a signal — the
/// conventional "killed by signal 2" code (128 + `SIGINT`), reused for `SIGTERM` too since this
/// process has no distinct "stopped early on purpose" code of its own and 130 is already the
/// familiar one from every other interactive CLI a person kills with ctrl-c.
const RUN_TICKET_INTERRUPTED_EXIT_CODE: i32 = 130;

/// Waits for a `SIGINT`/`SIGTERM`-shaped interrupt during [`run_ticket`]'s foreground poll loop.
/// `tokio::signal::ctrl_c` alone only ever covers `SIGINT` (and, on Windows, ctrl-break); a `tm
/// run` a scheduler or supervisor stops with a plain `SIGTERM` (no controlling terminal at all,
/// e.g. `kill <pid>`) would otherwise leave the ticket stuck `Leased`/`Running` forever once its
/// process is gone, exactly the dogfood symptom this exists to fix. `recv` can be awaited
/// repeatedly, so the same watcher also detects a *second* signal arriving while
/// [`handle_run_interrupt`]'s cleanup is still in flight.
struct InterruptWatcher {
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
}

impl InterruptWatcher {
    #[cfg(unix)]
    fn new() -> tm_types::Result<Self> {
        let sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|e| tm_types::TmError::Io(e.to_string()))?;
        Ok(InterruptWatcher { sigterm })
    }

    #[cfg(not(unix))]
    fn new() -> tm_types::Result<Self> {
        Ok(InterruptWatcher {})
    }

    #[cfg(unix)]
    async fn recv(&mut self) {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = self.sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    async fn recv(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// [`run_ticket`]'s interrupt handling: the first `SIGINT`/`SIGTERM` records an "interrupted by
/// user" attempt failure via [`tm_core::Store::record_failure`], which (see that method's own
/// doc comment) both releases every lease the ticket currently holds *and* drives the same
/// retry-vs-escalate decision any other failed attempt goes through — so an interrupted ticket
/// returns to `Ready` exactly the way a crashed or expired-lease attempt already does, rather
/// than sitting `Leased`/`Running` forever (the dogfood symptom this exists to fix: a killed `tm
/// run T-2` left `tm ticket list` showing `T-2 work active` with no way back to `Ready` short of
/// a manual lease sweep). The write runs on a blocking task specifically so a *second* signal
/// arriving before it finishes can still win the race below and exit immediately without waiting
/// on it — there is no in-process handle to cancel the dispatched agent loop itself (`tm-core`'s
/// `Executor::cancel` is an unimplemented stub everywhere today), so "cancel the agent loop" here
/// means stopping this foreground process, which is the only thing actually driving the wait;
/// the background task the scheduler dispatched keeps running until the process exit below tears
/// it down, and any write it still attempts after that races harmlessly against the ticket's
/// already-changed state the same way a lease-expiry race already does elsewhere in this file.
async fn handle_run_interrupt(
    project: &Project,
    ticket: &TicketId,
    renderer: &Renderer,
    interrupts: &mut InterruptWatcher,
) -> ! {
    renderer.note(&format!(
        "Interrupted -- recording a failure and releasing the lease for {ticket}..."
    ));
    let store = project.store.clone();
    let ticket_for_cleanup = ticket.clone();
    let actor = project.actor.clone();
    let mut cleanup = tokio::task::spawn_blocking(move || {
        store.record_failure(
            &ticket_for_cleanup,
            tm_core::FailureClass::ExecutorCrash,
            "interrupted by user".to_string(),
            actor,
        )
    });
    tokio::select! {
        result = &mut cleanup => {
            if let Ok(Err(e)) = result {
                tracing::warn!(ticket = %ticket, error = %e, "failed to record the interrupted attempt");
            }
        }
        _ = interrupts.recv() => {
            renderer.note("Interrupted again -- exiting immediately without waiting for cleanup.");
        }
    }
    std::process::exit(RUN_TICKET_INTERRUPTED_EXIT_CODE);
}

/// `tm run <ticket>`: execute one ticket to completion in the foreground, outside the scheduler
/// loop — the single-ticket path a human runs interactively or in CI. Dispatches through the
/// same [`crate::dispatch::build_dispatcher`] `tm sched run` uses (`SPEC.md` §24, audit
/// B-01/B-04), then blocks (polling, since [`tm_scheduler::ExecutorDispatcher::dispatch`] itself
/// returns as soon as the lease is acquired) until the ticket leaves `Leased`/`Running`, or until
/// [`RUN_TICKET_MAX_WAIT`] elapses.
///
/// `--worktree` (`docs/decisions/D-012-run-worktree-isolation.md`) additionally: creates a fresh
/// `git worktree` for the ticket before dispatching (`crate::worktree::create`), points the
/// dispatcher's [`BuiltinExecutor`]/[`AcpExecutor`]/snapshot `repo_root` at it instead of the main
/// checkout, and — once the run is confirmed fully finished, not just the ticket's state — either
/// removes it (a clean, forward-progress finish) or leaves it on disk for inspection (anything
/// else: a retry/escalation, a `dispatch` failure before any work started keeps nothing to
/// inspect and cleans up immediately, or a detach past [`RUN_TICKET_MAX_WAIT`] where the run may
/// still be using it).
///
/// `--record <path>` (`docs/decisions/D-028-record-replay-harness.md`) additionally dispatches
/// through [`crate::dispatch::build_dispatcher_with_recording`] instead of
/// [`crate::dispatch::build_dispatcher`], so every provider call this run makes is captured as a
/// cassette written to `path` and stored as the ticket's `ArtifactKind::Transcript` artifact once
/// the run finishes (see [`write_recording_cassette`]). Without `--record`, behavior is
/// unchanged.
///
/// `--replay <path>` (`docs/decisions/D-028-record-replay-harness.md`, `replay-cli-replay-flag`)
/// dispatches through [`crate::dispatch::build_dispatcher_with_replay`] instead: the ticket's own
/// role gets one [`tm_provider::MockProvider`] scripted with
/// [`tm_provider::MockProvider::script_from_cassette`] and no other candidate is registered, so
/// the run reaches its outcome purely from `path`'s recorded completions — no network call is
/// reachable. `--replay` conflicts with `--record` (replaying and recording the same run makes
/// no sense) and combines with `--worktree` the same way `--record` does. Once the run finishes,
/// [`report_replay_divergences`] reports how many served requests didn't hash-match what was
/// recorded at their position, the first such divergence's `seq`, and how many calls ran past the
/// cassette's last entry (exhaustion); under `--strict-replay`, either one turns the run's own
/// outcome into a hard error, even when the replayed ticket itself otherwise reached a
/// forward-progress state.
pub async fn run_ticket(
    args: &RunArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    if let Some(role) = &args.role {
        return Err(tm_types::TmError::InvalidTransition(format!(
            "--role is not supported yet (requested `{role}`); ticket executor requirements remain authoritative"
        )));
    }
    start_notification_watcher(project);

    let ticket = TicketId::new(&args.ticket)?;
    let mut view = project.store.view()?;
    let executor_requirements = view
        .tickets
        .get(&ticket)
        .ok_or_else(|| tm_types::TmError::not_found("ticket", &ticket))?
        .executor
        .clone();
    let is_draft = view.tickets[&ticket].state == tm_core::TicketState::Draft;

    // Fail fast, before any state change, lease or worktree is created, when the ticket's
    // required role has no provider that can actually serve it (a `providers.toml` candidate
    // naming a provider that was never registered/credentialed) -- see
    // `preflight_provider_or_fail`'s own doc comment. Deliberately ahead of the draft-activation
    // below: activating first and failing after would leave a real, visible state change (and a
    // `Ready` ticket a concurrent `tm sched run`/`tm serve`/the TUI could pick up and lease) from
    // a run that never got past its own preflight. Skipped when:
    // - `--replay` is set: that path serves the role from a `MockProvider` regardless of
    //   `providers.toml`, so the real fabric's configuration is beside the point.
    // - the ticket is `human_required`: `tm-scheduler`'s dispatcher routes it to `HumanExecutor`
    //   without ever consulting the role/fabric at all (`tm_scheduler::dispatch`'s
    //   `human_required` bypass), so a broken `providers.toml` candidate for its nominal role is
    //   not this ticket's problem.
    // - `acp.toml` overrides this exact role to an external ACP agent: that role is served by
    //   `AcpExecutor` instead of `BuiltinExecutor`'s fabric, so `providers.toml` never enters into
    //   it for this role either.
    let acp_role = crate::dispatch::acp_override_role(project)?;
    if provider_preflight_applies(&executor_requirements, args.replay.is_some(), acp_role) {
        preflight_provider_or_fail(project, &ticket, executor_requirements.role)?;
    }

    // Asking to run a draft is asking for it to be ready: activate it rather than refusing.
    if is_draft {
        project.store.activate(&ticket, project.actor.clone())?;
        renderer.note(&format!("Activated {ticket} (was a draft)."));
        view = project.store.view()?;
    }
    let ticket_state = view
        .tickets
        .get(&ticket)
        .ok_or_else(|| tm_types::TmError::not_found("ticket", &ticket))?;
    let failures_before = ticket_state.failures.len();

    let worktree = if args.worktree {
        Some(crate::worktree::create(project, &ticket)?)
    } else {
        None
    };
    let exec_root = worktree.as_ref().map(|w| w.path.as_path());

    let (step_tx, mut step_rx) = tokio::sync::mpsc::unbounded_channel();
    let cassette_sink: crate::agent::CassetteSink = Arc::new(std::sync::Mutex::new(Vec::new()));
    // `Some((mock, recorded))` under `--replay`: `recorded` is the cassette's own entry count,
    // read before the cassette is moved into the dispatcher, so `report_replay_divergences` can
    // tell an exhausted replay (more calls served than `recorded`) from a merely divergent one.
    let mut replay_mock: Option<(Arc<tm_provider::MockProvider>, usize)> = None;
    let dispatcher = if let Some(replay_path) = &args.replay {
        let cassette = Cassette::read_jsonl(replay_path).map_err(|e| {
            tm_types::TmError::Io(format!(
                "couldn't read the cassette at {}: {e}",
                replay_path.display()
            ))
        })?;
        let recorded = cassette.entries.len();
        let (dispatcher, mock) = crate::dispatch::build_dispatcher_with_replay(
            project,
            tokio::runtime::Handle::current(),
            exec_root,
            Some(step_tx),
            ticket_state.executor.role,
            cassette,
        )?;
        replay_mock = Some((mock, recorded));
        dispatcher
    } else {
        match &args.record {
            Some(path) => {
                // Opened (and its header durably written) before the run is even dispatched, so
                // a kill in the window between this and the first completion still leaves a
                // valid, header-only cassette rather than no file at all.
                let writer = tm_provider::cassette::CassetteWriter::create(
                    path,
                    &CassetteHeader {
                        format_version: CASSETTE_FORMAT_VERSION,
                        harness_epoch: Some(current_harness_epoch(project)),
                        recorded_at: project.clock.now(),
                    },
                )
                .map_err(|e| {
                    tm_types::TmError::Io(format!(
                        "couldn't open the cassette at {}: {e}",
                        path.display()
                    ))
                })?;
                crate::dispatch::build_dispatcher_with_recording(
                    project,
                    tokio::runtime::Handle::current(),
                    exec_root,
                    Some(step_tx),
                    ticket_state.executor.role,
                    cassette_sink.clone(),
                    Some(Arc::new(std::sync::Mutex::new(writer))),
                )?
            }
            None => crate::dispatch::build_dispatcher(
                project,
                tokio::runtime::Handle::current(),
                exec_root,
                Some(step_tx),
            )?,
        }
    };
    let ttl_seconds = u32::try_from(ticket_state.budget.wall_seconds)
        .unwrap_or(u32::MAX)
        .clamp(60, RUN_TICKET_MAX_TTL_SECONDS);

    let completion_rx = if worktree.is_some() {
        match dispatcher.dispatch_with_completion(
            &ticket,
            ticket_state,
            ttl_seconds,
            project.actor.clone(),
        ) {
            Ok((_events, rx)) => Some(rx),
            Err(e) => {
                // Nothing ever ran in the worktree: dispatch failed synchronously (no executor
                // for the role, a lease race, ...) before any work started, so there is nothing
                // to inspect — remove it rather than leaving an empty, never-touched checkout
                // behind.
                if let Some(w) = worktree {
                    w.cleanup();
                }
                return Err(active_lease_guidance(project, &ticket, e));
            }
        }
    } else {
        dispatcher
            .dispatch(&ticket, ticket_state, ttl_seconds, project.actor.clone())
            .map_err(|e| active_lease_guidance(project, &ticket, e))?;
        None
    };
    renderer.note(&format!("Ticket {ticket} dispatched for execution."));

    let deadline = project
        .clock
        .now()
        .plus_seconds(RUN_TICKET_MAX_WAIT.as_secs() as i64);
    let mut detached = false;
    let mut final_state = None;
    let mut outcome = None;
    let mut interrupts = InterruptWatcher::new()?;
    loop {
        if project.clock.now() >= deadline {
            renderer.note(&format!(
                "Ticket {ticket} is still running after {}s; detaching (the run continues in the background).",
                RUN_TICKET_MAX_WAIT.as_secs()
            ));
            detached = true;
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(RUN_TICKET_POLL_INTERVAL) => {}
            _ = interrupts.recv() => {
                handle_run_interrupt(project, &ticket, renderer, &mut interrupts).await;
            }
        }
        // Live progress: each step the run has taken since the last poll, as `tm -p` prints it.
        while let Ok(step) = step_rx.try_recv() {
            renderer.note(&crate::agent::format_step(&step));
        }
        let view = project.store.view()?;
        let Some(t) = view.tickets.get(&ticket) else {
            break;
        };
        if !matches!(
            t.state,
            tm_core::ticket::TicketState::Leased | tm_core::ticket::TicketState::Running
        ) {
            final_state = Some(t.state);
            outcome = Some(run_outcome(t, failures_before));
            break;
        }
    }
    while let Ok(step) = step_rx.try_recv() {
        renderer.note(&crate::agent::format_step(&step));
    }

    if let Some(path) = &args.record {
        write_recording_cassette(project, &ticket, path, &cassette_sink, renderer)?;
    }

    let mut replay_error = None;
    if let Some((mock, recorded)) = &replay_mock {
        replay_error =
            report_replay_divergences(&ticket, mock, *recorded, args.strict_replay, renderer)?;
    }

    if let Some(worktree) = worktree {
        finish_worktree_run(
            worktree,
            detached,
            final_state,
            completion_rx,
            renderer,
            &ticket,
        )
        .await;
    }

    match outcome {
        Some(Ok(message)) => {
            renderer.note(&message);
            match replay_error {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }
        Some(Err(err)) => {
            // The dispatcher has already recorded the failure and applied the ticket's retry
            // policy. For no-submit turns, keep this foreground invocation alive until that
            // scheduled retry is due when the ticket still has retry/budget capacity.
            let retry = project
                .store
                .view()
                .ok()
                .and_then(|view| view.tickets.get(&ticket).cloned())
                .and_then(|current| {
                    let no_submit = current.failures.last().is_some_and(|failure| {
                        failure
                            .detail
                            .to_ascii_lowercase()
                            .contains("ended turn without submitting")
                    });
                    if !no_submit || current.state != tm_core::TicketState::Ready {
                        return None;
                    }
                    let decision = tm_scheduler::retry::decide_retry(
                        &current,
                        current.failures.last()?.class,
                        project.clock.now(),
                    );
                    match decision.outcome {
                        tm_scheduler::RetryOutcome::Retry { after } => {
                            Some(after.seconds_since(project.clock.now()).max(0) as u64)
                        }
                        tm_scheduler::RetryOutcome::Escalate(_) => None,
                    }
                });
            if let Some(delay) = retry {
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                // A second invocation uses the same ticket, options and project. Its own
                // failure count bounds this recursion through the ticket retry policy.
                Box::pin(run_ticket(args, project, renderer)).await
            } else {
                Err(err)
            }
        }
        None => match replay_error {
            Some(err) => Err(err),
            None => Ok(()),
        },
    }
}

/// Turn a lease-race conflict into a useful recovery instruction while preserving other errors.
fn active_lease_guidance(
    project: &Project,
    ticket: &TicketId,
    error: tm_types::TmError,
) -> tm_types::TmError {
    let Ok(view) = project.store.view() else {
        return error;
    };
    let Some(lease) = view
        .leases
        .values()
        .find(|lease| lease.ticket == *ticket && !lease.is_expired(project.clock.now()))
    else {
        return error;
    };
    let session = project
        .store
        .events_for_subject(&tm_types::Id::from(ticket.clone()))
        .ok()
        .and_then(|events| {
            events.iter().rev().find_map(|event| {
                (event.kind == tm_events::EventKind::TicketLeased && event.actor == lease.holder)
                    .then(|| event.session.clone())
                    .flatten()
            })
        });
    let recovery = match session {
        Some(session) => format!(
            "The ticket is being worked by {}. Inspect it with `tm ticket show {ticket}`; resume session {session} with `tm --resume {session}`.",
            lease.holder
        ),
        None => format!(
            "The ticket is being worked by {}. Inspect its progress with `tm ticket show {ticket}`.",
            lease.holder
        ),
    };
    match error {
        tm_types::TmError::Conflict(message) => {
            tm_types::TmError::Conflict(format!("{message}. {recovery}"))
        }
        other => other,
    }
}

/// Whether [`run_ticket`]'s provider preflight should actually run for a ticket with
/// `requirements`, given whether this run is a `--replay` and whichever role (if any)
/// `acp.toml` overrides to an external ACP agent. `false` in exactly the three cases where a
/// broken `providers.toml` candidate for `requirements.role` is not this ticket's problem at
/// all -- see the call site's own comment for why each one is real, not a hypothetical:
/// `--replay`'s role is served by a `MockProvider` regardless of configuration; a
/// `human_required` ticket is routed to `HumanExecutor` without ever consulting the role/fabric;
/// and a role `acp.toml` overrides is served by `AcpExecutor` instead of `BuiltinExecutor`'s
/// fabric.
fn provider_preflight_applies(
    requirements: &tm_core::ExecutorRequirements,
    is_replay: bool,
    acp_override_role: Option<tm_types::Role>,
) -> bool {
    !is_replay && !requirements.human_required && acp_override_role != Some(requirements.role)
}

/// Validates `role`'s effective provider candidates against what is actually registered and
/// credentialed *before* [`run_ticket`] leases or dispatches anything at all
/// (`t20260925-1314-sindresorhus-ky-878-provider-retry-guidance`). Before this check existed, a
/// `providers.toml` candidate naming a provider that was never registered (no credential, never
/// wired up) only surfaced after `tm run` had already leased the ticket and dispatched an agent
/// turn that failed immediately -- one whole lease cycle (`no worker attached; lease lapsed`)
/// burned just to reach the same "provider not registered: <name>" diagnosis this function
/// reaches for free, before any of that happens. Builds its own short-lived [`tm_provider::Fabric`]
/// via [`crate::agent::build_fabric_for_project`] (the same one [`crate::dispatch::build_dispatcher`]
/// builds moments later for the real run) purely to ask [`tm_provider::Fabric::preflight_role`]
/// this question -- registering providers does no network I/O, so building it twice costs
/// nothing observable.
fn preflight_provider_or_fail(
    project: &Project,
    ticket: &TicketId,
    role: tm_types::Role,
) -> tm_types::Result<()> {
    let fabric = crate::agent::build_fabric_for_project(project, project.clock.clone())?;
    preflight_fabric_or_fail(&fabric, ticket, role)
}

/// The pure half of [`preflight_provider_or_fail`]: asks an already-built [`tm_provider::Fabric`]
/// (real, or a hand-built one in a test) whether `role` can be served at all, and shapes its
/// `Err` into the same [`tm_types::TmError::TurnFailed`] `run_ticket` returns for every other
/// failed-attempt reason. Split out so a test can check this wiring against a `Fabric` it built
/// by hand from a known role table, instead of going through [`crate::agent::build_fabric_for_project`]'s
/// real environment-credential autodetection -- which would make the test's outcome depend on
/// whatever `DEVPASS_*`/`ANTHROPIC_API_KEY`/etc. happen to be set in the process running the
/// test, a real, already-seen source of cross-test flakiness in this same file (see
/// `record_test_env_lock`'s doc comment on the sibling `--record`/`--replay` tests).
fn preflight_fabric_or_fail(
    fabric: &tm_provider::Fabric,
    ticket: &TicketId,
    role: tm_types::Role,
) -> tm_types::Result<()> {
    fabric
        .preflight_role(role)
        .map_err(|msg| tm_types::TmError::TurnFailed(format!("Ticket {ticket}: {msg}")))
}

/// `tm run <ticket> --record <path>`'s tail: writes everything `cassette_sink` accumulated
/// during the run to `path` as a cassette (`docs/decisions/D-028-record-replay-harness.md`), then
/// stores those same bytes as `ticket`'s `ArtifactKind::Transcript` artifact — the first real use
/// of that previously-declared-but-unused kind. `harness_epoch` is read the same way
/// `tm-scheduler`'s own dispatch computes it (the last promoted epoch, or the genesis epoch `0`
/// if none has promoted yet), so a cassette recorded this way carries the same epoch a scheduler-
/// dispatched run would have stamped on it.
/// The last promoted harness epoch, or the genesis epoch `0` if none has promoted yet — the same
/// rule `tm-scheduler`'s own dispatch uses, so any cassette header stamped with this carries the
/// epoch a scheduler-dispatched run would have. Shared by [`write_recording_cassette`]'s final
/// write and the incremental [`tm_provider::cassette::CassetteWriter`] created before the run
/// even starts, so both ever write the same epoch for one `tm run --record` invocation.
fn current_harness_epoch(project: &Project) -> u64 {
    project
        .store
        .harness_epochs()
        .ok()
        .and_then(|epochs| epochs.last().map(|epoch| epoch.epoch))
        .unwrap_or(0)
}

fn write_recording_cassette(
    project: &Project,
    ticket: &TicketId,
    path: &std::path::Path,
    cassette_sink: &crate::agent::CassetteSink,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let harness_epoch = current_harness_epoch(project);
    let entries = cassette_sink
        .lock()
        .expect("cassette sink mutex poisoned")
        .clone();
    let entry_count = entries.len();
    let cassette = Cassette {
        header: CassetteHeader {
            format_version: CASSETTE_FORMAT_VERSION,
            harness_epoch: Some(harness_epoch),
            recorded_at: project.clock.now(),
        },
        entries,
    };
    cassette
        .write_jsonl(path)
        .map_err(|e| tm_types::TmError::Io(e.to_string()))?;
    let bytes = std::fs::read(path)?;
    project.store.store_artifact(
        ArtifactKind::Transcript,
        "application/x-ndjson".to_string(),
        bytes,
        serde_json::json!({ "source": "tm run --record", "entries": entry_count }),
        Some(ticket.clone()),
        project.actor.clone(),
    )?;
    renderer.note(&format!(
        "Recorded {entry_count} provider call(s) to {} (stored as a Transcript artifact on {ticket}).",
        path.display()
    ));
    Ok(())
}

/// The divergence summary `tm run <ticket> --replay <path>` reports once the run finishes —
/// [`Renderer::emit`]'s JSON payload under `--json`.
#[derive(Debug, Clone, Serialize)]
struct ReplayReport {
    /// The ticket that was replayed.
    ticket: String,
    /// How many served requests didn't hash-match what the cassette recorded at their position.
    divergence_count: usize,
    /// The recorded [`tm_provider::CassetteEntry::seq`] of the first divergence, if any.
    first_divergent_seq: Option<u64>,
    /// How many calls the run made past the cassette's last recorded entry — each one exhausted
    /// the ordered replay and fell through to `ProviderError::Unscripted`.
    unscripted_calls: usize,
}

/// `tm run <ticket> --replay <path>`'s tail: reports how many requests `mock` served diverged
/// from what the cassette recorded (`MockProvider::divergences`), and how many ran past the
/// cassette's `recorded` entries into unscripted territory (`mock.call_log().len() - recorded`,
/// since every `complete()` call is logged whether it was served from the cassette or fell
/// through to `Unscripted`), as JSON under `--json` (`ReplayReport`) or a one-line human summary
/// otherwise.
///
/// Returns `Some(TmError)` when `strict` is set and either count is nonzero —
/// `run_ticket` turns that into a hard error even when the replayed ticket itself otherwise
/// reached a forward-progress state, per `docs/decisions/D-028-record-replay-harness.md`'s
/// `--strict-replay` contract ("any divergence or exhaustion is a hard error"). Without `strict`,
/// both are reported but never fail this function's own return — though an exhausted call still
/// fails on its own, since `MockProvider` returns a real `Err` for it either way; only a
/// *diverging* (still-served) call needs `strict` to turn into a failure at all.
fn report_replay_divergences(
    ticket: &TicketId,
    mock: &tm_provider::MockProvider,
    recorded: usize,
    strict: bool,
    renderer: &Renderer,
) -> tm_types::Result<Option<tm_types::TmError>> {
    let divergences = mock.divergences();
    let unscripted_calls = mock.call_log().len().saturating_sub(recorded);
    let report = ReplayReport {
        ticket: ticket.to_string(),
        divergence_count: divergences.len(),
        first_divergent_seq: divergences.first().map(|d| d.seq),
        unscripted_calls,
    };
    let human = if divergences.is_empty() && unscripted_calls == 0 {
        format!("Replay of {ticket} matched the recording: no divergences.")
    } else {
        let mut parts = Vec::new();
        if !divergences.is_empty() {
            parts.push(format!(
                "differed from the recording at {} call(s) (first at recorded call {})",
                report.divergence_count,
                report.first_divergent_seq.unwrap_or_default()
            ));
        }
        if unscripted_calls > 0 {
            parts.push(format!(
                "ran {unscripted_calls} call(s) past the end of the recording"
            ));
        }
        format!(
            "Replay of {ticket} {}. Re-record with `tm run {ticket} --record <path>`, or drop \
             `--strict-replay` to continue anyway.",
            parts.join("; ")
        )
    };
    renderer.emit(&report, &human)?;
    if strict && (!divergences.is_empty() || unscripted_calls > 0) {
        return Ok(Some(tm_types::TmError::Conflict(format!(
            "--strict-replay: replay of {ticket} differed from the recording at {} call(s) and \
             ran {} call(s) past its end",
            report.divergence_count, unscripted_calls
        ))));
    }
    Ok(None)
}

/// How one `tm run` attempt ended, read from the ticket once it left `Leased`/`Running`: a
/// forward-progress state is success; anything else is reported as the failure it recorded (with
/// what happens next), as the error `tm run` exits with — never as a quiet "finished".
fn run_outcome(ticket: &tm_core::Ticket, failures_before: usize) -> tm_types::Result<String> {
    if worktree_run_reached_success(ticket.state) {
        return Ok(if ticket.state == tm_core::TicketState::Submitted {
            format!(
                "Ticket {id} submitted its work. Review it (tm ticket show {id}), then \
                 `tm ticket accept {id}` or `tm ticket reject {id} --reason \"...\"`.",
                id = ticket.id
            )
        } else {
            let state = state_label(ticket.state);
            format!("Ticket {} submitted its work ({state}).", ticket.id)
        });
    }
    let failure = ticket.failures.get(failures_before..).and_then(<[_]>::last);
    let did_not_submit = failure.is_some_and(|f| {
        f.detail
            .to_ascii_lowercase()
            .contains("ended turn without submitting")
    });
    let reason = failure
        .map(|f| {
            let class_desc = failure_class_description(f.class);
            if did_not_submit {
                f.detail.clone()
            } else {
                format!("{class_desc}: {}", f.detail)
            }
        })
        .unwrap_or_else(|| "unknown failure".to_string());
    // A `ProviderUnavailable` failure whose detail names an unregistered provider is a
    // configuration problem, not a transient one (unlike, say, "provider at capacity"): the
    // preflight check in `preflight_provider_or_fail` is meant to catch this before a lease is
    // ever granted, but a provider that goes missing/uncredentialed *between* that check and this
    // attempt's own call (or a role change mid-run) can still reach this path. Blindly suggesting
    // `tm run <ticket>` again would fail identically every time, so point at the real fix instead.
    let is_unregistered_provider_failure = failure.is_some_and(|f| {
        f.class == tm_core::FailureClass::ProviderUnavailable
            && f.detail.contains("provider not registered")
    });
    let next = match ticket.state {
        tm_core::TicketState::Ready | tm_core::TicketState::Blocked
            if is_unregistered_provider_failure =>
        {
            "This is a provider configuration problem, not a transient one: check providers.toml's \
             candidates for this ticket's role and run `tm provider list` to see what's actually \
             registered/credentialed, then fix the candidate (or its credential) -- rerunning \
             unchanged will fail identically."
                .to_string()
        }
        tm_core::TicketState::Ready | tm_core::TicketState::Blocked if did_not_submit => {
            format!(
                "Inspect the saved attempt with `tm ticket show {}`. To continue its work, resume the saved session with `tm --resume <session>` and ask it to focus on the remaining change, run the relevant checks, and submit evidence; avoid rerunning the unchanged ticket.",
                ticket.id
            )
        }
        tm_core::TicketState::Ready | tm_core::TicketState::Blocked => {
            format!("Run `tm run {}` again to retry.", ticket.id)
        }
        tm_core::TicketState::Escalated if is_unregistered_provider_failure => {
            "This is a provider configuration problem, not a transient one: fix providers.toml's \
             candidates for this ticket's role (check with `tm provider list`) before retrying -- \
             `tm ticket retry` alone will fail identically."
                .to_string()
        }
        tm_core::TicketState::Escalated if did_not_submit => {
            format!(
                "Inspect the saved attempt with `tm ticket show {}`. To continue its work, resume the saved session with `tm --resume <session>` and ask it to focus on the remaining change, run the relevant checks, and submit evidence; avoid retrying the unchanged ticket.",
                ticket.id
            )
        }
        tm_core::TicketState::Escalated => {
            format!("Run `tm ticket retry {}` to try again.", ticket.id)
        }
        _ => "".to_string(),
    };
    let message = if did_not_submit {
        let resume = if next.is_empty() {
            String::new()
        } else {
            format!(" {next}")
        };
        format!(
            "Ticket {}: no patch or evidence was submitted (this was not a test failure). Failure: {}.{}",
            ticket.id,
            reason,
            resume
        )
    } else if next.is_empty() {
        format!("Ticket {}: {}.", ticket.id, reason)
    } else {
        format!("Ticket {}: {}. {}", ticket.id, reason, next)
    };
    Err(tm_types::TmError::TurnFailed(message))
}

/// Plain-English description of a failure class for user-facing messages.
pub(crate) fn failure_class_description(class: tm_core::FailureClass) -> &'static str {
    use tm_core::FailureClass;
    match class {
        FailureClass::ExecutorCrash => "executor crashed",
        FailureClass::VerificationFailed => "verification failed",
        FailureClass::AuditRejected => "audit rejected the result",
        FailureClass::ProviderUnavailable => "provider was unavailable",
        FailureClass::BudgetExhausted => "budget exhausted",
        FailureClass::AuthorityDenied => "authority denied",
        FailureClass::ResourceConflict => "resource conflict",
        FailureClass::Other => "something went wrong",
    }
}

/// Plain-English description of an [`tm_scheduler::EscalationReason`] for `tm sched plan`'s
/// human output, purpose-built so it never `{:?}`-debug-formats the enum directly.
fn escalation_reason_description(reason: tm_scheduler::EscalationReason) -> String {
    use tm_scheduler::EscalationReason;
    match reason {
        EscalationReason::AttemptsExhausted => "ran out of retry attempts".to_string(),
        EscalationReason::TokensExhausted => "ran out of token budget".to_string(),
        EscalationReason::DollarsExhausted => "ran out of dollar budget".to_string(),
        EscalationReason::WallSecondsExhausted => "ran out of wall-clock time budget".to_string(),
        EscalationReason::CycleIterationsExhausted => "ran out of cycle iterations".to_string(),
        EscalationReason::NonRetryable(class) => {
            format!("won't retry: {}", failure_class_description(class))
        }
    }
}

/// A ticket state's canonical lowercase name (`"ready"`, `"submitted"`, ...).
fn state_label(state: tm_core::TicketState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{state:?}").to_ascii_lowercase())
}

/// The success set [`finish_worktree_run`] treats as "confirmed forward progress" — a submitted
/// ticket, or anything further along its verification pipeline. Everything else (`Ready`/
/// `Blocked` after a `RetryScheduled`, `Escalated`/`Cancelled` after `RetryExhausted`, `Rework`,
/// `Replan`, or a poll that never observed a terminal state at all) is treated as "keep it" —
/// deliberately the conservative default, not an exhaustive "which states truly indicate
/// failure" enumeration (see `docs/decisions/D-012-run-worktree-isolation.md` for why).
fn worktree_run_reached_success(state: tm_core::ticket::TicketState) -> bool {
    use tm_core::ticket::TicketState;
    matches!(
        state,
        TicketState::Submitted
            | TicketState::Verifying
            | TicketState::Auditing
            | TicketState::Closed
    )
}

/// [`run_ticket`]'s `--worktree` epilogue: decide whether to remove `worktree` or keep it for
/// inspection, and tell the user which happened (and, if kept, how to remove it by hand).
async fn finish_worktree_run(
    worktree: crate::worktree::TicketWorktree,
    detached: bool,
    final_state: Option<tm_core::ticket::TicketState>,
    completion_rx: Option<tokio::sync::oneshot::Receiver<()>>,
    renderer: &Renderer,
    ticket: &TicketId,
) {
    let run_confirmed_finished = if detached {
        false
    } else {
        match completion_rx {
            Some(rx) => matches!(
                tokio::time::timeout(WORKTREE_COMPLETION_GRACE, rx).await,
                Ok(Ok(()))
            ),
            None => false,
        }
    };

    let succeeded = final_state.is_some_and(worktree_run_reached_success);

    if run_confirmed_finished && succeeded {
        let path = worktree.path.clone();
        let branch = worktree.branch.clone();
        worktree.cleanup();
        renderer.note(&format!(
            "Removed worktree for {ticket} at {} (the checkout only — its commits are still on \
             branch {branch})",
            path.display()
        ));
    } else {
        renderer.note(&format!(
            "Kept worktree for {ticket} at {} (branch {}) for inspection — remove with \
             `git worktree remove --force {}` from the main checkout once you're done",
            worktree.path.display(),
            worktree.branch,
            worktree.path.display()
        ));
    }
}

/// Convert a scheduler action to a summary for display.
fn action_to_summary(action: &tm_scheduler::SchedulerAction) -> ActionSummary {
    use tm_scheduler::SchedulerAction;
    match action {
        SchedulerAction::MarkReady(ticket) => ActionSummary {
            kind: "MarkReady".to_string(),
            target: ticket.to_string(),
        },
        SchedulerAction::MarkBlocked(ticket) => ActionSummary {
            kind: "MarkBlocked".to_string(),
            target: ticket.to_string(),
        },
        SchedulerAction::Lease {
            ticket,
            executor,
            ttl_seconds,
        } => ActionSummary {
            kind: "Lease".to_string(),
            target: format!("{} to {} for {} secs", ticket, executor, ttl_seconds),
        },
        SchedulerAction::ExpireLease(lease) => ActionSummary {
            kind: "ExpireLease".to_string(),
            target: lease.to_string(),
        },
        SchedulerAction::Retry { ticket, after } => ActionSummary {
            kind: "Retry".to_string(),
            target: format!("{} after {}", ticket, after),
        },
        SchedulerAction::Escalate { ticket, reason } => ActionSummary {
            kind: "Escalate".to_string(),
            target: format!("{}: {}", ticket, escalation_reason_description(*reason)),
        },
        SchedulerAction::OpenRecovery(ticket) => ActionSummary {
            kind: "OpenRecovery".to_string(),
            target: ticket.to_string(),
        },
        SchedulerAction::CloseMilestone(milestone) => ActionSummary {
            kind: "CloseMilestone".to_string(),
            target: milestone.to_string(),
        },
        SchedulerAction::Noop => ActionSummary {
            kind: "Noop".to_string(),
            target: String::new(),
        },
    }
}

/// Convert a scheduler loop event to a summary for display.
fn event_to_summary(event: &tm_scheduler::SchedulerLoopEvent) -> EventSummary {
    use tm_scheduler::SchedulerLoopEvent;
    match event {
        SchedulerLoopEvent::Ticked { at: _, planned } => EventSummary {
            kind: "Scheduler ticked".to_string(),
            detail: match planned {
                0 => "No work to do right now".to_string(),
                1 => "1 action queued".to_string(),
                n => format!("{n} actions queued"),
            },
        },
        SchedulerLoopEvent::Applied { action, events } => {
            let summary = action_to_summary(action);
            EventSummary {
                kind: "Applied".to_string(),
                detail: format!(
                    "{}: {} ({} events)",
                    summary.kind,
                    summary.target,
                    events.len()
                ),
            }
        }
        SchedulerLoopEvent::Failed { action, error } => {
            let summary = action_to_summary(action);
            EventSummary {
                kind: "Failed".to_string(),
                detail: format!("{}: {} - {}", summary.kind, summary.target, error),
            }
        }
    }
}

/// Get the path to the pause flag file.
fn pause_flag_path(project: &Project) -> PathBuf {
    project.state_dir.join("sched.paused")
}

/// Check whether the scheduler is paused.
fn is_scheduler_paused(project: &Project) -> tm_types::Result<bool> {
    Ok(pause_flag_path(project).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_summary_mark_ready() {
        let ticket = TicketId::new("T-1").unwrap();
        let action = tm_scheduler::SchedulerAction::MarkReady(ticket.clone());
        let summary = action_to_summary(&action);
        assert_eq!(summary.kind, "MarkReady");
        assert_eq!(summary.target, "T-1");
    }

    #[test]
    fn action_summary_mark_blocked() {
        let ticket = TicketId::new("T-2").unwrap();
        let action = tm_scheduler::SchedulerAction::MarkBlocked(ticket.clone());
        let summary = action_to_summary(&action);
        assert_eq!(summary.kind, "MarkBlocked");
        assert_eq!(summary.target, "T-2");
    }

    #[test]
    fn action_summary_escalate_uses_plain_english_reason_not_debug_output() {
        // critic-format-debug-strings-cleanup: `tm sched plan`'s human output must not
        // `{:?}`-debug-format an `EscalationReason` (e.g. "AttemptsExhausted").
        let ticket = TicketId::new("T-3").unwrap();
        let action = tm_scheduler::SchedulerAction::Escalate {
            ticket: ticket.clone(),
            reason: tm_scheduler::EscalationReason::AttemptsExhausted,
        };
        let summary = action_to_summary(&action);
        assert_eq!(summary.target, "T-3: ran out of retry attempts");
        assert!(!summary.target.contains("AttemptsExhausted"));
    }

    #[test]
    fn action_summary_noop() {
        let action = tm_scheduler::SchedulerAction::Noop;
        let summary = action_to_summary(&action);
        assert_eq!(summary.kind, "Noop");
    }

    #[test]
    fn event_summary_ticked() {
        let at = tm_types::Timestamp::EPOCH;
        let event = tm_scheduler::SchedulerLoopEvent::Ticked { at, planned: 3 };
        let summary = event_to_summary(&event);
        assert_eq!(summary.kind, "Scheduler ticked");
        assert_eq!(summary.detail, "3 actions queued");
    }

    #[test]
    fn event_summary_ticked_no_work() {
        // s1-events-sched-copy-and-quiet: 0 planned actions reads as plain prose, not
        // "0 actions planned at <ISO timestamp>".
        let at = tm_types::Timestamp::EPOCH;
        let event = tm_scheduler::SchedulerLoopEvent::Ticked { at, planned: 0 };
        let summary = event_to_summary(&event);
        assert_eq!(summary.kind, "Scheduler ticked");
        assert_eq!(summary.detail, "No work to do right now");
        assert_eq!(
            format!("{}: {}", summary.kind, summary.detail),
            "Scheduler ticked: No work to do right now"
        );
    }

    #[test]
    fn event_summary_ticked_one_action() {
        let at = tm_types::Timestamp::EPOCH;
        let event = tm_scheduler::SchedulerLoopEvent::Ticked { at, planned: 1 };
        let summary = event_to_summary(&event);
        assert_eq!(summary.detail, "1 action queued");
    }

    #[test]
    fn event_summary_applied() {
        let ticket = TicketId::new("T-1").unwrap();
        let action = tm_scheduler::SchedulerAction::MarkReady(ticket.clone());
        let event = tm_scheduler::SchedulerLoopEvent::Applied {
            action: action.clone(),
            events: Vec::new(),
        };
        let summary = event_to_summary(&event);
        assert_eq!(summary.kind, "Applied");
        assert!(summary.detail.contains("MarkReady"));
        assert!(summary.detail.contains("0 events"));
    }

    #[test]
    fn event_summary_failed() {
        let ticket = TicketId::new("T-1").unwrap();
        let action = tm_scheduler::SchedulerAction::MarkReady(ticket.clone());
        let event = tm_scheduler::SchedulerLoopEvent::Failed {
            action: action.clone(),
            error: "something went wrong".to_string(),
        };
        let summary = event_to_summary(&event);
        assert_eq!(summary.kind, "Failed");
        assert!(summary.detail.contains("MarkReady"));
        assert!(summary.detail.contains("something went wrong"));
    }

    /// A provider that holds its fabric slot for a while before answering (text only, so every
    /// attempt ends "without submitting"), counting calls and the most it ever had in flight.
    struct SlowProvider {
        clock: Arc<dyn tm_types::Clock>,
        hold: Duration,
        calls: std::sync::atomic::AtomicUsize,
        in_flight: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl tm_provider::Provider for SlowProvider {
        fn id(&self) -> &str {
            "slow"
        }

        async fn complete(
            &self,
            _req: tm_provider::CompletionRequest,
        ) -> Result<tm_provider::Completion, tm_provider::ProviderError> {
            use std::sync::atomic::Ordering::SeqCst;
            self.calls.fetch_add(1, SeqCst);
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            tokio::time::sleep(self.hold).await;
            self.in_flight.fetch_sub(1, SeqCst);
            Ok(tm_provider::Completion {
                model: tm_provider::ModelId::new("slow", "m1"),
                candidates: vec![tm_provider::Candidate {
                    content: vec![tm_provider::ContentBlock::Text {
                        text: "looked, not done".to_string(),
                    }],
                    stop_reason: tm_provider::StopReason::EndTurn,
                }],
                usage: tm_provider::Usage {
                    input_tokens: 10,
                    output_tokens: 10,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                latency: Duration::from_millis(0),
                received_at: self.clock.now(),
            })
        }

        async fn embed(
            &self,
            _req: tm_provider::EmbedRequest,
        ) -> Result<tm_provider::Embeddings, tm_provider::ProviderError> {
            Err(tm_provider::ProviderError::InvalidRequest(
                "not used in this test".to_string(),
            ))
        }
    }

    /// A project in `dir` on a fixed clock, so a failed attempt's retry delay never elapses and
    /// each ticket gets exactly the attempts the first tick starts.
    fn test_project(dir: &std::path::Path) -> Arc<Project> {
        let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::FixedClock::epoch());
        let ids: Arc<dyn tm_types::IdSource> = Arc::new(tm_types::CounterIds::new());
        let store = Arc::new(
            tm_core::Store::open_with(dir, clock.clone(), ids.clone()).expect("open store"),
        );
        Arc::new(Project::for_test(dir, store, clock, ids))
    }

    /// A fabric whose only `coder.fast` candidate is `provider`, one request at a time.
    fn one_slot_fabric(project: &Project, provider: Arc<SlowProvider>) -> Arc<tm_provider::Fabric> {
        let table = tm_provider::RoleTable::parse(
            "[coder_fast]\ncandidates = [{ provider = \"slow\", model = \"m1\", max_concurrency = 1 }]\n",
        )
        .expect("role table parses");
        let fabric = tm_provider::Fabric::new(table, project.clock.clone());
        fabric.register_provider(provider);
        Arc::new(fabric)
    }

    fn slow_provider(project: &Project, hold: Duration) -> Arc<SlowProvider> {
        Arc::new(SlowProvider {
            clock: project.clock.clone(),
            hold,
            calls: Default::default(),
            in_flight: Default::default(),
            peak: Default::default(),
        })
    }

    fn queued_ticket(project: &Project, objective: &str) -> TicketId {
        let ticket =
            crate::tickets::create_worker_ticket(project, objective).expect("create ticket");
        project
            .store
            .activate(&ticket, project.actor.clone())
            .expect("activate");
        ticket
    }

    /// Poll until every one of `tickets` has at least one recorded failure (each attempt here
    /// ends in one), then return their current state.
    async fn wait_for_first_failures(
        project: &Project,
        tickets: &[&TicketId],
    ) -> Vec<tm_core::Ticket> {
        for _ in 0..500 {
            let view = project.store.view().expect("view");
            let now: Vec<_> = tickets.iter().map(|t| view.tickets[*t].clone()).collect();
            if now.iter().all(|t| !t.failures.is_empty()) {
                return now;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the tickets' first attempts never finished");
    }

    /// The bug: two tickets dispatched in the same tick against a provider that serves one
    /// request at a time. The second request found the slot taken, the fabric said "wait", and
    /// that was recorded as a `ProviderUnavailable` failure that spent one of the ticket's
    /// attempts. Waiting for capacity is not a failed attempt (D-023).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_capacity_wait_does_not_spend_either_tickets_attempt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let first = queued_ticket(&project, "write the changelog");
        let second = queued_ticket(&project, "write the release notes");

        let provider = slow_provider(&project, Duration::from_millis(300));
        let fabric = one_slot_fabric(&project, provider.clone());
        let dispatcher = crate::dispatch::build_dispatcher_with_fabric(
            &project,
            tokio::runtime::Handle::current(),
            None,
            None,
            fabric,
            std::sync::Arc::new(crate::dispatch::HeadlessApprovalSink),
        )
        .expect("dispatcher");
        // One tick of the scheduler the in-process runner drives, with the same dispatcher and
        // policy: it leases both tickets together and starts exactly one attempt of each (a
        // runner would keep ticking and start the retries too, which is not what this checks).
        let mut policy = tm_scheduler::SchedulingPolicy::conservative_default();
        policy.available_roles = tm_types::Role::ALL.iter().copied().collect();
        let events =
            tm_scheduler::SchedulerLoop::new(&project.store, project.clock.clone(), policy)
                .with_dispatcher(dispatcher)
                .tick(scheduler_actor())
                .expect("tick");
        let leased = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    tm_scheduler::SchedulerLoopEvent::Applied {
                        action: tm_scheduler::SchedulerAction::Lease { .. },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            leased, 2,
            "both tickets dispatched in the same tick: {events:?}"
        );
        let tickets = wait_for_first_failures(&project, &[&first, &second]).await;

        use std::sync::atomic::Ordering::SeqCst;
        // Each attempt makes one or more calls (the loop may nudge a text-only reply once);
        // what matters is that both tickets got served, one request at a time.
        assert!(
            provider.calls.load(SeqCst) >= 2,
            "both tickets reached the provider"
        );
        assert_eq!(provider.peak.load(SeqCst), 1, "never two requests at once");
        for t in &tickets {
            assert_eq!(t.attempts, 1, "{}: {:?}", t.id, t.failures);
            assert_eq!(t.failures.len(), 1, "{}: {:?}", t.id, t.failures);
            assert_ne!(
                t.failures[0].class,
                tm_core::FailureClass::ProviderUnavailable,
                "{} spent an attempt waiting for capacity: {}",
                t.id,
                t.failures[0].detail
            );
        }
    }

    /// Leases and attempt starts from the in-process runner are the scheduler's doing, recorded
    /// as `system`, while the activation stays the human's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_background_runner_leases_and_starts_attempts_as_the_scheduler() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let ticket = queued_ticket(&project, "write the changelog");

        let provider = slow_provider(&project, Duration::from_millis(0));
        let fabric = one_slot_fabric(&project, provider);
        let dispatcher = crate::dispatch::build_dispatcher_with_fabric(
            &project,
            tokio::runtime::Handle::current(),
            None,
            None,
            fabric,
            std::sync::Arc::new(crate::dispatch::HeadlessApprovalSink),
        )
        .expect("dispatcher");
        let runner = spawn_runner_loop(project.clone(), Duration::from_millis(20), dispatcher);
        wait_for_first_failures(&project, &[&ticket]).await;
        runner.abort();

        let log = tm_events::EventLog::open(&project.state_dir.join("project.db")).expect("log");
        let events = log
            .read_subject(&tm_types::Id::from(ticket.clone()))
            .expect("events");
        let state_change = |from: &str, to: &str| {
            events
                .iter()
                .find(|e| {
                    e.payload
                        .as_ticket_state_changed()
                        .is_some_and(|p| p.from == from && p.to == to)
                })
                .unwrap_or_else(|| panic!("no {from} -> {to} change in {events:#?}"))
        };
        assert_eq!(state_change("draft", "blocked").actor, project.actor);
        assert_eq!(state_change("blocked", "ready").actor, project.actor);
        let system = ParticipantId::system();
        assert_eq!(state_change("ready", "leased").actor, system);
        assert_eq!(state_change("leased", "running").actor, system);
        let leased = events
            .iter()
            .find(|e| e.payload.as_ticket_leased().is_some())
            .expect("ticket.leased");
        assert_eq!(leased.actor, system);
        let attempt_started = events
            .iter()
            .find(|e| {
                e.payload
                    .as_ticket_updated()
                    .is_some_and(|p| p.fields.get("attempts").is_some())
            })
            .expect("the attempts bump");
        assert_eq!(attempt_started.actor, system);
    }

    #[test]
    fn lease_summary_serializable() {
        let lease = LeaseSummary {
            id: "L-1".to_string(),
            ticket: "T-1".to_string(),
            holder: "agent:test".to_string(),
            acquired: "2025-01-01T00:00:00Z".to_string(),
            expires: "2025-01-01T01:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&lease).expect("should serialize");
        assert!(json.contains("L-1"));
        assert!(json.contains("T-1"));
    }

    /// p1-sched-run-failure-message-copy: run_outcome should produce plain-English failure
    /// messages with no debug-printed enum variants, no false "will be retried" claims, and the
    /// exact next command to run.
    #[test]
    fn run_outcome_ready_state_plain_message() {
        use tm_core::{
            ExecutorRequirements, FailureClass, FailureRecord, RetryPolicy, Ticket, TicketKind,
            TicketState, VerificationPolicy,
        };
        use tm_types::{Authority, Budget, Timestamp, Tolerance};

        let now = Timestamp::EPOCH;
        let ticket = Ticket {
            id: TicketId::new("T-1").unwrap(),
            kind: TicketKind::Work,
            objective: "test objective".to_string(),
            state: TicketState::Ready,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            authority: Authority::none(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: tm_types::Role::CoderDeep,
                human_required: false,
                min_capability: Tolerance::Strict,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 10,
                backoff_multiplier: 2.0,
                max_delay_seconds: 300,
            },
            cycle: None,
            attempts: 1,
            failures: vec![FailureRecord {
                class: FailureClass::Other,
                detail: "model ended turn without submitting".to_string(),
                at: now,
                attempt: 1,
            }],
            priority: 0,
            created: now,
            updated: now,
        };

        let result = run_outcome(&ticket, 0);
        assert!(result.is_err());
        if let Err(tm_types::TmError::TurnFailed(msg)) = result {
            // Should not contain debug-printed enum variant
            assert!(!msg.contains("Other:"));
            assert!(!msg.contains("{:?}"));
            // Should not contain false auto-retry claim
            assert!(!msg.contains("will be retried"));
            // Should not have stacked error prefix
            assert!(!msg.contains("did not finish:"));
            // Should contain the correct next step for Ready state
            assert!(msg.contains("Inspect the saved attempt with `tm ticket show T-1`"));
            assert!(msg.contains("tm --resume <session>"));
            assert!(msg.contains("focus on the remaining change"));
            assert!(!msg.contains("Run `tm run T-1` again to retry."));
            // Should have plain-English failure reason
            assert!(!msg.contains("something went wrong"));
            assert!(msg.contains("no patch or evidence was submitted"));
            assert!(msg.contains("not a test failure"));
            assert!(msg.contains("model ended turn without submitting"));
            // Should contain the ticket ID
            assert!(msg.contains("T-1"));
        } else {
            panic!("Expected TurnFailed error");
        }
    }

    #[test]
    fn repeated_no_submit_failure_gets_focused_recovery_guidance() {
        use tm_core::{
            ExecutorRequirements, FailureClass, FailureRecord, RetryPolicy, Ticket, TicketKind,
            TicketState, VerificationPolicy,
        };
        use tm_types::{Authority, Budget, Timestamp, Tolerance};

        let now = Timestamp::EPOCH;
        let mut ticket = Ticket {
            id: TicketId::new("T-1").unwrap(),
            kind: TicketKind::Work,
            objective: "test objective".to_string(),
            state: TicketState::Ready,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            authority: Authority::none(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: tm_types::Role::CoderDeep,
                human_required: false,
                min_capability: Tolerance::Strict,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 10,
                backoff_multiplier: 2.0,
                max_delay_seconds: 300,
            },
            cycle: None,
            attempts: 2,
            failures: (1..=2)
                .map(|attempt| FailureRecord {
                    class: FailureClass::Other,
                    detail: "model ended turn without submitting".to_string(),
                    at: now,
                    attempt,
                })
                .collect(),
            priority: 0,
            created: now,
            updated: now,
        };

        // A subsequent ordinary test failure keeps its existing retry advice.
        let repeated_no_submit = run_outcome(&ticket, 1).unwrap_err().to_string();
        assert!(repeated_no_submit.contains("model ended turn without submitting"));
        assert!(repeated_no_submit.contains("Inspect the saved attempt"));
        assert!(repeated_no_submit.contains("tm --resume <session>"));
        assert!(!repeated_no_submit.contains("something went wrong"));

        ticket.failures.push(FailureRecord {
            class: FailureClass::VerificationFailed,
            detail: "tests failed".to_string(),
            at: now,
            attempt: 3,
        });
        let test_failure = run_outcome(&ticket, 2).unwrap_err().to_string();
        assert!(test_failure.contains("Run `tm run T-1` again to retry."));
    }

    #[test]
    fn run_outcome_escalated_state_plain_message() {
        use tm_core::{
            ExecutorRequirements, FailureClass, FailureRecord, RetryPolicy, Ticket, TicketKind,
            TicketState, VerificationPolicy,
        };
        use tm_types::{Authority, Budget, Timestamp, Tolerance};

        let now = Timestamp::EPOCH;
        let ticket = Ticket {
            id: TicketId::new("T-2").unwrap(),
            kind: TicketKind::Work,
            objective: "test objective".to_string(),
            state: TicketState::Escalated,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            authority: Authority::none(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: tm_types::Role::CoderDeep,
                human_required: false,
                min_capability: Tolerance::Strict,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 10,
                backoff_multiplier: 2.0,
                max_delay_seconds: 300,
            },
            cycle: None,
            attempts: 3,
            failures: vec![FailureRecord {
                class: FailureClass::BudgetExhausted,
                detail: "token budget exceeded".to_string(),
                at: now,
                attempt: 3,
            }],
            priority: 0,
            created: now,
            updated: now,
        };

        let result = run_outcome(&ticket, 0);
        assert!(result.is_err());
        if let Err(tm_types::TmError::TurnFailed(msg)) = result {
            // Should contain the correct next step for Escalated state
            assert!(msg.contains("Run `tm ticket retry T-2`"));
            // Should have plain-English failure reason
            assert!(msg.contains("budget exhausted"));
            // Should not contain false auto-retry claim
            assert!(!msg.contains("will be retried"));
            // Should not have debug-printed enum variant
            assert!(!msg.contains("BudgetExhausted"));
        } else {
            panic!("Expected TurnFailed error");
        }
    }

    #[test]
    fn run_outcome_blocked_state_plain_message() {
        use tm_core::{
            ExecutorRequirements, FailureClass, FailureRecord, RetryPolicy, Ticket, TicketKind,
            TicketState, VerificationPolicy,
        };
        use tm_types::{Authority, Budget, Timestamp, Tolerance};

        let now = Timestamp::EPOCH;
        let ticket = Ticket {
            id: TicketId::new("T-3").unwrap(),
            kind: TicketKind::Work,
            objective: "test objective".to_string(),
            state: TicketState::Blocked,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            authority: Authority::none(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: tm_types::Role::CoderDeep,
                human_required: false,
                min_capability: Tolerance::Strict,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 10,
                backoff_multiplier: 2.0,
                max_delay_seconds: 300,
            },
            cycle: None,
            attempts: 1,
            failures: vec![FailureRecord {
                class: FailureClass::ProviderUnavailable,
                detail: "provider at capacity".to_string(),
                at: now,
                attempt: 1,
            }],
            priority: 0,
            created: now,
            updated: now,
        };

        let result = run_outcome(&ticket, 0);
        assert!(result.is_err());
        if let Err(tm_types::TmError::TurnFailed(msg)) = result {
            // Should contain the correct next step for Blocked state (same as Ready)
            assert!(msg.contains("Run `tm run T-3` again to retry."));
            // Should have plain-English failure reason
            assert!(msg.contains("provider was unavailable"));
        } else {
            panic!("Expected TurnFailed error");
        }
    }

    /// `t20260925-1314-sindresorhus-ky-878-provider-retry-guidance`: a `ProviderUnavailable`
    /// failure whose detail names an unregistered provider (the fabric's own
    /// `Fabric::preflight_role`/`execute_priced` wording) is a configuration problem, not a
    /// transient one like "provider at capacity" (covered above) -- rerunning `tm run <ticket>`
    /// unchanged would fail identically every time, so `run_outcome` must not recommend a blind
    /// retry for it and must instead name a concrete recovery step.
    #[test]
    fn run_outcome_names_a_concrete_fix_for_an_unregistered_provider_mid_run() {
        use tm_core::{
            ExecutorRequirements, FailureClass, FailureRecord, RetryPolicy, Ticket, TicketKind,
            TicketState, VerificationPolicy,
        };
        use tm_types::{Authority, Budget, Timestamp, Tolerance};

        let now = Timestamp::EPOCH;
        let ticket = Ticket {
            id: TicketId::new("T-1").unwrap(),
            kind: TicketKind::Work,
            objective: "test objective".to_string(),
            state: TicketState::Ready,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            authority: Authority::none(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: tm_types::Role::CoderFast,
                human_required: false,
                min_capability: Tolerance::Strict,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 10,
                backoff_multiplier: 2.0,
                max_delay_seconds: 300,
            },
            cycle: None,
            attempts: 1,
            failures: vec![FailureRecord {
                class: FailureClass::ProviderUnavailable,
                detail: "no candidate can serve role coder.fast: provider not registered: devpass"
                    .to_string(),
                at: now,
                attempt: 1,
            }],
            priority: 0,
            created: now,
            updated: now,
        };

        let result = run_outcome(&ticket, 0);
        assert!(result.is_err());
        if let Err(tm_types::TmError::TurnFailed(msg)) = result {
            assert!(
                !msg.contains("Run `tm run T-1` again to retry."),
                "must not recommend rerunning the same broken configuration unchanged: {msg}"
            );
            assert!(
                msg.contains("providers.toml"),
                "must point at the actual config to fix: {msg}"
            );
            assert!(
                msg.contains("tm provider list"),
                "must point at a real diagnostic command, not a blind retry: {msg}"
            );
        } else {
            panic!("Expected TurnFailed error");
        }
    }

    /// A hand-built [`tm_provider::Fabric`] whose only `coder.fast` candidate names `provider`
    /// but never has anything registered under that id -- the same shape a `providers.toml`
    /// candidate naming a real but uncredentialed/never-wired-up provider produces. Deliberately
    /// bypasses [`crate::agent::build_fabric_for_project`]'s real environment-credential
    /// autodetection so this test's outcome can never depend on what happens to be set in the
    /// process running it (see `preflight_fabric_or_fail`'s own doc comment).
    fn fabric_with_unregistered_candidate(provider: &str) -> tm_provider::Fabric {
        let table = tm_provider::RoleTable::parse(&format!(
            "[coder_fast]\ncandidates = [{{ provider = \"{provider}\", model = \"m1\", max_concurrency = 1 }}]\n"
        ))
        .expect("role table parses");
        tm_provider::Fabric::new(table, Arc::new(tm_types::FixedClock::epoch()))
    }

    /// `t20260925-1314-sindresorhus-ky-878-provider-retry-guidance`: with a `coder.fast`
    /// candidate naming a provider (`devpass`) that was never registered, the preflight check
    /// `run_ticket` calls before any lease/dispatch must fail with a message naming that provider
    /// and a real repair path -- not a blind "run again" retry.
    #[test]
    fn preflight_fabric_or_fail_names_the_missing_provider_before_any_dispatch() {
        let fabric = fabric_with_unregistered_candidate("devpass");
        let ticket = TicketId::new("T-1").unwrap();

        let err = preflight_fabric_or_fail(&fabric, &ticket, tm_types::Role::CoderFast)
            .expect_err("devpass is never registered");
        match err {
            tm_types::TmError::TurnFailed(msg) => {
                assert!(
                    msg.contains("provider not registered: devpass"),
                    "names the missing provider: {msg}"
                );
                assert!(
                    msg.contains("providers.toml") && msg.contains("tm provider list"),
                    "names a real repair path: {msg}"
                );
                assert!(
                    !msg.to_lowercase().contains("run `tm run"),
                    "must not recommend rerunning the same broken config unchanged: {msg}"
                );
            }
            other => panic!("expected TurnFailed, got {other:?}"),
        }
    }

    /// `t20260925-1314-sindresorhus-ky-878-provider-retry-guidance`: the exact skip decision
    /// `run_ticket` makes before calling `preflight_provider_or_fail`, exercised directly. A
    /// `human_required` ticket is routed by `tm-scheduler`'s dispatcher to `HumanExecutor`
    /// without ever consulting its nominal role or the fabric, `--replay` serves the role from a
    /// `MockProvider` regardless of `providers.toml`, and a role `acp.toml` overrides is served
    /// by `AcpExecutor` instead of `BuiltinExecutor`'s fabric -- none of those three should ever
    /// reach the real preflight check, and the everyday case (no replay, not human-required, no
    /// matching `acp.toml` override) must still reach it.
    #[test]
    fn provider_preflight_applies_skips_human_required_replay_and_acp_override() {
        let requirements =
            |role: tm_types::Role, human_required: bool| tm_core::ExecutorRequirements {
                role,
                human_required,
                min_capability: tm_types::Tolerance::Any,
            };

        assert!(
            provider_preflight_applies(
                &requirements(tm_types::Role::CoderFast, false),
                false,
                None
            ),
            "the everyday case must still run the preflight"
        );
        assert!(
            !provider_preflight_applies(
                &requirements(tm_types::Role::CoderFast, true),
                false,
                None
            ),
            "human_required must skip the preflight"
        );
        assert!(
            !provider_preflight_applies(
                &requirements(tm_types::Role::CoderFast, false),
                true,
                None
            ),
            "--replay must skip the preflight"
        );
        assert!(
            !provider_preflight_applies(
                &requirements(tm_types::Role::CoderFast, false),
                false,
                Some(tm_types::Role::CoderFast),
            ),
            "a matching acp.toml override must skip the preflight"
        );
        assert!(
            provider_preflight_applies(
                &requirements(tm_types::Role::CoderFast, false),
                false,
                Some(tm_types::Role::CoderDeep),
            ),
            "an acp.toml override for a *different* role must not skip this role's preflight"
        );
    }

    /// An `acp.toml` naming a role bypasses that role's fabric entirely (`AcpExecutor` instead of
    /// `BuiltinExecutor`), so `run_ticket`'s preflight must not run for a ticket whose role
    /// matches -- `crate::dispatch::acp_override_role` is how it knows.
    #[test]
    fn acp_override_role_names_the_role_acp_toml_overrides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        assert_eq!(
            crate::dispatch::acp_override_role(&project).expect("no acp.toml"),
            None
        );

        std::fs::write(
            project.root.join("acp.toml"),
            "[agent]\ncommand = [\"claude-code-acp\"]\nrole = \"coder_fast\"\n",
        )
        .expect("write acp.toml");
        assert_eq!(
            crate::dispatch::acp_override_role(&project).expect("acp.toml parses"),
            Some(tm_types::Role::CoderFast)
        );
    }

    #[test]
    fn failure_class_description_all_variants() {
        use tm_core::FailureClass;

        // Ensure all variants are covered with plain English descriptions
        assert_eq!(
            failure_class_description(FailureClass::ExecutorCrash),
            "executor crashed"
        );
        assert_eq!(
            failure_class_description(FailureClass::VerificationFailed),
            "verification failed"
        );
        assert_eq!(
            failure_class_description(FailureClass::AuditRejected),
            "audit rejected the result"
        );
        assert_eq!(
            failure_class_description(FailureClass::ProviderUnavailable),
            "provider was unavailable"
        );
        assert_eq!(
            failure_class_description(FailureClass::BudgetExhausted),
            "budget exhausted"
        );
        assert_eq!(
            failure_class_description(FailureClass::AuthorityDenied),
            "authority denied"
        );
        assert_eq!(
            failure_class_description(FailureClass::ResourceConflict),
            "resource conflict"
        );
        assert_eq!(
            failure_class_description(FailureClass::Other),
            "something went wrong"
        );
    }

    // s1-events-sched-copy-and-quiet: `sched_plan`/`sched_tick` must not emit under `--quiet`,
    // matching `sched_run`'s existing `if !renderer.is_quiet()` guard. These don't capture
    // stdout (no capture seam exists in `Renderer`, and `render.rs` is out of scope for this
    // change) — they instead prove the underlying success path still completes cleanly with a
    // quiet renderer, which is what the `if !renderer.is_quiet()` guard added around each
    // `renderer.emit()` call gates.

    #[test]
    fn sched_plan_succeeds_quietly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let renderer = Renderer::new(false, true, true, false);
        sched_plan(&project, &renderer).expect("quiet sched plan should still succeed");
    }

    #[test]
    fn sched_tick_succeeds_quietly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let renderer = Renderer::new(false, true, true, false);
        sched_tick(&project, &renderer).expect("quiet sched tick should still succeed");
    }

    // ---- `tm run --record`: `replay-cli-record-flag` ----

    /// No other test in this crate mutates `TEST_MOCK_PROVIDER_ENV` inside `sched.rs`, but this
    /// guards against a future one racing this test's transient env mutation under `cargo test`'s
    /// default multi-threaded runner — same convention as `agent.rs`'s
    /// `devpass_build_fabric_env_lock`.
    fn record_test_env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// `replay-cli-record-flag`'s acceptance check: dispatch one ticket through a fabric built by
    /// [`crate::dispatch::build_dispatcher_with_recording`] under `TM_TEST_MOCK_PROVIDER=1`, let
    /// it finish, then call [`write_recording_cassette`] directly (what `run_ticket` does when
    /// `--record` is set). The cassette on disk has at least one entry, and the ticket's
    /// `ArtifactKind::Transcript` artifact's bytes equal the cassette file's bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_writes_a_cassette_and_stores_it_as_a_transcript_artifact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let ticket = queued_ticket(&project, "write the changelog");
        let ticket_state = project.store.view().expect("view").tickets[&ticket].clone();

        let cassette_sink: crate::agent::CassetteSink = Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatcher = {
            let _guard = record_test_env_lock()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::env::set_var(crate::agent::TEST_MOCK_PROVIDER_ENV, "1");
            let dispatcher = crate::dispatch::build_dispatcher_with_recording(
                &project,
                tokio::runtime::Handle::current(),
                None,
                None,
                ticket_state.executor.role,
                cassette_sink.clone(),
                None,
            );
            std::env::remove_var(crate::agent::TEST_MOCK_PROVIDER_ENV);
            dispatcher.expect("dispatcher")
        };

        dispatcher
            .dispatch(&ticket, &ticket_state, 30, project.actor.clone())
            .expect("dispatch");

        for _ in 0..500 {
            let view = project.store.view().expect("view");
            if !matches!(
                view.tickets[&ticket].state,
                tm_core::TicketState::Leased | tm_core::TicketState::Running
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let cassette_path = dir.path().join("cassette.jsonl");
        let renderer = Renderer::new(false, true, true, false);
        write_recording_cassette(&project, &ticket, &cassette_path, &cassette_sink, &renderer)
            .expect("write recording cassette");

        let cassette = Cassette::read_jsonl(&cassette_path).expect("read cassette");
        assert!(
            !cassette.entries.is_empty(),
            "expected at least one recorded provider call"
        );

        let cassette_bytes = std::fs::read(&cassette_path).expect("read cassette bytes");
        let view = project.store.view().expect("view");
        let transcript = view
            .artifacts
            .values()
            .find(|a| a.kind == ArtifactKind::Transcript)
            .expect("a Transcript artifact was stored");
        let stored_bytes = match &transcript.storage {
            tm_core::ArtifactStorage::Inline(bytes) => bytes.clone(),
            tm_core::ArtifactStorage::OnDisk(path) => {
                std::fs::read(path).expect("read on-disk artifact")
            }
        };
        assert_eq!(stored_bytes, cassette_bytes);
    }

    // ---- `tm run --replay`: `replay-cli-replay-flag` ----

    /// Records a cassette for `objective` the same way
    /// `record_writes_a_cassette_and_stores_it_as_a_transcript_artifact` does, writes it to
    /// `path`, and returns the recorded ticket's role — the caller replays against the same role.
    async fn record_cassette_for_test(
        project: &Project,
        objective: &str,
        path: &std::path::Path,
    ) -> tm_types::Role {
        let ticket = queued_ticket(project, objective);
        let ticket_state = project.store.view().expect("view").tickets[&ticket].clone();
        let cassette_sink: crate::agent::CassetteSink = Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatcher = {
            let _guard = record_test_env_lock()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::env::set_var(crate::agent::TEST_MOCK_PROVIDER_ENV, "1");
            let dispatcher = crate::dispatch::build_dispatcher_with_recording(
                project,
                tokio::runtime::Handle::current(),
                None,
                None,
                ticket_state.executor.role,
                cassette_sink.clone(),
                None,
            );
            std::env::remove_var(crate::agent::TEST_MOCK_PROVIDER_ENV);
            dispatcher.expect("dispatcher")
        };
        dispatcher
            .dispatch(&ticket, &ticket_state, 30, project.actor.clone())
            .expect("dispatch");
        for _ in 0..500 {
            let view = project.store.view().expect("view");
            if !matches!(
                view.tickets[&ticket].state,
                tm_core::TicketState::Leased | tm_core::TicketState::Running
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let renderer = Renderer::new(false, true, true, false);
        write_recording_cassette(project, &ticket, path, &cassette_sink, &renderer)
            .expect("write recording cassette");
        ticket_state.executor.role
    }

    /// `replay-cli-replay-flag`'s acceptance check, first half: record a mock run in tempdir A,
    /// then replay it (via [`crate::dispatch::build_dispatcher_with_replay`]) against a fresh
    /// ticket in tempdir B. The replayed run reaches a forward-progress state and
    /// [`tm_provider::MockProvider::divergences`] is empty — the cassette's path-normalized
    /// requests still match a freshly built prompt against different tempdir state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replay_reruns_a_cassette_with_zero_divergences() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let project_a = test_project(dir_a.path());
        let cassette_path = dir_a.path().join("cassette.jsonl");
        let role =
            record_cassette_for_test(&project_a, "write the changelog", &cassette_path).await;

        let dir_b = tempfile::tempdir().expect("tempdir b");
        let project_b = test_project(dir_b.path());
        let replay_ticket = queued_ticket(&project_b, "write the changelog");
        let replay_state = project_b.store.view().expect("view").tickets[&replay_ticket].clone();
        assert_eq!(replay_state.executor.role, role);

        let cassette = Cassette::read_jsonl(&cassette_path).expect("read cassette");
        let (dispatcher, mock) = crate::dispatch::build_dispatcher_with_replay(
            &project_b,
            tokio::runtime::Handle::current(),
            None,
            None,
            role,
            cassette,
        )
        .expect("replay dispatcher");
        dispatcher
            .dispatch(&replay_ticket, &replay_state, 30, project_b.actor.clone())
            .expect("dispatch");
        for _ in 0..500 {
            let view = project_b.store.view().expect("view");
            if !matches!(
                view.tickets[&replay_ticket].state,
                tm_core::TicketState::Leased | tm_core::TicketState::Running
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
            mock.divergences().is_empty(),
            "a same-scenario replay should diverge on nothing: {:?}",
            mock.divergences()
        );
    }

    /// `replay-cli-replay-flag`'s acceptance check, second half: a cassette with one mutated
    /// entry (its `request_hash` no longer matches what will actually be served at that
    /// position) reports the divergence via [`report_replay_divergences`], and under
    /// `--strict-replay` that divergence becomes a hard error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replay_reports_and_strict_replay_fails_on_a_mutated_cassette_entry() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let project_a = test_project(dir_a.path());
        let cassette_path = dir_a.path().join("cassette.jsonl");
        let role =
            record_cassette_for_test(&project_a, "write the changelog", &cassette_path).await;

        let mut cassette = Cassette::read_jsonl(&cassette_path).expect("read cassette");
        assert!(!cassette.entries.is_empty(), "expected a recorded entry");
        cassette.entries[0].request_hash ^= 1;
        let recorded = cassette.entries.len();

        let dir_b = tempfile::tempdir().expect("tempdir b");
        let project_b = test_project(dir_b.path());
        let replay_ticket = queued_ticket(&project_b, "write the changelog");
        let replay_state = project_b.store.view().expect("view").tickets[&replay_ticket].clone();

        let (dispatcher, mock) = crate::dispatch::build_dispatcher_with_replay(
            &project_b,
            tokio::runtime::Handle::current(),
            None,
            None,
            role,
            cassette,
        )
        .expect("replay dispatcher");
        dispatcher
            .dispatch(&replay_ticket, &replay_state, 30, project_b.actor.clone())
            .expect("dispatch");
        for _ in 0..500 {
            let view = project_b.store.view().expect("view");
            if !matches!(
                view.tickets[&replay_ticket].state,
                tm_core::TicketState::Leased | tm_core::TicketState::Running
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            mock.divergences().len(),
            1,
            "the mutated entry should be the sole divergence"
        );

        let renderer = Renderer::new(false, true, true, false);
        let lenient = report_replay_divergences(&replay_ticket, &mock, recorded, false, &renderer)
            .expect("report divergences");
        assert!(
            lenient.is_none(),
            "without --strict-replay, a divergence is reported, not a hard error"
        );

        let strict = report_replay_divergences(&replay_ticket, &mock, recorded, true, &renderer)
            .expect("report divergences");
        assert!(
            strict.is_some(),
            "--strict-replay must turn a recorded divergence into a hard error"
        );
    }

    /// `replay-cli-replay-flag`'s acceptance check, exhaustion half: a cassette with its entries
    /// dropped (every call runs "past the end of the recording") reports `unscripted_calls > 0`
    /// via [`report_replay_divergences`], and under `--strict-replay` that becomes a hard error
    /// too, matching D-028's "any divergence or exhaustion is a hard error" `--strict-replay`
    /// contract.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replay_reports_and_strict_replay_fails_on_cassette_exhaustion() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let project_a = test_project(dir_a.path());
        let cassette_path = dir_a.path().join("cassette.jsonl");
        let role =
            record_cassette_for_test(&project_a, "write the changelog", &cassette_path).await;

        let mut cassette = Cassette::read_jsonl(&cassette_path).expect("read cassette");
        assert!(!cassette.entries.is_empty(), "expected a recorded entry");
        cassette.entries.clear();
        let recorded = cassette.entries.len();

        let dir_b = tempfile::tempdir().expect("tempdir b");
        let project_b = test_project(dir_b.path());
        let replay_ticket = queued_ticket(&project_b, "write the changelog");
        let replay_state = project_b.store.view().expect("view").tickets[&replay_ticket].clone();

        let (dispatcher, mock) = crate::dispatch::build_dispatcher_with_replay(
            &project_b,
            tokio::runtime::Handle::current(),
            None,
            None,
            role,
            cassette,
        )
        .expect("replay dispatcher");
        dispatcher
            .dispatch(&replay_ticket, &replay_state, 30, project_b.actor.clone())
            .expect("dispatch");
        for _ in 0..500 {
            let view = project_b.store.view().expect("view");
            if !matches!(
                view.tickets[&replay_ticket].state,
                tm_core::TicketState::Leased | tm_core::TicketState::Running
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
            !mock.call_log().is_empty(),
            "the run should have made at least one provider call"
        );

        let renderer = Renderer::new(false, true, true, false);
        let strict = report_replay_divergences(&replay_ticket, &mock, recorded, true, &renderer)
            .expect("report divergences");
        assert!(
            strict.is_some(),
            "--strict-replay must turn cassette exhaustion into a hard error"
        );
    }
}
