//! Milestones as graph cuts (`SPEC.md` §4.6).
//!
//! A milestone is a named set of member tickets. Closing requires every member `Closed` or
//! `Cancelled`; reopening cascades `ticket.reopened` to affected descendants (tickets outside
//! the milestone that depended on one of its members). Pure logic over an injected view; `Store`
//! owns persistence and event emission.

use tm_types::{DecisionId, MilestoneId, ParticipantId, TicketId};

use crate::graph::DependencyGraph;
use crate::ticket::TicketState;

/// Open or closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MilestoneState {
    /// Still accepting/tracking work.
    Open,
    /// Closed: every member ticket was `Closed` or `Cancelled` at close time.
    Closed,
}

/// A milestone record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    /// Identity.
    pub id: MilestoneId,
    /// Human-readable title.
    pub title: String,
    /// Member ticket ids.
    pub tickets: Vec<TicketId>,
    /// Open or closed.
    pub state: MilestoneState,
    /// Who closed it, if closed.
    pub closed_by: Option<ParticipantId>,
    /// Decisions this milestone's scope rests on.
    pub assumptions: Vec<DecisionId>,
}

/// Why [`MilestoneStore::close`] refused to close a milestone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CloseError {
    /// At least one member ticket is neither `Closed` nor `Cancelled`.
    #[error("milestone {0} has an open member ticket")]
    OpenMember(MilestoneId),
}

/// Facade over pure milestone operations; `Store` wraps these with authority checks, event
/// emission and persistence.
pub struct MilestoneStore;

impl MilestoneStore {
    /// Close `milestone`, given the current state of every one of its member tickets.
    ///
    /// # Errors
    /// [`CloseError::OpenMember`] unless every ticket in `member_states` is
    /// [`TicketState::Closed`] or [`TicketState::Cancelled`].
    pub fn close(
        milestone: &Milestone,
        member_states: &std::collections::BTreeMap<TicketId, TicketState>,
        closed_by: ParticipantId,
    ) -> Result<Milestone, CloseError> {
        for ticket_id in &milestone.tickets {
            if let Some(&state) = member_states.get(ticket_id) {
                if state != TicketState::Closed && state != TicketState::Cancelled {
                    return Err(CloseError::OpenMember(milestone.id.clone()));
                }
            } else {
                return Err(CloseError::OpenMember(milestone.id.clone()));
            }
        }
        Ok(Milestone {
            id: milestone.id.clone(),
            title: milestone.title.clone(),
            tickets: milestone.tickets.clone(),
            state: MilestoneState::Closed,
            closed_by: Some(closed_by),
            assumptions: milestone.assumptions.clone(),
        })
    }

    /// Reopen `milestone`, returning the reopened milestone plus the set of descendant tickets
    /// (outside the milestone) whose dependency on a milestone member means they must also
    /// receive `ticket.reopened` (`SPEC.md` §4.6: "cascades ticket.reopened to affected
    /// descendants"). Authority checking (`authority.project.reopen_milestone`) happens in
    /// `Store`, not here.
    pub fn reopen(milestone: &Milestone, graph: &DependencyGraph) -> (Milestone, Vec<TicketId>) {
        let mut all_descendants = std::collections::BTreeSet::new();
        let milestone_members: std::collections::BTreeSet<TicketId> =
            milestone.tickets.iter().cloned().collect();

        for member in &milestone.tickets {
            let descendants = graph.descendants(member);
            all_descendants.extend(descendants);
        }

        // Remove the milestone's own members from the descendants list
        for member in &milestone_members {
            all_descendants.remove(member);
        }

        let reopened = Milestone {
            id: milestone.id.clone(),
            title: milestone.title.clone(),
            tickets: milestone.tickets.clone(),
            state: MilestoneState::Open,
            closed_by: None,
            assumptions: milestone.assumptions.clone(),
        };

        let affected: Vec<TicketId> = all_descendants.into_iter().collect();
        (reopened, affected)
    }

    /// Every milestone (from `all`) that lists `ticket` as a member.
    pub fn membership_of<'a>(all: &'a [Milestone], ticket: &TicketId) -> Vec<&'a Milestone> {
        all.iter().filter(|m| m.tickets.contains(ticket)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::DependencyEdge;
    use std::collections::BTreeMap;
    use tm_types::ParticipantId;

    fn test_ids() -> tm_types::CounterIds {
        tm_types::CounterIds::new()
    }

    fn test_milestone(_ids: &tm_types::CounterIds, id: &str, tickets: Vec<TicketId>) -> Milestone {
        Milestone {
            id: MilestoneId::new(id).expect("invalid milestone id"),
            title: format!("Milestone {}", id),
            tickets,
            state: MilestoneState::Open,
            closed_by: None,
            assumptions: Vec::new(),
        }
    }

    fn test_ticket_id(prefix: &str, n: u64) -> TicketId {
        TicketId::new(format!("{}-{}", prefix, n)).expect("invalid ticket id")
    }

    #[test]
    fn close_all_tickets_closed() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1), test_ticket_id("T", 2)];
        let milestone = test_milestone(&ids, "M-1", tickets.clone());

        let mut member_states = BTreeMap::new();
        member_states.insert(tickets[0].clone(), TicketState::Closed);
        member_states.insert(tickets[1].clone(), TicketState::Closed);

        let closer = ParticipantId::system();
        let result = MilestoneStore::close(&milestone, &member_states, closer.clone());

        assert!(result.is_ok());
        let closed = result.unwrap();
        assert_eq!(closed.state, MilestoneState::Closed);
        assert_eq!(closed.closed_by, Some(closer));
        assert_eq!(closed.id, milestone.id);
        assert_eq!(closed.tickets, milestone.tickets);
    }

    #[test]
    fn close_all_tickets_cancelled() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1), test_ticket_id("T", 2)];
        let milestone = test_milestone(&ids, "M-1", tickets.clone());

        let mut member_states = BTreeMap::new();
        member_states.insert(tickets[0].clone(), TicketState::Cancelled);
        member_states.insert(tickets[1].clone(), TicketState::Cancelled);

        let closer = ParticipantId::system();
        let result = MilestoneStore::close(&milestone, &member_states, closer);

        assert!(result.is_ok());
        let closed = result.unwrap();
        assert_eq!(closed.state, MilestoneState::Closed);
    }

    #[test]
    fn close_mixed_closed_and_cancelled() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1), test_ticket_id("T", 2)];
        let milestone = test_milestone(&ids, "M-1", tickets.clone());

        let mut member_states = BTreeMap::new();
        member_states.insert(tickets[0].clone(), TicketState::Closed);
        member_states.insert(tickets[1].clone(), TicketState::Cancelled);

        let closer = ParticipantId::system();
        let result = MilestoneStore::close(&milestone, &member_states, closer);

        assert!(result.is_ok());
        let closed = result.unwrap();
        assert_eq!(closed.state, MilestoneState::Closed);
    }

    #[test]
    fn close_fails_when_member_is_open() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1), test_ticket_id("T", 2)];
        let milestone = test_milestone(&ids, "M-1", tickets.clone());

        let mut member_states = BTreeMap::new();
        member_states.insert(tickets[0].clone(), TicketState::Closed);
        member_states.insert(tickets[1].clone(), TicketState::Ready);

        let closer = ParticipantId::system();
        let result = MilestoneStore::close(&milestone, &member_states, closer);

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            CloseError::OpenMember(milestone.id.clone())
        );
    }

    #[test]
    fn close_fails_when_member_is_running() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1), test_ticket_id("T", 2)];
        let milestone = test_milestone(&ids, "M-1", tickets.clone());

        let mut member_states = BTreeMap::new();
        member_states.insert(tickets[0].clone(), TicketState::Closed);
        member_states.insert(tickets[1].clone(), TicketState::Running);

        let closer = ParticipantId::system();
        let result = MilestoneStore::close(&milestone, &member_states, closer);

        assert!(result.is_err());
    }

    #[test]
    fn close_fails_when_member_is_submitted() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1), test_ticket_id("T", 2)];
        let milestone = test_milestone(&ids, "M-1", tickets.clone());

        let mut member_states = BTreeMap::new();
        member_states.insert(tickets[0].clone(), TicketState::Submitted);
        member_states.insert(tickets[1].clone(), TicketState::Closed);

        let closer = ParticipantId::system();
        let result = MilestoneStore::close(&milestone, &member_states, closer);

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            CloseError::OpenMember(milestone.id.clone())
        );
    }

    #[test]
    fn close_fails_when_member_missing_from_states() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1), test_ticket_id("T", 2)];
        let milestone = test_milestone(&ids, "M-1", tickets.clone());

        let mut member_states = BTreeMap::new();
        member_states.insert(tickets[0].clone(), TicketState::Closed);
        // Intentionally omit tickets[1]

        let closer = ParticipantId::system();
        let result = MilestoneStore::close(&milestone, &member_states, closer);

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            CloseError::OpenMember(milestone.id.clone())
        );
    }

    #[test]
    fn close_preserves_title_and_assumptions() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1)];
        let mut milestone = test_milestone(&ids, "M-1", tickets.clone());
        milestone.title = "Important Release".to_string();
        milestone.assumptions = vec![DecisionId::new("D-5").expect("invalid decision id")];

        let mut member_states = BTreeMap::new();
        member_states.insert(tickets[0].clone(), TicketState::Closed);

        let closer = ParticipantId::system();
        let closed = MilestoneStore::close(&milestone, &member_states, closer).unwrap();

        assert_eq!(closed.title, "Important Release");
        assert_eq!(closed.assumptions.len(), 1);
    }

    #[test]
    fn reopen_basic_no_descendants() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1)];
        let mut milestone = test_milestone(&ids, "M-1", tickets.clone());
        milestone.state = MilestoneState::Closed;
        milestone.closed_by = Some(ParticipantId::system());

        let graph = DependencyGraph::build(vec![tickets[0].clone()], vec![], vec![]);

        let (reopened, affected) = MilestoneStore::reopen(&milestone, &graph);

        assert_eq!(reopened.state, MilestoneState::Open);
        assert_eq!(reopened.closed_by, None);
        assert_eq!(reopened.id, milestone.id);
        assert_eq!(reopened.tickets, milestone.tickets);
        assert!(affected.is_empty());
    }

    #[test]
    fn reopen_with_descendants() {
        let ids = test_ids();
        let t1 = test_ticket_id("T", 1);
        let t2 = test_ticket_id("T", 2);
        let t3 = test_ticket_id("T", 3);

        let milestone = test_milestone(&ids, "M-1", vec![t1.clone()]);

        // t2 depends on t1, t3 depends on t2
        use crate::ticket::DependencyKind;
        let edges = vec![
            DependencyEdge {
                from: t2.clone(),
                to: t1.clone(),
                kind: DependencyKind::Hard,
            },
            DependencyEdge {
                from: t3.clone(),
                to: t2.clone(),
                kind: DependencyKind::Hard,
            },
        ];
        let graph = DependencyGraph::build(vec![t1.clone(), t2.clone(), t3.clone()], edges, vec![]);

        let (reopened, affected) = MilestoneStore::reopen(&milestone, &graph);

        assert_eq!(reopened.state, MilestoneState::Open);
        assert_eq!(reopened.closed_by, None);
        // Affected should be t2 and t3
        assert_eq!(affected.len(), 2);
        assert!(affected.contains(&t2));
        assert!(affected.contains(&t3));
        // t1 (the milestone member) should not be in affected
        assert!(!affected.contains(&t1));
    }

    #[test]
    fn reopen_excludes_milestone_members_from_descendants() {
        let ids = test_ids();
        let t1 = test_ticket_id("T", 1);
        let t2 = test_ticket_id("T", 2);
        let t3 = test_ticket_id("T", 3);

        // t1 and t2 are milestone members
        let milestone = test_milestone(&ids, "M-1", vec![t1.clone(), t2.clone()]);

        // t1 -> t2 (both in milestone), t2 -> t3 (t3 outside)
        use crate::ticket::DependencyKind;
        let edges = vec![
            DependencyEdge {
                from: t2.clone(),
                to: t1.clone(),
                kind: DependencyKind::Hard,
            },
            DependencyEdge {
                from: t3.clone(),
                to: t2.clone(),
                kind: DependencyKind::Hard,
            },
        ];
        let graph = DependencyGraph::build(vec![t1.clone(), t2.clone(), t3.clone()], edges, vec![]);

        let (reopened, affected) = MilestoneStore::reopen(&milestone, &graph);

        assert_eq!(reopened.state, MilestoneState::Open);
        // Only t3 should be affected (t1 and t2 are milestone members and should be excluded)
        assert_eq!(affected.len(), 1);
        assert!(affected.contains(&t3));
        assert!(!affected.contains(&t1));
        assert!(!affected.contains(&t2));
    }

    #[test]
    fn reopen_multiple_members_union_descendants() {
        let ids = test_ids();
        let t1 = test_ticket_id("T", 1);
        let t2 = test_ticket_id("T", 2);
        let t3 = test_ticket_id("T", 3);
        let t4 = test_ticket_id("T", 4);

        // t1 and t2 are milestone members
        let milestone = test_milestone(&ids, "M-1", vec![t1.clone(), t2.clone()]);

        // t3 depends on t1, t4 depends on t2
        use crate::ticket::DependencyKind;
        let edges = vec![
            DependencyEdge {
                from: t3.clone(),
                to: t1.clone(),
                kind: DependencyKind::Hard,
            },
            DependencyEdge {
                from: t4.clone(),
                to: t2.clone(),
                kind: DependencyKind::Hard,
            },
        ];
        let graph = DependencyGraph::build(
            vec![t1.clone(), t2.clone(), t3.clone(), t4.clone()],
            edges,
            vec![],
        );

        let (reopened, affected) = MilestoneStore::reopen(&milestone, &graph);

        assert_eq!(reopened.state, MilestoneState::Open);
        // Both t3 and t4 should be affected
        assert_eq!(affected.len(), 2);
        assert!(affected.contains(&t3));
        assert!(affected.contains(&t4));
    }

    #[test]
    fn reopen_preserves_title_and_assumptions() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1)];
        let mut milestone = test_milestone(&ids, "M-1", tickets.clone());
        milestone.title = "Release v2.0".to_string();
        milestone.assumptions = vec![DecisionId::new("D-10").expect("invalid decision id")];
        milestone.state = MilestoneState::Closed;
        milestone.closed_by = Some(ParticipantId::system());

        let graph = DependencyGraph::build(vec![tickets[0].clone()], vec![], vec![]);

        let (reopened, _) = MilestoneStore::reopen(&milestone, &graph);

        assert_eq!(reopened.title, "Release v2.0");
        assert_eq!(reopened.assumptions.len(), 1);
        assert_eq!(
            reopened.assumptions[0],
            DecisionId::new("D-10").expect("invalid decision id")
        );
    }

    #[test]
    fn membership_of_empty_list() {
        let ticket = test_ticket_id("T", 1);
        let result = MilestoneStore::membership_of(&[], &ticket);
        assert!(result.is_empty());
    }

    #[test]
    fn membership_of_no_matches() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1), test_ticket_id("T", 2)];
        let milestone = test_milestone(&ids, "M-1", tickets);

        let query_ticket = test_ticket_id("T", 99);
        let milestones = [milestone];
        let result = MilestoneStore::membership_of(&milestones, &query_ticket);

        assert!(result.is_empty());
    }

    #[test]
    fn membership_of_single_match() {
        let ids = test_ids();
        let t1 = test_ticket_id("T", 1);
        let t2 = test_ticket_id("T", 2);
        let milestone = test_milestone(&ids, "M-1", vec![t1.clone(), t2]);

        let milestones = [milestone.clone()];
        let result = MilestoneStore::membership_of(&milestones, &t1);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, milestone.id);
    }

    #[test]
    fn membership_of_multiple_milestones() {
        let ids = test_ids();
        let t1 = test_ticket_id("T", 1);
        let t2 = test_ticket_id("T", 2);

        let m1 = test_milestone(&ids, "M-1", vec![t1.clone(), t2.clone()]);
        let m2 = test_milestone(&ids, "M-2", vec![t1.clone()]);
        let m3 = test_milestone(&ids, "M-3", vec![t2.clone()]);

        let milestones = [m1.clone(), m2.clone(), m3];
        let result = MilestoneStore::membership_of(&milestones, &t1);

        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|m| m.id == m1.id));
        assert!(result.iter().any(|m| m.id == m2.id));
    }

    #[test]
    fn membership_of_all_milestones_when_in_all() {
        let ids = test_ids();
        let t1 = test_ticket_id("T", 1);

        let m1 = test_milestone(&ids, "M-1", vec![t1.clone()]);
        let m2 = test_milestone(&ids, "M-2", vec![t1.clone()]);
        let m3 = test_milestone(&ids, "M-3", vec![t1.clone()]);

        let milestones = [m1.clone(), m2.clone(), m3.clone()];
        let result = MilestoneStore::membership_of(&milestones, &t1);

        assert_eq!(result.len(), 3);
    }

    #[test]
    fn close_does_not_modify_original() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1)];
        let original = test_milestone(&ids, "M-1", tickets.clone());

        let mut member_states = BTreeMap::new();
        member_states.insert(tickets[0].clone(), TicketState::Closed);

        let closer = ParticipantId::system();
        let _ = MilestoneStore::close(&original, &member_states, closer);

        // Original should still be open
        assert_eq!(original.state, MilestoneState::Open);
        assert_eq!(original.closed_by, None);
    }

    #[test]
    fn reopen_does_not_modify_original() {
        let ids = test_ids();
        let tickets = vec![test_ticket_id("T", 1)];
        let mut original = test_milestone(&ids, "M-1", tickets.clone());
        original.state = MilestoneState::Closed;
        original.closed_by = Some(ParticipantId::system());

        let graph = DependencyGraph::build(vec![tickets[0].clone()], vec![], vec![]);

        let _ = MilestoneStore::reopen(&original, &graph);

        // Original should still be closed
        assert_eq!(original.state, MilestoneState::Closed);
        assert_eq!(original.closed_by, Some(ParticipantId::system()));
    }
}
