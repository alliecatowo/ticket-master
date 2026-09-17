//! The `sched`, `lease`, and `run` command groups: everything that drives the scheduler
//! ([`tm_scheduler`]) or executes a ticket through [`tm_agent`].

use std::path::PathBuf;
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
pub fn dispatch_sched(
    cmd: &SchedCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        SchedCommand::Plan => sched_plan(project, renderer),
        SchedCommand::Tick => sched_tick(project, renderer),
        SchedCommand::Run(args) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(sched_run(args, project, renderer))
        }
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

/// `tm sched run`: run the scheduler loop continuously until interrupted (ctrl-c).
pub async fn sched_run(
    args: &SchedRunArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let policy = tm_scheduler::SchedulingPolicy::conservative_default();
    let tick_interval_secs = args
        .interval_secs
        .unwrap_or(u64::from(policy.tick_interval_seconds));
    let interval = Duration::from_secs(tick_interval_secs);

    let loop_driver =
        tm_scheduler::SchedulerLoop::new(&project.store, project.clock.clone(), policy);
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

/// `tm run <ticket>`: execute one ticket to completion in the foreground, outside the scheduler
/// loop — the single-ticket path a human runs interactively or in CI.
pub async fn run_ticket(
    args: &RunArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket = TicketId::new(&args.ticket)?;
    let view = project.store.view()?;

    let ticket_state = view
        .tickets
        .get(&ticket)
        .ok_or_else(|| tm_types::TmError::not_found("ticket", &ticket))?;

    let lease_events = project.store.acquire_lease(
        &ticket,
        project.actor.clone(),
        Authority::none(),
        Vec::new(),
        3600,
        project.actor.clone(),
    )?;

    let _code_intel = project.code_intel()?;
    let _conventions: Vec<String> = Vec::new();

    let _context_pack = tm_context::pack::compile(
        ticket_state,
        &view,
        &_code_intel,
        tm_context::tokens::TokenBudget::even(10_000),
        tm_codeintel::SignalWeights::default(),
        &_conventions,
    )?;

    if !lease_events.is_empty() {
        renderer.note(&format!("Ticket {} acquired for execution.", ticket));
    }

    renderer.note("Agent loop execution not yet implemented.");
    Ok(())
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
    project.root.join(".tm").join("sched.paused")
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
