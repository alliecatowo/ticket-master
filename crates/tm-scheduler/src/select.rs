//! Deterministic selection: ranking `Ready` tickets and matching them to executors.
//!
//! Everything here is pure: no clock reads (`now` is a parameter), no I/O, no randomness.
//! [`rank_ready`] is the ordering documented in `SPEC.md` §5 — priority desc, critical-path
//! length desc, milestone deadline proximity (soonest first), ticket id asc — and it is total:
//! two equal-priority, equal-depth, equal-deadline tickets always compare by id, so the sort
//! never depends on input order. [`select_next`] then walks that ranking looking for the first
//! ticket whose [`tm_core::ticket::ExecutorRequirements`] can be met right now.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use tm_core::executor::ExecutorCapabilities;
use tm_core::graph::DependencyGraph;
use tm_core::ticket::{ExecutorRequirements, Ticket, TicketState};
use tm_core::view::SchedulerView;
use tm_types::{MilestoneId, Role, TicketId, Timestamp};

use crate::policy::SchedulingPolicy;

/// A ticket ranked and matched to the role that should execute it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorMatch {
    /// The ticket selected to run next.
    pub ticket: TicketId,
    /// The role its lease should be executed as.
    pub role: Role,
}

/// Why [`select_next`] found nothing to run, for callers (`plan.rs`) that need to distinguish
/// "nothing is ready" from "something is ready but no executor can take it" (the latter drives
/// a `provider.exhausted` record, per `SPEC.md` §5).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectionError {
    /// No ticket in `Ready` state exists in the view at all.
    #[error("no ready tickets")]
    NoReadyTickets,
    /// At least one `Ready` ticket exists, but every one of them needs a role with no healthy
    /// provider available right now. Carries the first (highest-ranked) such role so the caller
    /// can record `provider.exhausted` for it.
    #[error("no executor available for role {0}")]
    NoExecutorAvailable(Role),
}

/// Read-only capability query the caller (ultimately backed by `tm-provider`'s `FabricState`)
/// supplies to [`select_next`]: is at least one healthy executor available for `role` right now?
/// Kept as a trait so `tm-scheduler` does not depend on `tm-provider`'s routing internals, only
/// on this narrow yes/no question.
///
/// Deliberately distinct from `tm_core::Executor` (the live dispatch abstraction a
/// `crate::dispatch::ExecutorDispatcher` drives) and from [`capabilities_satisfy`] (the
/// structural capability check against one concrete executor): this trait only answers "is
/// *some* executor free for this role right now", asked before a ticket is ranked/selected at
/// all, with no knowledge of which concrete executor would take it.
pub trait ExecutorAvailability {
    /// True when `role` currently has at least one admissible executor (human or provider).
    fn is_available(&self, role: Role) -> bool;
}

/// Why a concrete executor's declared [`ExecutorCapabilities`] cannot serve a ticket's
/// [`ExecutorRequirements`], from [`capabilities_satisfy`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CapabilityMismatch {
    /// The ticket needs a human and this executor cannot suspend for one.
    #[error("ticket requires a human executor but the candidate is not interactive")]
    NotInteractive,
    /// A non-human ticket needs an executor that can call tools; this one cannot.
    #[error("ticket requires tool use but the candidate declares tool_use = false")]
    NoToolUse,
    /// Every executor is expected to accept a compiled context pack (`SPEC.md` §24.1); one that
    /// does not is not a usable executor at all, regardless of the ticket.
    #[error("candidate does not accept a compiled context pack")]
    RejectsContextPack,
}

/// Match a ticket's [`ExecutorRequirements`] against one concrete executor's declared
/// [`ExecutorCapabilities`], refusing a mismatch rather than assuming compatibility
/// (`SPEC.md` §24.2). Pure and total: never panics, never needs live state.
pub fn capabilities_satisfy(
    requirements: &ExecutorRequirements,
    capabilities: &ExecutorCapabilities,
) -> Result<(), CapabilityMismatch> {
    if !capabilities.accepts_context_pack {
        return Err(CapabilityMismatch::RejectsContextPack);
    }
    if requirements.human_required {
        if !capabilities.interactive {
            return Err(CapabilityMismatch::NotInteractive);
        }
        return Ok(());
    }
    if !capabilities.tool_use {
        return Err(CapabilityMismatch::NoToolUse);
    }
    Ok(())
}

/// Compare two tickets per the `SPEC.md` §5 ordering: priority desc, then critical-path length
/// desc, then milestone deadline proximity asc (a ticket with no milestone, or a milestone with
/// no recorded deadline, sorts after every ticket with a known deadline), then ticket id asc.
/// Total: never returns `Equal` for two distinct ticket ids.
pub fn compare_tickets(
    a: &Ticket,
    b: &Ticket,
    graph: &DependencyGraph,
    milestone_deadlines: &BTreeMap<MilestoneId, Timestamp>,
) -> Ordering {
    // Priority descending: higher priority first, so reverse the comparison.
    match b.priority.cmp(&a.priority) {
        Ordering::Equal => {}
        other => return other,
    }

    // Critical-path length descending: longer path first, so reverse the comparison.
    let a_depth = graph.critical_path_length(&a.id);
    let b_depth = graph.critical_path_length(&b.id);
    match b_depth.cmp(&a_depth) {
        Ordering::Equal => {}
        other => return other,
    }

    // Milestone deadline proximity ascending: soonest deadline first.
    // None sorts after any Some value.
    let a_deadline = a
        .milestone
        .as_ref()
        .and_then(|m| milestone_deadlines.get(m));
    let b_deadline = b
        .milestone
        .as_ref()
        .and_then(|m| milestone_deadlines.get(m));
    match (a_deadline, b_deadline) {
        (Some(a_ts), Some(b_ts)) => match a_ts.cmp(b_ts) {
            Ordering::Equal => {}
            other => return other,
        },
        (Some(_), None) => return Ordering::Less,
        (None, Some(_)) => return Ordering::Greater,
        (None, None) => {}
    }

    // Ticket id ascending (total ordering since ids are unique).
    a.id.cmp(&b.id)
}

/// Every ticket in [`SchedulerView::tickets`] currently in [`TicketState::Ready`], as a stable
/// `BTreeMap`-ordered slice (the map's key order gives a deterministic starting point before
/// [`rank_ready`] imposes the real ordering).
pub fn ready_tickets(view: &SchedulerView) -> Vec<&Ticket> {
    view.tickets
        .values()
        .filter(|t| t.state == TicketState::Ready)
        .collect()
}

/// Rank every `Ready` ticket in `view` per [`compare_tickets`], returning ids best-first.
/// Deterministic: shuffling `view.tickets`' insertion order (impossible for a `BTreeMap`, but
/// relevant for whatever collection the caller built it from) never changes the result, because
/// `compare_tickets` is total and `sort_by` is applied to a fully materialized `Vec`.
pub fn rank_ready(view: &SchedulerView, policy: &SchedulingPolicy) -> Vec<TicketId> {
    let mut tickets = ready_tickets(view);
    tickets.sort_by(|a, b| compare_tickets(a, b, &view.graph, &policy.milestone_deadlines));
    tickets.into_iter().map(|t| t.id.clone()).collect()
}

/// True when `requirements` can be met right now: a human is available if `human_required`, or
/// `availability.is_available(requirements.role)` — [`tm_types::Tolerance`] on the requirement
/// does not loosen this call (tolerance governs provider *degrade*, handled inside
/// `ExecutorAvailability`'s implementation, not here).
pub fn executor_matches(
    requirements: &ExecutorRequirements,
    availability: &dyn ExecutorAvailability,
) -> bool {
    // Human-required tickets are always matchable (human assignment is out of band).
    if requirements.human_required {
        return true;
    }
    // Otherwise check if the role has an available executor.
    availability.is_available(requirements.role)
}

/// Walk `ranked` in order, returning the first ticket whose executor requirements
/// [`executor_matches`], paired with the role it should lease as.
///
/// # Errors
/// - [`SelectionError::NoReadyTickets`] if `ranked` is empty.
/// - [`SelectionError::NoExecutorAvailable`] with the first ranked ticket's role if none of
///   `ranked` currently matches an available executor.
pub fn select_next(
    ranked: &[TicketId],
    view: &SchedulerView,
    availability: &dyn ExecutorAvailability,
) -> Result<ExecutorMatch, SelectionError> {
    if ranked.is_empty() {
        return Err(SelectionError::NoReadyTickets);
    }

    for ticket_id in ranked {
        // Invariant: every id in ranked came from rank_ready(view, ..), so it is present.
        let ticket = view
            .tickets
            .get(ticket_id)
            .expect("ranked ticket not found in view (invariant: rank_ready source was this view)");

        if executor_matches(&ticket.executor, availability) {
            return Ok(ExecutorMatch {
                ticket: ticket_id.clone(),
                role: ticket.executor.role,
            });
        }
    }

    // No match found; report the first ranked ticket's role as exhausted.
    let first_ticket = view.tickets.get(&ranked[0]).expect(
        "first ranked ticket not found in view (invariant: rank_ready source was this view)",
    );
    Err(SelectionError::NoExecutorAvailable(
        first_ticket.executor.role,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tm_core::graph::DependencyGraph;
    use tm_core::ticket::{
        ExecutorRequirements, RetryPolicy, Ticket, TicketKind, TicketState, VerificationPolicy,
    };
    use tm_types::{Authority, Budget, MilestoneId, Role, TicketId, Timestamp, Tolerance};

    /// Constructs a minimal test ticket with the given id, state, and priority.
    fn make_ticket(id: &str, state: TicketState, priority: i32) -> Ticket {
        Ticket {
            id: TicketId::new(id).unwrap(),
            kind: TicketKind::Work,
            objective: "test".to_string(),
            state,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            authority: Authority::none(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: Role::CoderFast,
                human_required: false,
                min_capability: Tolerance::Preferred,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::None,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 10,
                backoff_multiplier: 2.0,
                max_delay_seconds: 120,
            },
            cycle: None,
            attempts: 0,
            failures: vec![],
            priority,
            created: Timestamp::EPOCH,
            updated: Timestamp::EPOCH,
        }
    }

    /// Constructs a test ticket with custom executor role and human_required flag.
    fn make_ticket_with_executor(
        id: &str,
        state: TicketState,
        role: Role,
        human_required: bool,
    ) -> Ticket {
        let mut t = make_ticket(id, state, 0);
        t.executor = ExecutorRequirements {
            role,
            human_required,
            min_capability: Tolerance::Preferred,
        };
        t
    }

    #[test]
    fn compare_tickets_priority_descending() {
        let low_priority = make_ticket("T-1", TicketState::Ready, 1);
        let high_priority = make_ticket("T-2", TicketState::Ready, 10);
        let graph = DependencyGraph::build([], [], []);
        let deadlines = BTreeMap::new();

        let result = compare_tickets(&high_priority, &low_priority, &graph, &deadlines);
        assert_eq!(result, Ordering::Less, "higher priority should come first");
    }

    #[test]
    fn compare_tickets_equal_priority_uses_critical_path_length() {
        let same_priority = 5;
        let t1 = make_ticket("T-1", TicketState::Ready, same_priority);
        let t2 = make_ticket("T-2", TicketState::Ready, same_priority);

        // Build a graph where T-1 depends on T-2 (T-1 has longer critical path).
        let graph = DependencyGraph::build(
            [],
            [tm_core::graph::DependencyEdge {
                from: t1.id.clone(),
                to: t2.id.clone(),
                kind: tm_core::ticket::DependencyKind::Hard,
            }],
            [],
        );
        let deadlines = BTreeMap::new();

        let result = compare_tickets(&t1, &t2, &graph, &deadlines);
        assert_eq!(
            result,
            Ordering::Less,
            "longer critical path should come first"
        );
    }

    #[test]
    fn compare_tickets_equal_depth_uses_milestone_deadline() {
        let same_priority = 5;
        let mut t1 = make_ticket("T-1", TicketState::Ready, same_priority);
        let mut t2 = make_ticket("T-2", TicketState::Ready, same_priority);

        let m1 = MilestoneId::new("M-1").unwrap();
        let m2 = MilestoneId::new("M-2").unwrap();
        t1.milestone = Some(m1.clone());
        t2.milestone = Some(m2.clone());

        let mut deadlines = BTreeMap::new();
        let early = Timestamp::from_unix_seconds(1000);
        let late = Timestamp::from_unix_seconds(2000);
        deadlines.insert(m1, early);
        deadlines.insert(m2, late);

        let graph = DependencyGraph::build([], [], []);

        let result = compare_tickets(&t1, &t2, &graph, &deadlines);
        assert_eq!(result, Ordering::Less, "earlier deadline should come first");
    }

    #[test]
    fn compare_tickets_milestone_with_deadline_sorts_before_no_milestone() {
        let same_priority = 5;
        let mut t_with_milestone = make_ticket("T-1", TicketState::Ready, same_priority);
        let t_no_milestone = make_ticket("T-2", TicketState::Ready, same_priority);

        let m1 = MilestoneId::new("M-1").unwrap();
        t_with_milestone.milestone = Some(m1.clone());

        let mut deadlines = BTreeMap::new();
        deadlines.insert(m1, Timestamp::from_unix_seconds(1000));

        let graph = DependencyGraph::build([], [], []);

        let result = compare_tickets(&t_with_milestone, &t_no_milestone, &graph, &deadlines);
        assert_eq!(
            result,
            Ordering::Less,
            "ticket with known deadline should come before one without"
        );
    }

    #[test]
    fn compare_tickets_equal_deadline_uses_ticket_id() {
        let same_priority = 5;
        let t1 = make_ticket("T-1", TicketState::Ready, same_priority);
        let t2 = make_ticket("T-2", TicketState::Ready, same_priority);

        let graph = DependencyGraph::build([], [], []);
        let deadlines = BTreeMap::new();

        let result = compare_tickets(&t1, &t2, &graph, &deadlines);
        assert_eq!(result, Ordering::Less, "T-1 should come before T-2");
    }

    #[test]
    fn compare_tickets_is_total_for_same_tickets() {
        let t = make_ticket("T-1", TicketState::Ready, 5);
        let graph = DependencyGraph::build([], [], []);
        let deadlines = BTreeMap::new();

        let result = compare_tickets(&t, &t, &graph, &deadlines);
        assert_eq!(result, Ordering::Equal, "same ticket should compare equal");
    }

    #[test]
    fn ready_tickets_filters_by_state() {
        let ready1 = make_ticket("T-1", TicketState::Ready, 0);
        let ready2 = make_ticket("T-2", TicketState::Ready, 0);
        let blocked = make_ticket("T-3", TicketState::Blocked, 0);

        let mut tickets = BTreeMap::new();
        tickets.insert(ready1.id.clone(), ready1);
        tickets.insert(ready2.id.clone(), ready2);
        tickets.insert(blocked.id.clone(), blocked);

        let view = SchedulerView {
            tickets,
            graph: DependencyGraph::build([], [], []),
            live_leases: BTreeMap::new(),
        };

        let result = ready_tickets(&view);
        assert_eq!(result.len(), 2, "should find exactly 2 ready tickets");
    }

    #[test]
    fn rank_ready_orders_by_priority_descending() {
        let low_priority = make_ticket("T-1", TicketState::Ready, 1);
        let high_priority = make_ticket("T-2", TicketState::Ready, 10);

        let mut tickets = BTreeMap::new();
        tickets.insert(low_priority.id.clone(), low_priority);
        tickets.insert(high_priority.id.clone(), high_priority);

        let view = SchedulerView {
            tickets,
            graph: DependencyGraph::build([], [], []),
            live_leases: BTreeMap::new(),
        };

        let policy = crate::policy::SchedulingPolicy::default();
        let ranked = rank_ready(&view, &policy);

        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].as_str(), "T-2", "high priority should be first");
        assert_eq!(ranked[1].as_str(), "T-1", "low priority should be second");
    }

    #[test]
    fn rank_ready_empty_view_returns_empty() {
        let view = SchedulerView {
            tickets: BTreeMap::new(),
            graph: DependencyGraph::build([], [], []),
            live_leases: BTreeMap::new(),
        };

        let policy = crate::policy::SchedulingPolicy::default();
        let ranked = rank_ready(&view, &policy);

        assert!(ranked.is_empty());
    }

    #[test]
    fn executor_matches_human_required_always_true() {
        let requirements = ExecutorRequirements {
            role: Role::CoderFast,
            human_required: true,
            min_capability: Tolerance::Preferred,
        };

        let availability = MockAvailability { available: false };
        assert!(executor_matches(&requirements, &availability));
    }

    #[test]
    fn executor_matches_role_available() {
        let requirements = ExecutorRequirements {
            role: Role::CoderFast,
            human_required: false,
            min_capability: Tolerance::Preferred,
        };

        let availability = MockAvailability { available: true };
        assert!(executor_matches(&requirements, &availability));
    }

    #[test]
    fn executor_matches_role_unavailable() {
        let requirements = ExecutorRequirements {
            role: Role::CoderFast,
            human_required: false,
            min_capability: Tolerance::Preferred,
        };

        let availability = MockAvailability { available: false };
        assert!(!executor_matches(&requirements, &availability));
    }

    #[test]
    fn select_next_empty_ranked_returns_no_ready_tickets() {
        let view = SchedulerView {
            tickets: BTreeMap::new(),
            graph: DependencyGraph::build([], [], []),
            live_leases: BTreeMap::new(),
        };

        let availability = MockAvailability { available: true };
        let result = select_next(&[], &view, &availability);

        assert_eq!(result, Err(SelectionError::NoReadyTickets));
    }

    #[test]
    fn select_next_returns_first_matching_ticket() {
        let t1 = make_ticket_with_executor("T-1", TicketState::Ready, Role::CoderFast, false);
        let t2 = make_ticket_with_executor("T-2", TicketState::Ready, Role::PlannerFrontier, false);

        let mut tickets = BTreeMap::new();
        tickets.insert(t1.id.clone(), t1.clone());
        tickets.insert(t2.id.clone(), t2.clone());

        let view = SchedulerView {
            tickets,
            graph: DependencyGraph::build([], [], []),
            live_leases: BTreeMap::new(),
        };

        let availability = MockAvailability { available: true };
        let ranked = vec![t2.id.clone(), t1.id.clone()];
        let result = select_next(&ranked, &view, &availability);

        assert!(result.is_ok());
        let matched = result.unwrap();
        assert_eq!(matched.ticket.as_str(), "T-2");
        assert_eq!(matched.role, Role::PlannerFrontier);
    }

    #[test]
    fn select_next_skips_unmatchable_tickets() {
        let t1 = make_ticket_with_executor("T-1", TicketState::Ready, Role::CoderFast, false);
        let t2 = make_ticket_with_executor("T-2", TicketState::Ready, Role::PlannerFrontier, false);

        let mut tickets = BTreeMap::new();
        tickets.insert(t1.id.clone(), t1.clone());
        tickets.insert(t2.id.clone(), t2.clone());

        let view = SchedulerView {
            tickets,
            graph: DependencyGraph::build([], [], []),
            live_leases: BTreeMap::new(),
        };

        // Only Role::PlannerFrontier is available
        let availability = SelectiveAvailability {
            available_role: Role::PlannerFrontier,
        };
        let ranked = vec![t1.id.clone(), t2.id.clone()];
        let result = select_next(&ranked, &view, &availability);

        assert!(result.is_ok());
        let matched = result.unwrap();
        assert_eq!(matched.ticket.as_str(), "T-2");
    }

    #[test]
    fn select_next_no_executor_available_reports_first_ranked_role() {
        let t1 = make_ticket_with_executor("T-1", TicketState::Ready, Role::CoderFast, false);
        let t2 = make_ticket_with_executor("T-2", TicketState::Ready, Role::PlannerFrontier, false);

        let mut tickets = BTreeMap::new();
        tickets.insert(t1.id.clone(), t1.clone());
        tickets.insert(t2.id.clone(), t2.clone());

        let view = SchedulerView {
            tickets,
            graph: DependencyGraph::build([], [], []),
            live_leases: BTreeMap::new(),
        };

        let availability = MockAvailability { available: false };
        let ranked = vec![t1.id.clone(), t2.id.clone()];
        let result = select_next(&ranked, &view, &availability);

        assert_eq!(
            result,
            Err(SelectionError::NoExecutorAvailable(Role::CoderFast))
        );
    }

    #[test]
    fn select_next_human_required_always_matches() {
        let human_required =
            make_ticket_with_executor("T-1", TicketState::Ready, Role::CoderFast, true);

        let mut tickets = BTreeMap::new();
        tickets.insert(human_required.id.clone(), human_required.clone());

        let view = SchedulerView {
            tickets,
            graph: DependencyGraph::build([], [], []),
            live_leases: BTreeMap::new(),
        };

        let availability = MockAvailability { available: false };
        let ranked = vec![human_required.id.clone()];
        let result = select_next(&ranked, &view, &availability);

        assert!(result.is_ok());
        let matched = result.unwrap();
        assert_eq!(matched.ticket.as_str(), "T-1");
    }

    #[test]
    fn equal_inputs_produce_equal_output() {
        // Build two identical SchedulerView + SchedulingPolicy pairs independently
        let make_view = || {
            let mut tickets = BTreeMap::new();
            let a = make_ticket("T-1", TicketState::Ready, 10);
            let b = make_ticket("T-2", TicketState::Ready, 5);
            tickets.insert(a.id.clone(), a);
            tickets.insert(b.id.clone(), b);

            SchedulerView {
                tickets,
                graph: DependencyGraph::build([], [], []),
                live_leases: BTreeMap::new(),
            }
        };

        let policy = crate::policy::SchedulingPolicy::default();

        let view1 = make_view();
        let view2 = make_view();

        let ranked1 = rank_ready(&view1, &policy);
        let ranked2 = rank_ready(&view2, &policy);

        assert_eq!(
            ranked1, ranked2,
            "identical views should produce identical rankings"
        );
    }

    // Mock implementation of ExecutorAvailability for testing
    struct MockAvailability {
        available: bool,
    }

    impl ExecutorAvailability for MockAvailability {
        fn is_available(&self, _role: Role) -> bool {
            self.available
        }
    }

    // Mock that only makes specific roles available
    struct SelectiveAvailability {
        available_role: Role,
    }

    impl ExecutorAvailability for SelectiveAvailability {
        fn is_available(&self, role: Role) -> bool {
            role == self.available_role
        }
    }

    fn caps(tool_use: bool, interactive: bool, accepts_context_pack: bool) -> ExecutorCapabilities {
        ExecutorCapabilities {
            streaming: false,
            tool_use,
            patch_output: true,
            interactive,
            accepts_context_pack,
            sandboxed: true,
            max_context_tokens: None,
            cost_class: tm_core::executor::CostClass::Standard,
        }
    }

    fn requirements(human_required: bool) -> ExecutorRequirements {
        ExecutorRequirements {
            role: Role::CoderFast,
            human_required,
            min_capability: Tolerance::Preferred,
        }
    }

    #[test]
    fn capabilities_satisfy_accepts_a_tool_using_executor_for_ordinary_work() {
        assert!(capabilities_satisfy(&requirements(false), &caps(true, false, true)).is_ok());
    }

    #[test]
    fn capabilities_satisfy_refuses_an_executor_without_tool_use() {
        assert_eq!(
            capabilities_satisfy(&requirements(false), &caps(false, false, true)),
            Err(CapabilityMismatch::NoToolUse)
        );
    }

    #[test]
    fn capabilities_satisfy_requires_interactive_for_human_required_tickets() {
        assert_eq!(
            capabilities_satisfy(&requirements(true), &caps(false, false, true)),
            Err(CapabilityMismatch::NotInteractive)
        );
        assert!(capabilities_satisfy(&requirements(true), &caps(false, true, true)).is_ok());
    }

    #[test]
    fn capabilities_satisfy_refuses_an_executor_that_rejects_context_packs() {
        assert_eq!(
            capabilities_satisfy(&requirements(false), &caps(true, false, false)),
            Err(CapabilityMismatch::RejectsContextPack)
        );
    }
}
