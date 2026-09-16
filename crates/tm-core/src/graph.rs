//! The dependency and parent/child graph, as pure functions over an in-memory view.
//!
//! Everything here operates on [`DependencyGraph`], a plain adjacency structure built once from
//! materialized state (`ProjectView::graph` in `view.rs`) and cheap to clone/rebuild. No SQLite,
//! no `Store` — that separation is what makes `tm-scheduler`'s priority computations and
//! `invariants.rs`'s acyclicity check independently unit-testable against hand-built graphs.

use std::collections::{BTreeMap, BTreeSet};

use tm_types::TicketId;

use crate::ticket::DependencyKind;

/// One dependency edge: `from` depends on `to` (i.e. `to` must be `Closed` before `from` can be
/// `Ready`, when `kind` is [`DependencyKind::Hard`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DependencyEdge {
    /// The dependent ticket.
    pub from: TicketId,
    /// The dependency.
    pub to: TicketId,
    /// Hard, Soft or Loop.
    pub kind: DependencyKind,
}

/// An in-memory view of the dependency graph plus the parent/child tree, sufficient for every
/// pure query this module exposes. Built fresh from [`crate::view::ProjectView`]; not itself
/// backed by SQLite.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DependencyGraph {
    edges: Vec<DependencyEdge>,
    children: BTreeMap<TicketId, Vec<TicketId>>,
    parent: BTreeMap<TicketId, TicketId>,
    /// Every ticket id that appears as a node, including ones with no edges.
    nodes: BTreeSet<TicketId>,
}

/// A cycle found in the dependency graph that is illegal: some edge in it is not
/// [`DependencyKind::Loop`], or the cycle lacks a [`crate::ticket::CycleBudget`] on its member
/// tickets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleViolation {
    /// The tickets forming the illegal cycle, in traversal order.
    pub members: Vec<TicketId>,
    /// Human-readable reason the cycle is illegal (a non-`Loop` edge, or no `CycleBudget`).
    pub reason: String,
}

impl DependencyGraph {
    /// Build a graph from its raw edges and parent/child pairs. `has_cycle_budget` answers,
    /// per ticket id, whether that ticket carries a live [`crate::ticket::CycleBudget`] — needed
    /// by [`DependencyGraph::find_illegal_cycles`] without this module depending on `ticket.rs`'s
    /// full `Ticket` type.
    pub fn build(
        nodes: impl IntoIterator<Item = TicketId>,
        edges: impl IntoIterator<Item = DependencyEdge>,
        children: impl IntoIterator<Item = (TicketId, TicketId)>,
    ) -> Self {
        let mut g = DependencyGraph::default();
        for n in nodes {
            g.nodes.insert(n);
        }
        for e in edges {
            g.nodes.insert(e.from.clone());
            g.nodes.insert(e.to.clone());
            g.edges.push(e);
        }
        for (parent, child) in children {
            g.nodes.insert(parent.clone());
            g.nodes.insert(child.clone());
            g.children
                .entry(parent.clone())
                .or_default()
                .push(child.clone());
            g.parent.insert(child, parent);
        }
        g
    }

    /// Every node in the graph.
    pub fn nodes(&self) -> impl Iterator<Item = &TicketId> {
        self.nodes.iter()
    }

    /// Every dependency edge.
    pub fn edges(&self) -> &[DependencyEdge] {
        &self.edges
    }

    /// The parent of `ticket`, if any.
    pub fn parent_of(&self, ticket: &TicketId) -> Option<&TicketId> {
        self.parent.get(ticket)
    }

    /// The direct children of `ticket`.
    pub fn children_of(&self, ticket: &TicketId) -> &[TicketId] {
        self.children
            .get(ticket)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Every ticket transitively depended on by `ticket` (i.e. reachable by following `from ->
    /// to` edges outward from `ticket`), not including `ticket` itself.
    pub fn ancestors(&self, ticket: &TicketId) -> BTreeSet<TicketId> {
        let mut out = BTreeSet::new();
        let mut stack = vec![ticket.clone()];
        let mut visited: BTreeSet<TicketId> = BTreeSet::new();
        visited.insert(ticket.clone());
        while let Some(cur) = stack.pop() {
            for e in self.edges.iter().filter(|e| e.from == cur) {
                if visited.insert(e.to.clone()) {
                    out.insert(e.to.clone());
                    stack.push(e.to.clone());
                }
            }
        }
        out
    }

    /// Every ticket that transitively depends on `ticket` (i.e. reachable by following `to ->
    /// from` edges outward from `ticket`), not including `ticket` itself. Used to find who is
    /// affected when `ticket` is reopened or its dependency status changes.
    pub fn descendants(&self, ticket: &TicketId) -> BTreeSet<TicketId> {
        let mut out = BTreeSet::new();
        let mut stack = vec![ticket.clone()];
        let mut visited: BTreeSet<TicketId> = BTreeSet::new();
        visited.insert(ticket.clone());
        while let Some(cur) = stack.pop() {
            for e in self.edges.iter().filter(|e| e.to == cur) {
                if visited.insert(e.from.clone()) {
                    out.insert(e.from.clone());
                    stack.push(e.from.clone());
                }
            }
        }
        out
    }

    /// True when every [`DependencyKind::Hard`] dependency of `ticket` is present in
    /// `closed`. Used by the scheduler-facing "is this ticket's dependency set satisfied" check
    /// (`SPEC.md` §4.3 rule 2): cancelled dependencies never satisfy.
    pub fn dependencies_satisfied(&self, ticket: &TicketId, closed: &BTreeSet<TicketId>) -> bool {
        self.edges
            .iter()
            .filter(|e| &e.from == ticket && e.kind == DependencyKind::Hard)
            .all(|e| closed.contains(&e.to))
    }

    /// Find every cycle in the graph that is illegal per `SPEC.md` §4.3: a cycle is legal only
    /// when every edge composing it is [`DependencyKind::Loop`] *and* `has_cycle_budget` is true
    /// for every member ticket. Any other cycle (through a `Hard` or `Soft` edge, or through a
    /// `Loop` edge whose ticket carries no budget) is reported here.
    pub fn find_illegal_cycles(
        &self,
        has_cycle_budget: impl Fn(&TicketId) -> bool,
    ) -> Vec<CycleViolation> {
        let sccs = self.tarjan_sccs();
        let mut violations = Vec::new();
        for members in sccs {
            let is_self_loop = members.len() == 1
                && self
                    .edges
                    .iter()
                    .any(|e| e.from == members[0] && e.to == members[0]);
            if members.len() < 2 && !is_self_loop {
                // Singleton with no self-loop: not a cycle at all.
                continue;
            }
            let member_set: BTreeSet<&TicketId> = members.iter().collect();
            let internal_edges: Vec<&DependencyEdge> = self
                .edges
                .iter()
                .filter(|e| member_set.contains(&e.from) && member_set.contains(&e.to))
                .collect();
            let all_loop = internal_edges
                .iter()
                .all(|e| e.kind == DependencyKind::Loop);
            let all_budgeted = members.iter().all(&has_cycle_budget);
            if !all_loop || !all_budgeted {
                let reason = if !all_loop {
                    "cycle contains a non-Loop dependency edge".to_string()
                } else {
                    "cycle member lacks a CycleBudget".to_string()
                };
                violations.push(CycleViolation { members, reason });
            }
        }
        violations
    }

    /// Strongly-connected components of size > 1, plus singleton self-loops, via Tarjan's
    /// algorithm. Returned in discovery order for readable diagnostics.
    fn tarjan_sccs(&self) -> Vec<Vec<TicketId>> {
        struct State<'a> {
            graph: &'a DependencyGraph,
            index: BTreeMap<TicketId, usize>,
            lowlink: BTreeMap<TicketId, usize>,
            on_stack: BTreeSet<TicketId>,
            stack: Vec<TicketId>,
            next_index: usize,
            sccs: Vec<Vec<TicketId>>,
        }

        impl<'a> State<'a> {
            fn strongconnect(&mut self, v: &TicketId) {
                self.index.insert(v.clone(), self.next_index);
                self.lowlink.insert(v.clone(), self.next_index);
                self.next_index += 1;
                self.stack.push(v.clone());
                self.on_stack.insert(v.clone());

                for e in self.graph.edges.iter().filter(|e| &e.from == v) {
                    let w = &e.to;
                    if !self.index.contains_key(w) {
                        self.strongconnect(w);
                        let w_low = *self
                            .lowlink
                            .get(w)
                            .expect("strongconnect(w) assigns w a lowlink before returning");
                        let v_low = self.lowlink.get(v).copied().unwrap_or(usize::MAX);
                        self.lowlink.insert(v.clone(), v_low.min(w_low));
                    } else if self.on_stack.contains(w) {
                        let w_idx = *self.index.get(w).expect("checked contains_key above");
                        let v_low = self.lowlink.get(v).copied().unwrap_or(usize::MAX);
                        self.lowlink.insert(v.clone(), v_low.min(w_idx));
                    }
                }

                if self.lowlink.get(v) == self.index.get(v) {
                    let mut component = Vec::new();
                    loop {
                        let w = self.stack.pop().expect("v is on the stack by construction");
                        self.on_stack.remove(&w);
                        let is_v = &w == v;
                        component.push(w);
                        if is_v {
                            break;
                        }
                    }
                    self.sccs.push(component);
                }
            }
        }

        let mut state = State {
            graph: self,
            index: BTreeMap::new(),
            lowlink: BTreeMap::new(),
            on_stack: BTreeSet::new(),
            stack: Vec::new(),
            next_index: 0,
            sccs: Vec::new(),
        };
        for n in self.nodes.iter() {
            if !state.index.contains_key(n) {
                state.strongconnect(n);
            }
        }
        state.sccs
    }

    /// Topological order of the acyclic region (every node not part of an illegal cycle per
    /// [`DependencyGraph::find_illegal_cycles`], and every `Loop` edge excluded from ordering
    /// since it is not meant to induce a linear order). `None` if the remaining graph (after
    /// excluding `Loop` edges) still contains a cycle, which would indicate an invariant
    /// violation the caller should surface via [`crate::invariants`] rather than silently order
    /// around.
    pub fn topological_order(&self) -> Option<Vec<TicketId>> {
        let acyclic_edges: Vec<&DependencyEdge> = self
            .edges
            .iter()
            .filter(|e| e.kind != DependencyKind::Loop)
            .collect();

        let mut in_degree: BTreeMap<TicketId, usize> =
            self.nodes.iter().map(|n| (n.clone(), 0)).collect();
        for e in &acyclic_edges {
            *in_degree.entry(e.from.clone()).or_insert(0) += 1;
        }

        let mut queue: BTreeSet<TicketId> = in_degree
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(n, _)| n.clone())
            .collect();

        let mut order = Vec::new();
        while let Some(n) = queue.iter().next().cloned() {
            queue.remove(&n);
            order.push(n.clone());
            // `to -> from` means `from` depends on `to`; `to` is a prerequisite of `from`, so
            // emitting `to` reduces the in-degree of every `from` that depended on it.
            for e in acyclic_edges.iter().filter(|e| e.to == n) {
                let deg = in_degree.entry(e.from.clone()).or_insert(0);
                *deg = deg.saturating_sub(1);
                if *deg == 0 {
                    queue.insert(e.from.clone());
                }
            }
        }

        if order.len() == self.nodes.len() {
            Some(order)
        } else {
            None
        }
    }

    /// The length (edge count) of the longest chain of [`DependencyKind::Hard`] dependencies
    /// ending at `ticket`, used by the scheduler as a priority signal (deeper chains schedule
    /// earlier, all else equal). `0` for a ticket with no unsatisfied hard dependencies upstream.
    pub fn critical_path_length(&self, ticket: &TicketId) -> u32 {
        let mut memo: BTreeMap<TicketId, u32> = BTreeMap::new();
        self.critical_path_length_memo(ticket, &mut memo, &mut BTreeSet::new())
    }

    /// Recursive helper for [`DependencyGraph::critical_path_length`]. `in_progress` guards
    /// against runaway recursion if the Hard-edge subgraph unexpectedly contains a cycle
    /// (an invariant violation elsewhere); such a node is treated as contributing 0 further
    /// depth rather than looping forever.
    fn critical_path_length_memo(
        &self,
        ticket: &TicketId,
        memo: &mut BTreeMap<TicketId, u32>,
        in_progress: &mut BTreeSet<TicketId>,
    ) -> u32 {
        if let Some(&len) = memo.get(ticket) {
            return len;
        }
        if !in_progress.insert(ticket.clone()) {
            return 0;
        }
        let mut best = 0;
        for e in self
            .edges
            .iter()
            .filter(|e| &e.from == ticket && e.kind == DependencyKind::Hard)
        {
            let dep_len = self.critical_path_length_memo(&e.to, memo, in_progress);
            best = best.max(dep_len + 1);
        }
        in_progress.remove(ticket);
        memo.insert(ticket.clone(), best);
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> TicketId {
        TicketId::new(s).unwrap()
    }

    fn edge(from: &str, to: &str, kind: DependencyKind) -> DependencyEdge {
        DependencyEdge {
            from: t(from),
            to: t(to),
            kind,
        }
    }

    #[test]
    fn ancestors_follows_transitive_hard_dependencies() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Hard),
                edge("T-2", "T-3", DependencyKind::Hard),
            ],
            [],
        );
        let anc = g.ancestors(&t("T-1"));
        assert_eq!(anc, [t("T-2"), t("T-3")].into_iter().collect());
    }

    #[test]
    fn ancestors_of_leaf_is_empty() {
        let g = DependencyGraph::build([], [edge("T-1", "T-2", DependencyKind::Hard)], []);
        assert!(g.ancestors(&t("T-2")).is_empty());
    }

    #[test]
    fn descendants_follows_transitive_incoming_dependencies() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Hard),
                edge("T-2", "T-3", DependencyKind::Hard),
            ],
            [],
        );
        let desc = g.descendants(&t("T-3"));
        assert_eq!(desc, [t("T-1"), t("T-2")].into_iter().collect());
    }

    #[test]
    fn ancestors_and_descendants_are_cycle_safe() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Loop),
                edge("T-2", "T-1", DependencyKind::Loop),
            ],
            [],
        );
        assert_eq!(g.ancestors(&t("T-1")), [t("T-2")].into_iter().collect());
        assert_eq!(g.descendants(&t("T-1")), [t("T-2")].into_iter().collect());
    }

    #[test]
    fn dependencies_satisfied_true_when_all_hard_deps_closed() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Hard),
                edge("T-1", "T-3", DependencyKind::Soft),
            ],
            [],
        );
        let mut closed = BTreeSet::new();
        closed.insert(t("T-2"));
        assert!(g.dependencies_satisfied(&t("T-1"), &closed));
    }

    #[test]
    fn dependencies_satisfied_false_when_hard_dep_not_closed() {
        let g = DependencyGraph::build([], [edge("T-1", "T-2", DependencyKind::Hard)], []);
        assert!(!g.dependencies_satisfied(&t("T-1"), &BTreeSet::new()));
    }

    #[test]
    fn dependencies_satisfied_ignores_soft_deps() {
        let g = DependencyGraph::build([], [edge("T-1", "T-2", DependencyKind::Soft)], []);
        assert!(g.dependencies_satisfied(&t("T-1"), &BTreeSet::new()));
    }

    #[test]
    fn hard_cycle_is_always_illegal() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Hard),
                edge("T-2", "T-1", DependencyKind::Hard),
            ],
            [],
        );
        let violations = g.find_illegal_cycles(|_| true);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].members.contains(&t("T-1")));
        assert!(violations[0].members.contains(&t("T-2")));
    }

    #[test]
    fn loop_cycle_with_budget_on_every_member_is_legal() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Loop),
                edge("T-2", "T-1", DependencyKind::Loop),
            ],
            [],
        );
        let violations = g.find_illegal_cycles(|_| true);
        assert!(violations.is_empty());
    }

    #[test]
    fn loop_cycle_without_budget_on_a_member_is_illegal() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Loop),
                edge("T-2", "T-1", DependencyKind::Loop),
            ],
            [],
        );
        let violations = g.find_illegal_cycles(|id| id != &t("T-2"));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].reason, "cycle member lacks a CycleBudget");
    }

    #[test]
    fn mixed_loop_and_hard_edge_cycle_is_illegal() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Loop),
                edge("T-2", "T-1", DependencyKind::Hard),
            ],
            [],
        );
        let violations = g.find_illegal_cycles(|_| true);
        assert_eq!(violations.len(), 1);
        assert_eq!(
            violations[0].reason,
            "cycle contains a non-Loop dependency edge"
        );
    }

    #[test]
    fn self_loop_with_budget_is_legal() {
        let g = DependencyGraph::build([], [edge("T-1", "T-1", DependencyKind::Loop)], []);
        assert!(g.find_illegal_cycles(|_| true).is_empty());
    }

    #[test]
    fn self_loop_without_budget_is_illegal() {
        let g = DependencyGraph::build([], [edge("T-1", "T-1", DependencyKind::Loop)], []);
        let violations = g.find_illegal_cycles(|_| false);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].members, vec![t("T-1")]);
    }

    #[test]
    fn acyclic_graph_has_no_illegal_cycles() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Hard),
                edge("T-2", "T-3", DependencyKind::Hard),
            ],
            [],
        );
        assert!(g.find_illegal_cycles(|_| true).is_empty());
    }

    #[test]
    fn topological_order_places_dependencies_before_dependents() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Hard),
                edge("T-2", "T-3", DependencyKind::Hard),
            ],
            [],
        );
        let order = g.topological_order().unwrap();
        let pos = |id: &TicketId| order.iter().position(|x| x == id).unwrap();
        assert!(pos(&t("T-3")) < pos(&t("T-2")));
        assert!(pos(&t("T-2")) < pos(&t("T-1")));
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn topological_order_excludes_loop_edges_from_ordering_constraints() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Loop),
                edge("T-2", "T-1", DependencyKind::Loop),
            ],
            [],
        );
        let order = g.topological_order().unwrap();
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn topological_order_is_none_for_residual_hard_cycle() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Hard),
                edge("T-2", "T-1", DependencyKind::Hard),
            ],
            [],
        );
        assert!(g.topological_order().is_none());
    }

    #[test]
    fn topological_order_includes_isolated_nodes() {
        let g = DependencyGraph::build([t("T-9")], [], []);
        assert_eq!(g.topological_order().unwrap(), vec![t("T-9")]);
    }

    #[test]
    fn critical_path_length_is_zero_with_no_hard_dependencies() {
        let g = DependencyGraph::build([t("T-1")], [], []);
        assert_eq!(g.critical_path_length(&t("T-1")), 0);
    }

    #[test]
    fn critical_path_length_counts_longest_hard_chain() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Hard),
                edge("T-2", "T-3", DependencyKind::Hard),
            ],
            [],
        );
        assert_eq!(g.critical_path_length(&t("T-1")), 2);
        assert_eq!(g.critical_path_length(&t("T-2")), 1);
        assert_eq!(g.critical_path_length(&t("T-3")), 0);
    }

    #[test]
    fn critical_path_length_ignores_soft_edges() {
        let g = DependencyGraph::build([], [edge("T-1", "T-2", DependencyKind::Soft)], []);
        assert_eq!(g.critical_path_length(&t("T-1")), 0);
    }

    #[test]
    fn critical_path_length_takes_the_longer_of_two_branches() {
        let g = DependencyGraph::build(
            [],
            [
                edge("T-1", "T-2", DependencyKind::Hard),
                edge("T-1", "T-3", DependencyKind::Hard),
                edge("T-3", "T-4", DependencyKind::Hard),
            ],
            [],
        );
        assert_eq!(g.critical_path_length(&t("T-1")), 2);
    }

    #[test]
    fn parent_and_children_reflect_build_input() {
        let g = DependencyGraph::build([], [], [(t("T-1"), t("T-2")), (t("T-1"), t("T-3"))]);
        assert_eq!(g.parent_of(&t("T-2")), Some(&t("T-1")));
        assert_eq!(g.children_of(&t("T-1")), &[t("T-2"), t("T-3")]);
        assert_eq!(g.parent_of(&t("T-1")), None);
    }
}
