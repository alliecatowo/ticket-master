//! Milestones as graph cuts over the ticket set.
//!
//! A milestone groups a set of tickets; closing it requires every member ticket to be terminal
//! (`Closed` or `Cancelled`); reopening it requires `authority.project.reopen_milestone` and
//! cascades `ticket.reopened` to affected descendants (`SPEC.md` §4.6). This module owns the pure
//! decision logic; `store.rs::create_milestone`/`close_milestone`/`reopen_milestone` own event
//! drafting and persistence.

use thiserror::Error;
use tm_types::{Authority, DecisionId, MilestoneId, ParticipantId, TicketId};

use crate::ticket::TicketState;

/// Whether a milestone is accepting/tracking work or has been closed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MilestoneState {
    /// Open: member tickets may still be in any state.
    Open,
    /// Closed: every member ticket was `Closed` or `Cancelled` at close time.
    Closed,
}

/// A graph cut over the ticket set: a named group of tickets tracked to completion together.
#[derive(Debug, Clone, PartialEq)]
pub struct Milestone {
    /// Stable identifier.
    pub id: MilestoneId,
    /// Human title.
    pub title: String,
    /// Member ticket ids.
    pub tickets: Vec<TicketId>,
    /// Current lifecycle state.
    pub state: MilestoneState,
    /// Who closed this milestone, if it is closed.
    pub closed_by: Option<ParticipantId>,
    /// Assumptions (decisions) this milestone's scope rests on.
    pub assumptions: Vec<DecisionId>,
}

/// Why [`close`] refused to close a milestone.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("milestone {milestone} has {count} member ticket(s) not Closed or Cancelled")]
pub struct NotAllMembersTerminal {
    /// The milestone that failed to close.
    pub milestone: MilestoneId,
    /// How many member tickets were neither `Closed` nor `Cancelled`.
    pub count: usize,
}

/// Why [`reopen`] refused to reopen a milestone.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReopenError {
    /// The milestone was not `Closed`.
    #[error("milestone {0} is not Closed")]
    NotClosed(MilestoneId),
    /// The caller's authority lacked `project.reopen_milestone`.
    #[error("authority lacks project.reopen_milestone")]
    AuthorityDenied,
}

/// True when every member ticket's state is `Closed` or `Cancelled`.
pub fn all_members_terminal(
    milestone: &Milestone,
    states: &std::collections::BTreeMap<TicketId, TicketState>,
) -> bool {
    milestone.tickets.iter().all(|ticket_id| {
        matches!(
            states.get(ticket_id),
            Some(TicketState::Closed) | Some(TicketState::Cancelled)
        )
    })
}

/// Attempt to close `milestone`, given the current state of every ticket. `SPEC.md` §4.6: closing
/// requires all member tickets `Closed|Cancelled`.
pub fn close(
    milestone: &Milestone,
    states: &std::collections::BTreeMap<TicketId, TicketState>,
    by: ParticipantId,
) -> Result<Milestone, NotAllMembersTerminal> {
    if all_members_terminal(milestone, states) {
        Ok(Milestone {
            state: MilestoneState::Closed,
            closed_by: Some(by),
            ..milestone.clone()
        })
    } else {
        let count = milestone
            .tickets
            .iter()
            .filter(|ticket_id| {
                !matches!(
                    states.get(ticket_id),
                    Some(TicketState::Closed) | Some(TicketState::Cancelled)
                )
            })
            .count();
        Err(NotAllMembersTerminal {
            milestone: milestone.id.clone(),
            count,
        })
    }
}

/// Attempt to reopen `milestone`, given the authority the caller is acting with. `SPEC.md` §4.6:
/// requires `authority.project.reopen_milestone`; returns the reopened milestone plus the
/// descendant tickets that must receive a cascaded `ticket.reopened`.
pub fn reopen(
    milestone: &Milestone,
    authority: &Authority,
) -> Result<(Milestone, Vec<TicketId>), ReopenError> {
    if milestone.state != MilestoneState::Closed {
        return Err(ReopenError::NotClosed(milestone.id.clone()));
    }
    if !authority.project.reopen_milestone {
        return Err(ReopenError::AuthorityDenied);
    }
    Ok((
        Milestone {
            state: MilestoneState::Open,
            closed_by: None,
            ..milestone.clone()
        },
        milestone.tickets.clone(),
    ))
}

/// Every milestone whose `tickets` contains `ticket`.
pub fn milestones_containing<'a>(all: &'a [Milestone], ticket: &TicketId) -> Vec<&'a Milestone> {
    all.iter().filter(|m| m.tickets.contains(ticket)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tm_types::{Authority, MilestoneId, ParticipantId, TicketId};

    fn make_milestone(id: &str, tickets: Vec<&str>) -> Milestone {
        Milestone {
            id: MilestoneId::new(id).unwrap(),
            title: format!("Milestone {}", id),
            tickets: tickets
                .into_iter()
                .map(|t| TicketId::new(t).unwrap())
                .collect(),
            state: MilestoneState::Open,
            closed_by: None,
            assumptions: vec![],
        }
    }

    fn make_states(pairs: Vec<(&str, TicketState)>) -> BTreeMap<TicketId, TicketState> {
        pairs
            .into_iter()
            .map(|(id, state)| (TicketId::new(id).unwrap(), state))
            .collect()
    }

    #[test]
    fn all_members_terminal_empty_milestone() {
        let milestone = make_milestone("M-1", vec![]);
        let states = make_states(vec![]);
        assert!(all_members_terminal(&milestone, &states));
    }

    #[test]
    fn all_members_terminal_single_closed() {
        let milestone = make_milestone("M-1", vec!["T-1"]);
        let states = make_states(vec![("T-1", TicketState::Closed)]);
        assert!(all_members_terminal(&milestone, &states));
    }

    #[test]
    fn all_members_terminal_single_cancelled() {
        let milestone = make_milestone("M-1", vec!["T-1"]);
        let states = make_states(vec![("T-1", TicketState::Cancelled)]);
        assert!(all_members_terminal(&milestone, &states));
    }

    #[test]
    fn all_members_terminal_mixed_closed_and_cancelled() {
        let milestone = make_milestone("M-1", vec!["T-1", "T-2", "T-3"]);
        let states = make_states(vec![
            ("T-1", TicketState::Closed),
            ("T-2", TicketState::Cancelled),
            ("T-3", TicketState::Closed),
        ]);
        assert!(all_members_terminal(&milestone, &states));
    }

    #[test]
    fn all_members_terminal_one_not_terminal() {
        let milestone = make_milestone("M-1", vec!["T-1", "T-2"]);
        let states = make_states(vec![
            ("T-1", TicketState::Closed),
            ("T-2", TicketState::Ready),
        ]);
        assert!(!all_members_terminal(&milestone, &states));
    }

    #[test]
    fn all_members_terminal_missing_ticket() {
        let milestone = make_milestone("M-1", vec!["T-1", "T-2"]);
        let states = make_states(vec![("T-1", TicketState::Closed)]);
        assert!(!all_members_terminal(&milestone, &states));
    }

    #[test]
    fn all_members_terminal_escalated() {
        let milestone = make_milestone("M-1", vec!["T-1"]);
        let states = make_states(vec![("T-1", TicketState::Escalated)]);
        assert!(!all_members_terminal(&milestone, &states));
    }

    #[test]
    fn close_all_terminal_empty_milestone() {
        let milestone = make_milestone("M-1", vec![]);
        let states = make_states(vec![]);
        let by = ParticipantId::system();

        let result = close(&milestone, &states, by);
        assert!(result.is_ok());

        let closed = result.unwrap();
        assert_eq!(closed.state, MilestoneState::Closed);
        assert_eq!(closed.closed_by, Some(ParticipantId::system()));
        assert_eq!(closed.id, milestone.id);
        assert_eq!(closed.title, milestone.title);
        assert_eq!(closed.tickets, milestone.tickets);
        assert_eq!(closed.assumptions, milestone.assumptions);
    }

    #[test]
    fn close_all_terminal_with_tickets() {
        let milestone = make_milestone("M-1", vec!["T-1", "T-2"]);
        let states = make_states(vec![
            ("T-1", TicketState::Closed),
            ("T-2", TicketState::Cancelled),
        ]);
        let by = ParticipantId::new("human:alice").unwrap();

        let result = close(&milestone, &states, by.clone());
        assert!(result.is_ok());

        let closed = result.unwrap();
        assert_eq!(closed.state, MilestoneState::Closed);
        assert_eq!(closed.closed_by, Some(by));
    }

    #[test]
    fn close_not_all_terminal_one_ready() {
        let milestone = make_milestone("M-1", vec!["T-1", "T-2", "T-3"]);
        let states = make_states(vec![
            ("T-1", TicketState::Closed),
            ("T-2", TicketState::Ready),
            ("T-3", TicketState::Closed),
        ]);
        let by = ParticipantId::system();

        let result = close(&milestone, &states, by);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.milestone, MilestoneId::new("M-1").unwrap());
        assert_eq!(err.count, 1);
    }

    #[test]
    fn close_not_all_terminal_multiple_not_terminal() {
        let milestone = make_milestone("M-1", vec!["T-1", "T-2", "T-3", "T-4"]);
        let states = make_states(vec![
            ("T-1", TicketState::Closed),
            ("T-2", TicketState::Running),
            ("T-3", TicketState::Submitted),
            ("T-4", TicketState::Escalated),
        ]);
        let by = ParticipantId::system();

        let result = close(&milestone, &states, by);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.count, 3);
    }

    #[test]
    fn close_not_all_terminal_missing_ticket() {
        let milestone = make_milestone("M-1", vec!["T-1", "T-2"]);
        let states = make_states(vec![("T-1", TicketState::Closed)]);
        let by = ParticipantId::system();

        let result = close(&milestone, &states, by);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.count, 1);
    }

    #[test]
    fn reopen_closed_with_permission() {
        let mut milestone = make_milestone("M-1", vec!["T-1", "T-2"]);
        milestone.state = MilestoneState::Closed;
        milestone.closed_by = Some(ParticipantId::system());

        let authority = Authority::root();

        let result = reopen(&milestone, &authority);
        assert!(result.is_ok());

        let (reopened, tickets_to_cascade) = result.unwrap();
        assert_eq!(reopened.state, MilestoneState::Open);
        assert_eq!(reopened.closed_by, None);
        assert_eq!(reopened.id, milestone.id);
        assert_eq!(reopened.title, milestone.title);
        assert_eq!(tickets_to_cascade, milestone.tickets);
    }

    #[test]
    fn reopen_open_fails() {
        let milestone = make_milestone("M-1", vec!["T-1"]);
        let authority = Authority::root();

        let result = reopen(&milestone, &authority);
        assert!(result.is_err());

        match result.unwrap_err() {
            ReopenError::NotClosed(id) => assert_eq!(id, MilestoneId::new("M-1").unwrap()),
            ReopenError::AuthorityDenied => panic!("Expected NotClosed error"),
        }
    }

    #[test]
    fn reopen_no_authority() {
        let mut milestone = make_milestone("M-1", vec!["T-1"]);
        milestone.state = MilestoneState::Closed;
        milestone.closed_by = Some(ParticipantId::system());

        let authority = Authority::none();

        let result = reopen(&milestone, &authority);
        assert!(result.is_err());

        match result.unwrap_err() {
            ReopenError::AuthorityDenied => (),
            ReopenError::NotClosed(_) => panic!("Expected AuthorityDenied error"),
        }
    }

    #[test]
    fn reopen_closed_empty_tickets() {
        let mut milestone = make_milestone("M-1", vec![]);
        milestone.state = MilestoneState::Closed;
        milestone.closed_by = Some(ParticipantId::system());

        let authority = Authority::root();

        let result = reopen(&milestone, &authority);
        assert!(result.is_ok());

        let (reopened, tickets_to_cascade) = result.unwrap();
        assert_eq!(reopened.state, MilestoneState::Open);
        assert!(tickets_to_cascade.is_empty());
    }

    #[test]
    fn milestones_containing_no_milestones() {
        let all = vec![];
        let ticket = TicketId::new("T-1").unwrap();
        let result = milestones_containing(&all, &ticket);
        assert!(result.is_empty());
    }

    #[test]
    fn milestones_containing_ticket_in_one() {
        let milestones = vec![
            make_milestone("M-1", vec!["T-1", "T-2"]),
            make_milestone("M-2", vec!["T-3", "T-4"]),
        ];
        let ticket = TicketId::new("T-1").unwrap();
        let result = milestones_containing(&milestones, &ticket);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, MilestoneId::new("M-1").unwrap());
    }

    #[test]
    fn milestones_containing_ticket_in_multiple() {
        let milestones = vec![
            make_milestone("M-1", vec!["T-1", "T-2"]),
            make_milestone("M-2", vec!["T-1", "T-3"]),
            make_milestone("M-3", vec!["T-4"]),
        ];
        let ticket = TicketId::new("T-1").unwrap();
        let result = milestones_containing(&milestones, &ticket);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, MilestoneId::new("M-1").unwrap());
        assert_eq!(result[1].id, MilestoneId::new("M-2").unwrap());
    }

    #[test]
    fn milestones_containing_ticket_not_in_any() {
        let milestones = vec![
            make_milestone("M-1", vec!["T-1", "T-2"]),
            make_milestone("M-2", vec!["T-3", "T-4"]),
        ];
        let ticket = TicketId::new("T-5").unwrap();
        let result = milestones_containing(&milestones, &ticket);
        assert!(result.is_empty());
    }
}
