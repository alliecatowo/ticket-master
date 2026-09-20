//! `glossary` page (`SPEC.md` §26.2): project vocabulary, cross-linked. Assembled from the ticket
//! kinds actually in use and every decision's subject — not a hardcoded dictionary.

use std::collections::BTreeSet;

use tm_core::view::ProjectView;

use crate::page::WikiPage;

/// Build the single `glossary.md` page.
pub fn page(view: &ProjectView) -> WikiPage {
    let mut body = String::from("# Glossary\n\n");

    let kinds: BTreeSet<String> = view
        .tickets
        .values()
        .map(|t| format!("{:?}", t.kind))
        .collect();
    if !kinds.is_empty() {
        body.push_str("## Ticket kinds in use\n\n");
        for kind in &kinds {
            body.push_str(&format!("- **{kind}** — see [tickets](tickets.md)\n"));
        }
        body.push('\n');
    }

    if !view.decisions.is_empty() {
        body.push_str("## Decisions\n\n");
        for (id, d) in &view.decisions {
            body.push_str(&format!(
                "- **{}** — {} ([{id}](decisions/{id}.md))\n",
                d.subject, d.decision
            ));
        }
        body.push('\n');
    }

    if kinds.is_empty() && view.decisions.is_empty() {
        body.push_str(
            "_No ticket kinds or decisions recorded yet; this project's vocabulary is empty \
             until `tm ticket new`/`tm decision new` record some, then `tm wiki generate` runs \
             again._\n",
        );
    }

    let derived_from: Vec<String> = view.decisions.keys().map(|k| k.to_string()).collect();
    WikiPage::new("glossary", "glossary.md", body, derived_from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::decision::Decision;
    use tm_types::{ArtifactId, DecisionId, ParticipantId, Timestamp};

    #[test]
    fn empty_view_still_renders_a_header() {
        let view = ProjectView::empty();
        let page = page(&view);
        assert_eq!(page.rel_path, "glossary.md");
        assert!(page.body.starts_with("# Glossary"));
        assert!(page.derived_from.is_empty());
    }

    #[test]
    fn empty_view_explains_why_the_glossary_is_empty() {
        let view = ProjectView::empty();
        let page = page(&view);
        assert!(
            page.body
                .contains("No ticket kinds or decisions recorded yet"),
            "an empty glossary should explain why it's empty, not just show a bare \
             header:\n{}",
            page.body
        );
    }

    #[test]
    fn cross_links_decisions() {
        let mut view = ProjectView::empty();
        let d = Decision::create(
            DecisionId::new("D-1").unwrap(),
            "Use SQLite".to_string(),
            "We use SQLite".to_string(),
            "Because it is embedded".to_string(),
            Vec::<ArtifactId>::new(),
            Vec::new(),
            Vec::new(),
            ParticipantId::system(),
            Timestamp::EPOCH,
        );
        view.decisions.insert(d.id.clone(), d);

        let page = page(&view);
        assert!(page.body.contains("Use SQLite"));
        assert!(page.body.contains("(decisions/D-1.md)"));
        assert_eq!(page.derived_from, vec!["D-1".to_string()]);
    }
}
