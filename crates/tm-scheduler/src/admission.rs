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
    /// The ticket's own remaining budget, after reserving
    /// [`SchedulingPolicy::verification_budget_reserve_fraction`] of its limit in some dimension
    /// for the verification phase that must follow, would not even cover that reserve —
    /// dispatching the work phase now would either strand it mid-run or leave nothing to verify
    /// with (`SPEC.md` §31.4, `docs/audit-2026-09-18-fable.md` B-10).
    #[error(
        "ticket {0}'s remaining budget cannot both fund this dispatch and its verification reserve"
    )]
    BudgetInsufficientForVerificationReserve(TicketId),
    /// The ticket's budget permits nothing more (`Budget::none()` included: it means zero, not
    /// "untracked"), so a worker could not take a single step. Dispatching it would only hand
    /// the lease straight back, over and over.
    #[error("ticket {0} has no budget left to work with")]
    BudgetExhausted(TicketId),
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

        // Gate 6: Budget, reserving a verification slice (`SPEC.md` §31.4,
        // `docs/audit-2026-09-18-fable.md` B-10). A budget that permits nothing more is refused
        // outright (`Budget::none()` included: it means zero, per `tm_types::Budget`'s `Default`
        // contract, which the agent loop and the store's usage accounting both enforce); a real,
        // finite budget is refused once what remains can't cover its verification reserve.
        if let Some(refusal) = Self::budget_reserve_refusal(ticket, policy) {
            return AdmissionDecision::Refuse(refusal);
        }

        AdmissionDecision::Admit
    }

    /// [`AdmissionGate::check`]'s Gate 6: `Some` if `ticket.budget` is a real, finite budget
    /// (not [`Budget::none`]'s "untracked" sentinel, not [`Budget::unlimited`]) whose remaining
    /// room in some dimension has fallen to or below the verification reserve
    /// [`SchedulingPolicy::verification_budget_reserve_fraction`] carves out of that dimension's
    /// limit. This is deliberately conservative rather than a precise cost estimate (`tm-scheduler`
    /// has no per-role pricing to draw on — see `tm_context::sections::build_budget` for where a
    /// worker actually sees the tier menu): if what remains cannot even cover the reserve, it
    /// certainly cannot cover both this dispatch's work and the verification after it.
    fn budget_reserve_refusal(
        ticket: &Ticket,
        policy: &SchedulingPolicy,
    ) -> Option<AdmissionRefusal> {
        let budget = ticket.budget;
        if budget.is_exhausted() {
            return Some(AdmissionRefusal::BudgetExhausted(ticket.id.clone()));
        }
        let remaining = budget.remaining();
        let fraction = policy.verification_budget_reserve_fraction.clamp(0.0, 1.0);

        let short = |limit: u64, remaining: u64| -> bool {
            limit != u64::MAX && remaining <= (limit as f64 * fraction).round() as u64
        };

        if short(budget.tokens, remaining.tokens)
            || short(budget.dollars_micros, remaining.dollars_micros)
            || short(budget.wall_seconds, remaining.wall_seconds)
        {
            Some(AdmissionRefusal::BudgetInsufficientForVerificationReserve(
                ticket.id.clone(),
            ))
        } else {
            None
        }
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
            due: None,
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
            // These fixtures don't care about budget: ask for unlimited explicitly, since
            // `Budget::none()` means zero and is refused.
            budget: tm_types::Budget::unlimited(),
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

    // ---- Gate 6: budget reserve (SPEC.md §31.4, docs/audit-2026-09-18-fable.md B-10) --------

    fn empty_gate() -> AdmissionGate {
        AdmissionGate {
            project_in_flight: 0,
            parent_in_flight: BTreeMap::new(),
            harness_in_flight: 0,
            commands_in_flight: 0,
        }
    }

    #[test]
    fn check_refuses_a_ticket_whose_budget_permits_nothing() {
        // `Budget::none()` means zero (`tm_types::Budget`'s `Default` contract), and a worker
        // leased such a ticket can't afford one step: it would hand the lease straight back,
        // and before this gate refused it the scheduler re-leased it every tick, forever.
        let gate = empty_gate();
        let mut ticket = test_ticket("T-1", TicketKind::Work);
        ticket.budget = tm_types::Budget::none();
        let policy = SchedulingPolicy::conservative_default();
        let availability = AlwaysAvailable;

        assert_eq!(
            gate.check(&ticket, &policy, &availability),
            AdmissionDecision::Refuse(AdmissionRefusal::BudgetExhausted(ticket.id.clone()))
        );
    }

    #[test]
    fn check_admits_a_ticket_with_plenty_of_real_budget_left() {
        let gate = empty_gate();
        let mut ticket = test_ticket("T-1", TicketKind::Work);
        ticket.budget = tm_types::Budget::new(1_000, 1_000, 1_000);
        let policy = SchedulingPolicy::conservative_default();
        let availability = AlwaysAvailable;

        assert_eq!(
            gate.check(&ticket, &policy, &availability),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn check_admits_a_ticket_with_an_unlimited_budget_regardless_of_spend() {
        let gate = empty_gate();
        let mut ticket = test_ticket("T-1", TicketKind::Work);
        ticket.budget = tm_types::Budget::unlimited();
        ticket
            .budget
            .try_spend(tm_types::Spend::tokens(u64::MAX / 2))
            .expect("unlimited always has room");
        let policy = SchedulingPolicy::conservative_default();
        let availability = AlwaysAvailable;

        assert_eq!(
            gate.check(&ticket, &policy, &availability),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn check_refuses_a_ticket_whose_remaining_budget_cannot_cover_its_verification_reserve() {
        let gate = empty_gate();
        let mut ticket = test_ticket("T-1", TicketKind::Work);
        // 20% conservative_default() reserve of a 1000-token limit is 200; spending 850 leaves
        // 150 remaining, below the reserve.
        ticket.budget = tm_types::Budget::new(1_000, u64::MAX, u64::MAX);
        ticket
            .budget
            .try_spend(tm_types::Spend::tokens(850))
            .expect("spend within the limit");
        let policy = SchedulingPolicy::conservative_default();
        let availability = AlwaysAvailable;

        assert_eq!(
            gate.check(&ticket, &policy, &availability),
            AdmissionDecision::Refuse(AdmissionRefusal::BudgetInsufficientForVerificationReserve(
                ticket.id.clone()
            ))
        );
    }

    #[test]
    fn check_admits_a_ticket_right_at_the_edge_of_its_reserve_with_one_token_more() {
        let gate = empty_gate();
        let mut ticket = test_ticket("T-1", TicketKind::Work);
        // Reserve is exactly 200; spending 799 leaves 201 remaining -- just clear of the
        // reserve boundary (the gate refuses at `remaining <= reserve`, so 200 itself refuses).
        ticket.budget = tm_types::Budget::new(1_000, u64::MAX, u64::MAX);
        ticket
            .budget
            .try_spend(tm_types::Spend::tokens(799))
            .expect("spend within the limit");
        let policy = SchedulingPolicy::conservative_default();
        let availability = AlwaysAvailable;

        assert_eq!(
            gate.check(&ticket, &policy, &availability),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn check_a_zero_reserve_fraction_still_refuses_true_exhaustion() {
        let gate = empty_gate();
        let mut ticket = test_ticket("T-1", TicketKind::Work);
        ticket.budget = tm_types::Budget::new(100, u64::MAX, u64::MAX);
        ticket
            .budget
            .try_spend(tm_types::Spend::tokens(100))
            .expect("spend the full limit");
        let mut policy = SchedulingPolicy::conservative_default();
        policy.verification_budget_reserve_fraction = 0.0;
        let availability = AlwaysAvailable;

        assert_eq!(
            gate.check(&ticket, &policy, &availability),
            AdmissionDecision::Refuse(AdmissionRefusal::BudgetExhausted(ticket.id.clone()))
        );
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
