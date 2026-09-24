//! `check_invariants`: every structural rule in `SPEC.md` §4.3, run after any transition.
//!
//! Pure function over [`crate::view::ProjectView`]; called by `Store` after every command
//! commits (and exposed to `tm-cli` as `tm doctor`) to catch a logic bug before it corrupts
//! state further, not to gate individual commands (those are gated by `machine.rs` and the
//! per-operation preconditions in `store.rs`).

use std::collections::BTreeSet;

use tm_types::TicketId;

use crate::ticket::{TicketKind, TicketState, VerificationPolicy};
use crate::view::ProjectView;

/// One invariant violation, naming the `SPEC.md` §4.3 rule it breaks so `tm doctor` output is
/// directly actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The invariant number/name from `SPEC.md` §4.3, e.g. `"4.3-no-ready-with-unsatisfied-dep"`.
    pub invariant: &'static str,
    /// The primary ticket (or other object) this violation concerns.
    pub subject: TicketId,
    /// Human-readable detail.
    pub detail: String,
}

/// Invariant name constants, so callers can match on `Violation::invariant` without
/// string-literal drift between this module and its tests.
pub mod names {
    /// No `Ready`/`Leased`/`Running` ticket may have an unsatisfied dependency.
    pub const READY_DEPENDENCIES_SATISFIED: &str = "4.3-ready-dependencies-satisfied";
    /// No ticket holds two live leases.
    pub const NO_DOUBLE_LEASE: &str = "4.3-no-double-lease";
    /// No two live leases hold conflicting exclusive resource claims.
    pub const NO_CONFLICTING_CLAIMS: &str = "4.3-no-conflicting-claims";
    /// Child authority must be contained by parent authority.
    pub const CHILD_AUTHORITY_CONTAINED: &str = "4.3-child-authority-contained";
    /// A `Closed` ticket must have verification evidence unless policy is `None` and its kind
    /// permits it.
    pub const CLOSED_HAS_EVIDENCE: &str = "4.3-closed-has-evidence";
    /// The auditor must differ from the executor.
    pub const AUDITOR_NOT_EXECUTOR: &str = "4.3-auditor-not-executor";
    /// The dependency graph's non-cycle region must be acyclic; cycles are legal only through
    /// `Loop` edges carrying a `CycleBudget`.
    pub const ACYCLIC_EXCEPT_LOOP: &str = "4.3-acyclic-except-loop";
}

/// Run every invariant in `SPEC.md` §4.3 against `view`, returning every violation found (empty
/// when the project state is fully consistent).
pub fn check_invariants(view: &ProjectView) -> Vec<Violation> {
    let mut out = Vec::new();
    out.extend(check_ready_dependencies_satisfied(view));
    out.extend(check_no_double_lease(view));
    out.extend(check_no_conflicting_claims(view));
    out.extend(check_child_authority_contained(view));
    out.extend(check_closed_has_evidence(view));
    out.extend(check_auditor_not_executor(view));
    out.extend(check_acyclic_except_loop(view));
    out
}

/// No ticket in [`crate::ticket::TicketState::Ready`], `Leased` or `Running` may have an
/// unsatisfied [`crate::ticket::DependencyKind::Hard`] dependency.
pub fn check_ready_dependencies_satisfied(view: &ProjectView) -> Vec<Violation> {
    let closed: BTreeSet<TicketId> = view
        .tickets
        .values()
        .filter(|t| t.state == TicketState::Closed)
        .map(|t| t.id.clone())
        .collect();

    view.tickets
        .values()
        .filter(|t| {
            matches!(
                t.state,
                TicketState::Ready | TicketState::Leased | TicketState::Running
            )
        })
        .filter(|t| !view.graph.dependencies_satisfied(&t.id, &closed))
        .map(|t| Violation {
            invariant: names::READY_DEPENDENCIES_SATISFIED,
            subject: t.id.clone(),
            detail: format!(
                "ticket {} is {:?} but has an unsatisfied Hard dependency",
                t.id, t.state
            ),
        })
        .collect()
}

/// No ticket may have two live leases at once.
pub fn check_no_double_lease(view: &ProjectView) -> Vec<Violation> {
    let mut by_ticket: std::collections::BTreeMap<TicketId, u32> =
        std::collections::BTreeMap::new();
    for lease in view.leases.values() {
        *by_ticket.entry(lease.ticket.clone()).or_insert(0) += 1;
    }
    by_ticket
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(ticket, count)| Violation {
            invariant: names::NO_DOUBLE_LEASE,
            subject: ticket.clone(),
            detail: format!("ticket {ticket} holds {count} live leases"),
        })
        .collect()
}

/// No two live leases may hold conflicting exclusive resource claims.
pub fn check_no_conflicting_claims(view: &ProjectView) -> Vec<Violation> {
    let leases: Vec<_> = view.leases.values().collect();
    let mut out = Vec::new();
    for i in 0..leases.len() {
        for j in (i + 1)..leases.len() {
            let a = leases[i];
            let b = leases[j];
            let conflicts = a.resources.iter().any(|ra| {
                b.resources
                    .iter()
                    .any(|rb| crate::lease::claims_conflict(ra, rb))
            });
            if conflicts {
                // Deterministic subject: the lexicographically smaller ticket id of the pair.
                let subject = if a.ticket <= b.ticket {
                    a.ticket.clone()
                } else {
                    b.ticket.clone()
                };
                out.push(Violation {
                    invariant: names::NO_CONFLICTING_CLAIMS,
                    subject,
                    detail: format!(
                        "lease {} on {} conflicts with lease {} on {}",
                        a.id, a.ticket, b.id, b.ticket
                    ),
                });
            }
        }
    }
    out
}

/// For every parent/child edge, the child's authority must be contained by the parent's.
pub fn check_child_authority_contained(view: &ProjectView) -> Vec<Violation> {
    let mut out = Vec::new();
    for child in view.tickets.values() {
        let Some(parent_id) = &child.parent else {
            continue;
        };
        let Some(parent) = view.tickets.get(parent_id) else {
            continue;
        };
        if !parent.authority.contains(&child.authority) {
            out.push(Violation {
                invariant: names::CHILD_AUTHORITY_CONTAINED,
                subject: child.id.clone(),
                detail: format!(
                    "ticket {}'s authority is not contained by parent {}'s authority",
                    child.id, parent.id
                ),
            });
        }
    }
    out
}

/// Every `Closed` ticket must have at least one verification-kind evidence record, unless its
/// `VerificationPolicy` is `None` and its `TicketKind` permits that
/// ([`crate::ticket::TicketKind::permits_unverified_close`]).
pub fn check_closed_has_evidence(view: &ProjectView) -> Vec<Violation> {
    view.tickets
        .values()
        .filter(|t| t.state == TicketState::Closed)
        .filter(|t| {
            let unverified_ok =
                t.verification == VerificationPolicy::None && t.kind.permits_unverified_close();
            !unverified_ok
        })
        .filter(|t| !view.evidence.iter().any(|e| e.ticket == t.id))
        .map(|t| Violation {
            invariant: names::CLOSED_HAS_EVIDENCE,
            subject: t.id.clone(),
            detail: format!(
                "ticket {} is Closed with verification policy {:?} but has no evidence",
                t.id, t.verification
            ),
        })
        .collect()
}

/// The auditor (participant who audited a submission) must differ from the executor (participant
/// who held the lease that produced the submission).
///
/// `ProjectView` carries no direct "audit record" type; the link between a `Work` ticket and the
/// `Audit`-kind ticket that judged it is the parent/child tree (`SPEC.md` §4.2: `A-*`/`V-*`
/// tickets are created as children of the ticket they check). For every ticket that has reached
/// or passed the `Auditing` state, this compares the set of participants who ever held a lease
/// on it (the executors) against the set of participants who ever held a lease on each `Audit`
/// child (the auditors); any overlap is a violation, per `SPEC.md` §4.3/§4.7 ("a worker never
/// certifies itself").
pub fn check_auditor_not_executor(view: &ProjectView) -> Vec<Violation> {
    let mut out = Vec::new();
    for ticket in view.tickets.values() {
        let has_been_audited = matches!(
            ticket.state,
            TicketState::Auditing | TicketState::Rework | TicketState::Replan | TicketState::Closed
        );
        if !has_been_audited {
            continue;
        }
        let executors: BTreeSet<_> = view
            .leases
            .values()
            .filter(|l| l.ticket == ticket.id)
            .map(|l| l.holder.clone())
            .collect();
        if executors.is_empty() {
            continue;
        }
        for child_id in view.graph.children_of(&ticket.id) {
            let Some(child) = view.tickets.get(child_id) else {
                continue;
            };
            if child.kind != TicketKind::Audit {
                continue;
            }
            let auditors: BTreeSet<_> = view
                .leases
                .values()
                .filter(|l| &l.ticket == child_id)
                .map(|l| l.holder.clone())
                .collect();
            if executors.intersection(&auditors).next().is_some() {
                out.push(Violation {
                    invariant: names::AUDITOR_NOT_EXECUTOR,
                    subject: ticket.id.clone(),
                    detail: format!(
                        "audit ticket {} shares a participant with the executor of {}",
                        child.id, ticket.id
                    ),
                });
            }
        }
    }
    out
}

/// The dependency graph's non-`Loop` region must be acyclic.
pub fn check_acyclic_except_loop(view: &ProjectView) -> Vec<Violation> {
    let violations = view.graph.find_illegal_cycles(|id| {
        view.tickets
            .get(id)
            .map(|t| t.cycle.is_some())
            .unwrap_or(false)
    });
    violations
        .into_iter()
        .filter_map(|v| {
            v.members.first().cloned().map(|subject| Violation {
                invariant: names::ACYCLIC_EXCEPT_LOOP,
                subject,
                detail: format!("illegal cycle through {:?}: {}", v.members, v.reason),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use tm_types::{Authority, LeaseId, ParticipantId, PatternSet, Timestamp};

    use crate::artifact::{Evidence, EvidenceKind};
    use crate::graph::{DependencyEdge, DependencyGraph};
    use crate::lease::Lease;
    use crate::ticket::{
        CycleBudget, DependencyKind, ExecutorRequirements, ResourceClaim, ResourceMode,
        RetryPolicy, Ticket,
    };
    use tm_types::{ArtifactId, Budget};

    fn tid(s: &str) -> TicketId {
        TicketId::new(s).unwrap()
    }

    fn lid(s: &str) -> LeaseId {
        LeaseId::new(format!("L-{s}")).unwrap()
    }

    fn participant(s: &str) -> ParticipantId {
        ParticipantId::new(format!("agent:planner/{s}")).unwrap()
    }

    fn base_ticket(id: &str, kind: TicketKind, state: TicketState) -> Ticket {
        Ticket {
            id: tid(id),
            kind,
            objective: "do it".to_string(),
            state,
            parent: None,
            children: vec![],
            dependencies: vec![],
            milestone: None,
            due: None,
            authority: Authority::none(),
            resources: vec![],
            executor: ExecutorRequirements {
                role: tm_types::Role::CoderFast,
                human_required: false,
                min_capability: tm_types::Tolerance::Any,
            },
            context_refs: vec![],
            success: vec![],
            verification: VerificationPolicy::Single,
            budget: Budget::unlimited(),
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay_seconds: 1,
                backoff_multiplier: 2.0,
                max_delay_seconds: 60,
            },
            cycle: None,
            attempts: 0,
            failures: vec![],
            priority: 0,
            created: Timestamp::EPOCH,
            updated: Timestamp::EPOCH,
        }
    }

    fn base_lease(ticket: &str, id: &str, holder: ParticipantId) -> Lease {
        Lease {
            id: lid(id),
            ticket: tid(ticket),
            holder,
            authority: Authority::none(),
            resources: vec![],
            acquired: Timestamp::EPOCH,
            heartbeat: Timestamp::EPOCH,
            ttl_seconds: 60,
            epoch: 0,
        }
    }

    fn view_with(tickets: Vec<Ticket>) -> ProjectView {
        let mut view = ProjectView::empty();
        let ids: Vec<TicketId> = tickets.iter().map(|t| t.id.clone()).collect();
        let edges: Vec<DependencyEdge> = tickets
            .iter()
            .flat_map(|t| {
                t.dependencies.iter().map(move |dep| DependencyEdge {
                    from: t.id.clone(),
                    to: dep.clone(),
                    kind: DependencyKind::Hard,
                })
            })
            .collect();
        let children: Vec<(TicketId, TicketId)> = tickets
            .iter()
            .filter_map(|t| t.parent.clone().map(|p| (p, t.id.clone())))
            .collect();
        view.graph = DependencyGraph::build(ids, edges, children);
        for t in tickets {
            view.tickets.insert(t.id.clone(), t);
        }
        view
    }

    #[test]
    fn ready_ticket_with_unsatisfied_hard_dependency_is_flagged() {
        let mut dependent = base_ticket("T-1", TicketKind::Work, TicketState::Ready);
        dependent.dependencies.push(tid("T-2"));
        let dependency = base_ticket("T-2", TicketKind::Work, TicketState::Ready);
        let view = view_with(vec![dependent, dependency]);
        let violations = check_ready_dependencies_satisfied(&view);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].invariant, names::READY_DEPENDENCIES_SATISFIED);
        assert_eq!(violations[0].subject, tid("T-1"));
    }

    #[test]
    fn ready_ticket_with_closed_dependency_passes() {
        let mut dependent = base_ticket("T-1", TicketKind::Work, TicketState::Ready);
        dependent.dependencies.push(tid("T-2"));
        let dependency = base_ticket("T-2", TicketKind::Work, TicketState::Closed);
        let view = view_with(vec![dependent, dependency]);
        assert!(check_ready_dependencies_satisfied(&view).is_empty());
    }

    #[test]
    fn draft_ticket_with_unsatisfied_dependency_is_not_flagged() {
        let mut dependent = base_ticket("T-1", TicketKind::Work, TicketState::Draft);
        dependent.dependencies.push(tid("T-2"));
        let dependency = base_ticket("T-2", TicketKind::Work, TicketState::Ready);
        let view = view_with(vec![dependent, dependency]);
        assert!(check_ready_dependencies_satisfied(&view).is_empty());
    }

    #[test]
    fn double_lease_on_same_ticket_is_flagged() {
        let mut view = ProjectView::empty();
        let a = base_lease("T-1", "aaaaaaaaaaaa", participant("alice"));
        let b = base_lease("T-1", "bbbbbbbbbbbb", participant("bob"));
        view.leases.insert(a.id.clone(), a);
        view.leases.insert(b.id.clone(), b);
        let violations = check_no_double_lease(&view);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].invariant, names::NO_DOUBLE_LEASE);
        assert_eq!(violations[0].subject, tid("T-1"));
    }

    #[test]
    fn single_lease_per_ticket_passes() {
        let mut view = ProjectView::empty();
        let a = base_lease("T-1", "aaaaaaaaaaaa", participant("alice"));
        let b = base_lease("T-2", "bbbbbbbbbbbb", participant("bob"));
        view.leases.insert(a.id.clone(), a);
        view.leases.insert(b.id.clone(), b);
        assert!(check_no_double_lease(&view).is_empty());
    }

    #[test]
    fn overlapping_exclusive_claims_across_leases_are_flagged() {
        let mut view = ProjectView::empty();
        let mut a = base_lease("T-1", "aaaaaaaaaaaa", participant("alice"));
        a.resources = vec![ResourceClaim {
            paths: PatternSet::parse(["src/**"]).unwrap(),
            mode: ResourceMode::Exclusive,
        }];
        let mut b = base_lease("T-2", "bbbbbbbbbbbb", participant("bob"));
        b.resources = vec![ResourceClaim {
            paths: PatternSet::parse(["src/lib.rs"]).unwrap(),
            mode: ResourceMode::Exclusive,
        }];
        view.leases.insert(a.id.clone(), a);
        view.leases.insert(b.id.clone(), b);
        let violations = check_no_conflicting_claims(&view);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].invariant, names::NO_CONFLICTING_CLAIMS);
        assert_eq!(violations[0].subject, tid("T-1"));
    }

    #[test]
    fn disjoint_exclusive_claims_pass() {
        let mut view = ProjectView::empty();
        let mut a = base_lease("T-1", "aaaaaaaaaaaa", participant("alice"));
        a.resources = vec![ResourceClaim {
            paths: PatternSet::parse(["src/**"]).unwrap(),
            mode: ResourceMode::Exclusive,
        }];
        let mut b = base_lease("T-2", "bbbbbbbbbbbb", participant("bob"));
        b.resources = vec![ResourceClaim {
            paths: PatternSet::parse(["docs/**"]).unwrap(),
            mode: ResourceMode::Exclusive,
        }];
        view.leases.insert(a.id.clone(), a);
        view.leases.insert(b.id.clone(), b);
        assert!(check_no_conflicting_claims(&view).is_empty());
    }

    #[test]
    fn child_authority_not_contained_is_flagged() {
        let mut parent = base_ticket("T-1", TicketKind::Work, TicketState::Ready);
        parent.authority = Authority::none();
        let mut child = base_ticket("T-2", TicketKind::Work, TicketState::Ready);
        child.parent = Some(tid("T-1"));
        child.authority = Authority::root();
        let view = view_with(vec![parent, child]);
        let violations = check_child_authority_contained(&view);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].invariant, names::CHILD_AUTHORITY_CONTAINED);
        assert_eq!(violations[0].subject, tid("T-2"));
    }

    #[test]
    fn child_authority_contained_passes() {
        let mut parent = base_ticket("T-1", TicketKind::Work, TicketState::Ready);
        parent.authority = Authority::root();
        let mut child = base_ticket("T-2", TicketKind::Work, TicketState::Ready);
        child.parent = Some(tid("T-1"));
        child.authority = Authority::none();
        let view = view_with(vec![parent, child]);
        assert!(check_child_authority_contained(&view).is_empty());
    }

    #[test]
    fn closed_ticket_without_evidence_is_flagged() {
        let mut t = base_ticket("T-1", TicketKind::Work, TicketState::Closed);
        t.verification = VerificationPolicy::Single;
        let view = view_with(vec![t]);
        let violations = check_closed_has_evidence(&view);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].invariant, names::CLOSED_HAS_EVIDENCE);
    }

    #[test]
    fn closed_ticket_with_evidence_passes() {
        let mut t = base_ticket("T-1", TicketKind::Work, TicketState::Closed);
        t.verification = VerificationPolicy::Single;
        let mut view = view_with(vec![t]);
        view.evidence.push(Evidence {
            ticket: tid("T-1"),
            kind: EvidenceKind::TestRun,
            artifact: ArtifactId::new("ART-000000000001").unwrap(),
            produced_by: participant("alice"),
            ts: Timestamp::EPOCH,
            summary: "tests pass".to_string(),
        });
        assert!(check_closed_has_evidence(&view).is_empty());
    }

    #[test]
    fn closed_investigation_with_none_policy_needs_no_evidence() {
        let mut t = base_ticket("T-1", TicketKind::Investigation, TicketState::Closed);
        t.verification = VerificationPolicy::None;
        let view = view_with(vec![t]);
        assert!(check_closed_has_evidence(&view).is_empty());
    }

    #[test]
    fn closed_work_ticket_with_none_policy_still_needs_evidence() {
        let mut t = base_ticket("T-1", TicketKind::Work, TicketState::Closed);
        t.verification = VerificationPolicy::None;
        let view = view_with(vec![t]);
        // Work does not permit unverified close, so None policy alone isn't sufficient.
        let violations = check_closed_has_evidence(&view);
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn auditor_same_as_executor_is_flagged() {
        let mut parent = base_ticket("T-1", TicketKind::Work, TicketState::Closed);
        parent.children.push(tid("A-1"));
        let mut audit = base_ticket("A-1", TicketKind::Audit, TicketState::Closed);
        audit.parent = Some(tid("T-1"));
        let shared = participant("mallory");
        let mut view = view_with(vec![parent, audit]);
        view.leases.insert(
            lid("aaaaaaaaaaaa"),
            base_lease("T-1", "aaaaaaaaaaaa", shared.clone()),
        );
        view.leases.insert(
            lid("bbbbbbbbbbbb"),
            base_lease("A-1", "bbbbbbbbbbbb", shared),
        );
        let violations = check_auditor_not_executor(&view);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].invariant, names::AUDITOR_NOT_EXECUTOR);
        assert_eq!(violations[0].subject, tid("T-1"));
    }

    #[test]
    fn auditor_different_from_executor_passes() {
        let mut parent = base_ticket("T-1", TicketKind::Work, TicketState::Closed);
        parent.children.push(tid("A-1"));
        let mut audit = base_ticket("A-1", TicketKind::Audit, TicketState::Closed);
        audit.parent = Some(tid("T-1"));
        let mut view = view_with(vec![parent, audit]);
        view.leases.insert(
            lid("aaaaaaaaaaaa"),
            base_lease("T-1", "aaaaaaaaaaaa", participant("alice")),
        );
        view.leases.insert(
            lid("bbbbbbbbbbbb"),
            base_lease("A-1", "bbbbbbbbbbbb", participant("bob")),
        );
        assert!(check_auditor_not_executor(&view).is_empty());
    }

    #[test]
    fn ticket_not_yet_audited_is_ignored() {
        let t = base_ticket("T-1", TicketKind::Work, TicketState::Running);
        let view = view_with(vec![t]);
        assert!(check_auditor_not_executor(&view).is_empty());
    }

    #[test]
    fn hard_cycle_is_reported_as_illegal() {
        let mut a = base_ticket("T-1", TicketKind::Work, TicketState::Draft);
        a.dependencies.push(tid("T-2"));
        let mut b = base_ticket("T-2", TicketKind::Work, TicketState::Draft);
        b.dependencies.push(tid("T-1"));
        let view = view_with(vec![a, b]);
        let violations = check_acyclic_except_loop(&view);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].invariant, names::ACYCLIC_EXCEPT_LOOP);
    }

    #[test]
    fn loop_cycle_with_cycle_budget_passes() {
        let mut view = ProjectView::empty();
        let mut a = base_ticket("T-1", TicketKind::Work, TicketState::Draft);
        a.cycle = Some(CycleBudget {
            max_iterations: 3,
            iterations: 0,
        });
        let mut b = base_ticket("T-2", TicketKind::Work, TicketState::Draft);
        b.cycle = Some(CycleBudget {
            max_iterations: 3,
            iterations: 0,
        });
        let edges = vec![
            DependencyEdge {
                from: tid("T-1"),
                to: tid("T-2"),
                kind: DependencyKind::Loop,
            },
            DependencyEdge {
                from: tid("T-2"),
                to: tid("T-1"),
                kind: DependencyKind::Loop,
            },
        ];
        view.graph = DependencyGraph::build([], edges, []);
        view.tickets.insert(a.id.clone(), a);
        view.tickets.insert(b.id.clone(), b);
        assert!(check_acyclic_except_loop(&view).is_empty());
    }

    #[test]
    fn acyclic_graph_passes_all_invariants() {
        let mut a = base_ticket("T-1", TicketKind::Work, TicketState::Draft);
        a.dependencies.push(tid("T-2"));
        let b = base_ticket("T-2", TicketKind::Work, TicketState::Draft);
        let view = view_with(vec![a, b]);
        assert!(check_invariants(&view).is_empty());
    }

    #[test]
    fn check_invariants_aggregates_multiple_violation_kinds() {
        let mut ready = base_ticket("T-1", TicketKind::Work, TicketState::Ready);
        ready.dependencies.push(tid("T-2"));
        let dep = base_ticket("T-2", TicketKind::Work, TicketState::Ready);
        let mut view = ProjectView::empty();
        let edges = vec![DependencyEdge {
            from: tid("T-1"),
            to: tid("T-2"),
            kind: DependencyKind::Hard,
        }];
        view.graph = DependencyGraph::build([], edges, []);
        view.tickets.insert(ready.id.clone(), ready);
        view.tickets.insert(dep.id.clone(), dep);
        let a = base_lease("T-1", "aaaaaaaaaaaa", participant("alice"));
        let b = base_lease("T-1", "bbbbbbbbbbbb", participant("bob"));
        view.leases.insert(a.id.clone(), a);
        view.leases.insert(b.id.clone(), b);
        let violations = check_invariants(&view);
        assert!(violations
            .iter()
            .any(|v| v.invariant == names::READY_DEPENDENCIES_SATISFIED));
        assert!(violations
            .iter()
            .any(|v| v.invariant == names::NO_DOUBLE_LEASE));
    }
}
