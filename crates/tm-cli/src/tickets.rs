//! The `ticket`, `dep`, `milestone`, and `decision` command groups: everything that reads or
//! mutates the project graph directly through [`tm_core::store::Store`], without going through
//! the scheduler or an executor.

use crate::args::{
    DecisionCommand, DecisionNewArgs, DecisionRefArgs, DecisionSupersedeArgs, DepCommand,
    DepEdgeArgs, DepGraphArgs, MilestoneCommand, MilestoneNewArgs, MilestoneRefArgs,
    TicketAcceptArgs, TicketCancelArgs, TicketCommand, TicketDelegateArgs, TicketEditArgs,
    TicketForkArgs, TicketListArgs, TicketNewArgs, TicketRefArgs, TicketRejectArgs,
    TicketRetryArgs, TicketStateArg, TicketSubmitArgs, TicketsArgs,
};
use crate::project::Project;

pub mod overview;
use crate::render::{
    authority_label, budget_label, dep_kind_label, kind_label, milestone_state_label, state_label,
    Renderer, Table, Tree,
};
use std::collections::BTreeMap;
use std::fs;
use tm_core::ticket::{
    DependencyKind, ExecutorRequirements, ResourceClaim, ResourceMode, RetryPolicy, TicketKind,
    TicketState, VerificationPolicy,
};
use tm_types::{Authority, Budget, DecisionId, MilestoneId, PatternSet, Role, TicketId, TmError};

/// One dependency edge, serializable for `--json` rendering (unlike
/// [`tm_core::graph::DependencyEdge`], which intentionally carries no serde impl).
#[derive(serde::Serialize)]
struct EdgeView {
    from: TicketId,
    to: TicketId,
    kind: DependencyKind,
}

/// A dependency graph projection, serializable for `--json` rendering (unlike
/// [`tm_core::graph::DependencyGraph`] itself).
#[derive(serde::Serialize)]
struct GraphView {
    nodes: Vec<TicketId>,
    edges: Vec<EdgeView>,
}

impl From<&tm_core::graph::DependencyGraph> for GraphView {
    fn from(g: &tm_core::graph::DependencyGraph) -> Self {
        GraphView {
            nodes: g.nodes().cloned().collect(),
            edges: g
                .edges()
                .iter()
                .map(|e| EdgeView {
                    from: e.from.clone(),
                    to: e.to.clone(),
                    kind: e.kind,
                })
                .collect(),
        }
    }
}

/// A milestone projection, serializable for `--json` rendering (unlike [`tm_core::milestone::Milestone`]
/// itself).
#[derive(serde::Serialize)]
struct MilestoneView<'a> {
    id: &'a MilestoneId,
    title: &'a str,
    tickets: &'a [TicketId],
    state: tm_core::milestone::MilestoneState,
    closed_by: &'a Option<tm_types::ParticipantId>,
    assumptions: &'a [DecisionId],
}

impl<'a> From<&'a tm_core::milestone::Milestone> for MilestoneView<'a> {
    fn from(m: &'a tm_core::milestone::Milestone) -> Self {
        MilestoneView {
            id: &m.id,
            title: &m.title,
            tickets: &m.tickets,
            state: m.state,
            closed_by: &m.closed_by,
            assumptions: &m.assumptions,
        }
    }
}

/// A decision projection, serializable for `--json` rendering (unlike
/// [`tm_core::decision::Decision`] itself).
#[derive(serde::Serialize)]
struct DecisionView<'a> {
    id: &'a DecisionId,
    subject: &'a str,
    decision: &'a str,
    reason: &'a str,
    active: bool,
    supersedes: &'a Option<DecisionId>,
    superseded_by: &'a Option<DecisionId>,
}

impl<'a> From<&'a tm_core::decision::Decision> for DecisionView<'a> {
    fn from(d: &'a tm_core::decision::Decision) -> Self {
        DecisionView {
            id: &d.id,
            subject: &d.subject,
            decision: &d.decision,
            reason: &d.reason,
            active: d.is_active(),
            supersedes: &d.supersedes,
            superseded_by: &d.superseded_by,
        }
    }
}

/// Re-parse an event's [`tm_types::Id`] subject as a [`TicketId`], when it looks like one.
pub(crate) fn event_ticket_id(subject: &tm_types::Id) -> Option<TicketId> {
    (subject.kind() == Some(tm_types::IdKind::Ticket))
        .then(|| TicketId::new(subject.as_str()).ok())
        .flatten()
}

/// Re-parse an event's [`tm_types::Id`] subject as a [`MilestoneId`], when it looks like one.
fn event_milestone_id(subject: &tm_types::Id) -> Option<MilestoneId> {
    (subject.kind() == Some(tm_types::IdKind::Milestone))
        .then(|| MilestoneId::new(subject.as_str()).ok())
        .flatten()
}

/// Re-parse an event's [`tm_types::Id`] subject as a [`DecisionId`], when it looks like one.
fn event_decision_id(subject: &tm_types::Id) -> Option<DecisionId> {
    (subject.kind() == Some(tm_types::IdKind::Decision))
        .then(|| DecisionId::new(subject.as_str()).ok())
        .flatten()
}

/// Re-parse an event's [`tm_types::Id`] subject as an [`tm_types::ArtifactId`], when it looks
/// like one.
fn event_artifact_id(subject: &tm_types::Id) -> Option<tm_types::ArtifactId> {
    (subject.kind() == Some(tm_types::IdKind::Artifact))
        .then(|| tm_types::ArtifactId::new(subject.as_str()).ok())
        .flatten()
}

/// Parse a `--kind` string into a [`TicketKind`]. `task` is accepted as an alias for `work`
/// since that's the CLI's historical default spelling.
fn parse_ticket_kind(s: &str) -> tm_types::Result<TicketKind> {
    match s {
        "task" | "work" => Ok(TicketKind::Work),
        "verification" => Ok(TicketKind::Verification),
        "audit" => Ok(TicketKind::Audit),
        "investigation" => Ok(TicketKind::Investigation),
        "recovery" => Ok(TicketKind::Recovery),
        "harness" => Ok(TicketKind::Harness),
        other => Err(TmError::parse(format!(
            "unknown ticket kind '{other}' (expected task, verification, audit, investigation, recovery, or harness)"
        ))),
    }
}

/// Build one exclusive [`ResourceClaim`] per glob pattern string.
fn parse_resource_claims(patterns: &[String]) -> tm_types::Result<Vec<ResourceClaim>> {
    patterns
        .iter()
        .map(|p| {
            Ok(ResourceClaim {
                paths: PatternSet::parse([p.clone()])?,
                mode: ResourceMode::Exclusive,
            })
        })
        .collect()
}

/// The executor requirements a freshly-created ticket gets when the caller specified none:
/// a fast coder, no human required, willing to degrade at most one capability tier.
pub(crate) fn default_executor_requirements() -> ExecutorRequirements {
    ExecutorRequirements {
        role: Role::CoderFast,
        human_required: false,
        min_capability: tm_types::Tolerance::default(),
    }
}

/// The retry policy a freshly-created ticket gets when the caller specified none: three
/// attempts, five-second base delay, doubling backoff, capped at five minutes.
pub(crate) fn default_retry_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay_seconds: 5,
        backoff_multiplier: 2.0,
        max_delay_seconds: 300,
    }
}

/// `tm tickets --json` (and the plain, non-tty form of `tm tickets`): the ticket list, like
/// `claude agents --json`. Open tickets only unless `args.all`; `--json` prints a JSON array of
/// [`overview::TicketOverview`], otherwise one line per ticket under its group's heading.
pub fn tickets_list(
    args: &TicketsArgs,
    project: Option<&Project>,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    use tm_types::Clock as _;
    let Some(project) = project else {
        // No project anywhere: there are no tickets, and listing must not create state.
        let empty: Vec<overview::TicketOverview> = Vec::new();
        return renderer.emit(&empty, "No tickets yet. Dispatch one from `tm tickets`.");
    };
    let view = project.store.view()?;
    let mut index = overview::ActivityIndex::new();
    index.refresh(project.store.state_dir())?;
    let mut rows = overview::overviews(&view, &index, project.clock.now(), false, args.all);
    rows.sort_by(|a, b| {
        a.group
            .cmp(&b.group)
            .then_with(|| b.id.number().cmp(&a.id.number()))
    });
    let mut text = String::new();
    let mut last_group = None;
    for row in &rows {
        if last_group != Some(row.group) {
            if last_group.is_some() {
                text.push('\n');
            }
            text.push_str(match row.group {
                overview::TicketGroup::NeedsInput => "Needs input\n",
                overview::TicketGroup::Working => "Working\n",
                overview::TicketGroup::Review => "Ready for review\n",
                overview::TicketGroup::Queued => "Queued\n",
                overview::TicketGroup::Completed => "Completed\n",
            });
            last_group = Some(row.group);
        }
        text.push_str(&format!("  {}  {}  {}\n", row.id, row.title, row.summary));
    }
    if rows.is_empty() {
        text.push_str("No open tickets.");
    }
    renderer.emit(&rows, text.trim_end())
}

/// Dispatch one [`TicketCommand`].
pub fn dispatch_ticket(
    cmd: &TicketCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        TicketCommand::List(args) => ticket_list(args, project, renderer),
        TicketCommand::Show(args) => ticket_show(args, project, renderer),
        TicketCommand::New(args) => ticket_new(args, project, renderer),
        TicketCommand::Dispatch(args) => {
            let id = create_and_queue(project, &args.objective)?;
            renderer.emit(
                &id,
                &format!("Dispatched ticket {id}: ready for a worker (tm run {id})"),
            )
        }
        TicketCommand::Edit(args) => ticket_edit(args, project, renderer),
        TicketCommand::Close(args) => ticket_close(args, project, renderer),
        TicketCommand::Cancel(args) => ticket_cancel(args, project, renderer),
        TicketCommand::Reopen(args) => ticket_reopen(args, project, renderer),
        TicketCommand::Activate(args) => ticket_activate(args, project, renderer),
        TicketCommand::Accept(args) => ticket_accept(args, project, renderer),
        TicketCommand::Reject(args) => ticket_reject(args, project, renderer),
        TicketCommand::Retry(args) => ticket_retry(args, project, renderer),
        TicketCommand::Tree(args) => ticket_tree(args, project, renderer),
        TicketCommand::Delegate(args) => ticket_delegate(args, project, renderer),
        TicketCommand::Submit(args) => ticket_submit(args, project, renderer),
        TicketCommand::Fork(args) => ticket_fork(args, project, renderer),
        TicketCommand::Context(args) => crate::ticket::ticket_context(args, project, renderer),
    }
}

/// Create a top-level work ticket with the defaults a background worker needs
/// (`Authority::worker()`, an unlimited budget, one verification pass), still a draft. The one
/// place `/bg` and any other "hand this to a worker" path create tickets, so they match
/// `tm ticket new`.
pub fn create_worker_ticket(project: &Project, objective: &str) -> tm_types::Result<TicketId> {
    let events = project.store.create_ticket(
        tm_core::TicketKind::Work,
        objective.to_string(),
        None,
        None,
        Authority::worker(),
        Vec::new(),
        default_executor_requirements(),
        Vec::new(),
        Vec::new(),
        VerificationPolicy::Single,
        Budget::unlimited(),
        default_retry_policy(),
        0,
        project.actor.clone(),
    )?;
    events
        .iter()
        .find_map(|e| e.payload.as_ticket_created().map(|p| p.ticket.clone()))
        .ok_or_else(|| TmError::invariant("Could not create the ticket."))
}

/// The `--state` filter's vocabulary, translated to the full [`TicketState`] it matches. The
/// inverse of [`crate::render::state_label`] for the states `--state` can filter on; kept as one
/// function so the filter and the table's displayed label can't silently drift apart.
fn state_from_arg(arg: TicketStateArg) -> TicketState {
    match arg {
        TicketStateArg::Draft => TicketState::Draft,
        TicketStateArg::Ready => TicketState::Ready,
        TicketStateArg::Active => TicketState::Running,
        TicketStateArg::Verification => TicketState::Verifying,
        TicketStateArg::Audit => TicketState::Auditing,
        TicketStateArg::Closed => TicketState::Closed,
        TicketStateArg::Cancelled => TicketState::Cancelled,
        TicketStateArg::Blocked => TicketState::Blocked,
    }
}

/// `tm ticket list`
pub fn ticket_list(
    args: &TicketListArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let view = project.store.view()?;

    let mut tickets: Vec<_> = view.tickets.values().collect();
    tickets.sort_by_key(|t| &t.id);

    if let Some(state_arg) = args.state {
        let target_state = state_from_arg(state_arg);
        tickets.retain(|t| t.state == target_state);
    }

    if let Some(milestone) = &args.milestone {
        let milestone_id = MilestoneId::new(milestone)?;
        tickets.retain(|t| t.milestone.as_ref() == Some(&milestone_id));
    }

    if let Some(parent) = &args.parent {
        let parent_id = TicketId::new(parent)?;
        tickets.retain(|t| t.parent.as_ref() == Some(&parent_id));
    }

    if renderer.is_json() {
        renderer.emit(&tickets, "")?;
    } else {
        let headers = vec![
            "ID".to_string(),
            "Kind".to_string(),
            "State".to_string(),
            "Priority".to_string(),
            "Objective".to_string(),
        ];
        let rows: Vec<Vec<String>> = tickets
            .iter()
            .map(|t| {
                let objective = if t.objective.chars().count() > 50 {
                    format!("{}...", t.objective.chars().take(47).collect::<String>())
                } else {
                    t.objective.clone()
                };
                vec![
                    t.id.to_string(),
                    kind_label(t.kind).to_string(),
                    state_label(t.state).to_string(),
                    t.priority.to_string(),
                    objective,
                ]
            })
            .collect();
        let table = Table::new(headers, rows);
        renderer.emit(&tickets, &table.render())?;
    }

    Ok(())
}

/// A one-line human summary of a [`VerificationPolicy`], `None` when it's the default
/// ([`VerificationPolicy::Single`]) so `ticket show` can omit the line entirely.
fn verification_summary(policy: &VerificationPolicy) -> Option<String> {
    match policy {
        VerificationPolicy::Single => None,
        VerificationPolicy::None => Some("not required".to_string()),
        VerificationPolicy::EveryPredicate => {
            Some("every success predicate is checked independently".to_string())
        }
        VerificationPolicy::Audited => {
            Some("must pass, and the pass is itself audited".to_string())
        }
    }
}

/// A one-line human summary of a [`RetryPolicy`], `None` when it's the default so `ticket show`
/// can omit the line entirely.
fn retry_summary(retry: &RetryPolicy) -> Option<String> {
    if *retry == RetryPolicy::default() {
        return None;
    }
    Some(format!("up to {} attempts", retry.max_attempts))
}

/// A one-line human summary of a ticket's [`ResourceClaim`]s, `None` when there are none.
fn resources_summary(resources: &[ResourceClaim]) -> Option<String> {
    if resources.is_empty() {
        return None;
    }
    let parts: Vec<String> = resources
        .iter()
        .map(|r| {
            let mode = match r.mode {
                ResourceMode::Exclusive => "exclusive",
                ResourceMode::Shared => "shared",
            };
            let paths: Vec<&str> = r.paths.patterns().iter().map(|p| p.as_str()).collect();
            format!("{} on {}", mode, paths.join(", "))
        })
        .collect();
    Some(parts.join("; "))
}

/// A one-line human summary of a ticket's [`ExecutorRequirements`], `None` when it's the default.
fn executor_summary(executor: &ExecutorRequirements) -> Option<String> {
    if *executor == ExecutorRequirements::default() {
        return None;
    }
    let mut parts = vec![executor.role.to_string()];
    if executor.human_required {
        parts.push("human required".to_string());
    }
    Some(parts.join(", "))
}

/// The plain-text body of `tm ticket show`: a person-readable summary, not a struct dump. Pulled
/// out of [`ticket_show`] so it can be unit-tested against a constructed [`tm_core::ticket::Ticket`]
/// without a real [`Project`]/[`tm_core::Store`].
fn format_ticket_text(ticket: &tm_core::ticket::Ticket) -> String {
    let mut text = format!("ID:           {}\n", ticket.id);
    text.push_str(&format!("State:        {}\n", state_label(ticket.state)));
    text.push_str(&format!("Kind:         {}\n", kind_label(ticket.kind)));
    text.push_str(&format!("Priority:     {}\n", ticket.priority));
    text.push_str(&format!("Objective:    {}\n", ticket.objective));
    if let Some(parent) = &ticket.parent {
        text.push_str(&format!("Parent:       {}\n", parent));
    }
    text.push_str(&format!(
        "Milestone:    {}\n",
        ticket
            .milestone
            .as_ref()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "none".to_string())
    ));
    let depends_on = if ticket.dependencies.is_empty() {
        "none".to_string()
    } else {
        ticket
            .dependencies
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    text.push_str(&format!("Depends on:   {depends_on}\n"));
    let children = if ticket.children.is_empty() {
        "none".to_string()
    } else {
        ticket
            .children
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    text.push_str(&format!("Children:     {children}\n"));
    text.push_str(&format!("Budget:       {}\n", budget_label(&ticket.budget)));
    text.push_str(&format!(
        "Authority:    {}\n",
        authority_label(&ticket.authority)
    ));
    if let Some(resources) = resources_summary(&ticket.resources) {
        text.push_str(&format!("Resources:    {resources}\n"));
    }
    if let Some(executor) = executor_summary(&ticket.executor) {
        text.push_str(&format!("Executor:     {executor}\n"));
    }
    if let Some(verification) = verification_summary(&ticket.verification) {
        text.push_str(&format!("Verification: {verification}\n"));
    }
    if let Some(retry) = retry_summary(&ticket.retry) {
        text.push_str(&format!("Retries:      {retry}\n"));
    }
    text.push_str(&format!("Created:      {}\n", ticket.created));
    text.push_str(&format!("Updated:      {}\n", ticket.updated));
    text
}

/// `tm ticket show`
pub fn ticket_show(
    args: &TicketRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;
    let view = project.store.view()?;

    let ticket = view
        .tickets
        .get(&ticket_id)
        .ok_or_else(|| TmError::not_found("ticket", &ticket_id))?;

    if renderer.is_json() {
        renderer.emit(ticket, "")?;
    } else {
        renderer.emit(ticket, &format_ticket_text(ticket))?;
    }

    Ok(())
}

/// `tm ticket new`
pub fn ticket_new(
    args: &TicketNewArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let kind = parse_ticket_kind(&args.kind)?;
    let resources = parse_resource_claims(&args.resources)?;

    let parent = if let Some(p) = &args.parent {
        Some(TicketId::new(p)?)
    } else {
        None
    };

    let view = project.store.view()?;

    let milestone = if let Some(m) = &args.milestone {
        let milestone_id = MilestoneId::new(m)?;
        if !view.milestones.contains_key(&milestone_id) {
            return Err(TmError::parse(format!(
                "no milestone {milestone_id}. Run `tm milestone list` to see what exists, or `tm milestone new \"<title>\"` to create it."
            )));
        }
        Some(milestone_id)
    } else {
        None
    };

    let authority = if let Some(parent_id) = &parent {
        let parent_ticket = view
            .tickets
            .get(parent_id)
            .ok_or_else(|| TmError::not_found("ticket", parent_id))?;
        parent_ticket.authority.clone()
    } else {
        // A root ticket is meant to be worked on: give its worker enough authority to do the
        // job (`Authority::worker`), not none at all.
        Authority::worker()
    };

    let events = project.store.create_ticket(
        kind,
        args.objective.clone(),
        parent,
        milestone,
        authority,
        resources,
        default_executor_requirements(),
        Vec::new(),
        Vec::new(),
        VerificationPolicy::Single,
        Budget::unlimited(),
        default_retry_policy(),
        args.priority,
        project.actor.clone(),
    )?;

    if let Some(event) = events.first() {
        if let Some(created_id) = event_ticket_id(&event.subject) {
            renderer.emit(&created_id, &format!("Created ticket {}", created_id))?;
        }
    }

    Ok(())
}

/// Create a worker ticket for `objective` ([`create_worker_ticket`]: the same defaults as `tm
/// ticket new` and `/bg`) and activate it (`Draft -> Ready`), so a background worker picks it up.
/// This is what the tickets screen's dispatch input does (D-019 §2); it lives here so the CLI
/// and the TUI cannot drift apart on what a dispatched ticket looks like.
///
/// # Errors
/// `TmError::parse` for an objective that is empty once trimmed.
pub fn create_and_queue(project: &Project, objective: &str) -> tm_types::Result<TicketId> {
    let objective = objective.trim();
    if objective.is_empty() {
        return Err(TmError::parse("describe the task first"));
    }
    let id = create_worker_ticket(project, objective)?;
    project.store.activate(&id, project.actor.clone())?;
    Ok(id)
}

/// `tm ticket edit`
pub fn ticket_edit(
    args: &TicketEditArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;

    if args.objective.is_none() && args.priority.is_none() {
        return Err(TmError::parse(
            "Provide --objective or --priority to update the ticket.",
        ));
    }

    let mut fields = serde_json::json!({});

    if let Some(obj) = &args.objective {
        fields["objective"] = serde_json::json!(obj);
    }

    if let Some(pri) = args.priority {
        fields["priority"] = serde_json::json!(pri);
    }

    project
        .store
        .update_ticket(&ticket_id, fields, project.actor.clone())?;
    renderer.emit(&ticket_id, &format!("Updated ticket {}", ticket_id))?;

    Ok(())
}

/// `tm ticket close`
pub fn ticket_close(
    args: &TicketRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;
    project
        .store
        .close(&ticket_id, None, project.actor.clone())
        .map_err(|e| friendly_lifecycle_error(e, &ticket_id, "closed", project))?;
    renderer.emit(&ticket_id, &format!("Closed ticket {}", ticket_id))?;
    Ok(())
}

/// `tm ticket cancel`
pub fn ticket_cancel(
    args: &TicketCancelArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;
    project
        .store
        .cancel(&ticket_id, args.reason.clone(), project.actor.clone())
        .map_err(|e| friendly_lifecycle_error(e, &ticket_id, "cancelled", project))?;
    renderer.emit(&ticket_id, &format!("Cancelled ticket {}", ticket_id))?;
    Ok(())
}

/// `tm ticket reopen`
pub fn ticket_reopen(
    args: &TicketRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;
    project
        .store
        .reopen(&ticket_id, None, project.actor.clone())
        .map_err(|e| friendly_lifecycle_error(e, &ticket_id, "reopened", project))?;
    renderer.emit(&ticket_id, &format!("Reopened ticket {}", ticket_id))?;
    Ok(())
}

/// `tm ticket activate`: `Draft -> Ready`, so the scheduler (or `tm run`) can pick it up.
pub fn ticket_activate(
    args: &TicketRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;
    project
        .store
        .activate(&ticket_id, project.actor.clone())
        .map_err(|e| friendly_lifecycle_error(e, &ticket_id, "activated", project))?;
    renderer.emit(
        &ticket_id,
        &format!("Activated ticket {ticket_id}: ready for a worker (tm run {ticket_id})"),
    )?;
    Ok(())
}

/// `tm ticket accept`: a human certifies a submission as done (`Submitted -> Closed`).
pub fn ticket_accept(
    args: &TicketAcceptArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;
    project
        .store
        .accept(&ticket_id, args.note.clone(), project.actor.clone())
        .map_err(|e| friendly_lifecycle_error(e, &ticket_id, "accepted", project))?;
    renderer.emit(&ticket_id, &format!("Accepted {ticket_id}: closed."))?;
    Ok(())
}

/// `tm ticket reject`: a human sends a submission back; the reason becomes part of the ticket's
/// failure history, which the next attempt's worker sees.
pub fn ticket_reject(
    args: &TicketRejectArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;
    project
        .store
        .reject(&ticket_id, args.reason.clone(), project.actor.clone())
        .map_err(|e| friendly_lifecycle_error(e, &ticket_id, "rejected", project))?;
    let state = project
        .store
        .view()?
        .tickets
        .get(&ticket_id)
        .map(|t| state_label(t.state).to_string())
        .unwrap_or_default();
    renderer.emit(
        &ticket_id,
        &format!("Rejected {ticket_id}: it is {state}, and the next attempt will see your reason."),
    )?;
    Ok(())
}

/// `tm ticket retry`
pub fn ticket_retry(
    args: &TicketRetryArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;
    project
        .store
        .retry(&ticket_id, args.guidance.clone(), project.actor.clone())
        .map_err(|e| friendly_lifecycle_error(e, &ticket_id, "retried", project))?;
    let told = if args.guidance.is_some() {
        ", and the next attempt will see your guidance"
    } else {
        ""
    };
    renderer.emit(
        &ticket_id,
        &format!("Retrying {ticket_id}: it is queued for a worker again{told}."),
    )?;
    Ok(())
}

/// The command that moves `ticket_id` off of `state`, used as the "next step" half of
/// [`friendly_lifecycle_error`]'s sentence. `None` for a terminal state with nothing left to do.
fn transition_hint(state: TicketState, ticket_id: &TicketId) -> Option<String> {
    match state {
        TicketState::Draft => Some(format!("Activate it first: tm ticket activate {ticket_id}")),
        TicketState::Blocked => Some(format!(
            "It's waiting on its dependencies. Check them: tm dep graph {ticket_id}"
        )),
        TicketState::Ready => Some(format!("Run it: tm run {ticket_id}")),
        TicketState::Leased | TicketState::Running => Some(format!(
            "It's already being worked. Check its progress: tm ticket show {ticket_id}"
        )),
        TicketState::Submitted => Some(format!("Accept it: tm ticket accept {ticket_id}")),
        TicketState::Verifying | TicketState::Auditing => Some(format!(
            "It's being checked. Check its progress: tm ticket show {ticket_id}"
        )),
        TicketState::Rework | TicketState::Replan | TicketState::Recovery => {
            Some(format!("Check its progress: tm ticket show {ticket_id}"))
        }
        TicketState::Escalated => Some(format!("Retry it: tm ticket retry {ticket_id}")),
        TicketState::Closed | TicketState::Cancelled => None,
    }
}

/// [`tm_core::machine::InvalidTransition`]'s own `Display` impl, verbatim: `"no transition from
/// {from:?} on {trigger:?}"`. `Store::*` wraps that string as-is via `e.to_string()` at every
/// `machine::transition` call site (e.g. `store.rs`'s `accept`/`reject`/`close`/`cancel`/
/// `reopen`/`activate`/`submit`), so this prefix is how [`friendly_lifecycle_error`] tells a real
/// "the state machine rejected this trigger" failure apart from `Store`'s own hand-written
/// `TmError::InvalidTransition` messages for a different reason. `Store::retry`'s own
/// not-escalated refusal (`store.rs` ~1216-1225) is state-shaped too but not prefix-shaped, so
/// the `verb == "retried"` arm below covers it separately; only `rejection_reason`'s blank-reason
/// refusal (not about state at all) is meant to pass through unrewritten.
const MACHINE_REJECTION_PREFIX: &str = "no transition from ";

/// Turn a lifecycle-verb (`activate`/`accept`/`reject`/`retry`/`close`/`cancel`/`reopen`/
/// `submit`) failure into a sentence naming the ticket, its actual state, and the exact next
/// command to run — but only when the failure is actually a state rejection: either the pure
/// state machine's own refusal (see [`MACHINE_REJECTION_PREFIX`]) or `Store::retry`'s
/// hand-written not-escalated refusal, which is state-shaped but not prefix-shaped since it
/// short-circuits before ever calling `machine::transition`. `rejection_reason`'s blank-reason
/// refusal is `Store`'s only other hand-written `InvalidTransition`, is not about state at all,
/// and passes through unchanged. `verb` is the past-tense action a person typed (e.g.
/// `"accepted"`), used to phrase "can't be accepted".
fn friendly_lifecycle_error(
    err: TmError,
    ticket_id: &TicketId,
    verb: &str,
    project: &Project,
) -> TmError {
    match err {
        TmError::InvalidTransition(msg)
            if msg.starts_with(MACHINE_REJECTION_PREFIX) || verb == "retried" =>
        {
            let state = project
                .store
                .view()
                .ok()
                .and_then(|v| v.tickets.get(ticket_id).map(|t| t.state));
            match state {
                Some(state) => {
                    let hint = transition_hint(state, ticket_id)
                        .map(|h| format!(" {h}"))
                        .unwrap_or_default();
                    // "a draft" reads better than the bare label; every other label already
                    // reads fine as a plain adjective ("is ready", "is escalated").
                    let phrase = if state == TicketState::Draft {
                        "a draft".to_string()
                    } else {
                        state_label(state).to_string()
                    };
                    TmError::InvalidTransition(format!(
                        "{ticket_id} is {phrase}, so it can't be {verb}.{hint}"
                    ))
                }
                None => {
                    TmError::InvalidTransition(format!("{ticket_id} can't be {verb} right now."))
                }
            }
        }
        // `Store::submit` checks for evidence before it ever looks at the ticket's state, so
        // this is a `TmError::Invariant` (whose `Display` prefix, "invariant violated:", isn't
        // ours to change without touching every match on `TmError` across the workspace) rather
        // than a transition rejection; remap it into `InvalidTransition` so the CLI-owned prefix
        // (`error.rs`) applies and the message names the exact command to run.
        TmError::Invariant(msg) if msg.contains("evidence") => TmError::InvalidTransition(format!(
            "{ticket_id} needs evidence before it can be submitted. Provide at least one piece \
             of evidence (code changes, test results, or documentation) with `tm ticket submit \
             {ticket_id} --evidence <path>`."
        )),
        other => other,
    }
}

/// `tm ticket tree`
pub fn ticket_tree(
    args: &TicketRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;
    let view = project.store.view()?;

    let root_ticket = view
        .tickets
        .get(&ticket_id)
        .ok_or_else(|| TmError::not_found("ticket", &ticket_id))?;

    let tree = build_tree(&ticket_id, &view.tickets);

    if renderer.is_json() {
        renderer.emit(&root_ticket, "")?;
    } else {
        renderer.emit(&root_ticket, &tree.render())?;
    }

    Ok(())
}

fn build_tree(ticket_id: &TicketId, tickets: &BTreeMap<TicketId, tm_core::ticket::Ticket>) -> Tree {
    let ticket = &tickets[ticket_id];
    let obj = &ticket.objective;
    let truncated_objective = if obj.len() > 30 {
        format!("{}...", &obj[..27])
    } else {
        obj.clone()
    };
    let label = format!(
        "{} {} {}",
        ticket.id,
        state_label(ticket.state),
        truncated_objective
    );

    let mut children = Vec::new();
    for child_id in &ticket.children {
        if let Some(_child) = tickets.get(child_id) {
            children.push(build_tree(child_id, tickets));
        }
    }

    Tree { label, children }
}

/// `tm ticket delegate`
pub fn ticket_delegate(
    args: &TicketDelegateArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;
    let view = project.store.view()?;

    let parent_ticket = view
        .tickets
        .get(&ticket_id)
        .ok_or_else(|| TmError::not_found("ticket", &ticket_id))?;

    let resources = parse_resource_claims(&args.resources)?;

    let authority = if !args.resources.is_empty() {
        let scope = PatternSet::parse(args.resources.iter().cloned())?;
        let mut narrowed = parent_ticket.authority.clone();
        narrowed.repository.read = narrowed.repository.read.intersect(&scope);
        narrowed.repository.write = narrowed.repository.write.intersect(&scope);
        narrowed
    } else {
        parent_ticket.authority.clone()
    };

    let events = project.store.create_ticket(
        TicketKind::Work,
        args.objective.clone(),
        Some(ticket_id),
        None,
        authority,
        resources,
        default_executor_requirements(),
        Vec::new(),
        Vec::new(),
        VerificationPolicy::Single,
        Budget::unlimited(),
        default_retry_policy(),
        0,
        project.actor.clone(),
    )?;

    if let Some(event) = events.first() {
        if let Some(created_id) = event_ticket_id(&event.subject) {
            renderer.emit(
                &created_id,
                &format!("Delegated to child ticket {}", created_id),
            )?;
        }
    }

    Ok(())
}

/// `tm ticket fork`: `Store::fork_ticket`, then render the new ticket id the same way `ticket
/// new`/`ticket delegate` do (`docs/decisions/D-008-ticket-checkpoint-fork.md`).
pub fn ticket_fork(
    args: &TicketForkArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let source = TicketId::new(&args.ticket)?;
    let (new_id, _events) = project
        .store
        .fork_ticket(&source, args.at, project.actor.clone())?;

    if renderer.is_json() {
        let json = serde_json::json!({
            "ticket": new_id.as_str(),
            "forked_from": source.as_str(),
            "at_seq": args.at,
        });
        renderer.emit(&json, "")?;
    } else {
        renderer.emit(
            &new_id,
            &format!("Forked {} at seq {} into {}", source, args.at, new_id),
        )?;
    }

    Ok(())
}

/// `tm ticket submit`
pub fn ticket_submit(
    args: &TicketSubmitArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let ticket_id = TicketId::new(&args.ticket)?;
    let mut evidence_ids = Vec::new();

    for path in &args.evidence {
        let bytes = fs::read(path).map_err(|e| {
            TmError::storage(format!(
                "could not read evidence file {}: {}",
                path.display(),
                e
            ))
        })?;

        let media_type = "application/octet-stream".to_string();
        let events = project.store.store_artifact(
            tm_core::artifact::ArtifactKind::File,
            media_type,
            bytes,
            serde_json::json!({}),
            Some(ticket_id.clone()),
            project.actor.clone(),
        )?;

        if let Some(event) = events.first() {
            if let Some(artifact_id) = event_artifact_id(&event.subject) {
                evidence_ids.push(artifact_id.clone());
            }
        }
    }

    let summary = args.summary.clone().unwrap_or_default();
    project
        .store
        .submit(&ticket_id, summary, evidence_ids, project.actor.clone())
        .map_err(|e| friendly_lifecycle_error(e, &ticket_id, "submitted", project))?;
    renderer.emit(&ticket_id, &format!("Submitted ticket {}", ticket_id))?;

    Ok(())
}

/// Dispatch one [`DepCommand`].
pub fn dispatch_dep(
    cmd: &DepCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        DepCommand::Add(args) => dep_add(args, project, renderer),
        DepCommand::Rm(args) => dep_rm(args, project, renderer),
        DepCommand::Graph(args) => dep_graph(args, project, renderer),
    }
}

/// `tm dep add`
pub fn dep_add(args: &DepEdgeArgs, project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let ticket = TicketId::new(&args.ticket)?;
    let depends_on = TicketId::new(&args.depends_on)?;

    let kind = match args.kind.as_str() {
        "blocks" => DependencyKind::Hard,
        "loop" => DependencyKind::Loop,
        k => {
            return Err(TmError::parse(format!(
                "unknown dependency kind '{}' (expected blocks or loop)",
                k
            )))
        }
    };

    project
        .store
        .add_dependency(&ticket, &depends_on, kind, project.actor.clone())?;
    renderer.emit(
        &ticket,
        &format!("Added dependency: {} -> {}", ticket, depends_on),
    )?;

    Ok(())
}

/// `tm dep rm`
pub fn dep_rm(args: &DepEdgeArgs, project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let ticket = TicketId::new(&args.ticket)?;
    let depends_on = TicketId::new(&args.depends_on)?;

    project
        .store
        .remove_dependency(&ticket, &depends_on, project.actor.clone())?;
    renderer.emit(
        &ticket,
        &format!("Removed dependency: {} -> {}", ticket, depends_on),
    )?;

    Ok(())
}

/// `tm dep graph`
pub fn dep_graph(
    args: &DepGraphArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let view = project.store.view()?;

    let root = args.ticket.as_deref().map(TicketId::new).transpose()?;
    if let Some(root) = &root {
        if !view.tickets.contains_key(root) {
            return Err(TmError::not_found("ticket", root));
        }
    }

    // With a `TICKET` root, both `--json` and the human list are scoped to just its transitive
    // dependencies/dependents (the whole-project graph is still `tm dep graph` with no argument).
    let rooted = root.as_ref().map(|r| rooted_subgraph(&view.graph, r));
    let graph = rooted.as_ref().unwrap_or(&view.graph);

    let graph_view = GraphView::from(graph);
    if renderer.is_json() {
        return renderer.emit(&graph_view, "");
    }

    let text = format_dep_graph_text(graph, &view.tickets, root.as_ref());

    renderer.emit(&graph_view, &text)?;

    Ok(())
}

/// The subgraph of `graph` reachable from `root` by following dependency edges in either
/// direction (its transitive dependencies plus its transitive dependents), including `root`
/// itself. Used so `tm dep graph <ticket>` (both `--json` and the human list) only ever shows
/// edges relevant to that ticket, not the whole project's graph.
fn rooted_subgraph(
    graph: &tm_core::graph::DependencyGraph,
    root: &TicketId,
) -> tm_core::graph::DependencyGraph {
    let mut relevant: std::collections::BTreeSet<TicketId> = graph.ancestors(root);
    relevant.extend(graph.descendants(root));
    relevant.insert(root.clone());
    let edges: Vec<tm_core::graph::DependencyEdge> = graph
        .edges()
        .iter()
        .filter(|e| relevant.contains(&e.from) && relevant.contains(&e.to))
        .cloned()
        .collect();
    tm_core::graph::DependencyGraph::build(relevant, edges, [])
}

/// Render `graph` as a human-readable, indented list of edges (each `T-A (objective) -> T-B
/// (objective) (kind)`), instead of `DependencyGraph`'s `Debug` form. `graph` is expected to
/// already be scoped to `root` (via [`rooted_subgraph`]) when `root` is `Some`; this only adds
/// the header line and formats what it's given.
fn format_dep_graph_text(
    graph: &tm_core::graph::DependencyGraph,
    tickets: &BTreeMap<TicketId, tm_core::ticket::Ticket>,
    root: Option<&TicketId>,
) -> String {
    let edges = graph.edges();

    let describe = |id: &TicketId| match tickets.get(id) {
        Some(t) => format!("{} ({})", id, t.objective),
        None => id.to_string(),
    };

    let mut text = if let Some(root) = root {
        format!("Dependency graph (subgraph from {}):", root)
    } else {
        "Dependency graph:".to_string()
    };

    if edges.is_empty() {
        text.push('\n');
        text.push_str("No dependencies yet. Add one: tm dep add <ticket> <depends-on>");
    } else {
        for e in edges {
            text.push('\n');
            text.push_str(&format!(
                "  {} -> {} ({})",
                describe(&e.from),
                describe(&e.to),
                dep_kind_label(e.kind)
            ));
        }
    }

    text
}

/// Dispatch one [`MilestoneCommand`].
pub fn dispatch_milestone(
    cmd: &MilestoneCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        MilestoneCommand::List => milestone_list(project, renderer),
        MilestoneCommand::New(args) => milestone_new(args, project, renderer),
        MilestoneCommand::Show(args) => milestone_show(args, project, renderer),
        MilestoneCommand::Close(args) => milestone_close(args, project, renderer),
        MilestoneCommand::Reopen(args) => milestone_reopen(args, project, renderer),
    }
}

/// `tm milestone list`
pub fn milestone_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let view = project.store.view()?;

    let mut milestones: Vec<_> = view.milestones.values().collect();
    milestones.sort_by_key(|m| &m.id);
    let milestone_views: Vec<MilestoneView> = milestones.iter().map(|m| (*m).into()).collect();

    if renderer.is_json() {
        renderer.emit(&milestone_views, "")?;
    } else if milestones.is_empty() {
        renderer.emit(
            &milestone_views,
            "No milestones yet. Create one: tm milestone new \"<title>\"\n",
        )?;
    } else {
        let headers = vec!["ID".to_string(), "State".to_string(), "Tickets".to_string()];
        let rows: Vec<Vec<String>> = milestones
            .iter()
            .map(|m| {
                vec![
                    m.id.to_string(),
                    milestone_state_label(m.state).to_string(),
                    milestone_members(m, &view.tickets).len().to_string(),
                ]
            })
            .collect();
        let table = Table::new(headers, rows);
        renderer.emit(&milestone_views, &table.render())?;
    }

    Ok(())
}

/// `tm milestone new` (alias `tm milestone create`)
pub fn milestone_new(
    args: &MilestoneNewArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let tickets = args
        .tickets
        .iter()
        .map(TicketId::new)
        .collect::<tm_types::Result<Vec<_>>>()?;

    let events = project.store.create_milestone(
        args.title.clone(),
        tickets,
        Vec::new(),
        project.actor.clone(),
    )?;

    let created_id = events.first().and_then(|e| event_milestone_id(&e.subject));
    match created_id {
        Some(id) => renderer.emit(&id, &format!("Created {}: {}", id, args.title))?,
        None => renderer.emit(&(), &format!("Created milestone: {}", args.title))?,
    }

    Ok(())
}

/// `tm milestone show`: the milestone's title, state, and each member ticket with its state
/// label, plus a done/total count (done = `Closed` or `Cancelled`, matching what
/// [`tm_core::milestone::MilestoneStore::close`] requires of every member).
pub fn milestone_show(
    args: &MilestoneRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let milestone_id = MilestoneId::new(&args.milestone)?;
    let view = project.store.view()?;

    let milestone = view
        .milestones
        .get(&milestone_id)
        .ok_or_else(|| TmError::not_found("milestone", &milestone_id))?;

    let members = milestone_members(milestone, &view.tickets);

    if renderer.is_json() {
        let json = serde_json::json!({
            "id": milestone.id.as_str(),
            "title": milestone.title,
            "state": milestone.state,
            "closed_by": milestone.closed_by,
            "assumptions": milestone.assumptions,
            "tickets": members,
        });
        renderer.emit(&json, "")?;
        return Ok(());
    }

    let text = format_milestone_text(milestone, &members, &view.tickets);
    let milestone_view: MilestoneView = milestone.into();
    renderer.emit(&milestone_view, &text)?;

    Ok(())
}

/// The tickets attached to `milestone`. `milestone.created`'s payload carries no ticket list at
/// all (just the id and title), and the `milestones` SQL table's own `tickets` column is always
/// written as `[]` and never updated afterward. `tm_core::Store::view` already reconstructs
/// `Milestone.tickets` by unioning that (always-empty) column with every ticket whose own
/// `milestone` field points at this id, so a `Milestone` obtained from `Store::view` already
/// carries real membership. This helper exists for a `Milestone` obtained some other way (e.g.
/// without a full view's `tickets` map already applied) -- it does the same union, deduplicated
/// in ticket-id order, and is a harmless no-op when `milestone.tickets` is already complete.
fn milestone_members(
    milestone: &tm_core::milestone::Milestone,
    tickets: &BTreeMap<TicketId, tm_core::ticket::Ticket>,
) -> Vec<TicketId> {
    let mut members: std::collections::BTreeSet<TicketId> =
        milestone.tickets.iter().cloned().collect();
    for (id, ticket) in tickets {
        if ticket.milestone.as_ref() == Some(&milestone.id) {
            members.insert(id.clone());
        }
    }
    members.into_iter().collect()
}

/// The plain-text body of `tm milestone show`. Pulled out of [`milestone_show`] so it can be
/// unit-tested against a constructed [`tm_core::milestone::Milestone`] and ticket map without a
/// real [`Project`]/[`tm_core::Store`].
fn format_milestone_text(
    milestone: &tm_core::milestone::Milestone,
    members: &[TicketId],
    tickets: &BTreeMap<TicketId, tm_core::ticket::Ticket>,
) -> String {
    let done = members
        .iter()
        .filter(|t| {
            matches!(
                tickets.get(*t).map(|t| t.state),
                Some(TicketState::Closed) | Some(TicketState::Cancelled)
            )
        })
        .count();
    let total = members.len();

    let mut text = format!("ID:      {}\n", milestone.id);
    text.push_str(&format!("Title:   {}\n", milestone.title));
    text.push_str(&format!(
        "State:   {}\n",
        milestone_state_label(milestone.state)
    ));
    text.push_str(&format!("Tickets: {done}/{total} done\n"));
    if members.is_empty() {
        text.push_str("  (no tickets yet)\n");
    } else {
        for ticket_id in members {
            let state = tickets
                .get(ticket_id)
                .map(|t| state_label(t.state).to_string())
                .unwrap_or_else(|| "unknown".to_string());
            text.push_str(&format!("  {ticket_id}  {state}\n"));
        }
    }
    text
}

/// `tm milestone close`
pub fn milestone_close(
    args: &MilestoneRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let milestone_id = MilestoneId::new(&args.milestone)?;
    project
        .store
        .close_milestone(&milestone_id, project.actor.clone())?;
    renderer.emit(&milestone_id, &format!("Closed milestone {}", milestone_id))?;
    Ok(())
}

/// `tm milestone reopen`
pub fn milestone_reopen(
    args: &MilestoneRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let milestone_id = MilestoneId::new(&args.milestone)?;
    project
        .store
        .reopen_milestone(&milestone_id, project.actor.clone())?;
    renderer.emit(
        &milestone_id,
        &format!("Reopened milestone {}", milestone_id),
    )?;
    Ok(())
}

/// Dispatch one [`DecisionCommand`].
pub fn dispatch_decision(
    cmd: &DecisionCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        DecisionCommand::List => decision_list(project, renderer),
        DecisionCommand::Show(args) => decision_show(args, project, renderer),
        DecisionCommand::New(args) => decision_new(args, project, renderer),
        DecisionCommand::Supersede(args) => decision_supersede(args, project, renderer),
    }
}

/// `tm decision list`
pub fn decision_list(project: &Project, renderer: &Renderer) -> tm_types::Result<()> {
    let view = project.store.view()?;

    let mut decisions: Vec<_> = view.decisions.values().collect();
    decisions.sort_by_key(|d| &d.id);
    let decision_views: Vec<DecisionView> = decisions.iter().map(|d| (*d).into()).collect();

    if renderer.is_json() {
        renderer.emit(&decision_views, "")?;
    } else {
        let headers = vec![
            "ID".to_string(),
            "Status".to_string(),
            "Summary".to_string(),
        ];
        let rows: Vec<Vec<String>> = decisions
            .iter()
            .map(|d| {
                let status = if d.is_active() {
                    "Active"
                } else {
                    "Superseded"
                }
                .to_string();
                let summary = if d.subject.len() > 40 {
                    format!("{}...", &d.subject[..37])
                } else {
                    d.subject.clone()
                };
                vec![d.id.to_string(), status, summary]
            })
            .collect();
        let table = Table::new(headers, rows);
        renderer.emit(&decision_views, &table.render())?;
    }

    Ok(())
}

/// `tm decision show`
pub fn decision_show(
    args: &DecisionRefArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let decision_id = DecisionId::new(&args.decision)?;
    let view = project.store.view()?;

    let decision = view
        .decisions
        .get(&decision_id)
        .ok_or_else(|| TmError::not_found("decision", &decision_id))?;

    let decision_view = DecisionView::from(decision);

    if renderer.is_json() {
        renderer.emit(&decision_view, "")?;
    } else {
        let mut text = format!("ID:       {}\n", decision.id);
        text.push_str(&format!("Active:   {}\n", decision.is_active()));
        text.push_str(&format!("Subject:  {}\n", decision.subject));
        text.push_str(&format!("Decision:\n{}\n", decision.decision));
        text.push_str(&format!("Reason:\n{}\n", decision.reason));
        if let Some(supersedes) = &decision.supersedes {
            text.push_str(&format!("Supersedes: {}\n", supersedes));
        }
        if let Some(superseded_by) = &decision.superseded_by {
            text.push_str(&format!("Superseded by: {}\n", superseded_by));
        }
        renderer.emit(&decision_view, &text)?;
    }

    Ok(())
}

/// `tm decision new`
pub fn decision_new(
    args: &DecisionNewArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let rationale = args.rationale.clone().unwrap_or_default();

    let events = project.store.record_decision(
        "decision".to_string(),
        args.summary.clone(),
        rationale,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        project.actor.clone(),
    )?;

    if let Some(event) = events.first() {
        if let Some(created_id) = event_decision_id(&event.subject) {
            renderer.emit(&created_id, &format!("Created decision {}", created_id))?;
        }
    }

    Ok(())
}

/// `tm decision supersede`
pub fn decision_supersede(
    args: &DecisionSupersedeArgs,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    let decision_id = DecisionId::new(&args.decision)?;
    let rationale = args.rationale.clone().unwrap_or_default();

    let events = project.store.supersede(
        &decision_id,
        "decision".to_string(),
        args.summary.clone(),
        rationale,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        project.actor.clone(),
    )?;

    if let Some(event) = events.first() {
        if let Some(new_id) = event_decision_id(&event.subject) {
            renderer.emit(
                &new_id,
                &format!("Created decision {} superseding {}", new_id, decision_id),
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tree_creates_leaf_for_no_children() {
        let mut tickets = BTreeMap::new();
        let id = TicketId::new("T-1").unwrap();
        tickets.insert(
            id.clone(),
            tm_core::ticket::Ticket {
                id: id.clone(),
                objective: "test".to_string(),
                kind: TicketKind::Work,
                parent: None,
                children: vec![],
                dependencies: vec![],
                milestone: None,
                state: TicketState::Draft,
                priority: 0,
                authority: Authority::default(),
                resources: vec![],
                executor: default_executor_requirements(),
                context_refs: vec![],
                success: vec![],
                verification: VerificationPolicy::Single,
                budget: Budget::unlimited(),
                retry: default_retry_policy(),
                attempts: 0,
                failures: vec![],
                created: tm_types::Timestamp::parse_rfc3339("2024-01-01T00:00:00Z").unwrap(),
                updated: tm_types::Timestamp::parse_rfc3339("2024-01-01T00:00:00Z").unwrap(),
                cycle: None,
            },
        );

        let tree = build_tree(&id, &tickets);
        assert!(tree.children.is_empty());
        assert!(tree.label.contains("T-1"));
    }

    #[test]
    fn ticket_id_parse_valid() {
        let id = TicketId::new("T-42");
        assert!(id.is_ok());
    }

    #[test]
    fn ticket_id_parse_invalid() {
        let id = TicketId::new("INVALID");
        assert!(id.is_err());
    }

    #[test]
    fn milestone_id_parse_valid() {
        let id = MilestoneId::new("M-1");
        assert!(id.is_ok());
    }

    #[test]
    fn milestone_id_parse_invalid() {
        let id = MilestoneId::new("T-1");
        assert!(id.is_err());
    }

    #[test]
    fn decision_id_parse_valid() {
        let id = DecisionId::new("D-001");
        assert!(id.is_ok());
    }

    #[test]
    fn decision_id_parse_invalid() {
        let id = DecisionId::new("INVALID");
        assert!(id.is_err());
    }

    #[test]
    fn dependency_kind_blocks() {
        let kind_result: tm_types::Result<DependencyKind> = match "blocks" {
            "blocks" => Ok(DependencyKind::Hard),
            "loop" => Ok(DependencyKind::Loop),
            k => Err(TmError::parse(format!("unknown dependency kind: {}", k))),
        };
        assert!(kind_result.is_ok());
    }

    #[test]
    fn dependency_kind_loop() {
        let kind_result: tm_types::Result<DependencyKind> = match "loop" {
            "blocks" => Ok(DependencyKind::Hard),
            "loop" => Ok(DependencyKind::Loop),
            k => Err(TmError::parse(format!("unknown dependency kind: {}", k))),
        };
        assert!(kind_result.is_ok());
    }

    #[test]
    fn dependency_kind_invalid() {
        let kind_result: tm_types::Result<DependencyKind> = match "unknown" {
            "blocks" => Ok(DependencyKind::Hard),
            "loop" => Ok(DependencyKind::Loop),
            k => Err(TmError::parse(format!("unknown dependency kind: {}", k))),
        };
        assert!(kind_result.is_err());
    }

    #[test]
    fn every_filterable_state_label_round_trips_through_the_state_filter_flag() {
        // For every `TicketStateArg` variant `--state` accepts: state_from_arg maps it to a
        // TicketState, state_label renders that TicketState, and the flag's own parser must read
        // that word back to a state_from_arg result that maps to the same TicketState. Driven
        // from `TicketStateArg::value_variants()`, not a hand-copied list, so a variant added to
        // args.rs without a matching arm here fails loudly instead of being silently skipped.
        use clap::ValueEnum;

        for arg in TicketStateArg::value_variants() {
            let state = state_from_arg(*arg);
            let label = state_label(state);
            let parsed = TicketStateArg::from_str(label, true)
                .unwrap_or_else(|e| panic!("label {label:?} didn't parse back: {e}"));
            assert_eq!(
                state_from_arg(parsed),
                state,
                "state_label({state:?}) = {label:?} didn't round-trip back to {state:?}"
            );
        }
    }

    #[test]
    fn every_ticket_state_has_a_lowercase_label_and_the_filterable_ones_round_trip() {
        // `TicketState` has more variants than `--state` exposes as a filter (Leased, Submitted,
        // Rework, Replan, Recovery, Escalated aren't filterable today) -- assert every state
        // still gets a real lowercase label, and additionally round-trip the ones `--state` does
        // support.
        use clap::ValueEnum;

        let not_filterable = [
            TicketState::Leased,
            TicketState::Submitted,
            TicketState::Rework,
            TicketState::Replan,
            TicketState::Recovery,
            TicketState::Escalated,
        ];

        for state in TicketState::ALL {
            let label = state_label(*state);
            assert!(!label.is_empty());
            assert_eq!(label, label.to_ascii_lowercase());

            if not_filterable.contains(state) {
                continue;
            }
            let parsed = TicketStateArg::from_str(label, true).unwrap_or_else(|e| {
                panic!("filterable state {state:?}'s label {label:?} didn't parse: {e}")
            });
            assert_eq!(state_from_arg(parsed), *state);
        }
    }

    fn sample_ticket(id: &str, objective: &str) -> tm_core::ticket::Ticket {
        tm_core::ticket::Ticket {
            id: TicketId::new(id).unwrap(),
            objective: objective.to_string(),
            kind: TicketKind::Work,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            state: TicketState::Draft,
            priority: 0,
            authority: Authority::default(),
            resources: vec![],
            executor: default_executor_requirements(),
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: default_retry_policy(),
            attempts: 0,
            failures: vec![],
            created: tm_types::Timestamp::parse_rfc3339("2024-01-01T00:00:00Z").unwrap(),
            updated: tm_types::Timestamp::parse_rfc3339("2024-01-01T00:00:00Z").unwrap(),
            cycle: None,
        }
    }

    #[test]
    fn format_ticket_text_has_no_debug_struct_syntax() {
        let ticket = sample_ticket("T-1", "fix the thing");
        let text = format_ticket_text(&ticket);
        assert!(!text.contains('{'), "text contained a struct brace: {text}");
        assert!(!text.contains("Some("), "text contained Some(...): {text}");
        assert!(
            !text.contains("18446744073709551615"),
            "text contained raw u64::MAX: {text}"
        );
        assert!(text.contains("Budget:       unlimited"));
        assert!(text.contains("State:        draft"));
        assert!(text.contains("Depends on:   none"));
        assert!(text.contains("Children:     none"));
    }

    #[test]
    fn multi_byte_objective_truncation_does_not_panic() {
        // ticket_list truncates a long objective with `.chars().take(47)` rather than a raw
        // byte-slice `&s[..47]`, which would panic here since `😀` is a 4-byte UTF-8 scalar that
        // byte offset 47 lands in the middle of.
        let objective: String = "😀".repeat(60);
        let truncated = if objective.chars().count() > 50 {
            format!("{}...", objective.chars().take(47).collect::<String>())
        } else {
            objective.clone()
        };
        assert!(truncated.ends_with("..."));
        assert_eq!(truncated.chars().count(), 47 + 3);
    }

    fn test_project(root: &std::path::Path) -> Project {
        use std::sync::Arc;
        use tm_types::{Clock, CounterIds, FixedClock, IdSource};
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Arc::new(
            tm_core::Store::open_with(root, clock.clone(), ids.clone()).expect("open store"),
        );
        Project::for_test(root, store, clock, ids)
    }

    #[test]
    fn milestone_members_unions_the_stale_milestone_tickets_field_with_each_ticket_own_field() {
        let milestone_id = MilestoneId::new("M-1").unwrap();
        let t1 = TicketId::new("T-1").unwrap();
        let t2 = TicketId::new("T-2").unwrap();
        // `Milestone.tickets` lists only T-1 here (standing in for the field's one real state
        // in this codebase: always stale, see `milestone_members`'s doc comment); T-2 is
        // attached only via its own `milestone` field, the way real membership actually works.
        let milestone = tm_core::milestone::Milestone {
            id: milestone_id.clone(),
            title: "v0".to_string(),
            tickets: vec![t1.clone()],
            state: tm_core::milestone::MilestoneState::Open,
            closed_by: None,
            assumptions: vec![],
        };

        let make_ticket = |id: TicketId, state: TicketState, milestone| tm_core::ticket::Ticket {
            id: id.clone(),
            objective: "test".to_string(),
            kind: TicketKind::Work,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone,
            state,
            priority: 0,
            authority: Authority::default(),
            resources: vec![],
            executor: default_executor_requirements(),
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: default_retry_policy(),
            attempts: 0,
            failures: vec![],
            created: tm_types::Timestamp::parse_rfc3339("2024-01-01T00:00:00Z").unwrap(),
            updated: tm_types::Timestamp::parse_rfc3339("2024-01-01T00:00:00Z").unwrap(),
            cycle: None,
        };

        let mut tickets = BTreeMap::new();
        tickets.insert(
            t1.clone(),
            make_ticket(t1.clone(), TicketState::Closed, Some(milestone_id.clone())),
        );
        tickets.insert(
            t2.clone(),
            make_ticket(t2.clone(), TicketState::Ready, Some(milestone_id.clone())),
        );

        let members = milestone_members(&milestone, &tickets);
        assert_eq!(members, vec![t1.clone(), t2.clone()]);

        let text = format_milestone_text(&milestone, &members, &tickets);
        assert!(text.contains("Title:   v0"));
        assert!(text.contains("Tickets: 1/2 done"));
        assert!(text.contains("T-1"));
        assert!(text.contains("T-2"));
    }

    #[test]
    fn milestone_new_then_show_lists_the_attached_ticket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let renderer = Renderer::new(false, true, true, false);

        let events = project
            .store
            .create_ticket(
                TicketKind::Work,
                "do the thing".to_string(),
                None,
                None,
                Authority::worker(),
                vec![],
                default_executor_requirements(),
                Vec::new(),
                Vec::new(),
                VerificationPolicy::Single,
                Budget::unlimited(),
                default_retry_policy(),
                0,
                project.actor.clone(),
            )
            .expect("create ticket");
        let ticket_id = event_ticket_id(&events[0].subject).expect("ticket id");

        milestone_new(
            &MilestoneNewArgs {
                title: "v0".to_string(),
                tickets: vec![ticket_id.to_string()],
            },
            &project,
            &renderer,
        )
        .expect("milestone new");

        let view = project.store.view().expect("view");
        let milestone = view.milestones.values().next().expect("one milestone");
        assert_eq!(milestone.title, "v0");
        // `tm_core::Store::view` already unions each ticket's own `milestone` pointer into
        // `Milestone.tickets` on read (`crates/tm-core/src/store.rs`'s materialization of the
        // `milestones` table), so it's already populated here -- `milestone_members` below is a
        // second, idempotent union over the same data, useful when a caller only has a
        // `Milestone` without a full view.
        assert_eq!(milestone.tickets, vec![ticket_id.clone()]);
        let members = milestone_members(milestone, &view.tickets);
        assert_eq!(members, vec![ticket_id.clone()]);

        let text = format_milestone_text(milestone, &members, &view.tickets);
        assert!(
            text.contains(ticket_id.as_str()),
            "milestone show text {text:?} should list {ticket_id}"
        );
        assert!(text.contains("Tickets: 0/1 done"));

        // `tm milestone show` on that id must succeed and not error.
        milestone_show(
            &MilestoneRefArgs {
                milestone: milestone.id.to_string(),
            },
            &project,
            &renderer,
        )
        .expect("milestone show");
    }

    #[test]
    fn ticket_new_with_unknown_milestone_errors_clearly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let renderer = Renderer::new(false, true, true, false);

        let err = ticket_new(
            &TicketNewArgs {
                objective: "x".to_string(),
                kind: "task".to_string(),
                parent: None,
                milestone: Some("M-99".to_string()),
                priority: 0,
                resources: vec![],
            },
            &project,
            &renderer,
        )
        .expect_err("M-99 does not exist");

        let message = err.to_string();
        assert!(
            message.contains("no milestone M-99"),
            "unexpected error message: {message}"
        );

        // Confirm no ticket was silently created with the milestone dropped.
        let view = project.store.view().expect("view");
        assert!(view.tickets.is_empty());
    }

    #[test]
    fn accepting_a_draft_ticket_names_the_state_and_the_next_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let renderer = Renderer::new(false, true, true, false);

        let events = project
            .store
            .create_ticket(
                TicketKind::Work,
                "do the thing".to_string(),
                None,
                None,
                Authority::worker(),
                vec![],
                default_executor_requirements(),
                Vec::new(),
                Vec::new(),
                VerificationPolicy::Single,
                Budget::unlimited(),
                default_retry_policy(),
                0,
                project.actor.clone(),
            )
            .expect("create ticket");
        let ticket_id = event_ticket_id(&events[0].subject).expect("ticket id");

        let err = ticket_accept(
            &TicketAcceptArgs {
                ticket: ticket_id.to_string(),
                note: None,
            },
            &project,
            &renderer,
        )
        .expect_err("a draft ticket can't be accepted");
        let message = err.to_string();

        assert!(
            message.contains(ticket_id.as_str()) && message.contains("draft"),
            "message should name the ticket and its state: {message:?}"
        );
        assert!(
            message.contains("tm ticket activate"),
            "message should name the next command: {message:?}"
        );
        assert!(
            !message.contains("InvalidTransition")
                && !message.contains("Draft")
                && !message.contains("trigger")
                && !message.contains("invariant violated"),
            "message leaked internal debug output: {message:?}"
        );
    }

    #[test]
    fn submitting_without_evidence_names_the_submit_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let renderer = Renderer::new(false, true, true, false);

        let events = project
            .store
            .create_ticket(
                TicketKind::Work,
                "do the thing".to_string(),
                None,
                None,
                Authority::worker(),
                vec![],
                default_executor_requirements(),
                Vec::new(),
                Vec::new(),
                VerificationPolicy::Single,
                Budget::unlimited(),
                default_retry_policy(),
                0,
                project.actor.clone(),
            )
            .expect("create ticket");
        let ticket_id = event_ticket_id(&events[0].subject).expect("ticket id");

        let err = ticket_submit(
            &TicketSubmitArgs {
                ticket: ticket_id.to_string(),
                summary: None,
                evidence: vec![],
            },
            &project,
            &renderer,
        )
        .expect_err("submitting with no evidence must fail");
        let message = err.to_string();

        assert!(
            message.contains("tm ticket submit") && message.contains("--evidence"),
            "message should name the exact command to run: {message:?}"
        );
        assert!(
            !message.contains("invariant violated"),
            "message leaked the internal invariant prefix: {message:?}"
        );
    }

    #[test]
    fn retrying_a_draft_ticket_names_the_state_and_the_next_command() {
        // `Store::retry`'s not-escalated refusal is state-shaped but hand-written, not routed
        // through `machine::transition`, so it doesn't carry `MACHINE_REJECTION_PREFIX` --
        // `friendly_lifecycle_error` special-cases `verb == "retried"` to cover it anyway.
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let renderer = Renderer::new(false, true, true, false);

        let events = project
            .store
            .create_ticket(
                TicketKind::Work,
                "do the thing".to_string(),
                None,
                None,
                Authority::worker(),
                vec![],
                default_executor_requirements(),
                Vec::new(),
                Vec::new(),
                VerificationPolicy::Single,
                Budget::unlimited(),
                default_retry_policy(),
                0,
                project.actor.clone(),
            )
            .expect("create ticket");
        let ticket_id = event_ticket_id(&events[0].subject).expect("ticket id");

        let err = ticket_retry(
            &TicketRetryArgs {
                ticket: ticket_id.to_string(),
                guidance: None,
            },
            &project,
            &renderer,
        )
        .expect_err("a draft ticket can't be retried");
        let message = err.to_string();

        assert!(
            message.contains(ticket_id.as_str()) && message.contains("can't be retried"),
            "message should name the ticket and the failed verb: {message:?}"
        );
        assert!(
            message.contains("tm ticket activate"),
            "message should name the next command: {message:?}"
        );
        assert!(
            !message.contains("only an escalated ticket can be retried"),
            "message should not leak Store's own hand-written refusal text: {message:?}"
        );
    }

    #[test]
    fn friendly_lifecycle_error_leaves_non_machine_invalid_transition_messages_alone() {
        // `Store::rejection_reason`'s blank-reason refusal is `TmError::InvalidTransition` too,
        // but it isn't about state at all -- rewriting it with a state/next-command sentence
        // would be actively misleading.
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());

        let events = project
            .store
            .create_ticket(
                TicketKind::Work,
                "do the thing".to_string(),
                None,
                None,
                Authority::worker(),
                vec![],
                default_executor_requirements(),
                Vec::new(),
                Vec::new(),
                VerificationPolicy::Single,
                Budget::unlimited(),
                default_retry_policy(),
                0,
                project.actor.clone(),
            )
            .expect("create ticket");
        let ticket_id = event_ticket_id(&events[0].subject).expect("ticket id");

        let original = TmError::InvalidTransition("Rejecting ticket T-1 needs a reason: describe what was wrong so the next attempt knows what to fix".to_string());
        let rewritten = friendly_lifecycle_error(original, &ticket_id, "rejected", &project);
        let message = rewritten.to_string();
        assert!(
            message.contains("needs a reason"),
            "a non-machine InvalidTransition message must pass through unchanged: {message:?}"
        );
        assert!(
            !message.contains("so it can't be rejected"),
            "must not be rewritten with an unrelated state/command sentence: {message:?}"
        );
    }

    fn dep_test_ticket(id: TicketId, objective: &str) -> tm_core::ticket::Ticket {
        tm_core::ticket::Ticket {
            id: id.clone(),
            objective: objective.to_string(),
            kind: TicketKind::Work,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            state: TicketState::Ready,
            priority: 0,
            authority: Authority::default(),
            resources: vec![],
            executor: default_executor_requirements(),
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: default_retry_policy(),
            attempts: 0,
            failures: vec![],
            created: tm_types::Timestamp::parse_rfc3339("2024-01-01T00:00:00Z").unwrap(),
            updated: tm_types::Timestamp::parse_rfc3339("2024-01-01T00:00:00Z").unwrap(),
            cycle: None,
        }
    }

    #[test]
    fn format_dep_graph_text_lists_readable_edges_not_struct_debug() {
        let t1 = TicketId::new("T-1").unwrap();
        let t2 = TicketId::new("T-2").unwrap();
        let graph = tm_core::graph::DependencyGraph::build(
            [t1.clone(), t2.clone()],
            [tm_core::graph::DependencyEdge {
                from: t1.clone(),
                to: t2.clone(),
                kind: DependencyKind::Hard,
            }],
            [],
        );
        let mut tickets = BTreeMap::new();
        tickets.insert(t1.clone(), dep_test_ticket(t1.clone(), "fix login"));
        tickets.insert(t2.clone(), dep_test_ticket(t2.clone(), "add tests"));

        let text = format_dep_graph_text(&graph, &tickets, None);

        assert_eq!(
            text,
            "Dependency graph:\n  T-1 (fix login) -> T-2 (add tests) (blocks)"
        );
        assert!(
            !text.contains("DependencyGraph"),
            "must not fall back to Debug output: {text:?}"
        );
    }

    #[test]
    fn format_dep_graph_text_with_no_edges_says_no_dependencies_yet() {
        let graph = tm_core::graph::DependencyGraph::default();
        let text = format_dep_graph_text(&graph, &BTreeMap::new(), None);
        assert_eq!(
            text,
            "Dependency graph:\nNo dependencies yet. Add one: tm dep add <ticket> <depends-on>"
        );
    }

    #[test]
    fn rooted_subgraph_keeps_only_the_roots_transitive_deps_and_dependents() {
        // T-1 -> T-2 -> T-3 is T-2's subgraph; T-4 -> T-5 is unrelated and must be dropped.
        let ids: Vec<TicketId> = (1..=5)
            .map(|n| TicketId::new(format!("T-{n}")).unwrap())
            .collect();
        let edge = |from: usize, to: usize| tm_core::graph::DependencyEdge {
            from: ids[from - 1].clone(),
            to: ids[to - 1].clone(),
            kind: DependencyKind::Hard,
        };
        let graph = tm_core::graph::DependencyGraph::build(
            ids.clone(),
            [edge(1, 2), edge(2, 3), edge(4, 5)],
            [],
        );

        let sub = rooted_subgraph(&graph, &ids[1]);

        assert_eq!(sub.edges().len(), 2, "edges: {:?}", sub.edges());
        assert!(sub
            .edges()
            .iter()
            .all(|e| e.from != ids[3] && e.to != ids[4]));
        let nodes: std::collections::BTreeSet<_> = sub.nodes().cloned().collect();
        assert!(nodes.contains(&ids[0]) && nodes.contains(&ids[1]) && nodes.contains(&ids[2]));
        assert!(
            !nodes.contains(&ids[3]) && !nodes.contains(&ids[4]),
            "unrelated ticket must not appear in the subgraph's nodes: {nodes:?}"
        );
    }

    #[test]
    fn format_dep_graph_text_with_root_prints_the_subgraph_header() {
        let t1 = TicketId::new("T-1").unwrap();
        let t2 = TicketId::new("T-2").unwrap();
        let graph = tm_core::graph::DependencyGraph::build(
            [t1.clone(), t2.clone()],
            [tm_core::graph::DependencyEdge {
                from: t1.clone(),
                to: t2.clone(),
                kind: DependencyKind::Hard,
            }],
            [],
        );
        let mut tickets = BTreeMap::new();
        tickets.insert(t1.clone(), dep_test_ticket(t1.clone(), "x"));
        tickets.insert(t2.clone(), dep_test_ticket(t2.clone(), "y"));

        let text = format_dep_graph_text(&graph, &tickets, Some(&t2));

        assert!(text.starts_with("Dependency graph (subgraph from T-2):"));
        assert!(text.contains("T-1 (x) -> T-2 (y) (blocks)"));
    }

    #[test]
    fn dep_graph_errors_not_found_for_an_unknown_root_ticket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let renderer = Renderer::new(false, true, true, false);

        let args = DepGraphArgs {
            ticket: Some("T-99".to_string()),
        };
        let err = dep_graph(&args, &project, &renderer).expect_err("unknown ticket must error");
        assert_eq!(err.to_string(), "not found: ticket T-99");
    }
}
