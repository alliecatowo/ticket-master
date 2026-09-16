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
// IMPL: for each entry, try `DecisionId::from_str`; on success it's a decision reference, on
// failure treat it as a path glob (this covers bare config-file names like `providers.toml`
// too — they are just patterns with no wildcard). Feed the path entries to
// `PatternSet::parse`, which returns `tm_types::Result`; propagate its error with `?`. Dedup
// decisions via the `BTreeSet`; path pattern dedup is `PatternSet::parse`'s job, not ours.
pub fn compile(doc: &DocRecord) -> tm_types::Result<CompiledProvenance> {
    todo!("split derived_from into path globs vs decision ids and compile, see IMPL note above")
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
    // IMPL: `docs.iter().map(compile).collect::<tm_types::Result<Vec<_>>>()?`, wrap in
    // `ProvenanceIndex { entries }`.
    pub fn build(docs: &[DocRecord]) -> tm_types::Result<Self> {
        todo!("compile every doc's provenance, see IMPL note above")
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
