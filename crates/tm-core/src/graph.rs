//! The dependency and parent/child graph over tickets.
//!
//! Pure functions over an in-memory [`GraphView`] snapshot (built from `ticket_deps` /
//! `ticket_children` rows by `view.rs`); nothing here touches SQLite. Owns cycle detection (which
//! must permit `Loop`-edge cycles carrying a `CycleBudget` and reject every other cycle),
//! dependency satisfaction, critical-path length for scheduling priority, and topological
//! ordering of the acyclic region.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use tm_types::TicketId;

use crate::ticket::{DependencyKind, TicketState};

/// One dependency edge: `from` depends on `to`, of the given kind.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DependencyEdge {
    /// The dependent ticket.
    pub from: TicketId,
    /// The ticket depended upon.
    pub to: TicketId,
    /// What kind of dependency this is; governs cycle legality and readiness gating.
    pub kind: DependencyKind,
}

/// An in-memory snapshot of the ticket graph: dependency edges plus parent/child edges plus
/// enough per-ticket state to answer satisfaction queries. Cheap to construct from `view.rs`,
/// cheap to clone for pure functions that want to try a hypothetical mutation without touching
/// the source.
#[derive(Debug, Clone, Default)]
pub struct GraphView {
    /// Every dependency edge in the project.
    pub edges: Vec<DependencyEdge>,
    /// `parent -> children`, mirroring `ticket_children`.
    pub children: BTreeMap<TicketId, Vec<TicketId>>,
    /// `child -> parent`, the reverse of `children`, for ancestor walks.
    pub parents: BTreeMap<TicketId, TicketId>,
    /// Current state of every ticket in the graph, needed for dependency satisfaction.
    pub states: BTreeMap<TicketId, TicketState>,
    /// Tickets that carry a `CycleBudget` (i.e. may legally sit inside a `Loop`-edge cycle).
    pub cycle_budgeted: BTreeSet<TicketId>,
}

/// A cycle was found that is not legal: not every edge in it is `DependencyKind::Loop`, or it is
/// but no ticket in the cycle carries a `CycleBudget`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("illegal cycle through {members:?}: {reason}")]
pub struct IllegalCycle {
    /// The tickets participating in the illegal cycle, in cycle order.
    pub members: Vec<TicketId>,
    /// Why this cycle is illegal.
    pub reason: String,
}

impl GraphView {
    /// Direct dependencies of `ticket` (edges where `from == ticket`).
    pub fn dependencies_of(&self, ticket: &TicketId) -> Vec<&DependencyEdge> {
        self.edges.iter().filter(|e| &e.from == ticket).collect()
    }

    /// Direct dependents of `ticket` (edges where `to == ticket`).
    pub fn dependents_of(&self, ticket: &TicketId) -> Vec<&DependencyEdge> {
        self.edges.iter().filter(|e| &e.to == ticket).collect()
    }

    /// Every ticket reachable by following `children` from `ticket`, transitively, not
    /// including `ticket` itself.
    pub fn descendants(&self, ticket: &TicketId) -> Vec<TicketId> {
        let mut visited: BTreeSet<TicketId> = BTreeSet::new();
        let mut queue: Vec<TicketId> = self.children.get(ticket).cloned().unwrap_or_default();
        let mut out = Vec::new();
        while let Some(next) = queue.pop() {
            if !visited.insert(next.clone()) {
                continue;
            }
            out.push(next.clone());
            if let Some(kids) = self.children.get(&next) {
                queue.extend(kids.iter().cloned());
            }
        }
        out
    }

    /// Every ticket reachable by following `parents` from `ticket`, transitively, not including
    /// `ticket` itself.
    pub fn ancestors(&self, ticket: &TicketId) -> Vec<TicketId> {
        let mut visited: BTreeSet<TicketId> = BTreeSet::new();
        let mut out = Vec::new();
        let mut current = ticket.clone();
        while let Some(parent) = self.parents.get(&current) {
            if !visited.insert(parent.clone()) {
                break;
            }
            out.push(parent.clone());
            current = parent.clone();
        }
        out
    }

    /// True when every `Hard` dependency of `ticket` is `Closed`. `SPEC.md` §4.3 rule 2:
    /// `Cancelled` dependencies do not satisfy; `Soft`/`Loop` edges never gate readiness.
    pub fn dependencies_satisfied(&self, ticket: &TicketId) -> bool {
        self.dependencies_of(ticket)
            .into_iter()
            .filter(|e| e.kind == DependencyKind::Hard)
            .all(|e| matches!(self.states.get(&e.to), Some(TicketState::Closed)))
    }

    /// Find every cycle in the dependency graph (ignoring parent/child edges, which are a
    /// separate tree structure) and classify each as legal or [`IllegalCycle`].
    ///
    /// A cycle is legal iff every edge composing it is `DependencyKind::Loop` and at least one
    /// ticket in the cycle is in `self.cycle_budgeted`. Everything else — any cycle containing a
    /// `Hard` or `Soft` edge, or an all-`Loop` cycle with no `CycleBudget` anywhere in it — is
    /// reported as an `IllegalCycle`.
    pub fn check_cycles(&self) -> Result<(), Vec<IllegalCycle>> {
        let sccs = self.tarjan_sccs();
        let mut illegal = Vec::new();
        for scc in sccs {
            let is_self_loop = scc.len() == 1
                && self
                    .edges
                    .iter()
                    .any(|e| &e.from == &scc[0] && &e.to == &scc[0]);
            if scc.len() <= 1 && !is_self_loop {
                continue;
            }
            let members: BTreeSet<&TicketId> = scc.iter().collect();
            let scc_edges: Vec<&DependencyEdge> = self
                .edges
                .iter()
                .filter(|e| members.contains(&e.from) && members.contains(&e.to))
                .collect();
            let all_loop = scc_edges.iter().all(|e| e.kind == DependencyKind::Loop);
            let has_budget = scc.iter().any(|t| self.cycle_budgeted.contains(t));
            if !all_loop || !has_budget {
                let reason = if !all_loop {
                    "cycle contains a non-Loop edge".to_string()
                } else {
                    "cycle carries no CycleBudget".to_string()
                };
                illegal.push(IllegalCycle {
                    members: scc,
                    reason,
                });
            }
        }
        if illegal.is_empty() {
            Ok(())
        } else {
            Err(illegal)
        }
    }

    /// Tarjan's strongly connected components algorithm over `self.edges`.
    fn tarjan_sccs(&self) -> Vec<Vec<TicketId>> {
        struct State<'a> {
            index: BTreeMap<TicketId, usize>,
            lowlink: BTreeMap<TicketId, usize>,
            on_stack: BTreeSet<TicketId>,
            stack: Vec<TicketId>,
            counter: usize,
            result: Vec<Vec<TicketId>>,
            adj: BTreeMap<&'a TicketId, Vec<&'a TicketId>>,
        }
        let mut adj: BTreeMap<&TicketId, Vec<&TicketId>> = BTreeMap::new();
        for e in &self.edges {
            adj.entry(&e.from).or_default().push(&e.to);
        }
        let mut nodes: BTreeSet<&TicketId> = BTreeSet::new();
        for e in &self.edges {
            nodes.insert(&e.from);
            nodes.insert(&e.to);
        }
        let mut state = State {
            index: BTreeMap::new(),
            lowlink: BTreeMap::new(),
            on_stack: BTreeSet::new(),
            stack: Vec::new(),
            counter: 0,
            result: Vec::new(),
            adj,
        };

        fn strongconnect<'a>(v: &'a TicketId, state: &mut State<'a>) {
            state.index.insert(v.clone(), state.counter);
            state.lowlink.insert(v.clone(), state.counter);
            state.counter += 1;
            state.stack.push(v.clone());
            state.on_stack.insert(v.clone());

            let neighbors = state.adj.get(v).cloned().unwrap_or_default();
            for w in neighbors {
                if !state.index.contains_key(w) {
                    strongconnect(w, state);
                    let w_low = *state.lowlink.get(w).expect("just computed");
                    let v_low = *state.lowlink.get(v).expect("just inserted");
                    state.lowlink.insert(v.clone(), v_low.min(w_low));
                } else if state.on_stack.contains(w) {
                    let w_idx = *state.index.get(w).expect("checked contains_key");
                    let v_low = *state.lowlink.get(v).expect("just inserted");
                    state.lowlink.insert(v.clone(), v_low.min(w_idx));
                }
            }

            if state.lowlink.get(v) == state.index.get(v) {
                let mut component = Vec::new();
                loop {
                    let w = state.stack.pop().expect("v is on stack, so pop succeeds");
                    state.on_stack.remove(&w);
                    let is_v = &w == v;
                    component.push(w);
                    if is_v {
                        break;
                    }
                }
                state.result.push(component);
            }
        }

        for &node in &nodes {
            if !state.index.contains_key(node) {
                strongconnect(node, &mut state);
            }
        }
        state.result
    }

    /// Longest path (by edge count) from any ticket with no incoming dependency edge down to
    /// `ticket`, used as scheduling priority input (deeper-blocking tickets schedule sooner).
    /// Only meaningful over the acyclic (non-`Loop`) region; `Loop` edges are excluded from the
    /// walk so a legal cycle can't produce an infinite path.
    pub fn critical_path_len(&self, ticket: &TicketId) -> u32 {
        let mut memo: BTreeMap<TicketId, u32> = BTreeMap::new();
        let mut in_progress: BTreeSet<TicketId> = BTreeSet::new();
        self.critical_path_len_rec(ticket, &mut memo, &mut in_progress)
    }

    fn critical_path_len_rec(
        &self,
        ticket: &TicketId,
        memo: &mut BTreeMap<TicketId, u32>,
        in_progress: &mut BTreeSet<TicketId>,
    ) -> u32 {
        if let Some(v) = memo.get(ticket) {
            return *v;
        }
        // A non-Loop cycle is illegal data (invariants.rs's job to flag); treat re-entry as a
        // dead end here rather than recursing forever.
        if !in_progress.insert(ticket.clone()) {
            return 0;
        }
        let predecessors: Vec<TicketId> = self
            .edges
            .iter()
            .filter(|e| &e.from == ticket && e.kind != DependencyKind::Loop)
            .map(|e| e.to.clone())
            .collect();
        let best = predecessors
            .iter()
            .map(|p| 1 + self.critical_path_len_rec(p, memo, in_progress))
            .max()
            .unwrap_or(0);
        in_progress.remove(ticket);
        memo.insert(ticket.clone(), best);
        best
    }

    /// A topological ordering of the acyclic region (all edges except `Loop` edges carrying a
    /// `CycleBudget`), or `None` if that region itself contains a cycle (which would indicate a
    /// data corruption `invariants.rs` should have already flagged).
    pub fn topological_order(&self) -> Option<Vec<TicketId>> {
        if self.check_cycles().is_err() {
            return None;
        }

        // Every cycle is legal here; collect the specific Loop edges that compose each legal
        // cycle so they can be excluded from the acyclic region (excluding the whole SCC's
        // edges, rather than just the cycle-internal ones, would be too aggressive).
        let mut excluded: BTreeSet<DependencyEdge> = BTreeSet::new();
        for scc in self.tarjan_sccs() {
            let is_self_loop = scc.len() == 1
                && self
                    .edges
                    .iter()
                    .any(|e| &e.from == &scc[0] && &e.to == &scc[0]);
            if scc.len() <= 1 && !is_self_loop {
                continue;
            }
            let members: BTreeSet<&TicketId> = scc.iter().collect();
            for e in &self.edges {
                if members.contains(&e.from) && members.contains(&e.to) {
                    excluded.insert(e.clone());
                }
            }
        }

        let mut nodes: BTreeSet<TicketId> = BTreeSet::new();
        for e in &self.edges {
            nodes.insert(e.from.clone());
            nodes.insert(e.to.clone());
        }
        for t in self.states.keys() {
            nodes.insert(t.clone());
        }

        let acyclic_edges: Vec<&DependencyEdge> = self
            .edges
            .iter()
            .filter(|e| !excluded.contains(*e))
            .collect();

        let mut in_degree: BTreeMap<TicketId, usize> =
            nodes.iter().map(|n| (n.clone(), 0)).collect();
        let mut adj: BTreeMap<TicketId, Vec<TicketId>> = BTreeMap::new();
        for e in &acyclic_edges {
            *in_degree.entry(e.from.clone()).or_insert(0) += 1;
            adj.entry(e.to.clone()).or_default().push(e.from.clone());
        }

        let mut queue: Vec<TicketId> = in_degree
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(n, _)| n.clone())
            .collect();
        queue.sort();
        let mut order = Vec::with_capacity(nodes.len());
        while let Some(n) = queue.pop() {
            order.push(n.clone());
            if let Some(dependents) = adj.get(&n) {
                let mut newly_free = Vec::new();
                for d in dependents {
                    let deg = in_degree.get_mut(d).expect("node present in in_degree");
                    *deg -= 1;
                    if *deg == 0 {
                        newly_free.push(d.clone());
                    }
                }
                queue.extend(newly_free);
                queue.sort();
            }
        }

        if order.len() == nodes.len() {
            Some(order)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ticket::{DependencyKind, TicketState};
    use std::str::FromStr;

    fn tid(s: &str) -> TicketId {
        TicketId::from_str(s).expect("valid ticket id in test fixture")
    }

    fn edge(from: &str, to: &str, kind: DependencyKind) -> DependencyEdge {
        DependencyEdge {
            from: tid(from),
            to: tid(to),
            kind,
        }
    }

    #[test]
    fn descendants_walks_children_transitively() {
        let mut view = GraphView::default();
        view.children
            .insert(tid("T-1"), vec![tid("T-2"), tid("T-3")]);
        view.children.insert(tid("T-2"), vec![tid("T-4")]);
        let mut result = view.descendants(&tid("T-1"));
        result.sort();
        assert_eq!(result, vec![tid("T-2"), tid("T-3"), tid("T-4")]);
    }

    #[test]
    fn descendants_of_leaf_is_empty() {
        let view = GraphView::default();
        assert!(view.descendants(&tid("T-1")).is_empty());
    }

    #[test]
    fn descendants_tolerates_malformed_child_cycle() {
        let mut view = GraphView::default();
        view.children.insert(tid("T-1"), vec![tid("T-2")]);
        view.children.insert(tid("T-2"), vec![tid("T-1")]);
        let mut result = view.descendants(&tid("T-1"));
        result.sort();
        assert_eq!(result, vec![tid("T-1"), tid("T-2")]);
    }

    #[test]
    fn ancestors_walks_parents_to_root() {
        let mut view = GraphView::default();
        view.parents.insert(tid("T-3"), tid("T-2"));
        view.parents.insert(tid("T-2"), tid("T-1"));
        assert_eq!(view.ancestors(&tid("T-3")), vec![tid("T-2"), tid("T-1")]);
    }

    #[test]
    fn ancestors_of_root_is_empty() {
        let view = GraphView::default();
        assert!(view.ancestors(&tid("T-1")).is_empty());
    }

    #[test]
    fn dependencies_satisfied_true_when_all_hard_deps_closed() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Hard));
        view.states.insert(tid("T-2"), TicketState::Closed);
        assert!(view.dependencies_satisfied(&tid("T-1")));
    }

    #[test]
    fn dependencies_satisfied_false_when_hard_dep_open() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Hard));
        view.states.insert(tid("T-2"), TicketState::Ready);
        assert!(!view.dependencies_satisfied(&tid("T-1")));
    }

    #[test]
    fn dependencies_satisfied_ignores_soft_and_loop_edges() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Soft));
        view.edges.push(edge("T-1", "T-3", DependencyKind::Loop));
        assert!(view.dependencies_satisfied(&tid("T-1")));
    }

    #[test]
    fn dependencies_satisfied_treats_dangling_edge_as_unsatisfied() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Hard));
        assert!(!view.dependencies_satisfied(&tid("T-1")));
    }

    #[test]
    fn check_cycles_ok_for_acyclic_graph() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Hard));
        view.edges.push(edge("T-2", "T-3", DependencyKind::Hard));
        assert!(view.check_cycles().is_ok());
    }

    #[test]
    fn check_cycles_rejects_hard_cycle() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Hard));
        view.edges.push(edge("T-2", "T-1", DependencyKind::Hard));
        let err = view.check_cycles().unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(err[0].reason.contains("non-Loop"));
    }

    #[test]
    fn check_cycles_rejects_loop_cycle_without_budget() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Loop));
        view.edges.push(edge("T-2", "T-1", DependencyKind::Loop));
        let err = view.check_cycles().unwrap_err();
        assert_eq!(err.len(), 1);
        assert!(err[0].reason.contains("CycleBudget"));
    }

    #[test]
    fn check_cycles_accepts_loop_cycle_with_budget() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Loop));
        view.edges.push(edge("T-2", "T-1", DependencyKind::Loop));
        view.cycle_budgeted.insert(tid("T-1"));
        assert!(view.check_cycles().is_ok());
    }

    #[test]
    fn critical_path_len_zero_for_no_dependencies() {
        let view = GraphView::default();
        assert_eq!(view.critical_path_len(&tid("T-1")), 0);
    }

    #[test]
    fn critical_path_len_counts_longest_chain() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Hard));
        view.edges.push(edge("T-2", "T-3", DependencyKind::Hard));
        assert_eq!(view.critical_path_len(&tid("T-1")), 2);
        assert_eq!(view.critical_path_len(&tid("T-2")), 1);
        assert_eq!(view.critical_path_len(&tid("T-3")), 0);
    }

    #[test]
    fn critical_path_len_ignores_loop_edges() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Loop));
        assert_eq!(view.critical_path_len(&tid("T-1")), 0);
    }

    #[test]
    fn topological_order_respects_dependency_direction() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Hard));
        view.edges.push(edge("T-2", "T-3", DependencyKind::Hard));
        let order = view.topological_order().expect("acyclic graph orders");
        let pos = |t: &TicketId| order.iter().position(|x| x == t).expect("present");
        assert!(pos(&tid("T-3")) < pos(&tid("T-2")));
        assert!(pos(&tid("T-2")) < pos(&tid("T-1")));
    }

    #[test]
    fn topological_order_none_for_illegal_cycle() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Hard));
        view.edges.push(edge("T-2", "T-1", DependencyKind::Hard));
        assert!(view.topological_order().is_none());
    }

    #[test]
    fn topological_order_excludes_legal_loop_cycle_but_orders_rest() {
        let mut view = GraphView::default();
        view.edges.push(edge("T-1", "T-2", DependencyKind::Loop));
        view.edges.push(edge("T-2", "T-1", DependencyKind::Loop));
        view.cycle_budgeted.insert(tid("T-1"));
        view.edges.push(edge("T-3", "T-1", DependencyKind::Hard));
        let order = view
            .topological_order()
            .expect("legal loop cycle still orders");
        assert_eq!(order.len(), 3);
        let pos = |t: &TicketId| order.iter().position(|x| x == t).expect("present");
        assert!(pos(&tid("T-1")) < pos(&tid("T-3")));
    }
}
