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

    renderer.emit(&summaries, &human)
}

/// `tm sched tick`: run one scheduler tick, applying its actions.
pub fn sched_tick(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let policy = tm_scheduler::SchedulingPolicy::conservative_default();
    let loop_driver =
        tm_scheduler::SchedulerLoop::new(&project.store, project.clock.clone(), policy);
    let events = loop_driver.tick(project.actor.clone())?;

    let summaries: Vec<EventSummary> = events.iter().map(event_to_summary).collect();

    let human = summaries
        .iter()
        .map(|s| format!("{}: {}", s.kind, s.detail))
        .collect::<Vec<_>>()
        .join("\n");

    renderer.emit(&summaries, &human)
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
                    match loop_driver.tick(project.actor.clone()) {
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
/// also running. Stops when the returned handle is aborted or the runtime shuts down.
///
/// # Errors
/// Fails up front if the dispatcher can't be built (typically: no provider configured).
pub fn spawn_background_runner(
    project: std::sync::Arc<Project>,
    interval: Duration,
) -> tm_types::Result<tokio::task::JoinHandle<()>> {
    let dispatcher =
        crate::dispatch::build_dispatcher(&project, tokio::runtime::Handle::current(), None, None)?;
    Ok(tokio::spawn(async move {
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
            match loop_driver.tick(project.actor.clone()) {
                Ok(events) if !events.is_empty() => {
                    tracing::debug!(events = events.len(), "background runner tick");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "background runner tick failed"),
            }
        }
    }))
}

/// `tm sched pause`: stop granting new leases; admitted work continues.
pub fn sched_pause(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let paused_path = pause_flag_path(project);
    std::fs::create_dir_all(paused_path.parent().ok_or_else(|| {
        tm_types::TmError::InvalidTransition("Cannot determine parent directory".to_string())
    })?)?;
    std::fs::write(&paused_path, b"paused")?;
    renderer.note("Scheduler paused.");
    Ok(())
}

/// `tm sched resume`: undo [`sched_pause`].
pub fn sched_resume(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let paused_path = pause_flag_path(project);
    if paused_path.exists() {
        std::fs::remove_file(&paused_path)?;
    }
    renderer.note("Scheduler resumed.");
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

    renderer.emit(&leases, &table.render())
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

    renderer.note(&format!("Lease acquired for {}", ticket));
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

    renderer.note(&format!("Lease {} released.", lease_id));
    Ok(())
}

/// `tm lease expire`
pub fn lease_expire(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let _events = project.store.expire_leases()?;

    renderer.note("Expired leases swept.");
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
    let state = state_label(ticket.state);
    if worktree_run_reached_success(ticket.state) {
        return Ok(if ticket.state == tm_core::TicketState::Submitted {
            format!(
                "Ticket {id} submitted its work. Review it (tm ticket show {id}), then \
                 `tm ticket accept {id}` or `tm ticket reject {id} --reason \"...\"`.",
                id = ticket.id
            )
        } else {
            format!("Ticket {} submitted its work ({state}).", ticket.id)
        });
    }
    let reason = ticket
        .failures
        .get(failures_before..)
        .and_then(<[_]>::last)
        .map(|f| format!("{:?}: {}", f.class, f.detail))
        .unwrap_or_else(|| "no failure was recorded".to_string());
    let next = match ticket.state {
        tm_core::TicketState::Ready | tm_core::TicketState::Blocked => {
            format!("it is {state} again and will be retried")
        }
        _ => format!("it is now {state}"),
    };
    Err(tm_types::TmError::TurnFailed(format!(
        "ticket {} attempt {} did not finish: {reason}; {next}",
        ticket.id, ticket.attempts
    )))
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
        SchedulerLoopEvent::Ticked { at, planned } => EventSummary {
            kind: "Ticked".to_string(),
            detail: format!("{} actions planned at {}", planned, at),
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
        assert_eq!(summary.kind, "Ticked");
        assert!(summary.detail.contains("3 actions"));
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
}
