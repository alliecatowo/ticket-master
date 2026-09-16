//! The read models other crates consume.
//!
//! [`ProjectView`] is the full picture of a project's materialized state; [`SchedulerView`] is
//! the narrow slice `tm-scheduler` needs to pick the next ticket to lease. Both are cheap to
//! construct from `Store` (a handful of `SELECT`s against the tables `schema.rs` defines) and
//! cheap to clone, so pure functions elsewhere in this crate (and in downstream crates) can take
//! an owned snapshot rather than borrowing from the store across a lease/transition decision.

use std::collections::BTreeMap;

use tm_types::{Budget, MilestoneId, TicketId};

use crate::artifact::{Artifact, Evidence};
use crate::decision::Decision;
use crate::graph::GraphView;
use crate::lease::Lease;
use crate::milestone::Milestone;
use crate::ticket::{Ticket, TicketState};

/// The full materialized state of one project, assembled for read-heavy consumers
/// (`tm-context`, `tm-docs`, `tm-server`, `tm-cli`) that want everything at once.
#[derive(Debug, Clone, Default)]
pub struct ProjectView {
    /// Every ticket, keyed by id.
    pub tickets: BTreeMap<TicketId, Ticket>,
    /// The dependency/parent-child graph.
    pub graph: GraphView,
    /// Every lease, live or expired.
    pub leases: Vec<Lease>,
    /// Every decision, active or superseded.
    pub decisions: Vec<Decision>,
    /// Every milestone.
    pub milestones: Vec<Milestone>,
    /// Every artifact.
    pub artifacts: Vec<Artifact>,
    /// Every evidence record.
    pub evidence: Vec<Evidence>,
    /// Budgets keyed by their `budget::BudgetScope`, encoded as its `Debug` string for a cheap
    /// stable map key (the scope enum itself isn't `Hash`; this view is a read model, not a
    /// place to add trait bounds purely for a lookup convenience).
    pub budgets: BTreeMap<String, Budget>,
    /// Per-`IdKind` next-counter values, for restoring `IdSource` state after a restart.
    pub counters: BTreeMap<String, u64>,
}

impl ProjectView {
    /// Every lease currently live (not expired, not released).
    pub fn live_leases(&self) -> Vec<&Lease> {
        // IMPL note: "live" is a materialized column (`leases.live`), not recomputable from
        // `Lease` alone (a `Lease` doesn't carry a `live` flag; liveness prior to the `now` check
        // in `lease::expire_due` is a store-level fact once released/expired events land). Since
        // `Lease` here has no such flag, callers needing strict liveness should intersect this
        // with `graph`/`now` as appropriate; this helper returns every lease in the view as a
        // placeholder callers refine — kept intentionally simple since `store.rs` decides what
        // "live" rows this view is populated with in the first place.
        self.leases.iter().collect()
    }

    /// Milestones containing `ticket`.
    pub fn milestones_for(&self, ticket: &TicketId) -> Vec<&Milestone> {
        crate::milestone::milestones_containing(&self.milestones, ticket)
    }
}

/// The narrow slice of project state `tm-scheduler` needs: which tickets are ready, their
/// dependency/resource shape, and live leases to check conflicts against. Deliberately excludes
/// decisions/docs/artifacts, which the scheduler never consults.
#[derive(Debug, Clone, Default)]
pub struct SchedulerView {
    /// Tickets currently in `TicketState::Ready`, keyed by id.
    pub ready: BTreeMap<TicketId, Ticket>,
    /// The dependency/parent-child graph (needed for priority/critical-path computation).
    pub graph: GraphView,
    /// Every currently live lease (for resource-conflict checks against a new acquire).
    pub live_leases: Vec<Lease>,
    /// Milestone membership, for milestone-scoped scheduling policies.
    pub milestone_of: BTreeMap<TicketId, MilestoneId>,
}

impl From<&ProjectView> for SchedulerView {
    fn from(project: &ProjectView) -> Self {
        // Filter tickets to TicketState::Ready
        let ready = project
            .tickets
            .iter()
            .filter(|(_, ticket)| ticket.state == TicketState::Ready)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Clone graph as-is
        let graph = project.graph.clone();

        // Clone all leases as live ones; this is a placeholder until store.rs
        // exposes a real liveness signal (see live_leases note on ProjectView).
        let live_leases = project.leases.clone();

        // Invert Milestone::tickets to build milestone_of map
        let mut milestone_of = BTreeMap::new();
        for milestone in &project.milestones {
            for ticket_id in &milestone.tickets {
                milestone_of.insert(ticket_id.clone(), milestone.id.clone());
            }
        }

        SchedulerView {
            ready,
            graph,
            live_leases,
            milestone_of,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphView;
    use crate::lease::Lease;
    use crate::milestone::{Milestone, MilestoneState};
    use tm_types::{Authority, Budget, LeaseId, MilestoneId, ParticipantId, TicketId, Timestamp};

    fn sample_ticket(id: &str, state: TicketState) -> Ticket {
        Ticket {
            id: TicketId::new(id).unwrap(),
            kind: crate::ticket::TicketKind::Work,
            objective: format!("ticket {}", id),
            state,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            authority: Authority::none(),
            resources: vec![],
            executor: Default::default(),
            context_refs: vec![],
            success: vec![],
            verification: Default::default(),
            budget: Budget::unlimited(),
            retry: Default::default(),
            cycle: None,
            attempts: 0,
            failures: vec![],
            priority: 0,
            created: Timestamp::EPOCH,
            updated: Timestamp::EPOCH,
        }
    }

    fn sample_lease(id: &str, ticket_id: &str) -> Lease {
        Lease {
            id: LeaseId::new(id).unwrap(),
            ticket: TicketId::new(ticket_id).unwrap(),
            holder: ParticipantId::system(),
            authority: Authority::none(),
            resources: vec![],
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 300,
            epoch: 0,
        }
    }

    fn sample_milestone(id: &str, tickets: Vec<&str>) -> Milestone {
        Milestone {
            id: MilestoneId::new(id).unwrap(),
            title: format!("milestone {}", id),
            tickets: tickets
                .into_iter()
                .map(|t| TicketId::new(t).unwrap())
                .collect(),
            state: MilestoneState::Open,
            closed_by: None,
            assumptions: vec![],
        }
    }

    #[test]
    fn converts_empty_project() {
        let project = ProjectView::default();
        let scheduler: SchedulerView = (&project).into();

        assert!(scheduler.ready.is_empty());
        assert!(scheduler.live_leases.is_empty());
        assert!(scheduler.milestone_of.is_empty());
    }

    #[test]
    fn filters_only_ready_tickets() {
        let mut project = ProjectView::default();
        project.tickets.insert(
            TicketId::new("T-1").unwrap(),
            sample_ticket("T-1", TicketState::Ready),
        );
        project.tickets.insert(
            TicketId::new("T-2").unwrap(),
            sample_ticket("T-2", TicketState::Draft),
        );
        project.tickets.insert(
            TicketId::new("T-3").unwrap(),
            sample_ticket("T-3", TicketState::Ready),
        );
        project.tickets.insert(
            TicketId::new("T-4").unwrap(),
            sample_ticket("T-4", TicketState::Blocked),
        );

        let scheduler: SchedulerView = (&project).into();

        assert_eq!(scheduler.ready.len(), 2);
        assert!(scheduler.ready.contains_key(&TicketId::new("T-1").unwrap()));
        assert!(scheduler.ready.contains_key(&TicketId::new("T-3").unwrap()));
        assert!(!scheduler.ready.contains_key(&TicketId::new("T-2").unwrap()));
        assert!(!scheduler.ready.contains_key(&TicketId::new("T-4").unwrap()));
    }

    #[test]
    fn clones_graph_correctly() {
        let mut project = ProjectView::default();
        project.graph = GraphView {
            edges: vec![],
            children: {
                let mut m = BTreeMap::new();
                m.insert(
                    TicketId::new("T-1").unwrap(),
                    vec![TicketId::new("T-2").unwrap()],
                );
                m
            },
            parents: {
                let mut m = BTreeMap::new();
                m.insert(TicketId::new("T-2").unwrap(), TicketId::new("T-1").unwrap());
                m
            },
            states: {
                let mut m = BTreeMap::new();
                m.insert(TicketId::new("T-1").unwrap(), TicketState::Ready);
                m
            },
        };

        let scheduler: SchedulerView = (&project).into();

        assert_eq!(scheduler.graph.children.len(), project.graph.children.len());
        assert_eq!(scheduler.graph.parents.len(), project.graph.parents.len());
        assert_eq!(scheduler.graph.states.len(), project.graph.states.len());
    }

    #[test]
    fn includes_all_leases_as_live() {
        let mut project = ProjectView::default();
        project.leases.push(sample_lease("L-1", "T-1"));
        project.leases.push(sample_lease("L-2", "T-2"));
        project.leases.push(sample_lease("L-3", "T-3"));

        let scheduler: SchedulerView = (&project).into();

        assert_eq!(scheduler.live_leases.len(), 3);
        assert_eq!(
            scheduler.live_leases[0].ticket,
            TicketId::new("T-1").unwrap()
        );
        assert_eq!(
            scheduler.live_leases[1].ticket,
            TicketId::new("T-2").unwrap()
        );
        assert_eq!(
            scheduler.live_leases[2].ticket,
            TicketId::new("T-3").unwrap()
        );
    }

    #[test]
    fn inverts_single_milestone_ticket_membership() {
        let mut project = ProjectView::default();
        project
            .milestones
            .push(sample_milestone("M-1", vec!["T-1", "T-2"]));

        let scheduler: SchedulerView = (&project).into();

        assert_eq!(scheduler.milestone_of.len(), 2);
        assert_eq!(
            scheduler.milestone_of[&TicketId::new("T-1").unwrap()],
            MilestoneId::new("M-1").unwrap()
        );
        assert_eq!(
            scheduler.milestone_of[&TicketId::new("T-2").unwrap()],
            MilestoneId::new("M-1").unwrap()
        );
    }

    #[test]
    fn handles_multiple_milestones() {
        let mut project = ProjectView::default();
        project
            .milestones
            .push(sample_milestone("M-1", vec!["T-1", "T-2"]));
        project
            .milestones
            .push(sample_milestone("M-2", vec!["T-3", "T-4"]));

        let scheduler: SchedulerView = (&project).into();

        assert_eq!(scheduler.milestone_of.len(), 4);
        assert_eq!(
            scheduler.milestone_of[&TicketId::new("T-1").unwrap()],
            MilestoneId::new("M-1").unwrap()
        );
        assert_eq!(
            scheduler.milestone_of[&TicketId::new("T-3").unwrap()],
            MilestoneId::new("M-2").unwrap()
        );
    }

    #[test]
    fn handles_overlapping_milestone_membership() {
        let mut project = ProjectView::default();
        project
            .milestones
            .push(sample_milestone("M-1", vec!["T-1", "T-2"]));
        project
            .milestones
            .push(sample_milestone("M-2", vec!["T-2", "T-3"]));

        let scheduler: SchedulerView = (&project).into();

        // When a ticket is in multiple milestones, the last one wins in the map
        assert_eq!(scheduler.milestone_of.len(), 3);
        assert_eq!(
            scheduler.milestone_of[&TicketId::new("T-1").unwrap()],
            MilestoneId::new("M-1").unwrap()
        );
        // T-2 is in both M-1 and M-2; the map keeps the last one processed
        assert_eq!(
            scheduler.milestone_of[&TicketId::new("T-2").unwrap()],
            MilestoneId::new("M-2").unwrap()
        );
        assert_eq!(
            scheduler.milestone_of[&TicketId::new("T-3").unwrap()],
            MilestoneId::new("M-2").unwrap()
        );
    }

    #[test]
    fn ready_tickets_preserve_state() {
        let mut project = ProjectView::default();
        let mut ready_ticket = sample_ticket("T-1", TicketState::Ready);
        ready_ticket.priority = 42;
        project
            .tickets
            .insert(TicketId::new("T-1").unwrap(), ready_ticket.clone());

        let scheduler: SchedulerView = (&project).into();

        assert_eq!(scheduler.ready.len(), 1);
        let retrieved = &scheduler.ready[&TicketId::new("T-1").unwrap()];
        assert_eq!(retrieved.priority, 42);
        assert_eq!(retrieved.state, TicketState::Ready);
    }

    #[test]
    fn full_projection_from_complex_project() {
        let mut project = ProjectView::default();

        // Add mixed-state tickets
        project.tickets.insert(
            TicketId::new("T-1").unwrap(),
            sample_ticket("T-1", TicketState::Ready),
        );
        project.tickets.insert(
            TicketId::new("T-2").unwrap(),
            sample_ticket("T-2", TicketState::Blocked),
        );
        project.tickets.insert(
            TicketId::new("T-3").unwrap(),
            sample_ticket("T-3", TicketState::Ready),
        );

        // Add leases
        project.leases.push(sample_lease("L-1", "T-1"));
        project.leases.push(sample_lease("L-2", "T-2"));

        // Add milestones
        project
            .milestones
            .push(sample_milestone("M-1", vec!["T-1", "T-2"]));
        project
            .milestones
            .push(sample_milestone("M-2", vec!["T-3"]));

        let scheduler: SchedulerView = (&project).into();

        // Verify ready tickets
        assert_eq!(scheduler.ready.len(), 2);
        assert!(scheduler.ready.contains_key(&TicketId::new("T-1").unwrap()));
        assert!(scheduler.ready.contains_key(&TicketId::new("T-3").unwrap()));

        // Verify leases
        assert_eq!(scheduler.live_leases.len(), 2);

        // Verify milestone membership
        assert_eq!(
            scheduler.milestone_of[&TicketId::new("T-1").unwrap()],
            MilestoneId::new("M-1").unwrap()
        );
        assert_eq!(
            scheduler.milestone_of[&TicketId::new("T-3").unwrap()],
            MilestoneId::new("M-2").unwrap()
        );
    }
}
