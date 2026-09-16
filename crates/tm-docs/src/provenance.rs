//! Provenance storage and matching (`SPEC.md` §9).
//!
//! A doc's `derived_from` list is free text (`["crates/tm-core/src/**", "D-019", "D-027",
//! "providers.toml"]`) written by whoever declared the doc's basis. This module compiles that
//! text into two matchable forms — a [`tm_types::PatternSet`] of path globs and a set of
//! [`tm_types::DecisionId`]s — and answers the question `assess` actually needs: given a set of
//! changed paths and/or superseded decisions, which doc ids does that touch. Compilation is the
//! only place `derived_from` strings are interpreted; everything downstream works with the
//! compiled [`CompiledProvenance`], never the raw strings again.

use std::collections::BTreeSet;
use std::str::FromStr;

use tm_types::{DecisionId, PatternSet};

use crate::registry::DocRecord;

/// One doc's `derived_from` list, changed to path matcher plus decision references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledProvenance {
    /// The doc this provenance belongs to.
    pub doc_id: String,
    /// Every `derived_from` entry that was not a decision id, compiled to a matcher.
    pub paths: PatternSet,
    /// Every `derived_from` entry that parsed as a [`DecisionId`] (`D-<n>`).
    pub decisions: BTreeSet<DecisionId>,
}

impl CompiledProvenance {
    /// True when `changed_paths` or `superseded` touches this doc's basis.
    pub fn is_touched_by(&self, changed_paths: &[String], superseded: &[DecisionId]) -> bool {
        changed_paths.iter().any(|p| self.paths.matches(p))
            || superseded.iter().any(|d| self.decisions.contains(d))
    }
}

/// Split one doc's raw `derived_from` entries into path patterns and decision references, and
/// compile the path patterns into a [`PatternSet`].
pub fn compile(doc: &DocRecord) -> tm_types::Result<CompiledProvenance> {
    let mut path_patterns = Vec::new();
    let mut decisions = BTreeSet::new();

    for entry in &doc.derived_from {
        match DecisionId::from_str(entry) {
            Ok(decision_id) => {
                decisions.insert(decision_id);
            }
            Err(_) => {
                // Not a decision id, treat as a path glob
                path_patterns.push(entry.clone());
            }
        }
    }

    let paths = PatternSet::parse(path_patterns)?;

    Ok(CompiledProvenance {
        doc_id: doc.id.clone(),
        paths,
        decisions,
    })
}

/// A batch of changes to assess docs against: paths touched by a commit/index update, and/or
/// decisions superseded since the last assessment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    /// Paths (relative to the project root) that changed.
    pub changed_paths: Vec<String>,
    /// Decisions superseded since the last assessment.
    pub superseded_decisions: Vec<DecisionId>,
}

impl ChangeSet {
    /// An empty change set (touches nothing).
    pub fn empty() -> Self {
        ChangeSet::default()
    }

    /// Build a [`ChangeSet`] from a `tm-codeintel` repo walk: every walked file's path becomes a
    /// changed path. This is the bridge `SPEC.md` §9's "after every commit/index update" wiring
    /// uses — the walk already knows, via blake3 comparison, which files actually changed; the
    /// caller is expected to have filtered `records` down to that set before calling this.
    pub fn from_file_records(records: &[tm_codeintel::FileRecord]) -> Self {
        ChangeSet {
            changed_paths: records.iter().map(|r| r.path.clone()).collect(),
            superseded_decisions: Vec::new(),
        }
    }
}

/// Every doc's compiled provenance, ready to answer "which docs does this change touch".
#[derive(Debug, Clone, Default)]
pub struct ProvenanceIndex {
    entries: Vec<CompiledProvenance>,
}

impl ProvenanceIndex {
    /// Compile every doc's provenance. Fails on the first doc whose `derived_from` contains an
    /// uncompilable path pattern.
    pub fn build(docs: &[DocRecord]) -> tm_types::Result<Self> {
        let entries = docs
            .iter()
            .map(compile)
            .collect::<tm_types::Result<Vec<_>>>()?;
        Ok(ProvenanceIndex { entries })
    }

    /// Doc ids whose basis is touched by `changed_paths`.
    pub fn docs_touched_by_paths(&self, changed_paths: &[String]) -> BTreeSet<String> {
        self.entries
            .iter()
            .filter(|e| changed_paths.iter().any(|p| e.paths.matches(p)))
            .map(|e| e.doc_id.clone())
            .collect()
    }

    /// Doc ids whose basis references any of `superseded`.
    pub fn docs_touched_by_decisions(&self, superseded: &[DecisionId]) -> BTreeSet<String> {
        self.entries
            .iter()
            .filter(|e| superseded.iter().any(|d| e.decisions.contains(d)))
            .map(|e| e.doc_id.clone())
            .collect()
    }

    /// Doc ids touched by either half of `changes`. What [`crate::assess::Assessor::assess`]
    /// calls to find its candidate set before applying the staleness state machine.
    pub fn docs_touched(&self, changes: &ChangeSet) -> BTreeSet<String> {
        let mut touched = self.docs_touched_by_paths(&changes.changed_paths);
        touched.extend(self.docs_touched_by_decisions(&changes.superseded_decisions));
        touched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{DocMode, DocRecord};

    #[test]
    fn compile_with_mixed_entries() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "test-doc".to_string(),
            "docs/test.md".to_string(),
            DocMode::Generated,
            vec![
                "crates/tm-core/src/**".to_string(),
                "D-019".to_string(),
                "providers.toml".to_string(),
                "D-027".to_string(),
            ],
        );

        let compiled = compile(&doc)?;

        assert_eq!(compiled.doc_id, "test-doc");
        assert_eq!(compiled.decisions.len(), 2);
        assert!(compiled.decisions.contains(&DecisionId::new("D-019")?));
        assert!(compiled.decisions.contains(&DecisionId::new("D-027")?));

        Ok(())
    }

    #[test]
    fn compile_with_only_paths() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "path-doc".to_string(),
            "docs/path.md".to_string(),
            DocMode::Maintained,
            vec!["crates/tm-docs/src/**".to_string(), "README.md".to_string()],
        );

        let compiled = compile(&doc)?;

        assert_eq!(compiled.doc_id, "path-doc");
        assert!(compiled.decisions.is_empty());
        assert!(compiled.paths.matches("crates/tm-docs/src/lib.rs"));
        assert!(compiled.paths.matches("README.md"));

        Ok(())
    }

    #[test]
    fn compile_with_only_decisions() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "decision-doc".to_string(),
            "docs/decision.md".to_string(),
            DocMode::Human,
            vec![
                "D-001".to_string(),
                "D-042".to_string(),
                "D-100".to_string(),
            ],
        );

        let compiled = compile(&doc)?;

        assert_eq!(compiled.doc_id, "decision-doc");
        assert_eq!(compiled.decisions.len(), 3);
        assert!(compiled.paths.is_empty());

        Ok(())
    }

    #[test]
    fn compile_with_empty_derived_from() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "empty-doc".to_string(),
            "docs/empty.md".to_string(),
            DocMode::Generated,
            vec![],
        );

        let compiled = compile(&doc)?;

        assert_eq!(compiled.doc_id, "empty-doc");
        assert!(compiled.decisions.is_empty());
        assert!(compiled.paths.is_empty());

        Ok(())
    }

    #[test]
    fn compile_invalid_pattern() {
        let doc = DocRecord::new(
            "invalid-doc".to_string(),
            "docs/invalid.md".to_string(),
            DocMode::Generated,
            vec!["[invalid(pattern".to_string()],
        );

        let result = compile(&doc);
        assert!(result.is_err());
    }

    #[test]
    fn is_touched_by_matching_path() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "doc".to_string(),
            "docs/doc.md".to_string(),
            DocMode::Generated,
            vec!["crates/tm-docs/src/**".to_string()],
        );

        let compiled = compile(&doc)?;

        assert!(compiled.is_touched_by(&["crates/tm-docs/src/lib.rs".to_string()], &[]));

        Ok(())
    }

    #[test]
    fn is_touched_by_non_matching_path() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "doc".to_string(),
            "docs/doc.md".to_string(),
            DocMode::Generated,
            vec!["crates/tm-core/src/**".to_string()],
        );

        let compiled = compile(&doc)?;

        assert!(!compiled.is_touched_by(&["crates/tm-docs/src/lib.rs".to_string()], &[]));

        Ok(())
    }

    #[test]
    fn is_touched_by_matching_decision() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "doc".to_string(),
            "docs/doc.md".to_string(),
            DocMode::Generated,
            vec!["D-019".to_string()],
        );

        let compiled = compile(&doc)?;
        let superseded = vec![DecisionId::new("D-019")?];

        assert!(compiled.is_touched_by(&[], &superseded));

        Ok(())
    }

    #[test]
    fn is_touched_by_non_matching_decision() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "doc".to_string(),
            "docs/doc.md".to_string(),
            DocMode::Generated,
            vec!["D-019".to_string()],
        );

        let compiled = compile(&doc)?;
        let superseded = vec![DecisionId::new("D-020")?];

        assert!(!compiled.is_touched_by(&[], &superseded));

        Ok(())
    }

    #[test]
    fn is_touched_by_both_path_and_decision() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "doc".to_string(),
            "docs/doc.md".to_string(),
            DocMode::Generated,
            vec!["crates/**".to_string(), "D-019".to_string()],
        );

        let compiled = compile(&doc)?;

        assert!(compiled.is_touched_by(&["crates/test.rs".to_string()], &[]));

        assert!(compiled.is_touched_by(&[], &[DecisionId::new("D-019")?]));

        assert!(compiled.is_touched_by(
            &["crates/test.rs".to_string()],
            &[DecisionId::new("D-019")?]
        ));

        Ok(())
    }

    #[test]
    fn is_touched_by_multiple_paths() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "doc".to_string(),
            "docs/doc.md".to_string(),
            DocMode::Generated,
            vec!["src/**".to_string()],
        );

        let compiled = compile(&doc)?;

        assert!(compiled.is_touched_by(
            &[
                "README.md".to_string(),
                "src/main.rs".to_string(),
                "tests/test.rs".to_string()
            ],
            &[]
        ));

        Ok(())
    }

    #[test]
    fn is_touched_by_empty_changes() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "doc".to_string(),
            "docs/doc.md".to_string(),
            DocMode::Generated,
            vec!["crates/**".to_string()],
        );

        let compiled = compile(&doc)?;

        assert!(!compiled.is_touched_by(&[], &[]));

        Ok(())
    }

    #[test]
    fn provenance_index_build_single_doc() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "doc1".to_string(),
            "docs/doc1.md".to_string(),
            DocMode::Generated,
            vec!["crates/**".to_string()],
        );

        let index = ProvenanceIndex::build(&[doc])?;

        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].doc_id, "doc1");

        Ok(())
    }

    #[test]
    fn provenance_index_build_multiple_docs() -> tm_types::Result<()> {
        let doc1 = DocRecord::new(
            "doc1".to_string(),
            "docs/doc1.md".to_string(),
            DocMode::Generated,
            vec!["crates/tm-core/**".to_string()],
        );
        let doc2 = DocRecord::new(
            "doc2".to_string(),
            "docs/doc2.md".to_string(),
            DocMode::Maintained,
            vec!["D-019".to_string()],
        );

        let index = ProvenanceIndex::build(&[doc1, doc2])?;

        assert_eq!(index.entries.len(), 2);

        Ok(())
    }

    #[test]
    fn docs_touched_by_paths() -> tm_types::Result<()> {
        let doc1 = DocRecord::new(
            "doc1".to_string(),
            "docs/doc1.md".to_string(),
            DocMode::Generated,
            vec!["crates/tm-core/**".to_string()],
        );
        let doc2 = DocRecord::new(
            "doc2".to_string(),
            "docs/doc2.md".to_string(),
            DocMode::Maintained,
            vec!["crates/tm-docs/**".to_string()],
        );

        let index = ProvenanceIndex::build(&[doc1, doc2])?;

        let touched = index.docs_touched_by_paths(&["crates/tm-core/src/lib.rs".to_string()]);

        assert_eq!(touched.len(), 1);
        assert!(touched.contains("doc1"));

        Ok(())
    }

    #[test]
    fn docs_touched_by_decisions() -> tm_types::Result<()> {
        let doc1 = DocRecord::new(
            "doc1".to_string(),
            "docs/doc1.md".to_string(),
            DocMode::Generated,
            vec!["D-019".to_string()],
        );
        let doc2 = DocRecord::new(
            "doc2".to_string(),
            "docs/doc2.md".to_string(),
            DocMode::Maintained,
            vec!["D-027".to_string()],
        );

        let index = ProvenanceIndex::build(&[doc1, doc2])?;

        let touched = index.docs_touched_by_decisions(&[DecisionId::new("D-019")?]);

        assert_eq!(touched.len(), 1);
        assert!(touched.contains("doc1"));

        Ok(())
    }

    #[test]
    fn docs_touched_by_paths_multiple_matches() -> tm_types::Result<()> {
        let doc1 = DocRecord::new(
            "doc1".to_string(),
            "docs/doc1.md".to_string(),
            DocMode::Generated,
            vec!["crates/**".to_string()],
        );
        let doc2 = DocRecord::new(
            "doc2".to_string(),
            "docs/doc2.md".to_string(),
            DocMode::Maintained,
            vec!["crates/tm-docs/**".to_string()],
        );

        let index = ProvenanceIndex::build(&[doc1, doc2])?;

        let touched = index.docs_touched_by_paths(&["crates/tm-core/src/lib.rs".to_string()]);

        assert_eq!(touched.len(), 1);
        assert!(touched.contains("doc1"));

        Ok(())
    }

    #[test]
    fn changeset_empty() {
        let cs = ChangeSet::empty();
        assert!(cs.changed_paths.is_empty());
        assert!(cs.superseded_decisions.is_empty());
    }

    #[test]
    fn changeset_from_file_records() {
        let records = vec![
            tm_codeintel::FileRecord {
                path: "src/main.rs".to_string(),
                blake3: "0".repeat(64),
                size: 100,
                lang: None,
                mtime: 0,
            },
            tm_codeintel::FileRecord {
                path: "Cargo.toml".to_string(),
                blake3: "1".repeat(64),
                size: 200,
                lang: None,
                mtime: 0,
            },
        ];

        let cs = ChangeSet::from_file_records(&records);

        assert_eq!(cs.changed_paths.len(), 2);
        assert!(cs.changed_paths.contains(&"src/main.rs".to_string()));
        assert!(cs.changed_paths.contains(&"Cargo.toml".to_string()));
        assert!(cs.superseded_decisions.is_empty());
    }

    #[test]
    fn docs_touched_with_changeset() -> tm_types::Result<()> {
        let doc1 = DocRecord::new(
            "doc1".to_string(),
            "docs/doc1.md".to_string(),
            DocMode::Generated,
            vec!["crates/tm-core/**".to_string()],
        );
        let doc2 = DocRecord::new(
            "doc2".to_string(),
            "docs/doc2.md".to_string(),
            DocMode::Maintained,
            vec!["D-019".to_string()],
        );

        let index = ProvenanceIndex::build(&[doc1, doc2])?;

        let mut cs = ChangeSet::empty();
        cs.changed_paths = vec!["crates/tm-core/src/lib.rs".to_string()];
        cs.superseded_decisions = vec![DecisionId::new("D-019")?];

        let touched = index.docs_touched(&cs);

        assert_eq!(touched.len(), 2);
        assert!(touched.contains("doc1"));
        assert!(touched.contains("doc2"));

        Ok(())
    }

    #[test]
    fn docs_touched_with_no_matches() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "doc".to_string(),
            "docs/doc.md".to_string(),
            DocMode::Generated,
            vec!["crates/tm-core/**".to_string()],
        );

        let index = ProvenanceIndex::build(&[doc])?;

        let cs = ChangeSet::empty();

        let touched = index.docs_touched(&cs);

        assert!(touched.is_empty());

        Ok(())
    }

    #[test]
    fn doc_with_wildcard_patterns() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "doc".to_string(),
            "docs/doc.md".to_string(),
            DocMode::Generated,
            vec!["*.md".to_string(), "src/*/main.rs".to_string()],
        );

        let compiled = compile(&doc)?;

        assert!(compiled.paths.matches("README.md"));
        assert!(compiled.paths.matches("CONTRIBUTING.md"));
        assert!(!compiled.paths.matches("docs/README.md")); // * doesn't cross /

        Ok(())
    }

    #[test]
    fn doc_with_double_star_pattern() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "doc".to_string(),
            "docs/doc.md".to_string(),
            DocMode::Generated,
            vec!["docs/**".to_string()],
        );

        let compiled = compile(&doc)?;

        assert!(compiled.paths.matches("docs/file.md"));
        assert!(compiled.paths.matches("docs/nested/dir/file.md"));

        Ok(())
    }

    #[test]
    fn provenance_index_build_with_invalid_pattern() {
        let doc = DocRecord::new(
            "doc".to_string(),
            "docs/doc.md".to_string(),
            DocMode::Generated,
            vec!["[invalid(".to_string()],
        );

        let result = ProvenanceIndex::build(&[doc]);
        assert!(result.is_err());
    }

    #[test]
    fn decision_id_deduplication() -> tm_types::Result<()> {
        let doc = DocRecord::new(
            "doc".to_string(),
            "docs/doc.md".to_string(),
            DocMode::Generated,
            vec![
                "D-019".to_string(),
                "D-019".to_string(), // Duplicate
                "D-027".to_string(),
            ],
        );

        let compiled = compile(&doc)?;

        // BTreeSet automatically deduplicates
        assert_eq!(compiled.decisions.len(), 2);

        Ok(())
    }
}
