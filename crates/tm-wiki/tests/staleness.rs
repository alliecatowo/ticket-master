//! Proves the actual point of SPEC.md §26.1/§9: a generated wiki page is a real `DocRecord`
//! routed through `tm-docs`'s own staleness state machine (`ProvenanceIndex` + `Assessor`), not
//! a parallel mechanism this crate invents. Pure in-memory, no filesystem or `CodeIntel` index
//! involved — this is specifically about whether the `derived_from` strings [`tm_wiki`]'s page
//! builders emit actually get picked up by `tm-docs`'s real glob/decision matching.

use tm_core::decision::Decision;
use tm_core::view::ProjectView;
use tm_docs::{Assessor, ChangeSet, DocMode, DocRecord, DocRegistry, DocState, ProvenanceIndex};
use tm_types::{ArtifactId, DecisionId, ParticipantId, Timestamp};

use tm_wiki::WikiPage;

/// Build the `DocRecord` `tm-wiki::page::write_page` would build for a freshly-written page
/// (mode = generated), already `Fresh` (as if a prior generation run had verified it), so the
/// only thing under test is whether `changes` moves it to `Stale`.
fn fresh_doc_record(page: &WikiPage) -> DocRecord {
    let mut record = DocRecord::new(
        page.id.clone(),
        page.project_path(),
        DocMode::Generated,
        page.derived_from.clone(),
    );
    record.state = DocState::Fresh;
    record
}

fn assessor_for(pages: &[WikiPage]) -> Assessor {
    let mut registry = DocRegistry::new();
    for page in pages {
        registry.insert(fresh_doc_record(page));
    }
    let docs: Vec<DocRecord> = registry.list().into_iter().cloned().collect();
    let provenance =
        ProvenanceIndex::build(&docs).expect("compile provenance for well-formed pages");
    Assessor::new(registry, provenance)
}

#[test]
fn architecture_page_goes_stale_when_its_crate_changes() {
    // Exactly the derived_from shape `architecture::pages` emits: a single `crates/<name>/src/**`
    // glob (see crates/tm-wiki/src/architecture.rs).
    let arch_page = WikiPage::new(
        "architecture/tm-core",
        "architecture/tm-core.md",
        "# Architecture: tm-core\n",
        vec!["crates/tm-core/src/**".to_string()],
    );

    let mut assessor = assessor_for(std::slice::from_ref(&arch_page));

    // A change to a real file inside that crate must touch the page...
    let touches = ChangeSet {
        changed_paths: vec!["crates/tm-core/src/store.rs".to_string()],
        superseded_decisions: Vec::new(),
    };
    let assessments = assessor.assess(&touches);
    assert_eq!(assessments.len(), 1);
    assert_eq!(assessments[0].doc_id, arch_page.id);
    assert_eq!(assessments[0].new_state, DocState::Stale);

    // ...and an unrelated crate's change must not.
    let mut other_assessor = assessor_for(std::slice::from_ref(&arch_page));
    let unrelated = ChangeSet {
        changed_paths: vec!["crates/tm-server/src/routes.rs".to_string()],
        superseded_decisions: Vec::new(),
    };
    assert!(other_assessor.assess(&unrelated).is_empty());
}

#[test]
fn decision_page_goes_stale_when_its_decision_is_superseded() {
    let mut view = ProjectView::empty();
    let decision = Decision::create(
        DecisionId::new("D-1").unwrap(),
        "subject".to_string(),
        "decision".to_string(),
        "reason".to_string(),
        Vec::<ArtifactId>::new(),
        Vec::new(),
        Vec::new(),
        ParticipantId::system(),
        Timestamp::EPOCH,
    );
    view.decisions.insert(decision.id.clone(), decision);

    // The real page builder: derived_from is the decision's own chain (just itself here).
    let pages = tm_wiki::decisions::pages(&view);
    let decision_page = pages
        .iter()
        .find(|p| p.rel_path == "decisions/D-1.md")
        .expect("decisions::pages built a page for D-1")
        .clone();
    assert_eq!(decision_page.derived_from, vec!["D-1".to_string()]);

    let mut assessor = assessor_for(std::slice::from_ref(&decision_page));

    let superseded = ChangeSet {
        changed_paths: Vec::new(),
        superseded_decisions: vec![DecisionId::new("D-1").unwrap()],
    };
    let assessments = assessor.assess(&superseded);
    assert_eq!(assessments.len(), 1);
    assert_eq!(assessments[0].doc_id, decision_page.id);
    assert_eq!(assessments[0].new_state, DocState::Stale);

    // An unrelated decision being superseded must not touch it.
    let mut other_assessor = assessor_for(std::slice::from_ref(&decision_page));
    let unrelated = ChangeSet {
        changed_paths: Vec::new(),
        superseded_decisions: vec![DecisionId::new("D-2").unwrap()],
    };
    assert!(other_assessor.assess(&unrelated).is_empty());
}
