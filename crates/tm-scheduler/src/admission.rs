//! Admission control: the ceilings a selected ticket must clear before it may be leased.
//!
//! [`select`](crate::select) answers "which ticket is best"; this module answers "may it
//! actually start right now". Both are pure functions of [`tm_core::view::SchedulerView`] plus
//! [`SchedulingPolicy`] — no I/O, no clock reads beyond what the caller already threaded through
//! `now`. Four gates apply, in the order [`AdmissionGate::check`] evaluates them: project worker
//! ceiling, parent-ticket worker ceiling, concurrent-command ceiling, then provider availability
//! by role; the harness capacity cap is folded into the project ceiling check since it is a
//! share *of* that same capacity, not an independent resource.

use std::collections::BTreeMap;

use tm_core::ticket::{Ticket, TicketKind, TicketState};
use tm_core::view::SchedulerView;
use tm_types::{Role, TicketId};

use crate::policy::SchedulingPolicy;
use crate::select::ExecutorAvailability;

/// The outcome of [`AdmissionGate::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// The ticket may be leased now.
    Admit,
    /// The ticket may not be leased now, with the specific gate that refused it.
    Refuse(AdmissionRefusal),
}

/// Which ceiling refused admission, carrying enough detail for the planner to decide what to do
/// next (e.g. `MarkBlocked` is never right here; the ticket stays `Ready` and is retried next
/// tick).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionRefusal {
    /// [`SchedulingPolicy::effective_max_in_flight_per_project`] is already reached.
    #[error("project in-flight ceiling reached")]
    ProjectCeiling,
    /// [`SchedulingPolicy::max_in_flight_per_parent`] is already reached for this ticket's
    /// parent.
    #[error("parent {0} in-flight ceiling reached")]
    ParentCeiling(TicketId),
    /// [`SchedulingPolicy::max_concurrent_commands`] is already reached.
    #[error("concurrent command ceiling reached")]
    CommandCeiling,
    /// No provider (or human, if required) is available for the ticket's role right now.
    #[error("no provider available for role {0}")]
    ProviderUnavailable(Role),
    /// This is `TicketKind::Harness` work and admitting it would push harness's share of
    /// in-flight capacity over [`SchedulingPolicy::harness_capacity_fraction`].
    #[error("harness capacity share exceeded")]
    HarnessCapacityExceeded,
}

/// Live in-flight counts derived from a [`SchedulerView`], the state [`AdmissionGate::check`]
/// evaluates ceilings against. Built once per planning pass (counts don't change mid-`plan`,
/// since `plan` never mutates its input) and consulted for every candidate ticket in ranked
/// order, since granting one candidate changes what the next candidate may be admitted against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionGate {
    /// Count of tickets currently `Leased` or `Running`, project-wide.
    project_in_flight: u32,
    /// Count of tickets currently `Leased` or `Running`, per parent ticket id.
    parent_in_flight: BTreeMap<TicketId, u32>,
    /// Count of tickets currently `Leased` or `Running` whose kind is `TicketKind::Harness`.
    harness_in_flight: u32,
    /// Count of currently in-flight commands, project-wide (tracked separately from ticket
    /// leases: one ticket's lease may span several commands over its lifetime).
    commands_in_flight: u32,
}

impl AdmissionGate {
    /// Derive in-flight counts from `view`: every ticket in `Leased` or `Running` state counts
    /// toward the project, its parent (if any) and, when its kind is `Harness`, the harness
    /// count. `commands_in_flight` cannot be derived from `SchedulerView` (it has no notion of
    /// an in-progress command, only ticket state), so it is supplied by the caller from whatever
    /// live-command tracking the driver maintains.
    pub fn from_view(view: &SchedulerView, commands_in_flight: u32) -> Self {
        let mut gate = AdmissionGate {
            project_in_flight: 0,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 0,
            commands_in_flight,
        };

        for ticket in view.tickets.values() {
            if matches!(ticket.state, TicketState::Leased | TicketState::Running) {
                gate.project_in_flight += 1;
                if let Some(parent) = &ticket.parent {
                    *gate.parent_in_flight.entry(parent.clone()).or_insert(0) += 1;
                }
                if ticket.kind == TicketKind::Harness {
                    gate.harness_in_flight += 1;
                }
            }
        }

        gate
    }

    /// Check every ceiling for `ticket` against `policy` and `availability`, in the documented
    /// order (project, parent, commands, harness share, provider). Returns the first violated
    /// gate, or [`AdmissionDecision::Admit`] if all clear.
    pub fn check(
        &self,
        ticket: &Ticket,
        policy: &SchedulingPolicy,
        availability: &dyn ExecutorAvailability,
    ) -> AdmissionDecision {
        // Gate 1: Project ceiling
        if self.project_in_flight >= policy.effective_max_in_flight_per_project() {
            return AdmissionDecision::Refuse(AdmissionRefusal::ProjectCeiling);
        }

        // Gate 2: Parent ceiling
        if let Some(parent) = &ticket.parent {
            let parent_count = self.parent_in_flight.get(parent).copied().unwrap_or(0);
            if parent_count >= policy.max_in_flight_per_parent {
                return AdmissionDecision::Refuse(AdmissionRefusal::ParentCeiling(parent.clone()));
            }
        }

        // Gate 3: Command ceiling
        if self.commands_in_flight >= policy.max_concurrent_commands {
            return AdmissionDecision::Refuse(AdmissionRefusal::CommandCeiling);
        }

        // Gate 4: Harness capacity share (only checked when not suspended during ignition)
        if ticket.kind == TicketKind::Harness {
            let should_check_harness_cap = match policy.mode {
                crate::policy::SchedulingMode::SteadyState => true,
                crate::policy::SchedulingMode::Ignition => !policy.ignition.suspend_harness_cap,
            };

            if should_check_harness_cap
                && self.harness_share_after_admitting() > policy.harness_capacity_fraction
            {
                return AdmissionDecision::Refuse(AdmissionRefusal::HarnessCapacityExceeded);
            }
        }

        // Gate 5: Provider availability (unless human_required)
        if !ticket.executor.human_required && !availability.is_available(ticket.executor.role) {
            return AdmissionDecision::Refuse(AdmissionRefusal::ProviderUnavailable(
                ticket.executor.role,
            ));
        }

        AdmissionDecision::Admit
    }

    /// Record that `ticket` was admitted, so the next [`AdmissionGate::check`] call in the same
    /// planning pass sees updated counts. Mutates in place; callers plan sequentially over the
    /// ranked candidate list.
    pub fn record_admission(&mut self, ticket: &Ticket) {
        self.project_in_flight += 1;
        if let Some(parent) = &ticket.parent {
            *self.parent_in_flight.entry(parent.clone()).or_insert(0) += 1;
        }
        if ticket.kind == TicketKind::Harness {
            self.harness_in_flight += 1;
        }
    }

    /// The fraction of `project_in_flight` (after a hypothetical harness admission) that would
    /// be harness work, used by [`AdmissionGate::check`]'s harness gate.
    fn harness_share_after_admitting(&self) -> f64 {
        let denom = (self.project_in_flight + 1).max(1);
        (self.harness_in_flight + 1) as f64 / denom as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::ticket::ExecutorRequirements;
    use tm_types::Tolerance;

    /// Build a test ticket with the given id and kind.
    fn test_ticket(id: &str, kind: TicketKind) -> Ticket {
        Ticket {
            id: TicketId::new(id).expect("valid id"),
            kind,
            objective: format!("Test ticket {}", id),
            state: TicketState::Draft,
            parent: None,
            children: Vec::new(),
            dependencies: Vec::new(),
            milestone: None,
            authority: tm_types::Authority::none(),
            resources: Vec::new(),
            executor: ExecutorRequirements {
                role: tm_types::Role::CoderFast,
                human_required: false,
                min_capability: Tolerance::Any,
            },
            context_refs: Vec::new(),
            success: Vec::new(),
            verification: tm_core::ticket::VerificationPolicy::None,
            budget: tm_types::Budget::none(),
            retry: tm_core::ticket::RetryPolicy {
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

    /// Build a test SchedulerView with no tickets.
    fn empty_view() -> SchedulerView {
        SchedulerView {
            tickets: BTreeMap::new(),
            graph: tm_core::graph::DependencyGraph::default(),
            live_leases: BTreeMap::new(),
        }
    }

    /// Mock availability: always available.
    struct AlwaysAvailable;
    impl ExecutorAvailability for AlwaysAvailable {
        fn is_available(&self, _role: Role) -> bool {
            true
        }
    }

    /// Mock availability: never available.
    struct NeverAvailable;
    impl ExecutorAvailability for NeverAvailable {
        fn is_available(&self, _role: Role) -> bool {
            false
        }
    }

    #[test]
    fn from_view_empty_returns_all_zeros() {
        let view = empty_view();
        let gate = AdmissionGate::from_view(&view, 0);

        assert_eq!(gate.project_in_flight, 0);
        assert!(gate.parent_in_flight.is_empty());
        assert_eq!(gate.harness_in_flight, 0);
        assert_eq!(gate.commands_in_flight, 0);
    }

    #[test]
    fn from_view_passes_through_commands_in_flight() {
        let view = empty_view();
        let gate = AdmissionGate::from_view(&view, 42);

        assert_eq!(gate.commands_in_flight, 42);
    }

    #[test]
    fn from_view_ignores_draft_tickets() {
        let mut view = empty_view();
        let mut ticket = test_ticket("T-1", TicketKind::Work);
        ticket.state = TicketState::Draft;
        view.tickets.insert(ticket.id.clone(), ticket);

        let gate = AdmissionGate::from_view(&view, 0);
        assert_eq!(gate.project_in_flight, 0);
    }

    #[test]
    fn from_view_counts_leased_ticket() {
        let mut view = empty_view();
        let mut ticket = test_ticket("T-1", TicketKind::Work);
        ticket.state = TicketState::Leased;
        view.tickets.insert(ticket.id.clone(), ticket);

        let gate = AdmissionGate::from_view(&view, 0);
        assert_eq!(gate.project_in_flight, 1);
    }

    #[test]
    fn from_view_counts_running_ticket() {
        let mut view = empty_view();
        let mut ticket = test_ticket("T-1", TicketKind::Work);
        ticket.state = TicketState::Running;
        view.tickets.insert(ticket.id.clone(), ticket);

        let gate = AdmissionGate::from_view(&view, 0);
        assert_eq!(gate.project_in_flight, 1);
    }

    #[test]
    fn from_view_counts_harness_ticket() {
        let mut view = empty_view();
        let mut ticket = test_ticket("T-1", TicketKind::Harness);
        ticket.state = TicketState::Leased;
        view.tickets.insert(ticket.id.clone(), ticket);

        let gate = AdmissionGate::from_view(&view, 0);
        assert_eq!(gate.harness_in_flight, 1);
    }

    #[test]
    fn from_view_tracks_parent_in_flight() {
        let mut view = empty_view();
        let parent_id = TicketId::new("T-1").expect("valid id");
        let mut ticket = test_ticket("T-2", TicketKind::Work);
        ticket.state = TicketState::Leased;
        ticket.parent = Some(parent_id.clone());
        view.tickets.insert(ticket.id.clone(), ticket);

        let gate = AdmissionGate::from_view(&view, 0);
        assert_eq!(gate.parent_in_flight.get(&parent_id), Some(&1));
    }

    #[test]
    fn from_view_counts_multiple_children_per_parent() {
        let mut view = empty_view();
        let parent_id = TicketId::new("T-1").expect("valid id");

        for i in 2..=4 {
            let mut ticket = test_ticket(&format!("T-{}", i), TicketKind::Work);
            ticket.state = TicketState::Leased;
            ticket.parent = Some(parent_id.clone());
            view.tickets.insert(ticket.id.clone(), ticket);
        }

        let gate = AdmissionGate::from_view(&view, 0);
        assert_eq!(gate.parent_in_flight.get(&parent_id), Some(&3));
    }

    #[test]
    fn record_admission_increments_project_in_flight() {
        let mut gate = AdmissionGate {
            project_in_flight: 5,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 0,
            commands_in_flight: 0,
        };
        let ticket = test_ticket("T-1", TicketKind::Work);

        gate.record_admission(&ticket);
        assert_eq!(gate.project_in_flight, 6);
    }

    #[test]
    fn record_admission_increments_parent_in_flight() {
        let mut gate = AdmissionGate {
            project_in_flight: 0,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 0,
            commands_in_flight: 0,
        };
        let parent_id = TicketId::new("T-1").expect("valid id");
        let mut ticket = test_ticket("T-2", TicketKind::Work);
        ticket.parent = Some(parent_id.clone());

        gate.record_admission(&ticket);
        assert_eq!(gate.parent_in_flight.get(&parent_id), Some(&1));
    }

    #[test]
    fn record_admission_increments_harness_in_flight() {
        let mut gate = AdmissionGate {
            project_in_flight: 0,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 0,
            commands_in_flight: 0,
        };
        let ticket = test_ticket("T-1", TicketKind::Harness);

        gate.record_admission(&ticket);
        assert_eq!(gate.harness_in_flight, 1);
    }

    #[test]
    fn record_admission_leaves_commands_in_flight_unchanged() {
        let mut gate = AdmissionGate {
            project_in_flight: 0,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 0,
            commands_in_flight: 42,
        };
        let ticket = test_ticket("T-1", TicketKind::Work);

        gate.record_admission(&ticket);
        assert_eq!(gate.commands_in_flight, 42);
    }

    #[test]
    fn check_all_clear_admits() {
        let gate = AdmissionGate {
            project_in_flight: 0,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 0,
            commands_in_flight: 0,
        };
        let ticket = test_ticket("T-1", TicketKind::Work);
        let policy = SchedulingPolicy::conservative_default();
        let availability = AlwaysAvailable;

        let decision = gate.check(&ticket, &policy, &availability);
        assert_eq!(decision, AdmissionDecision::Admit);
    }

    #[test]
    fn check_project_ceiling_refuses() {
        let policy = SchedulingPolicy::conservative_default();
        let gate = AdmissionGate {
            project_in_flight: policy.effective_max_in_flight_per_project(),
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 0,
            commands_in_flight: 0,
        };
        let ticket = test_ticket("T-1", TicketKind::Work);
        let availability = AlwaysAvailable;

        let decision = gate.check(&ticket, &policy, &availability);
        assert_eq!(
            decision,
            AdmissionDecision::Refuse(AdmissionRefusal::ProjectCeiling)
        );
    }

    #[test]
    fn check_parent_ceiling_refuses() {
        let policy = SchedulingPolicy::conservative_default();
        let parent_id = TicketId::new("T-1").expect("valid id");
        let mut parent_map = BTreeMap::new();
        parent_map.insert(parent_id.clone(), policy.max_in_flight_per_parent);

        let gate = AdmissionGate {
            project_in_flight: 0,
            parent_in_flight: parent_map,
            harness_in_flight: 0,
            commands_in_flight: 0,
        };
        let mut ticket = test_ticket("T-2", TicketKind::Work);
        ticket.parent = Some(parent_id.clone());
        let availability = AlwaysAvailable;

        let decision = gate.check(&ticket, &policy, &availability);
        assert_eq!(
            decision,
            AdmissionDecision::Refuse(AdmissionRefusal::ParentCeiling(parent_id))
        );
    }

    #[test]
    fn check_command_ceiling_refuses() {
        let policy = SchedulingPolicy::conservative_default();
        let gate = AdmissionGate {
            project_in_flight: 0,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 0,
            commands_in_flight: policy.max_concurrent_commands,
        };
        let ticket = test_ticket("T-1", TicketKind::Work);
        let availability = AlwaysAvailable;

        let decision = gate.check(&ticket, &policy, &availability);
        assert_eq!(
            decision,
            AdmissionDecision::Refuse(AdmissionRefusal::CommandCeiling)
        );
    }

    #[test]
    fn check_harness_capacity_exceeded_in_steady_state() {
        let mut policy = SchedulingPolicy::conservative_default();
        policy.mode = crate::policy::SchedulingMode::SteadyState;
        policy.harness_capacity_fraction = 0.25; // Allow at most 1 harness in 4

        let gate = AdmissionGate {
            project_in_flight: 3, // 3 work tickets already
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 1, // 1 harness already
            commands_in_flight: 0,
        };
        let ticket = test_ticket("T-5", TicketKind::Harness);
        let availability = AlwaysAvailable;

        // After admitting another harness: (1+1)/(3+1) = 0.5 > 0.25
        let decision = gate.check(&ticket, &policy, &availability);
        assert_eq!(
            decision,
            AdmissionDecision::Refuse(AdmissionRefusal::HarnessCapacityExceeded)
        );
    }

    #[test]
    fn check_harness_capacity_suspended_during_ignition() {
        let mut policy = SchedulingPolicy::conservative_default();
        policy.mode = crate::policy::SchedulingMode::Ignition;
        policy.ignition.suspend_harness_cap = true;
        policy.harness_capacity_fraction = 0.0; // Even 0% would normally block

        let gate = AdmissionGate {
            project_in_flight: 5,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 5,
            commands_in_flight: 0,
        };
        let ticket = test_ticket("T-11", TicketKind::Harness);
        let availability = AlwaysAvailable;

        // Should admit despite impossible fraction, because cap is suspended
        let decision = gate.check(&ticket, &policy, &availability);
        assert_eq!(decision, AdmissionDecision::Admit);
    }

    #[test]
    fn check_harness_capacity_checked_in_ignition_if_not_suspended() {
        let mut policy = SchedulingPolicy::conservative_default();
        policy.mode = crate::policy::SchedulingMode::Ignition;
        policy.ignition.suspend_harness_cap = false;
        policy.harness_capacity_fraction = 0.1;
        // High enough that gate 1 (project ceiling) never fires here; this test isolates gate 4
        // (harness capacity), which is otherwise unreachable once project_in_flight >= the
        // conservative default's max_in_flight_per_project of 8.
        policy.max_in_flight_per_project = 20;

        let gate = AdmissionGate {
            project_in_flight: 9,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 1,
            commands_in_flight: 0,
        };
        let ticket = test_ticket("T-11", TicketKind::Harness);
        let availability = AlwaysAvailable;

        // (1+1)/(9+1) = 0.2 > 0.1 should be refused
        let decision = gate.check(&ticket, &policy, &availability);
        assert_eq!(
            decision,
            AdmissionDecision::Refuse(AdmissionRefusal::HarnessCapacityExceeded)
        );
    }

    #[test]
    fn check_provider_unavailable_refuses() {
        let policy = SchedulingPolicy::conservative_default();
        let gate = AdmissionGate {
            project_in_flight: 0,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 0,
            commands_in_flight: 0,
        };
        let ticket = test_ticket("T-1", TicketKind::Work);
        let availability = NeverAvailable;

        let decision = gate.check(&ticket, &policy, &availability);
        assert_eq!(
            decision,
            AdmissionDecision::Refuse(AdmissionRefusal::ProviderUnavailable(
                tm_types::Role::CoderFast
            ))
        );
    }

    #[test]
    fn check_human_required_bypasses_availability() {
        let policy = SchedulingPolicy::conservative_default();
        let gate = AdmissionGate {
            project_in_flight: 0,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 0,
            commands_in_flight: 0,
        };
        let mut ticket = test_ticket("T-1", TicketKind::Work);
        ticket.executor.human_required = true;
        let availability = NeverAvailable; // Provider unavailable, but human required

        let decision = gate.check(&ticket, &policy, &availability);
        assert_eq!(decision, AdmissionDecision::Admit);
    }

    #[test]
    fn check_gates_evaluated_in_order_project_before_parent() {
        // Both project and parent ceilings are exceeded, but project should refuse first
        let policy = SchedulingPolicy::conservative_default();
        let parent_id = TicketId::new("T-1").expect("valid id");
        let mut parent_map = BTreeMap::new();
        parent_map.insert(parent_id.clone(), policy.max_in_flight_per_parent);

        let gate = AdmissionGate {
            project_in_flight: policy.effective_max_in_flight_per_project(),
            parent_in_flight: parent_map,
            harness_in_flight: 0,
            commands_in_flight: 0,
        };
        let mut ticket = test_ticket("T-2", TicketKind::Work);
        ticket.parent = Some(parent_id);
        let availability = AlwaysAvailable;

        let decision = gate.check(&ticket, &policy, &availability);
        assert_eq!(
            decision,
            AdmissionDecision::Refuse(AdmissionRefusal::ProjectCeiling)
        );
    }

    #[test]
    fn check_gates_evaluated_in_order_parent_before_command() {
        let policy = SchedulingPolicy::conservative_default();
        let parent_id = TicketId::new("T-1").expect("valid id");
        let mut parent_map = BTreeMap::new();
        parent_map.insert(parent_id.clone(), policy.max_in_flight_per_parent);

        let gate = AdmissionGate {
            project_in_flight: 0,
            parent_in_flight: parent_map,
            harness_in_flight: 0,
            commands_in_flight: policy.max_concurrent_commands,
        };
        let mut ticket = test_ticket("T-2", TicketKind::Work);
        ticket.parent = Some(parent_id.clone());
        let availability = AlwaysAvailable;

        let decision = gate.check(&ticket, &policy, &availability);
        assert_eq!(
            decision,
            AdmissionDecision::Refuse(AdmissionRefusal::ParentCeiling(parent_id))
        );
    }

    #[test]
    fn check_harness_not_checked_for_non_harness_work() {
        let mut policy = SchedulingPolicy::conservative_default();
        policy.harness_capacity_fraction = 0.0; // Impossible fraction

        let gate = AdmissionGate {
            project_in_flight: 1,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 1,
            commands_in_flight: 0,
        };
        let ticket = test_ticket("T-2", TicketKind::Work); // Not Harness
        let availability = AlwaysAvailable;

        // Should not check harness capacity for non-harness work
        let decision = gate.check(&ticket, &policy, &availability);
        assert_eq!(decision, AdmissionDecision::Admit);
    }

    #[test]
    fn harness_share_calculation_with_zero_current_project() {
        let gate = AdmissionGate {
            project_in_flight: 0,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 0,
            commands_in_flight: 0,
        };

        let share = gate.harness_share_after_admitting();
        // (0+1) / (0+1).max(1) = 1/1 = 1.0
        assert_eq!(share, 1.0);
    }

    #[test]
    fn harness_share_calculation_mixed() {
        let gate = AdmissionGate {
            project_in_flight: 8,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 2,
            commands_in_flight: 0,
        };

        let share = gate.harness_share_after_admitting();
        // (2+1) / (8+1) = 3/9 = 0.333...
        assert!((share - (3.0 / 9.0)).abs() < 0.0001);
    }
}
