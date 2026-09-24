//! The `sched`, `lease`, and `run` command groups: everything that drives the scheduler
//! ([`tm_scheduler`]) or executes a ticket through [`tm_agent`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
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
    let is_draft = view
        .tickets
        .get(&ticket)
        .ok_or_else(|| tm_types::TmError::not_found("ticket", &ticket))?
        .state
        == tm_core::TicketState::Draft;
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
    let dispatcher = crate::dispatch::build_dispatcher(
        project,
        tokio::runtime::Handle::current(),
        exec_root,
        Some(step_tx),
    )?;
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
                return Err(e);
            }
        }
    } else {
        dispatcher.dispatch(&ticket, ticket_state, ttl_seconds, project.actor.clone())?;
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
    loop {
        if project.clock.now() >= deadline {
            renderer.note(&format!(
                "Ticket {ticket} is still running after {}s; detaching (the run continues in the background).",
                RUN_TICKET_MAX_WAIT.as_secs()
            ));
            detached = true;
            break;
        }
        tokio::time::sleep(RUN_TICKET_POLL_INTERVAL).await;
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
            Ok(())
        }
        Some(Err(err)) => Err(err),
        None => Ok(()),
    }
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
    let reason = failure
        .map(|f| {
            let class_desc = failure_class_description(f.class);
            format!("{class_desc}: {}", f.detail)
        })
        .unwrap_or_else(|| "unknown failure".to_string());
    let next = match ticket.state {
        tm_core::TicketState::Ready | tm_core::TicketState::Blocked => {
            format!("Run `tm run {}` again to retry.", ticket.id)
        }
        tm_core::TicketState::Escalated => {
            format!("Run `tm ticket retry {}` to try again.", ticket.id)
        }
        _ => "".to_string(),
    };
    let message = if next.is_empty() {
        format!("Ticket {}: {}.", ticket.id, reason)
    } else {
        format!("Ticket {}: {}. {}", ticket.id, reason, next)
    };
    Err(tm_types::TmError::TurnFailed(message))
}

/// Plain-English description of a failure class for user-facing messages.
fn failure_class_description(class: tm_core::FailureClass) -> &'static str {
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
            target: format!("{}: {:?}", ticket, reason),
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
            assert!(msg.contains("Run `tm run T-1` again to retry."));
            // Should have plain-English failure reason
            assert!(msg.contains("something went wrong"));
            // Should contain the ticket ID
            assert!(msg.contains("T-1"));
        } else {
            panic!("Expected TurnFailed error");
        }
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
}
