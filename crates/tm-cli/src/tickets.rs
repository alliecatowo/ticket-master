//! The `ticket`, `dep`, `milestone`, and `decision` command groups: everything that reads or
//! mutates the project graph directly through [`tm_core::store::Store`], without going through
//! the scheduler or an executor.

use crate::args::{
    DecisionCommand, DecisionNewArgs, DecisionRefArgs, DecisionSupersedeArgs, DepCommand,
    DepEdgeArgs, DepGraphArgs, MilestoneCommand, MilestoneRefArgs, TicketAcceptArgs,
    TicketCancelArgs, TicketCommand, TicketDelegateArgs, TicketEditArgs, TicketForkArgs,
    TicketListArgs, TicketNewArgs, TicketRefArgs, TicketRejectArgs, TicketRetryArgs,
    TicketStateArg, TicketSubmitArgs, TicketsArgs,
};
use crate::project::Project;

pub mod overview;
use crate::render::{Renderer, Table, Tree};
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
        other => Err(TmError::parse(format!("unknown ticket kind: {other}"))),
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
        .ok_or_else(|| TmError::invariant("create_ticket did not emit ticket.created"))
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
        let target_state = match state_arg {
            TicketStateArg::Draft => TicketState::Draft,
            TicketStateArg::Ready => TicketState::Ready,
            TicketStateArg::Active => TicketState::Running,
            TicketStateArg::Verification => TicketState::Verifying,
            TicketStateArg::Audit => TicketState::Auditing,
            TicketStateArg::Closed => TicketState::Closed,
            TicketStateArg::Cancelled => TicketState::Cancelled,
            TicketStateArg::Blocked => TicketState::Blocked,
        };
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
                let objective = if t.objective.len() > 50 {
                    format!("{}...", &t.objective[..47])
                } else {
                    t.objective.clone()
                };
                vec![
                    t.id.to_string(),
                    format!("{:?}", t.kind),
                    format!("{:?}", t.state),
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
        let mut text = format!("ID:           {}\n", ticket.id);
        text.push_str(&format!("State:        {:?}\n", ticket.state));
        text.push_str(&format!("Kind:         {:?}\n", ticket.kind));
        text.push_str(&format!("Priority:     {}\n", ticket.priority));
        text.push_str(&format!("Objective:    {}\n", ticket.objective));
        if let Some(parent) = &ticket.parent {
            text.push_str(&format!("Parent:       {}\n", parent));
        }
        if let Some(milestone) = &ticket.milestone {
            text.push_str(&format!("Milestone:    {}\n", milestone));
        }
        text.push_str(&format!("Authority:    {:?}\n", ticket.authority));
        text.push_str(&format!("Resources:    {:?}\n", ticket.resources));
        text.push_str(&format!("Executor:     {:?}\n", ticket.executor));
        text.push_str(&format!("Verification: {:?}\n", ticket.verification));
        text.push_str(&format!("Budget:       {:?}\n", ticket.budget));
        text.push_str(&format!("Retry Policy: {:?}\n", ticket.retry));
        text.push_str(&format!("Children:     {:?}\n", ticket.children));
        text.push_str(&format!("Created:      {}\n", ticket.created));
        text.push_str(&format!("Updated:      {}\n", ticket.updated));
        renderer.emit(ticket, &text)?;
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

    let milestone = if let Some(m) = &args.milestone {
        Some(MilestoneId::new(m)?)
    } else {
        None
    };

    let view = project.store.view()?;

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
            "ticket edit requires at least --objective or --priority",
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
        .close(&ticket_id, None, project.actor.clone())?;
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
        .cancel(&ticket_id, args.reason.clone(), project.actor.clone())?;
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
        .reopen(&ticket_id, None, project.actor.clone())?;
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
    project.store.activate(&ticket_id, project.actor.clone())?;
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
        .accept(&ticket_id, args.note.clone(), project.actor.clone())?;
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
        .reject(&ticket_id, args.reason.clone(), project.actor.clone())?;
    let state = project
        .store
        .view()?
        .tickets
        .get(&ticket_id)
        .map(|t| format!("{:?}", t.state).to_ascii_lowercase())
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
        .retry(&ticket_id, args.guidance.clone(), project.actor.clone())?;
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
    let label = format!("{} {:?} {}", ticket.id, ticket.state, truncated_objective);

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
        let bytes = fs::read(path)
            .map_err(|e| TmError::storage(format!("could not read {}: {}", path.display(), e)))?;

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
        .submit(&ticket_id, summary, evidence_ids, project.actor.clone())?;
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
        k => return Err(TmError::parse(format!("unknown dependency kind: {}", k))),
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

    let view = project.store.view()?;

    if !view.tickets.contains_key(&ticket) {
        return Err(TmError::not_found("ticket", &ticket));
    }
    if !view.tickets.contains_key(&depends_on) {
        return Err(TmError::not_found("ticket", &depends_on));
    }

    let edges = view.graph.edges();
    let mut updated_edges = Vec::new();
    let mut found = false;

    for edge in edges {
        if edge.from == ticket && edge.to == depends_on {
            found = true;
        } else {
            updated_edges.push(serde_json::json!({
                "from": edge.from,
                "to": edge.to,
                "kind": edge.kind,
            }));
        }
    }

    if !found {
        return Err(TmError::not_found(
            "dependency edge",
            format!("{} -> {}", ticket, depends_on),
        ));
    }

    let fields = serde_json::json!({"dependencies": updated_edges});
    project
        .store
        .update_ticket(&ticket, fields, project.actor.clone())?;
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

    let graph_view = GraphView::from(&view.graph);

    if renderer.is_json() {
        renderer.emit(&graph_view, "")?;
    } else {
        let text = if let Some(ticket_str) = &args.ticket {
            let ticket = TicketId::new(ticket_str)?;
            format!(
                "Dependency graph (subgraph from {}):\n{:?}",
                ticket, view.graph
            )
        } else {
            format!("Dependency graph:\n{:?}", view.graph)
        };
        renderer.emit(&graph_view, &text)?;
    }

    Ok(())
}

/// Dispatch one [`MilestoneCommand`].
pub fn dispatch_milestone(
    cmd: &MilestoneCommand,
    project: &Project,
    renderer: &Renderer,
) -> tm_types::Result<()> {
    match cmd {
        MilestoneCommand::List => milestone_list(project, renderer),
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
    } else {
        let headers = vec!["ID".to_string(), "State".to_string(), "Tickets".to_string()];
        let rows: Vec<Vec<String>> = milestones
            .iter()
            .map(|m| {
                vec![
                    m.id.to_string(),
                    format!("{:?}", m.state),
                    m.tickets.len().to_string(),
                ]
            })
            .collect();
        let table = Table::new(headers, rows);
        renderer.emit(&milestone_views, &table.render())?;
    }

    Ok(())
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
}
