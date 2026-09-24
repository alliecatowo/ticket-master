//! The read models other crates consume.
//!
//! [`ProjectView`] is the full picture: every table `tm-core` materializes, assembled into an
//! in-memory snapshot that is cheap to construct from `Store` and cheap to clone, so pure
//! functions elsewhere in this crate (and in `tm-scheduler`, `tm-context`, ...) can take it by
//! value or `&` without touching SQLite again. [`SchedulerView`] is the narrower projection
//! `tm-scheduler` actually needs, so that crate doesn't have to depend on decision/doc/provider
//! state it never reads.

use std::collections::BTreeMap;

use tm_types::{ParticipantId, TicketId};

use crate::artifact::{Artifact, Evidence};
use crate::budget::ScopedBudget;
use crate::decision::Decision;
use crate::graph::DependencyGraph;
use crate::lease::Lease;
use crate::milestone::Milestone;
use crate::ticket::Ticket;

/// Everything `tm-core` materializes, snapshotted for pure consumption.
#[derive(Debug, Clone)]
pub struct ProjectView {
    /// Every ticket, keyed by id.
    pub tickets: BTreeMap<TicketId, Ticket>,
    /// The dependency/parent-child graph over `tickets`.
    pub graph: DependencyGraph,
    /// Every lease, live or historical, keyed by id.
    pub leases: BTreeMap<tm_types::LeaseId, Lease>,
    /// Every decision, active or superseded, keyed by id.
    pub decisions: BTreeMap<tm_types::DecisionId, Decision>,
    /// Every milestone, keyed by id.
    pub milestones: BTreeMap<tm_types::MilestoneId, Milestone>,
    /// Every artifact, keyed by id.
    pub artifacts: BTreeMap<tm_types::ArtifactId, Artifact>,
    /// Every evidence record.
    pub evidence: Vec<Evidence>,
    /// The full budget hierarchy, keyed by scope.
    pub budgets: Vec<ScopedBudget>,
    /// Known participants and their last-seen presence, if tracked.
    pub participants: BTreeMap<ParticipantId, ParticipantSummary>,
    /// Current high-water mark of every id counter, for `Store::rebuild` to restore
    /// `CounterIds` correctly after replay.
    pub counters: BTreeMap<String, u64>,
}

/// A minimal per-participant summary, enough for `tm-cli`/`tm-server` presence display without
/// pulling in the full session/event history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantSummary {
    /// The participant's id.
    pub id: ParticipantId,
    /// Free-form last-known status string (e.g. `"active"`, `"idle"`).
    pub status: String,
}

impl ProjectView {
    /// An empty view, useful as a starting accumulator and in tests.
    pub fn empty() -> Self {
        ProjectView {
            tickets: BTreeMap::new(),
            graph: DependencyGraph::default(),
            leases: BTreeMap::new(),
            decisions: BTreeMap::new(),
            milestones: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            evidence: Vec::new(),
            budgets: Vec::new(),
            participants: BTreeMap::new(),
            counters: BTreeMap::new(),
        }
    }
}

/// The narrow projection `tm-scheduler` needs: enough to decide what's `Ready`, rank it, and
/// lease it, without the decision/doc/provider machinery `ProjectView` also carries.
#[derive(Debug, Clone)]
pub struct SchedulerView {
    /// Every ticket, keyed by id (same source as `ProjectView::tickets`).
    pub tickets: BTreeMap<TicketId, Ticket>,
    /// The dependency/parent-child graph.
    pub graph: DependencyGraph,
    /// Live leases only.
    pub live_leases: BTreeMap<tm_types::LeaseId, Lease>,
}

impl From<&ProjectView> for SchedulerView {
    fn from(view: &ProjectView) -> Self {
        SchedulerView {
            tickets: view.tickets.clone(),
            graph: view.graph.clone(),
            live_leases: view.leases.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::LeaseId;

    fn test_ticket(id: TicketId, objective: &str) -> Ticket {
        Ticket {
            id,
            kind: crate::ticket::TicketKind::Work,
            objective: objective.to_string(),
            state: crate::ticket::TicketState::Draft,
            parent: None,
            children: Vec::new(),
            dependencies: Vec::new(),
            milestone: None,
            due: None,
            authority: tm_types::Authority::none(),
            resources: Vec::new(),
            executor: crate::ticket::ExecutorRequirements {
                role: tm_types::Role::CoderFast,
                human_required: false,
                min_capability: tm_types::Tolerance::Any,
            },
            context_refs: Vec::new(),
            success: Vec::new(),
            verification: crate::ticket::VerificationPolicy::None,
            budget: tm_types::Budget::none(),
            retry: crate::ticket::RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 1,
                backoff_multiplier: 2.0,
                max_delay_seconds: 60,
            },
            cycle: None,
            attempts: 0,
            failures: Vec::new(),
            priority: 0,
            created: tm_types::Timestamp::EPOCH,
            updated: tm_types::Timestamp::EPOCH,
        }
    }

    #[test]
    fn empty_project_view_converts_to_empty_scheduler_view() {
        let project_view = ProjectView::empty();
        let scheduler_view = SchedulerView::from(&project_view);

        assert!(scheduler_view.tickets.is_empty());
        assert_eq!(scheduler_view.graph, DependencyGraph::default());
        assert!(scheduler_view.live_leases.is_empty());
    }

    #[test]
    fn scheduler_view_copies_tickets_from_project_view() {
        let mut project_view = ProjectView::empty();
        let ticket_id = TicketId::new("T-1").expect("valid ticket id");
        let ticket = test_ticket(ticket_id.clone(), "Test ticket");

        project_view
            .tickets
            .insert(ticket_id.clone(), ticket.clone());
        let scheduler_view = SchedulerView::from(&project_view);

        assert_eq!(scheduler_view.tickets.len(), 1);
        assert!(scheduler_view.tickets.contains_key(&ticket_id));
        let retrieved = &scheduler_view.tickets[&ticket_id];
        assert_eq!(retrieved.id, ticket_id);
        assert_eq!(retrieved.objective, "Test ticket");
    }

    #[test]
    fn scheduler_view_copies_graph_from_project_view() {
        let mut project_view = ProjectView::empty();
        let graph = DependencyGraph::default();
        project_view.graph = graph.clone();

        let scheduler_view = SchedulerView::from(&project_view);

        assert_eq!(scheduler_view.graph, project_view.graph);
    }

    #[test]
    fn scheduler_view_copies_leases_from_project_view() {
        let mut project_view = ProjectView::empty();
        let lease_id = LeaseId::new("L-abc123def456").expect("valid lease id");
        let ticket_id = TicketId::new("T-1").expect("valid ticket id");
        let participant_id = tm_types::ParticipantId::system();

        let lease = Lease {
            id: lease_id.clone(),
            ticket: ticket_id.clone(),
            holder: participant_id,
            authority: tm_types::Authority::none(),
            resources: Vec::new(),
            acquired: tm_types::Timestamp::EPOCH,
            heartbeat: tm_types::Timestamp::EPOCH,
            ttl_seconds: 60,
            epoch: 1,
        };

        project_view.leases.insert(lease_id.clone(), lease.clone());
        let scheduler_view = SchedulerView::from(&project_view);

        assert_eq!(scheduler_view.live_leases.len(), 1);
        assert!(scheduler_view.live_leases.contains_key(&lease_id));
        assert_eq!(scheduler_view.live_leases[&lease_id].ticket, ticket_id);
    }

    #[test]
    fn scheduler_view_is_cloneable() {
        let scheduler_view = SchedulerView {
            tickets: BTreeMap::new(),
            graph: DependencyGraph::default(),
            live_leases: BTreeMap::new(),
        };

        let cloned = scheduler_view.clone();
        assert_eq!(cloned.tickets, scheduler_view.tickets);
        assert_eq!(cloned.graph, scheduler_view.graph);
        assert_eq!(cloned.live_leases, scheduler_view.live_leases);
    }

    #[test]
    fn project_view_empty_creates_all_empty_collections() {
        let view = ProjectView::empty();

        assert!(view.tickets.is_empty());
        assert_eq!(view.graph, DependencyGraph::default());
        assert!(view.leases.is_empty());
        assert!(view.decisions.is_empty());
        assert!(view.milestones.is_empty());
        assert!(view.artifacts.is_empty());
        assert!(view.evidence.is_empty());
        assert!(view.budgets.is_empty());
        assert!(view.participants.is_empty());
        assert!(view.counters.is_empty());
    }

    #[test]
    fn project_view_is_cloneable() {
        let mut view = ProjectView::empty();
        let ticket_id = TicketId::new("T-1").expect("valid ticket id");
        let ticket = test_ticket(ticket_id.clone(), "Test");

        view.tickets.insert(ticket_id.clone(), ticket);
        let cloned = view.clone();

        assert_eq!(cloned.tickets.len(), view.tickets.len());
        assert_eq!(cloned.tickets[&ticket_id].id, ticket_id);
    }

    #[test]
    fn participant_summary_is_debuggable() {
        let summary = ParticipantSummary {
            id: tm_types::ParticipantId::system(),
            status: "active".to_string(),
        };

        let debug_str = format!("{:?}", summary);
        assert!(debug_str.contains("ParticipantSummary"));
        assert!(debug_str.contains("active"));
    }

    #[test]
    fn participant_summary_equality() {
        let summary1 = ParticipantSummary {
            id: tm_types::ParticipantId::system(),
            status: "active".to_string(),
        };

        let summary2 = ParticipantSummary {
            id: tm_types::ParticipantId::system(),
            status: "active".to_string(),
        };

        assert_eq!(summary1, summary2);
    }

    #[test]
    fn scheduler_view_conversion_preserves_data_integrity() {
        let mut project_view = ProjectView::empty();

        let ticket_id1 = TicketId::new("T-1").expect("valid ticket id");
        let ticket_id2 = TicketId::new("T-2").expect("valid ticket id");

        let make_ticket = |id: TicketId| -> Ticket {
            let objective = format!("Ticket {}", id);
            test_ticket(id, &objective)
        };

        project_view
            .tickets
            .insert(ticket_id1.clone(), make_ticket(ticket_id1.clone()));
        project_view
            .tickets
            .insert(ticket_id2.clone(), make_ticket(ticket_id2.clone()));

        let scheduler_view = SchedulerView::from(&project_view);

        assert_eq!(scheduler_view.tickets.len(), 2);
        assert_eq!(scheduler_view.tickets[&ticket_id1].objective, "Ticket T-1");
        assert_eq!(scheduler_view.tickets[&ticket_id2].objective, "Ticket T-2");
    }
}
