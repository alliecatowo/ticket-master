//! `tickets/` page (`SPEC.md` §26.2): ticket and milestone history, assembled entirely from
//! `Store::view()`.

use tm_core::view::ProjectView;

use crate::page::WikiPage;

/// Build the single `tickets.md` page: every ticket's id/kind/state/objective, plus a milestone
/// table when any exist.
pub fn page(view: &ProjectView) -> WikiPage {
    let mut body =
        String::from("# Tickets\n\n| Ticket | Kind | State | Objective |\n|---|---|---|---|\n");
    for (id, t) in &view.tickets {
        let objective = t.objective.replace('|', "\\|");
        body.push_str(&format!(
            "| {id} | {:?} | {:?} | {objective} |\n",
            t.kind, t.state
        ));
    }
    if view.tickets.is_empty() {
        body.push_str("\n_No tickets recorded yet; run `tm ticket new` to create one._\n");
    }

    if !view.milestones.is_empty() {
        body.push_str("\n## Milestones\n\n| Milestone | Title | Tickets |\n|---|---|---|\n");
        for (id, m) in &view.milestones {
            let tickets: Vec<String> = m.tickets.iter().map(|t| t.to_string()).collect();
            body.push_str(&format!(
                "| {id} | {} | {} |\n",
                m.title,
                tickets.join(", ")
            ));
        }
    }

    // Ticket/milestone state has no natural file-glob or decision-id basis, so
    // `tm_docs::provenance::ProvenanceIndex` (which only matches changed *paths* and superseded
    // *decisions*) cannot auto-flag this page stale the way `architecture/*` and `decisions/*`
    // are. Documented limitation: this page is only as fresh as the last explicit
    // `tm_wiki::generate::run` call. A ticket-state-aware `ChangeSet` source is a natural
    // follow-up but belongs with `tm-docs`'s own provenance model (SPEC.md §9), not invented here.
    WikiPage::new("tickets", "tickets.md", body, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::{
        ExecutorRequirements, RetryPolicy, Ticket, TicketKind, TicketState, VerificationPolicy,
    };
    use tm_types::{Authority, Budget, Role, TicketId, Timestamp, Tolerance};

    fn ticket(id: &str, objective: &str) -> Ticket {
        Ticket {
            id: TicketId::new(id).unwrap(),
            kind: TicketKind::Work,
            objective: objective.to_string(),
            state: TicketState::Draft,
            parent: None,
            children: Vec::new(),
            dependencies: Vec::new(),
            milestone: None,
            authority: Authority::none(),
            resources: Vec::new(),
            executor: ExecutorRequirements {
                role: Role::CoderFast,
                human_required: false,
                min_capability: Tolerance::Any,
            },
            context_refs: Vec::new(),
            success: Vec::new(),
            verification: VerificationPolicy::None,
            budget: Budget::none(),
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

    #[test]
    fn empty_view_renders_header_only() {
        let view = ProjectView::empty();
        let page = page(&view);
        assert_eq!(page.rel_path, "tickets.md");
        assert!(page.body.contains("# Tickets"));
        assert!(page.derived_from.is_empty());
    }

    #[test]
    fn empty_view_explains_why_the_table_is_empty() {
        let view = ProjectView::empty();
        let page = page(&view);
        assert!(
            page.body.contains("No tickets recorded yet"),
            "an empty tickets page should explain why it's empty, not just show a bare table \
             header:\n{}",
            page.body
        );
    }

    #[test]
    fn lists_every_ticket() {
        let mut view = ProjectView::empty();
        let t = ticket("T-1", "Do the thing");
        view.tickets.insert(t.id.clone(), t);

        let page = page(&view);
        assert!(page.body.contains("T-1"));
        assert!(page.body.contains("Do the thing"));
    }

    #[test]
    fn escapes_pipe_in_objective_for_the_markdown_table() {
        let mut view = ProjectView::empty();
        let t = ticket("T-1", "Do A | B");
        view.tickets.insert(t.id.clone(), t);

        let page = page(&view);
        assert!(page.body.contains("Do A \\| B"));
    }
}
