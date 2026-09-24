//! The pure planner: `plan()` composes readiness, lease expiry, admission, selection and retry.
//!
//! This is the headline property of the whole crate: `plan(view, now, policy)` is a pure
//! function. Identical `SchedulerView` + `Timestamp` + `SchedulingPolicy` must produce an
//! identical `Vec<SchedulerAction>`, in the same order, every time — including when the caller
//! shuffles whatever collection it built `view` from, since every internal collection this
//! module touches (`BTreeMap`, sorted `Vec`) is already order-independent by construction.
//! **Never introduce a `HashMap`/`HashSet` or iterate a non-`BTree`-ordered collection anywhere
//! in this module or its callees** — that is the one mistake that would silently break the
//! determinism guarantee.

use std::collections::{BTreeMap, BTreeSet};

use tm_core::lease::{Lease, LeaseStore, LeaseView};
use tm_core::ticket::{DependencyKind, FailureClass, Ticket, TicketKind, TicketState};
use tm_core::view::SchedulerView;
use tm_types::{LeaseId, MilestoneId, Role, TicketId, Timestamp};

use crate::admission::AdmissionGate;
use crate::policy::SchedulingPolicy;
use crate::retry::{decide_retry, EscalationReason, RetryOutcome};
use crate::select::{rank_ready, select_next, ExecutorAvailability, SelectionError};

/// [`ExecutorAvailability`] backed by [`SchedulingPolicy::available_roles`]: the plain-data
/// stand-in for live provider-fabric state that lets [`plan`] stay pure and three-argument
/// (`SPEC.md` §5) while still reusing [`crate::select`]'s trait-based matching.
struct PolicyAvailability<'a>(&'a std::collections::BTreeSet<Role>);

impl ExecutorAvailability for PolicyAvailability<'_> {
    fn is_available(&self, role: Role) -> bool {
        self.0.contains(&role)
    }
}

/// One action the driver should apply. Every variant is a description of a `tm-core` state
/// change plus enough detail to construct the `Store` call and the events it emits; `plan()`
/// never applies these itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerAction {
    /// The ticket's dependencies are now satisfied: move `Blocked` -> `Ready`.
    MarkReady(TicketId),
    /// The ticket's dependencies are no longer satisfied (a dependency reopened): move `Ready`
    /// -> `Blocked`.
    MarkBlocked(TicketId),
    /// Lease `ticket` to an executor of `executor`'s role, for `ttl_seconds`.
    Lease {
        /// The ticket to lease.
        ticket: TicketId,
        /// The role that should execute it.
        executor: Role,
        /// Lease time-to-live in seconds from grant.
        ttl_seconds: u32,
    },
    /// The lease has passed its TTL without a heartbeat: revert its ticket to `Ready` with
    /// authority reverted and an attempt counted (mirrors `tm_core::lease::expire_due`).
    ExpireLease(LeaseId),
    /// Schedule a retry for `ticket`, eligible again no earlier than `after`.
    Retry {
        /// The ticket to retry.
        ticket: TicketId,
        /// Earliest retry time.
        after: Timestamp,
    },
    /// Stop retrying `ticket` and escalate, with the reason.
    Escalate {
        /// The ticket being escalated.
        ticket: TicketId,
        /// Why retrying stopped.
        reason: EscalationReason,
    },
    /// Open a recovery ticket/path for `ticket` (`SPEC.md` §4.3 rule 8, `Recovery` state).
    OpenRecovery(TicketId),
    /// Every member of `MilestoneId` is `Closed` or `Cancelled`: close the milestone.
    CloseMilestone(MilestoneId),
    /// Nothing to do this tick. Emitted (rather than an empty `Vec`) when the caller needs a
    /// uniform "the planner ran and found nothing" signal, e.g. for driver event logging;
    /// `plan()` itself may return an empty `Vec` instead — callers should treat both the same.
    Noop,
}

/// The pure planner. See the module docs for the determinism contract.
///
/// Composition order (each phase's output can affect a later phase's input within the same
/// call, since later phases read `view` plus the actions already decided, not `view` alone):
/// 1. **Lease expiry** — every live lease past its TTL as of `now` yields
///    [`SchedulerAction::ExpireLease`]; the ticket it held is treated as `Ready` again for the
///    remaining phases (its actual state in `view` is still `Leased`/`Running`, but the expiry
///    action implies the revert, so selection must not skip it as still-leased).
/// 2. **Readiness** — every `Blocked` ticket whose hard dependencies are all `Closed` in `view`
///    yields [`SchedulerAction::MarkReady`]; every `Ready` ticket whose dependencies are no
///    longer all `Closed` (a dependency reopened) yields [`SchedulerAction::MarkBlocked`].
/// 3. **Retry/escalation** — every ticket in `TicketState::Recovery` (its most recent
///    `failures` entry gives the `FailureClass`) is passed to [`decide_retry`], yielding
///    [`SchedulerAction::Retry`] or [`SchedulerAction::Escalate`]; an escalation from a ticket
///    that is a member of an active `Loop` cycle also yields [`SchedulerAction::OpenRecovery`]
///    for its cycle's designated recovery ticket, if the crate's cycle-recovery convention
///    identifies one (see `SPEC.md` §4.3 rule 8 / rule on cycles).
/// 4. **Admission + selection** — build an [`AdmissionGate`] from `view` (plus lease-expiry
///    phase 1's freed capacity), rank every `Ready`-or-just-marked-ready ticket via
///    [`rank_ready`], and walk the ranking: for each candidate, check the gate; on `Admit`,
///    emit [`SchedulerAction::Lease`], call `record_admission`, and continue to the next
///    candidate (multiple leases may be granted in one tick, bounded by the ceilings); on
///    `Refuse`, skip that ticket and continue ranking (a per-parent or per-role refusal must not
///    block unrelated tickets later in the ranking).
/// 5. **Milestones** — for every distinct `MilestoneId` referenced by any ticket in `view`
///    (via `ticket.milestone`), if every ticket sharing that id is `Closed` or `Cancelled`,
///    emit [`SchedulerAction::CloseMilestone`]. Skip milestones already implied closed by an
///    earlier planning pass — `plan()` has no memory across calls, so the driver is responsible
///    for not re-emitting `CloseMilestone` for a milestone already `Closed` in durable state;
///    this phase only reads ticket state from `view`, which is enough since `view` is a fresh
///    snapshot each call.
///
/// All actions from phases 1-5 are concatenated in that phase order; within a phase, ties are
/// broken by ticket id ascending (or, for milestones, milestone id ascending) — never by
/// iteration order of a non-deterministic collection.
pub fn plan(
    view: &SchedulerView,
    now: Timestamp,
    policy: &SchedulingPolicy,
) -> Vec<SchedulerAction> {
    let availability = PolicyAvailability(&policy.available_roles);
    let mut actions = Vec::new();

    // Phase 1: lease expiry.
    struct LiveLeaseAdapter<'a>(&'a BTreeMap<LeaseId, Lease>);
    impl LeaseView for LiveLeaseAdapter<'_> {
        fn live_leases(&self) -> Vec<&Lease> {
            self.0.values().collect()
        }
    }
    let reversions = LeaseStore::expire_due(&LiveLeaseAdapter(&view.live_leases), now);
    let expired_tickets: BTreeSet<TicketId> = reversions.iter().map(|r| r.ticket.clone()).collect();
    for reversion in &reversions {
        actions.push(SchedulerAction::ExpireLease(reversion.lease.clone()));
    }

    // Phase 2: readiness.
    let closed: BTreeSet<TicketId> = view
        .tickets
        .values()
        .filter(|t| t.state == TicketState::Closed)
        .map(|t| t.id.clone())
        .collect();
    for ticket in view.tickets.values() {
        if ticket.state == TicketState::Blocked
            && view.graph.dependencies_satisfied(&ticket.id, &closed)
        {
            actions.push(SchedulerAction::MarkReady(ticket.id.clone()));
        } else if ticket.state == TicketState::Ready
            && !view.graph.dependencies_satisfied(&ticket.id, &closed)
        {
            actions.push(SchedulerAction::MarkBlocked(ticket.id.clone()));
        }
    }

    // Phase 3: retry/escalation.
    for ticket in view.tickets.values() {
        if ticket.state != TicketState::Recovery {
            continue;
        }
        let failure = ticket
            .failures
            .last()
            .map(|f| f.class)
            .unwrap_or(FailureClass::Other);
        let decision = decide_retry(ticket, failure, now);
        match decision.outcome {
            RetryOutcome::Retry { after } => actions.push(SchedulerAction::Retry {
                ticket: ticket.id.clone(),
                after,
            }),
            RetryOutcome::Escalate(reason) => {
                actions.push(SchedulerAction::Escalate {
                    ticket: ticket.id.clone(),
                    reason,
                });
                if let Some(recovery_ticket) = cycle_recovery_ticket(view, &ticket.id) {
                    actions.push(SchedulerAction::OpenRecovery(recovery_ticket));
                }
            }
        }
    }

    // Phase 4: admission + selection.
    let mut gate = AdmissionGate::from_view(view, 0);
    let mut ranked = rank_ready(view, policy);
    for expired in &expired_tickets {
        if !ranked.contains(expired) {
            ranked.push(expired.clone());
        }
    }
    ranked.sort_by(|a, b| {
        let ta = view.tickets.get(a);
        let tb = view.tickets.get(b);
        match (ta, tb) {
            (Some(ta), Some(tb)) => {
                crate::select::compare_tickets(ta, tb, &view.graph, &policy.milestone_deadlines)
            }
            _ => a.cmp(b),
        }
    });
    loop {
        match select_next(&ranked, view, &availability) {
            Ok(matched) => {
                let Some(ticket) = view.tickets.get(&matched.ticket) else {
                    // invariant: `select_next` only returns ids present in `ranked`, which are
                    // all drawn from `view.tickets` or `expired_tickets` (also all ids in
                    // `view.tickets`).
                    break;
                };
                match gate.check(ticket, policy, &availability) {
                    crate::admission::AdmissionDecision::Admit => {
                        actions.push(SchedulerAction::Lease {
                            ticket: ticket.id.clone(),
                            executor: matched.role,
                            ttl_seconds: policy.default_lease_ttl_seconds,
                        });
                        gate.record_admission(ticket);
                    }
                    crate::admission::AdmissionDecision::Refuse(_) => {}
                }
                ranked.retain(|t| t != &matched.ticket);
            }
            Err(SelectionError::NoReadyTickets) => break,
            Err(SelectionError::NoExecutorAvailable(role)) => {
                // Nothing left in `ranked` can be matched against `role`'s current
                // availability; every remaining candidate needing that role is stuck for this
                // tick, and any candidate with a different role was already tried by
                // `select_next`'s ranked walk. No candidate is admissible this tick.
                let _ = role;
                break;
            }
        }
    }

    // Phase 5: milestones.
    let mut by_milestone: BTreeMap<MilestoneId, Vec<&Ticket>> = BTreeMap::new();
    for ticket in view.tickets.values() {
        if let Some(milestone) = &ticket.milestone {
            by_milestone
                .entry(milestone.clone())
                .or_default()
                .push(ticket);
        }
    }
    for (milestone, tickets) in &by_milestone {
        let all_done = tickets
            .iter()
            .all(|t| matches!(t.state, TicketState::Closed | TicketState::Cancelled));
        if all_done {
            actions.push(SchedulerAction::CloseMilestone(milestone.clone()));
        }
    }

    actions
}

/// If `ticket` is a member of an active `Loop` cycle (has at least one `Loop`-kind edge
/// touching it) and that cycle has a designated recovery ticket — the lowest-id ticket, among
/// the tickets directly joined to `ticket` by a `Loop` edge, whose `kind` is
/// [`TicketKind::Recovery`] — return that recovery ticket's id. Deterministic: candidates are
/// collected into a `BTreeSet` before taking the minimum, so iteration order of `view.graph`'s
/// edges never affects the result.
fn cycle_recovery_ticket(view: &SchedulerView, ticket: &TicketId) -> Option<TicketId> {
    let mut candidates: BTreeSet<TicketId> = BTreeSet::new();
    for edge in view.graph.edges() {
        if edge.kind != DependencyKind::Loop {
            continue;
        }
        let other = if &edge.from == ticket {
            Some(&edge.to)
        } else if &edge.to == ticket {
            Some(&edge.from)
        } else {
            None
        };
        if let Some(other) = other {
            if let Some(t) = view.tickets.get(other) {
                if t.kind == TicketKind::Recovery {
                    candidates.insert(other.clone());
                }
            }
        }
    }
    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use tm_core::graph::{DependencyEdge, DependencyGraph};
    use tm_core::ticket::{
        ContextRef, ExecutorRequirements, FailureRecord, ResourceClaim, RetryPolicy,
        VerificationPolicy,
    };
    use tm_types::{Authority, Budget, Role, Tolerance};

    fn tid(s: &str) -> TicketId {
        TicketId::new(s).unwrap()
    }

    fn mid(s: &str) -> MilestoneId {
        MilestoneId::new(s).unwrap()
    }

    fn base_ticket(id: &str, state: TicketState) -> Ticket {
        Ticket {
            id: tid(id),
            kind: TicketKind::Work,
            objective: "do the thing".to_string(),
            state,
            parent: None,
            children: Vec::new(),
            dependencies: Vec::new(),
            milestone: None,
            due: None,
            authority: Authority::none(),
            resources: Vec::<ResourceClaim>::new(),
            executor: ExecutorRequirements {
                role: Role::CoderFast,
                human_required: false,
                min_capability: Tolerance::Any,
            },
            context_refs: Vec::<ContextRef>::new(),
            success: Vec::new(),
            verification: VerificationPolicy::None,
            // Budget::none() would make every fixture ticket immediately budget-exhausted per
            // decide_retry's tokens/dollars/wall_seconds checks; tests that want to exercise
            // budget exhaustion override this field explicitly.
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 1,
                backoff_multiplier: 2.0,
                max_delay_seconds: 60,
            },
            cycle: None,
            attempts: 0,
            failures: Vec::new(),
            priority: 0,
            created: Timestamp::EPOCH,
            updated: Timestamp::EPOCH,
        }
    }

    fn view_of(tickets: Vec<Ticket>, graph: DependencyGraph) -> SchedulerView {
        SchedulerView {
            tickets: tickets.into_iter().map(|t| (t.id.clone(), t)).collect(),
            graph,
            live_leases: BTreeMap::new(),
        }
    }

    fn no_roles_policy() -> SchedulingPolicy {
        SchedulingPolicy::conservative_default()
    }

    fn all_roles_policy() -> SchedulingPolicy {
        let mut policy = SchedulingPolicy::conservative_default();
        policy.available_roles = Role::ALL.iter().copied().collect();
        policy
    }

    #[test]
    fn identical_inputs_produce_identical_output() {
        let build = || {
            let graph = DependencyGraph::build(vec![tid("T-1")], vec![], vec![]);
            let view = view_of(vec![base_ticket("T-1", TicketState::Ready)], graph);
            (view, Timestamp::EPOCH, all_roles_policy())
        };
        let (view_a, now_a, policy_a) = build();
        let (view_b, now_b, policy_b) = build();
        assert_eq!(
            plan(&view_a, now_a, &policy_a),
            plan(&view_b, now_b, &policy_b)
        );
    }

    #[test]
    fn shuffled_ticket_insertion_order_does_not_change_plan_output() {
        let graph =
            DependencyGraph::build(vec![tid("T-1"), tid("T-2"), tid("T-3")], vec![], vec![]);
        let forward = vec![
            base_ticket("T-1", TicketState::Ready),
            base_ticket("T-2", TicketState::Ready),
            base_ticket("T-3", TicketState::Ready),
        ];
        let mut shuffled = forward.clone();
        shuffled.reverse();

        let view_forward = view_of(forward, graph.clone());
        let view_shuffled = view_of(shuffled, graph);
        let policy = all_roles_policy();

        assert_eq!(
            plan(&view_forward, Timestamp::EPOCH, &policy),
            plan(&view_shuffled, Timestamp::EPOCH, &policy)
        );
    }

    // SPEC.md §16 item 4: scheduler determinism as a property, generalizing the two fixed-example
    // tests above (`identical_inputs_produce_identical_output`,
    // `shuffled_ticket_insertion_order_does_not_change_plan_output`) across randomized ticket
    // counts, ids, roles, priorities and insertion orders rather than one hand-picked case each.
    mod determinism_props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            /// `plan()` is pure (same view twice yields the same actions) and its output does not
            /// depend on the order `SchedulerView.tickets` was built from, only on ticket identity
            /// and content — the guarantee `plan.rs`'s module docs promise by construction via
            /// `BTreeMap`/sorted-`Vec` internals, checked here against inputs a hand-written
            /// example wouldn't think to try.
            #[test]
            fn plan_is_pure_and_order_independent(
                specs in proptest::collection::vec(
                    (1u32..1000, 0usize..Role::ALL.len(), 0i32..10, 0u32..1000),
                    1..8,
                ),
            ) {
                let mut seen = BTreeSet::new();
                let mut tickets: Vec<(Ticket, u32)> = Vec::new();
                for (id_n, role_idx, priority, shuffle_key) in &specs {
                    let id = format!("T-{id_n}");
                    if !seen.insert(id.clone()) {
                        continue;
                    }
                    let mut t = base_ticket(&id, TicketState::Ready);
                    t.executor.role = Role::ALL[*role_idx];
                    t.priority = *priority;
                    tickets.push((t, *shuffle_key));
                }
                prop_assume!(!tickets.is_empty());

                let forward: Vec<Ticket> = tickets.iter().map(|(t, _)| t.clone()).collect();
                let mut shuffled = tickets.clone();
                shuffled.sort_by_key(|(_, key)| *key);
                let shuffled: Vec<Ticket> = shuffled.into_iter().map(|(t, _)| t).collect();

                let tids: Vec<TicketId> = forward.iter().map(|t| t.id.clone()).collect();
                let graph = DependencyGraph::build(tids, vec![], vec![]);

                let mut policy = SchedulingPolicy::conservative_default();
                policy.available_roles = Role::ALL.iter().copied().collect();

                let view_forward = view_of(forward, graph.clone());
                let view_shuffled = view_of(shuffled, graph);

                let a1 = plan(&view_forward, Timestamp::EPOCH, &policy);
                let a2 = plan(&view_forward, Timestamp::EPOCH, &policy);
                prop_assert_eq!(
                    &a1, &a2,
                    "plan() must be pure: calling it twice on the same view produced different output"
                );

                let b = plan(&view_shuffled, Timestamp::EPOCH, &policy);
                prop_assert_eq!(
                    a1, b,
                    "plan() output must not depend on ticket insertion order"
                );
            }
        }
    }

    #[test]
    fn blocked_ticket_with_closed_dependency_becomes_ready() {
        let graph = DependencyGraph::build(
            vec![tid("T-1"), tid("T-2")],
            vec![DependencyEdge {
                from: tid("T-2"),
                to: tid("T-1"),
                kind: DependencyKind::Hard,
            }],
            vec![],
        );
        let view = view_of(
            vec![
                base_ticket("T-1", TicketState::Closed),
                base_ticket("T-2", TicketState::Blocked),
            ],
            graph,
        );
        let actions = plan(&view, Timestamp::EPOCH, &no_roles_policy());
        assert!(actions.contains(&SchedulerAction::MarkReady(tid("T-2"))));
    }

    #[test]
    fn blocked_ticket_with_open_dependency_stays_blocked() {
        let graph = DependencyGraph::build(
            vec![tid("T-1"), tid("T-2")],
            vec![DependencyEdge {
                from: tid("T-2"),
                to: tid("T-1"),
                kind: DependencyKind::Hard,
            }],
            vec![],
        );
        let view = view_of(
            vec![
                base_ticket("T-1", TicketState::Ready),
                base_ticket("T-2", TicketState::Blocked),
            ],
            graph,
        );
        let actions = plan(&view, Timestamp::EPOCH, &no_roles_policy());
        assert!(!actions.contains(&SchedulerAction::MarkReady(tid("T-2"))));
    }

    #[test]
    fn ready_ticket_with_reopened_dependency_becomes_blocked() {
        let graph = DependencyGraph::build(
            vec![tid("T-1"), tid("T-2")],
            vec![DependencyEdge {
                from: tid("T-2"),
                to: tid("T-1"),
                kind: DependencyKind::Hard,
            }],
            vec![],
        );
        let view = view_of(
            vec![
                base_ticket("T-1", TicketState::Blocked),
                base_ticket("T-2", TicketState::Ready),
            ],
            graph,
        );
        let actions = plan(&view, Timestamp::EPOCH, &no_roles_policy());
        assert!(actions.contains(&SchedulerAction::MarkBlocked(tid("T-2"))));
    }

    #[test]
    fn expired_lease_yields_expire_lease_action() {
        let graph = DependencyGraph::build(vec![tid("T-1")], vec![], vec![]);
        let mut view = view_of(vec![base_ticket("T-1", TicketState::Leased)], graph);
        let lease = Lease {
            id: LeaseId::new("L-abc123abc123").unwrap(),
            ticket: tid("T-1"),
            holder: tm_types::ParticipantId::system(),
            authority: Authority::none(),
            resources: Vec::new(),
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 10,
            epoch: 1,
        };
        view.live_leases.insert(lease.id.clone(), lease.clone());
        let now = Timestamp::EPOCH.plus_seconds(100);
        let actions = plan(&view, now, &no_roles_policy());
        assert!(actions.contains(&SchedulerAction::ExpireLease(lease.id)));
    }

    #[test]
    fn live_lease_within_ttl_does_not_expire() {
        let graph = DependencyGraph::build(vec![tid("T-1")], vec![], vec![]);
        let mut view = view_of(vec![base_ticket("T-1", TicketState::Leased)], graph);
        let lease = Lease {
            id: LeaseId::new("L-abc123abc123").unwrap(),
            ticket: tid("T-1"),
            holder: tm_types::ParticipantId::system(),
            authority: Authority::none(),
            resources: Vec::new(),
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 1000,
            epoch: 1,
        };
        view.live_leases.insert(lease.id.clone(), lease.clone());
        let now = Timestamp::EPOCH.plus_seconds(10);
        let actions = plan(&view, now, &no_roles_policy());
        assert!(!actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::ExpireLease(_))));
    }

    #[test]
    fn recovery_ticket_with_retryable_failure_schedules_retry() {
        let graph = DependencyGraph::build(vec![tid("T-1")], vec![], vec![]);
        let mut ticket = base_ticket("T-1", TicketState::Recovery);
        ticket.attempts = 0;
        ticket.failures.push(FailureRecord {
            class: FailureClass::ExecutorCrash,
            detail: "crashed".to_string(),
            at: Timestamp::EPOCH,
            attempt: 1,
        });
        let view = view_of(vec![ticket], graph);
        let actions = plan(&view, Timestamp::EPOCH, &no_roles_policy());
        assert!(actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Retry { ticket, .. } if ticket == &tid("T-1"))));
    }

    #[test]
    fn recovery_ticket_with_exhausted_attempts_escalates() {
        let graph = DependencyGraph::build(vec![tid("T-1")], vec![], vec![]);
        let mut ticket = base_ticket("T-1", TicketState::Recovery);
        ticket.attempts = 3;
        ticket.retry.max_attempts = 3;
        ticket.failures.push(FailureRecord {
            class: FailureClass::ExecutorCrash,
            detail: "crashed".to_string(),
            at: Timestamp::EPOCH,
            attempt: 3,
        });
        let view = view_of(vec![ticket], graph);
        let actions = plan(&view, Timestamp::EPOCH, &no_roles_policy());
        assert!(actions.iter().any(|a| matches!(
            a,
            SchedulerAction::Escalate { ticket, reason: EscalationReason::AttemptsExhausted }
                if ticket == &tid("T-1")
        )));
    }

    #[test]
    fn recovery_ticket_with_no_failure_history_falls_back_to_other() {
        let graph = DependencyGraph::build(vec![tid("T-1")], vec![], vec![]);
        let ticket = base_ticket("T-1", TicketState::Recovery);
        assert!(ticket.failures.is_empty());
        let view = view_of(vec![ticket], graph);
        // Must not panic; FailureClass::Other is retryable, so this should schedule a retry.
        let actions = plan(&view, Timestamp::EPOCH, &no_roles_policy());
        assert!(actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Retry { ticket, .. } if ticket == &tid("T-1"))));
    }

    #[test]
    fn ready_ticket_is_leased_when_capacity_and_provider_available() {
        let graph = DependencyGraph::build(vec![tid("T-1")], vec![], vec![]);
        let view = view_of(vec![base_ticket("T-1", TicketState::Ready)], graph);
        let actions = plan(&view, Timestamp::EPOCH, &all_roles_policy());
        assert!(actions.iter().any(|a| matches!(
            a,
            SchedulerAction::Lease { ticket, .. } if ticket == &tid("T-1")
        )));
    }

    #[test]
    fn ready_ticket_is_not_leased_without_available_provider() {
        let graph = DependencyGraph::build(vec![tid("T-1")], vec![], vec![]);
        let view = view_of(vec![base_ticket("T-1", TicketState::Ready)], graph);
        let actions = plan(&view, Timestamp::EPOCH, &no_roles_policy());
        assert!(!actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Lease { .. })));
    }

    #[test]
    fn parent_ceiling_refuses_a_second_sibling_but_not_unrelated_tickets() {
        let graph = DependencyGraph::build(
            vec![tid("T-1"), tid("T-2"), tid("T-3"), tid("T-9")],
            vec![],
            vec![(tid("T-9"), tid("T-1")), (tid("T-9"), tid("T-2"))],
        );
        let mut policy = all_roles_policy();
        policy.max_in_flight_per_parent = 1;
        let mut leased_sibling = base_ticket("T-1", TicketState::Leased);
        leased_sibling.parent = Some(tid("T-9"));
        let mut ready_sibling = base_ticket("T-2", TicketState::Ready);
        ready_sibling.parent = Some(tid("T-9"));
        let unrelated = base_ticket("T-3", TicketState::Ready);
        let parent = base_ticket("T-9", TicketState::Ready);
        let view = view_of(
            vec![leased_sibling, ready_sibling, unrelated, parent],
            graph,
        );
        let actions = plan(&view, Timestamp::EPOCH, &policy);
        assert!(!actions.iter().any(|a| matches!(
            a,
            SchedulerAction::Lease { ticket, .. } if ticket == &tid("T-2")
        )));
        assert!(actions.iter().any(|a| matches!(
            a,
            SchedulerAction::Lease { ticket, .. } if ticket == &tid("T-3")
        )));
    }

    #[test]
    fn milestone_closes_once_every_member_ticket_is_closed_or_cancelled() {
        let graph = DependencyGraph::build(vec![tid("T-1"), tid("T-2")], vec![], vec![]);
        let mut a = base_ticket("T-1", TicketState::Closed);
        a.milestone = Some(mid("M-1"));
        let mut b = base_ticket("T-2", TicketState::Cancelled);
        b.milestone = Some(mid("M-1"));
        let view = view_of(vec![a, b], graph);
        let actions = plan(&view, Timestamp::EPOCH, &no_roles_policy());
        assert!(actions.contains(&SchedulerAction::CloseMilestone(mid("M-1"))));
    }

    #[test]
    fn milestone_does_not_close_while_a_member_is_still_open() {
        let graph = DependencyGraph::build(vec![tid("T-1"), tid("T-2")], vec![], vec![]);
        let mut a = base_ticket("T-1", TicketState::Closed);
        a.milestone = Some(mid("M-1"));
        let mut b = base_ticket("T-2", TicketState::Ready);
        b.milestone = Some(mid("M-1"));
        let view = view_of(vec![a, b], graph);
        let actions = plan(&view, Timestamp::EPOCH, &no_roles_policy());
        assert!(!actions.contains(&SchedulerAction::CloseMilestone(mid("M-1"))));
    }

    #[test]
    fn empty_view_produces_no_actions() {
        let graph = DependencyGraph::build(BTreeSet::<TicketId>::new(), vec![], vec![]);
        let view = view_of(vec![], graph);
        let actions = plan(&view, Timestamp::EPOCH, &no_roles_policy());
        assert!(actions.is_empty());
    }
}
