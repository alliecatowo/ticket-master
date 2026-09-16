//! Whole-project invariant checks: `SPEC.md` §4.3's invariant list, run after every transition
//! and exposed as `tm doctor`.
//!
//! [`check_invariants`] is read-only and total: it never panics, and it returns every violation
//! it finds rather than stopping at the first (so `tm doctor` reports everything wrong with a
//! project in one pass). Each [`Violation`] names the invariant number from the spec so tooling
//! and humans can cross-reference `SPEC.md` directly.

use tm_types::TicketId;

use crate::ticket::{TicketKind, TicketState, VerificationPolicy};
use crate::view::ProjectView;

/// One violated invariant, naming the `SPEC.md` §4.3 rule it breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The invariant number from `SPEC.md` §4.3's bulleted list (1-indexed in document order).
    pub invariant: u32,
    /// The primary ticket (or other object) this violation concerns, if any.
    pub subject: Option<TicketId>,
    /// Human-readable description of what is wrong.
    pub detail: String,
}

/// Run every invariant check in `SPEC.md` §4.3 over `view`, returning every violation found.
///
/// Checks performed (spec order):
/// 1. No `Ready|Leased|Running` ticket has an unsatisfied `Hard` dependency.
/// 2. No ticket holds two live leases at once.
/// 3. No two live leases hold conflicting exclusive resource claims.
/// 4. Child authority is contained by parent authority, for every parent/child edge.
/// 5. No `Closed` ticket lacks verification evidence, unless its `VerificationPolicy` is `None`
///    and its `TicketKind` is `Investigation` or `Harness`.
/// 6. Every audit's auditor differs from the executor that produced the change being audited.
/// 7. The dependency graph's non-cycle region is acyclic (cycles legal only per
///    `graph.rs::check_cycles`).
pub fn check_invariants(view: &ProjectView) -> Vec<Violation> {
    let mut violations = Vec::new();

    // 1. No Ready|Leased|Running ticket has an unsatisfied Hard dependency.
    for (id, ticket) in &view.tickets {
        let live_state = matches!(
            ticket.state,
            TicketState::Ready | TicketState::Leased | TicketState::Running
        );
        if live_state && !view.graph.dependencies_satisfied(id) {
            violations.push(Violation {
                invariant: 1,
                subject: Some(id.clone()),
                detail: format!(
                    "ticket {id} is {:?} but has an unsatisfied Hard dependency",
                    ticket.state
                ),
            });
        }
    }

    // 2. No ticket holds two live leases at once.
    let live_leases = view.live_leases();
    {
        use std::collections::BTreeMap;
        let mut by_ticket: BTreeMap<&TicketId, u32> = BTreeMap::new();
        for lease in &live_leases {
            *by_ticket.entry(&lease.ticket).or_insert(0) += 1;
        }
        for (ticket, count) in by_ticket {
            if count > 1 {
                violations.push(Violation {
                    invariant: 2,
                    subject: Some(ticket.clone()),
                    detail: format!("ticket {ticket} holds {count} live leases at once"),
                });
            }
        }
    }

    // 3. No two live leases hold conflicting exclusive resource claims.
    for i in 0..live_leases.len() {
        for j in (i + 1)..live_leases.len() {
            let a = live_leases[i];
            let b = live_leases[j];
            for claim_a in &a.resources {
                for claim_b in &b.resources {
                    if crate::lease::conflicts_with(claim_a, claim_b) {
                        violations.push(Violation {
                            invariant: 3,
                            subject: Some(a.ticket.clone()),
                            detail: format!(
                                "lease {} (ticket {}) conflicts with lease {} (ticket {}) over an exclusive resource claim",
                                a.id, a.ticket, b.id, b.ticket
                            ),
                        });
                    }
                }
            }
        }
    }

    // 4. Child authority is contained by parent authority, for every parent/child edge.
    for (child_id, child) in &view.tickets {
        if let Some(parent_id) = &child.parent {
            if let Some(parent) = view.tickets.get(parent_id) {
                if !parent.authority.contains(&child.authority) {
                    let failures = parent.authority.containment_failures(&child.authority);
                    violations.push(Violation {
                        invariant: 4,
                        subject: Some(child_id.clone()),
                        detail: format!(
                            "ticket {child_id}'s authority exceeds parent {parent_id}'s: {}",
                            failures.join("; ")
                        ),
                    });
                }
            }
        }
    }

    // 5. No Closed ticket lacks verification evidence, unless policy is None and kind permits.
    for (id, ticket) in &view.tickets {
        if ticket.state != TicketState::Closed {
            continue;
        }
        let exempt = matches!(ticket.verification, VerificationPolicy::None)
            && permits_unverified_close(ticket.kind);
        if exempt {
            continue;
        }
        let has_evidence = view.evidence.iter().any(|e| &e.ticket == id);
        if !has_evidence {
            violations.push(Violation {
                invariant: 5,
                subject: Some(id.clone()),
                detail: format!(
                    "ticket {id} is Closed but has no verification evidence record (policy: {:?}, kind: {:?})",
                    ticket.verification, ticket.kind
                ),
            });
        }
    }

    // 6. Every audit's auditor differs from the executor that produced the change being audited.
    for (audit_id, audit_ticket) in &view.tickets {
        if audit_ticket.kind != TicketKind::Audit {
            continue;
        }
        let Some(audited) = &audit_ticket.parent else {
            continue;
        };
        let executors: Vec<_> = view
            .evidence
            .iter()
            .filter(|e| &e.ticket == audited)
            .map(|e| &e.produced_by)
            .collect();
        let auditors: Vec<_> = view
            .leases
            .iter()
            .filter(|l| &l.ticket == audit_id)
            .map(|l| &l.holder)
            .collect();
        for auditor in &auditors {
            if executors.contains(auditor) {
                violations.push(Violation {
                    invariant: 6,
                    subject: Some(audit_id.clone()),
                    detail: format!(
                        "audit {audit_id} of ticket {audited} was performed by {auditor}, who also produced evidence for it"
                    ),
                });
            }
        }
    }

    // 7. The dependency graph's non-cycle region is acyclic (cycles legal only per
    // graph.rs::check_cycles).
    if let Err(illegal) = view.graph.check_cycles() {
        for cycle in illegal {
            violations.push(Violation {
                invariant: 7,
                subject: cycle.members.first().cloned(),
                detail: format!(
                    "illegal cycle through {:?}: {}",
                    cycle.members, cycle.reason
                ),
            });
        }
    }

    violations
}

/// Whether `kind` is permitted to use `VerificationPolicy::None` when closing (invariant 5).
pub fn permits_unverified_close(kind: TicketKind) -> bool {
    matches!(kind, TicketKind::Investigation | TicketKind::Harness)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::{Evidence, EvidenceKind};
    use crate::graph::DependencyEdge;
    use crate::lease::Lease;
    use crate::ticket::{
        DependencyKind, ExecutorRequirements, ResourceClaim, ResourceMode, RetryPolicy, Ticket,
    };
    use std::str::FromStr;
    use tm_types::{
        Authority, Budget, LeaseId, ParticipantId, PathPattern, Role, Timestamp, Tolerance,
    };

    fn tid(s: &str) -> TicketId {
        TicketId::from_str(s).expect("valid ticket id in test fixture")
    }

    fn base_ticket(id: &str, kind: TicketKind, state: TicketState) -> Ticket {
        Ticket {
            id: tid(id),
            kind,
            objective: "do the thing".to_string(),
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
                min_capability: Tolerance::Any,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 1,
                base_delay_seconds: 0,
                backoff_multiplier: 1.0,
                max_delay_seconds: 0,
                non_retryable: vec![],
            },
            cycle: None,
            attempts: 0,
            failures: vec![],
            priority: 0,
            created: Timestamp::EPOCH,
            updated: Timestamp::EPOCH,
        }
    }

    fn lease_for(ticket: &str, holder: ParticipantId, id: &str) -> Lease {
        Lease {
            id: LeaseId::new(id).unwrap(),
            ticket: tid(ticket),
            holder,
            authority: Authority::none(),
            resources: vec![],
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 300,
            epoch: 0,
        }
    }

    fn claim(path: &str, mode: ResourceMode) -> ResourceClaim {
        ResourceClaim {
            pattern: PathPattern::new(path).unwrap(),
            mode,
        }
    }

    #[test]
    fn empty_view_has_no_violations() {
        let view = ProjectView::default();
        assert!(check_invariants(&view).is_empty());
    }

    #[test]
    fn ready_ticket_with_unsatisfied_hard_dependency_is_flagged() {
        let mut view = ProjectView::default();
        let t1 = base_ticket("T-1", TicketKind::Work, TicketState::Ready);
        view.tickets.insert(tid("T-1"), t1);
        view.graph.edges.push(DependencyEdge {
            from: tid("T-1"),
            to: tid("T-2"),
            kind: DependencyKind::Hard,
        });
        view.graph.states.insert(tid("T-2"), TicketState::Ready);

        let violations = check_invariants(&view);
        assert!(violations
            .iter()
            .any(|v| v.invariant == 1 && v.subject == Some(tid("T-1"))));
    }

    #[test]
    fn ready_ticket_with_satisfied_hard_dependency_is_clean() {
        let mut view = ProjectView::default();
        let t1 = base_ticket("T-1", TicketKind::Work, TicketState::Ready);
        view.tickets.insert(tid("T-1"), t1);
        view.graph.edges.push(DependencyEdge {
            from: tid("T-1"),
            to: tid("T-2"),
            kind: DependencyKind::Hard,
        });
        view.graph.states.insert(tid("T-2"), TicketState::Closed);

        let violations = check_invariants(&view);
        assert!(!violations.iter().any(|v| v.invariant == 1));
    }

    #[test]
    fn blocked_ticket_with_unsatisfied_dependency_is_not_flagged() {
        let mut view = ProjectView::default();
        let t1 = base_ticket("T-1", TicketKind::Work, TicketState::Blocked);
        view.tickets.insert(tid("T-1"), t1);
        view.graph.edges.push(DependencyEdge {
            from: tid("T-1"),
            to: tid("T-2"),
            kind: DependencyKind::Hard,
        });

        let violations = check_invariants(&view);
        assert!(!violations.iter().any(|v| v.invariant == 1));
    }

    #[test]
    fn double_lease_on_same_ticket_is_flagged() {
        let mut view = ProjectView::default();
        view.leases
            .push(lease_for("T-1", ParticipantId::system(), "L-000000000001"));
        view.leases
            .push(lease_for("T-1", ParticipantId::system(), "L-000000000002"));

        let violations = check_invariants(&view);
        assert!(violations
            .iter()
            .any(|v| v.invariant == 2 && v.subject == Some(tid("T-1"))));
    }

    #[test]
    fn single_lease_per_ticket_is_clean() {
        let mut view = ProjectView::default();
        view.leases
            .push(lease_for("T-1", ParticipantId::system(), "L-000000000001"));
        view.leases
            .push(lease_for("T-2", ParticipantId::system(), "L-000000000002"));

        let violations = check_invariants(&view);
        assert!(!violations.iter().any(|v| v.invariant == 2));
    }

    #[test]
    fn conflicting_exclusive_resource_claims_across_leases_are_flagged() {
        let mut view = ProjectView::default();
        let mut lease_a = lease_for("T-1", ParticipantId::system(), "L-000000000001");
        lease_a.resources = vec![claim("src/lib.rs", ResourceMode::Exclusive)];
        let mut lease_b = lease_for("T-2", ParticipantId::system(), "L-000000000002");
        lease_b.resources = vec![claim("src/lib.rs", ResourceMode::Exclusive)];
        view.leases.push(lease_a);
        view.leases.push(lease_b);

        let violations = check_invariants(&view);
        assert!(violations.iter().any(|v| v.invariant == 3));
    }

    #[test]
    fn shared_resource_claims_across_leases_are_clean() {
        let mut view = ProjectView::default();
        let mut lease_a = lease_for("T-1", ParticipantId::system(), "L-000000000001");
        lease_a.resources = vec![claim("src/lib.rs", ResourceMode::Shared)];
        let mut lease_b = lease_for("T-2", ParticipantId::system(), "L-000000000002");
        lease_b.resources = vec![claim("src/lib.rs", ResourceMode::Shared)];
        view.leases.push(lease_a);
        view.leases.push(lease_b);

        let violations = check_invariants(&view);
        assert!(!violations.iter().any(|v| v.invariant == 3));
    }

    #[test]
    fn child_authority_not_contained_by_parent_is_flagged() {
        let mut view = ProjectView::default();
        let mut parent = base_ticket("T-1", TicketKind::Work, TicketState::Draft);
        parent.authority = Authority::none();
        let mut child = base_ticket("T-2", TicketKind::Work, TicketState::Draft);
        child.parent = Some(tid("T-1"));
        child.authority = Authority::root();
        view.tickets.insert(tid("T-1"), parent);
        view.tickets.insert(tid("T-2"), child);

        let violations = check_invariants(&view);
        assert!(violations
            .iter()
            .any(|v| v.invariant == 4 && v.subject == Some(tid("T-2"))));
    }

    #[test]
    fn child_authority_contained_by_parent_is_clean() {
        let mut view = ProjectView::default();
        let parent = base_ticket("T-1", TicketKind::Work, TicketState::Draft);
        let mut child = base_ticket("T-2", TicketKind::Work, TicketState::Draft);
        child.parent = Some(tid("T-1"));
        view.tickets.insert(tid("T-1"), parent);
        view.tickets.insert(tid("T-2"), child);

        let violations = check_invariants(&view);
        assert!(!violations.iter().any(|v| v.invariant == 4));
    }

    #[test]
    fn closed_ticket_without_evidence_is_flagged() {
        let mut view = ProjectView::default();
        let t1 = base_ticket("T-1", TicketKind::Work, TicketState::Closed);
        view.tickets.insert(tid("T-1"), t1);

        let violations = check_invariants(&view);
        assert!(violations
            .iter()
            .any(|v| v.invariant == 5 && v.subject == Some(tid("T-1"))));
    }

    #[test]
    fn closed_ticket_with_evidence_is_clean() {
        let mut view = ProjectView::default();
        let t1 = base_ticket("T-1", TicketKind::Work, TicketState::Closed);
        view.tickets.insert(tid("T-1"), t1);
        view.evidence.push(Evidence {
            ticket: tid("T-1"),
            kind: EvidenceKind::TestRun,
            artifact: tm_types::ArtifactId::new("ART-000000000001").unwrap(),
            produced_by: ParticipantId::system(),
            ts: Timestamp::EPOCH,
            summary: "tests passed".to_string(),
        });

        let violations = check_invariants(&view);
        assert!(!violations.iter().any(|v| v.invariant == 5));
    }

    #[test]
    fn closed_investigation_with_none_policy_and_no_evidence_is_exempt() {
        let mut view = ProjectView::default();
        let mut t1 = base_ticket("T-1", TicketKind::Investigation, TicketState::Closed);
        t1.verification = VerificationPolicy::None;
        view.tickets.insert(tid("T-1"), t1);

        let violations = check_invariants(&view);
        assert!(!violations.iter().any(|v| v.invariant == 5));
    }

    #[test]
    fn closed_work_ticket_with_none_policy_is_still_flagged() {
        let mut view = ProjectView::default();
        let mut t1 = base_ticket("T-1", TicketKind::Work, TicketState::Closed);
        t1.verification = VerificationPolicy::None;
        view.tickets.insert(tid("T-1"), t1);

        let violations = check_invariants(&view);
        assert!(violations.iter().any(|v| v.invariant == 5));
    }

    #[test]
    fn auditor_matching_executor_is_flagged() {
        let mut view = ProjectView::default();
        let worker = ParticipantId::new("agent:coder/1").unwrap();
        let mut work = base_ticket("T-1", TicketKind::Work, TicketState::Auditing);
        work.children.push(tid("A-1"));
        let mut audit = base_ticket("A-1", TicketKind::Audit, TicketState::Auditing);
        audit.parent = Some(tid("T-1"));
        view.tickets.insert(tid("T-1"), work);
        view.tickets.insert(tid("A-1"), audit);
        view.evidence.push(Evidence {
            ticket: tid("T-1"),
            kind: EvidenceKind::Diff,
            artifact: tm_types::ArtifactId::new("ART-000000000001").unwrap(),
            produced_by: worker.clone(),
            ts: Timestamp::EPOCH,
            summary: "the patch".to_string(),
        });
        view.leases.push(lease_for("A-1", worker, "L-000000000001"));

        let violations = check_invariants(&view);
        assert!(violations
            .iter()
            .any(|v| v.invariant == 6 && v.subject == Some(tid("A-1"))));
    }

    #[test]
    fn auditor_differing_from_executor_is_clean() {
        let mut view = ProjectView::default();
        let worker = ParticipantId::new("agent:coder/1").unwrap();
        let auditor = ParticipantId::new("agent:reviewer/1").unwrap();
        let mut work = base_ticket("T-1", TicketKind::Work, TicketState::Auditing);
        work.children.push(tid("A-1"));
        let mut audit = base_ticket("A-1", TicketKind::Audit, TicketState::Auditing);
        audit.parent = Some(tid("T-1"));
        view.tickets.insert(tid("T-1"), work);
        view.tickets.insert(tid("A-1"), audit);
        view.evidence.push(Evidence {
            ticket: tid("T-1"),
            kind: EvidenceKind::Diff,
            artifact: tm_types::ArtifactId::new("ART-000000000001").unwrap(),
            produced_by: worker,
            ts: Timestamp::EPOCH,
            summary: "the patch".to_string(),
        });
        view.leases
            .push(lease_for("A-1", auditor, "L-000000000001"));

        let violations = check_invariants(&view);
        assert!(!violations.iter().any(|v| v.invariant == 6));
    }

    #[test]
    fn illegal_hard_cycle_is_flagged() {
        let mut view = ProjectView::default();
        view.graph.edges.push(DependencyEdge {
            from: tid("T-1"),
            to: tid("T-2"),
            kind: DependencyKind::Hard,
        });
        view.graph.edges.push(DependencyEdge {
            from: tid("T-2"),
            to: tid("T-1"),
            kind: DependencyKind::Hard,
        });

        let violations = check_invariants(&view);
        assert!(violations.iter().any(|v| v.invariant == 7));
    }

    #[test]
    fn legal_loop_cycle_with_budget_is_clean() {
        let mut view = ProjectView::default();
        view.graph.edges.push(DependencyEdge {
            from: tid("T-1"),
            to: tid("T-2"),
            kind: DependencyKind::Loop,
        });
        view.graph.edges.push(DependencyEdge {
            from: tid("T-2"),
            to: tid("T-1"),
            kind: DependencyKind::Loop,
        });
        view.graph.cycle_budgeted.insert(tid("T-1"));

        let violations = check_invariants(&view);
        assert!(!violations.iter().any(|v| v.invariant == 7));
    }

    #[test]
    fn permits_unverified_close_allows_investigation_and_harness_only() {
        assert!(permits_unverified_close(TicketKind::Investigation));
        assert!(permits_unverified_close(TicketKind::Harness));
        assert!(!permits_unverified_close(TicketKind::Work));
        assert!(!permits_unverified_close(TicketKind::Verification));
        assert!(!permits_unverified_close(TicketKind::Audit));
        assert!(!permits_unverified_close(TicketKind::Recovery));
    }

    #[test]
    fn check_invariants_accumulates_multiple_violations_without_short_circuiting() {
        let mut view = ProjectView::default();
        view.leases
            .push(lease_for("T-1", ParticipantId::system(), "L-000000000001"));
        view.leases
            .push(lease_for("T-1", ParticipantId::system(), "L-000000000002"));
        let t1 = base_ticket("T-1", TicketKind::Work, TicketState::Closed);
        view.tickets.insert(tid("T-1"), t1);

        let violations = check_invariants(&view);
        assert!(violations.iter().any(|v| v.invariant == 2));
        assert!(violations.iter().any(|v| v.invariant == 5));
    }
}
