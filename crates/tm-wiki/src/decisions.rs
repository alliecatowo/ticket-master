//! `decisions/` pages (`SPEC.md` §26.2): one page per decision, rendering its full supersession
//! chain, plus a `decisions.md` index. Assembled entirely from `Store::view()`'s `decisions` map
//! (`tm_core::Decision::supersedes`/`superseded_by`) — no new analysis.

use std::collections::HashSet;

use tm_core::view::ProjectView;
use tm_types::DecisionId;

use crate::page::WikiPage;

/// Walk one decision's full chain: predecessors (via `supersedes`, oldest first), then `id`
/// itself, then successors (via `superseded_by`, newest last). Bounded by `seen` in case a
/// hand-built `ProjectView` (e.g. in a test) has a cyclical edge — real chains built through
/// `Store::supersede` cannot cycle, since a decision may only be superseded once.
fn chain(view: &ProjectView, id: &DecisionId) -> Vec<DecisionId> {
    let mut seen = HashSet::new();
    seen.insert(id.clone());

    let mut before = Vec::new();
    let mut cursor = view.decisions.get(id).and_then(|d| d.supersedes.clone());
    while let Some(prev_id) = cursor {
        if !seen.insert(prev_id.clone()) {
            break;
        }
        before.push(prev_id.clone());
        cursor = view
            .decisions
            .get(&prev_id)
            .and_then(|d| d.supersedes.clone());
    }
    before.reverse();

    let mut after = Vec::new();
    let mut cursor = view.decisions.get(id).and_then(|d| d.superseded_by.clone());
    while let Some(next_id) = cursor {
        if !seen.insert(next_id.clone()) {
            break;
        }
        after.push(next_id.clone());
        cursor = view
            .decisions
            .get(&next_id)
            .and_then(|d| d.superseded_by.clone());
    }

    let mut full = before;
    full.push(id.clone());
    full.extend(after);
    full
}

fn render_page(view: &ProjectView, id: &DecisionId) -> Option<WikiPage> {
    let d = view.decisions.get(id)?;
    let chain_ids = chain(view, id);

    let mut body = format!("# Decision {}: {}\n\n", d.id, d.subject);
    body.push_str(&format!("**Decision:** {}\n\n", d.decision));
    body.push_str(&format!("**Reason:** {}\n\n", d.reason));

    let status = if d.is_active() {
        "Active".to_string()
    } else {
        format!(
            "Superseded by {}",
            d.superseded_by
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_default()
        )
    };
    body.push_str(&format!("**Status:** {status}\n\n"));

    if !d.affected_tickets.is_empty() {
        let tickets: Vec<String> = d.affected_tickets.iter().map(|t| t.to_string()).collect();
        body.push_str(&format!(
            "**Affected tickets:** {}\n\n",
            tickets
                .iter()
                .map(|t| format!("[{t}](../tickets.md)"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !d.affected_paths.is_empty() {
        body.push_str(&format!(
            "**Affected paths:** {}\n\n",
            d.affected_paths.join(", ")
        ));
    }

    if chain_ids.len() > 1 {
        body.push_str("## Supersession chain\n\n");
        for chain_id in &chain_ids {
            if let Some(cd) = view.decisions.get(chain_id) {
                let marker = if chain_id == id { "->" } else { "  " };
                body.push_str(&format!(
                    "{marker} [{}](../decisions/{}.md) — {}\n",
                    cd.id, cd.id, cd.subject
                ));
            }
        }
        body.push('\n');
    }

    let derived_from = chain_ids.iter().map(|c| c.to_string()).collect();

    Some(WikiPage::new(
        format!("decisions/{}", d.id),
        format!("decisions/{}.md", d.id),
        body,
        derived_from,
    ))
}

/// One page per decision (with its chain) plus a `decisions.md` index listing every decision and
/// its status.
pub fn pages(view: &ProjectView) -> Vec<WikiPage> {
    let mut out = Vec::new();

    let mut index_body =
        String::from("# Decisions\n\n| Decision | Subject | Status |\n|---|---|---|\n");
    for (id, d) in &view.decisions {
        let status = if d.is_active() {
            "Active"
        } else {
            "Superseded"
        };
        index_body.push_str(&format!(
            "| [{id}](decisions/{id}.md) | {} | {status} |\n",
            d.subject
        ));
        if let Some(page) = render_page(view, id) {
            out.push(page);
        }
    }
    if view.decisions.is_empty() {
        index_body
            .push_str("\n_No decisions recorded yet; run `tm decision new` to record one._\n");
    }

    let derived_from: Vec<String> = view.decisions.keys().map(|k| k.to_string()).collect();
    out.push(WikiPage::new(
        "decisions",
        "decisions.md",
        index_body,
        derived_from,
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::decision::Decision;
    use tm_types::{ArtifactId, ParticipantId, Timestamp};

    fn decision(id: &str, subject: &str) -> Decision {
        Decision::create(
            DecisionId::new(id).unwrap(),
            subject.to_string(),
            "decision text".to_string(),
            "reason text".to_string(),
            Vec::<ArtifactId>::new(),
            Vec::new(),
            Vec::new(),
            ParticipantId::system(),
            Timestamp::EPOCH,
        )
    }

    #[test]
    fn active_decision_renders_without_chain_section() {
        let mut view = ProjectView::empty();
        let d = decision("D-1", "Use SQLite");
        view.decisions.insert(d.id.clone(), d.clone());

        let page = render_page(&view, &d.id).unwrap();
        assert!(page.body.contains("**Status:** Active"));
        assert!(!page.body.contains("Supersession chain"));
        assert_eq!(page.derived_from, vec!["D-1".to_string()]);
    }

    #[test]
    fn supersession_chain_renders_in_order_and_marks_current() {
        let mut view = ProjectView::empty();

        let mut d1 = decision("D-1", "Use SQLite");
        let mut d2 = d1.superseding(
            DecisionId::new("D-2").unwrap(),
            "Use SQLite (revised)".to_string(),
            "decision text".to_string(),
            "reason text".to_string(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            ParticipantId::system(),
            Timestamp::EPOCH,
        );
        let d3 = d2.superseding(
            DecisionId::new("D-3").unwrap(),
            "Use Postgres".to_string(),
            "decision text".to_string(),
            "reason text".to_string(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            ParticipantId::system(),
            Timestamp::EPOCH,
        );
        d1.superseded_by = Some(d2.id.clone());
        d2.superseded_by = Some(d3.id.clone());

        view.decisions.insert(d1.id.clone(), d1.clone());
        view.decisions.insert(d2.id.clone(), d2.clone());
        view.decisions.insert(d3.id.clone(), d3.clone());

        // The middle decision's page must show the full chain, in order, with itself marked.
        let page = render_page(&view, &d2.id).unwrap();
        let chain_section = page.body.split("## Supersession chain").nth(1).unwrap();
        let d1_pos = chain_section.find("D-1").unwrap();
        let d2_pos = chain_section.find("D-2").unwrap();
        let d3_pos = chain_section.find("D-3").unwrap();
        assert!(
            d1_pos < d2_pos && d2_pos < d3_pos,
            "chain must render oldest to newest"
        );
        assert!(chain_section.contains("-> [D-2]"));
        assert!(page.body.contains("**Status:** Superseded by D-3"));

        // The chain's basis: staleness should trigger if *any* decision in the chain changes.
        assert_eq!(
            page.derived_from,
            vec!["D-1".to_string(), "D-2".to_string(), "D-3".to_string()]
        );

        // The root's page also sees the whole chain.
        let root_page = render_page(&view, &d1.id).unwrap();
        assert!(root_page.body.contains("**Status:** Superseded by D-2"));
        assert_eq!(
            root_page.derived_from,
            vec!["D-1".to_string(), "D-2".to_string(), "D-3".to_string()]
        );
    }

    #[test]
    fn index_page_lists_every_decision_with_status() {
        let mut view = ProjectView::empty();
        let d1 = decision("D-1", "Use SQLite");
        let d2_source = d1.clone();
        let d2 = d2_source.superseding(
            DecisionId::new("D-2").unwrap(),
            "Use SQLite (revised)".to_string(),
            "decision".to_string(),
            "reason".to_string(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            ParticipantId::system(),
            Timestamp::EPOCH,
        );
        let mut d1_superseded = d1.clone();
        d1_superseded.superseded_by = Some(d2.id.clone());
        view.decisions
            .insert(d1_superseded.id.clone(), d1_superseded);
        view.decisions.insert(d2.id.clone(), d2);

        let pages = pages(&view);
        let index = pages.iter().find(|p| p.rel_path == "decisions.md").unwrap();
        assert!(index.body.contains("D-1"));
        assert!(index.body.contains("Superseded"));
        assert!(index.body.contains("D-2"));
        assert!(index.body.contains("Active"));

        // One page per decision, plus the index.
        assert_eq!(pages.len(), 3);
    }

    #[test]
    fn index_page_explains_itself_when_no_decisions_are_recorded() {
        let view = ProjectView::empty();
        let pages = pages(&view);
        let index = pages.iter().find(|p| p.rel_path == "decisions.md").unwrap();
        assert!(
            index.body.contains("No decisions recorded yet"),
            "an empty decisions page should explain why it's empty, not just show a bare \
             table header:\n{}",
            index.body
        );
        assert_eq!(pages.len(), 1, "only the index page, no per-decision pages");
    }
}
