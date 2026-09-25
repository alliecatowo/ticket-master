//! The staleness state machine (`SPEC.md` §9).
//!
//! [`Assessor::assess`] is what runs after every commit/index update: it resolves a
//! [`ChangeSet`] to the doc ids it touches ([`ProvenanceIndex::docs_touched`]) and moves each
//! touched doc from `Fresh`/`Unverified` to `Stale`, unless an explicit [`Dismissal`] covers it.
//! [`Assessor`] owns the [`DocRegistry`] across calls precisely so it can answer "is this doc
//! already `Stale`" — a doc that is already `Stale` is left alone rather than re-invalidated, so
//! repeated assessment never produces a second `doc.invalidated` for the same standing flag.
//! [`Assessor::check`] is the pure predicate behind `tm docs check`'s exit code.

use std::collections::BTreeMap;

use tm_types::{ParticipantId, Timestamp};

use crate::provenance::{ChangeSet, ProvenanceIndex};
use crate::registry::{DocRegistry, DocState};

/// A human's explicit dismissal of a staleness flag on one doc: "I looked, this is fine as is".
///
/// Recorded so [`Assessor::assess`] does not immediately re-flag the same doc for the same
/// already-reviewed basis change. A dismissal is evidence, like an [`crate::reconcile::Attestation`],
/// but weaker: it does not move the doc back to `Fresh` (nothing was actually reconciled), only
/// to [`DocState::Unverified`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dismissal {
    /// The doc this dismissal applies to.
    pub doc_id: String,
    /// Why the human judged the flag not worth acting on.
    pub note: String,
    /// Who dismissed it.
    pub dismissed_by: ParticipantId,
    /// When.
    pub ts: Timestamp,
}

/// One doc's outcome from a single [`Assessor::assess`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocAssessment {
    /// The doc assessed.
    pub doc_id: String,
    /// State before this assessment.
    pub previous_state: DocState,
    /// State after this assessment.
    pub new_state: DocState,
    /// Human-readable reasons this doc was touched (matched path(s), superseded decision(s)).
    /// Empty when the doc was not touched by this [`ChangeSet`] at all.
    pub reasons: Vec<String>,
    /// True when a live [`Dismissal`] suppressed what would otherwise have been a `Stale`
    /// transition.
    pub dismissed: bool,
}

/// Owns the doc registry, its compiled provenance, and outstanding dismissals across repeated
/// assessment calls. The registry's mutated state (never re-invalidating an already-`Stale` doc)
/// is what makes `assess` idempotent under repeated identical input.
pub struct Assessor {
    registry: DocRegistry,
    provenance: ProvenanceIndex,
    dismissals: BTreeMap<String, Dismissal>,
}

impl Assessor {
    /// Build an assessor over an already-populated registry and its compiled provenance
    /// (typically [`ProvenanceIndex::build`] over `registry.list()`).
    pub fn new(registry: DocRegistry, provenance: ProvenanceIndex) -> Self {
        Assessor {
            registry,
            provenance,
            dismissals: BTreeMap::new(),
        }
    }

    /// Read-only access to the current registry (e.g. for `tm docs list`).
    pub fn registry(&self) -> &DocRegistry {
        &self.registry
    }

    /// Mutable access, for callers (`tm-core`-backed persistence, [`crate::reconcile`]) that
    /// need to apply state transitions this module does not itself own.
    pub fn registry_mut(&mut self) -> &mut DocRegistry {
        &mut self.registry
    }

    /// Record an explicit dismissal for a doc, replacing any prior live dismissal for it.
    pub fn dismiss(&mut self, dismissal: Dismissal) {
        self.dismissals.insert(dismissal.doc_id.clone(), dismissal);
    }

    /// Remove a doc's live dismissal, if any (e.g. because its basis changed again in a way the
    /// dismissal note did not anticipate). Returns the removed dismissal.
    pub fn clear_dismissal(&mut self, doc_id: &str) -> Option<Dismissal> {
        self.dismissals.remove(doc_id)
    }

    /// Run the staleness state machine over `changes`, mutating the owned registry in place and
    /// returning one [`DocAssessment`] per doc touched by `changes`.
    ///
    /// Invariants this must hold:
    /// - A doc not touched by `changes` (per [`ProvenanceIndex::docs_touched`]) does not appear
    ///   in the result at all.
    /// - A touched doc already in [`DocState::Stale`] is left `Stale` (no-op transition, still
    ///   reported with `previous_state == new_state == Stale`) — this is the "must not thrash"
    ///   rule: no second `doc.invalidated` for a flag that is already raised. A touched doc in
    ///   [`DocState::Reconciling`] is left `Reconciling` for the same reason: a caller that diffs
    ///   an uncommitted edit against `HEAD` on every invocation (a real `ChangeSet` source that
    ///   has no "already assessed this exact change" memory of its own) must not re-flag a doc
    ///   `Stale` — opening a second regeneration/review ticket — just because the edit that opened
    ///   its still-open reconciliation ticket hasn't been committed yet.
    /// - A touched doc with a live [`Dismissal`] (recorded via [`Assessor::dismiss`]) transitions
    ///   to [`DocState::Unverified`] instead of `Stale`, is reported with `dismissed: true`, and
    ///   the dismissal is then consumed (removed from `self.dismissals`) so it does not silently
    ///   suppress a *future*, unrelated staleness flag.
    /// - Otherwise the doc transitions `Fresh`/`Unverified` -> `Stale`.
    pub fn assess(&mut self, changes: &ChangeSet) -> Vec<DocAssessment> {
        let touched = self.provenance.docs_touched(changes);
        let mut assessments = Vec::with_capacity(touched.len());

        for doc_id in touched {
            // The provenance index and registry are built from the same doc list; a doc id the
            // registry no longer has is a build/registry desync, not something `assess` should
            // panic over, so it is skipped rather than unwrapped.
            let Some(record) = self.registry.get(&doc_id) else {
                debug_assert!(
                    false,
                    "provenance index references doc {doc_id} missing from registry"
                );
                continue;
            };

            // Recompile this doc's provenance to recover which specific paths/decisions matched,
            // for the human-readable reason strings; `ProvenanceIndex` only exposes the touched
            // doc id set, not the per-doc match detail.
            let reasons = match crate::provenance::compile(record) {
                Ok(compiled) => {
                    let mut reasons: Vec<String> = changes
                        .changed_paths
                        .iter()
                        .filter(|p| compiled.paths.matches(p))
                        .map(|p| format!("path {p}"))
                        .collect();
                    reasons.extend(
                        changes
                            .superseded_decisions
                            .iter()
                            .filter(|d| compiled.decisions.contains(d))
                            .map(|d| format!("decision {d}")),
                    );
                    reasons
                }
                Err(_) => Vec::new(),
            };

            let previous_state = record.state;
            let live_dismissal = self.dismissals.remove(&doc_id);
            let dismissed = live_dismissal.is_some();

            let new_state =
                if previous_state == DocState::Stale || previous_state == DocState::Reconciling {
                    previous_state
                } else if dismissed {
                    DocState::Unverified
                } else {
                    DocState::Stale
                };

            if let Some(record) = self.registry.get_mut(&doc_id) {
                record.state = new_state;
            }

            assessments.push(DocAssessment {
                doc_id,
                previous_state,
                new_state,
                reasons,
                dismissed,
            });
        }

        assessments
    }

    /// Build the `doc.invalidated` payloads for a batch of [`DocAssessment`]s: one per doc that
    /// actually transitioned *into* `Stale` this call (i.e. `previous_state != Stale &&
    /// new_state == Stale`) — a no-op `Stale -> Stale` thrash-guard result does not get a second
    /// event, and a dismissed result (`Unverified`) does not get one either.
    pub fn invalidation_events(
        assessments: &[DocAssessment],
        registry: &DocRegistry,
    ) -> Vec<tm_events::payload::DocInvalidatedPayload> {
        assessments
            .iter()
            .filter(|a| a.previous_state != DocState::Stale && a.new_state == DocState::Stale)
            .filter_map(|a| {
                registry
                    .get(&a.doc_id)
                    .map(|record| tm_events::payload::DocInvalidatedPayload {
                        path: record.path.clone(),
                        reason: a.reasons.join("; "),
                    })
            })
            .collect()
    }

    /// The predicate behind `tm docs check`: `Ok(())` when no doc is `Stale`, otherwise
    /// `Err(TmError::invariant(..))` naming every stale doc id. The CLI turns this `Result` into
    /// the process exit code via `TmError::exit_code()`.
    pub fn check(&self) -> tm_types::Result<()> {
        // `DocRegistry::list` is already sorted by id (it iterates a `BTreeMap`).
        let stale_ids: Vec<&str> = self
            .registry
            .list()
            .into_iter()
            .filter(|r| r.state == DocState::Stale)
            .map(|r| r.id.as_str())
            .collect();

        if stale_ids.is_empty() {
            Ok(())
        } else {
            Err(tm_types::TmError::invariant(format!(
                "stale docs: {}",
                stale_ids.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{DocMode, DocRecord};
    use std::str::FromStr;
    use tm_types::{DecisionId, ParticipantId, Timestamp};

    fn ts() -> Timestamp {
        Timestamp::parse_rfc3339("2026-01-01T00:00:00Z").expect("valid rfc3339 fixture")
    }

    fn doc(id: &str, mode: DocMode, derived_from: &[&str]) -> DocRecord {
        DocRecord::new(
            id.to_string(),
            format!("docs/{id}.md"),
            mode,
            derived_from.iter().map(|s| s.to_string()).collect(),
        )
    }

    fn assessor(docs: Vec<DocRecord>) -> Assessor {
        let mut registry = DocRegistry::new();
        for d in docs {
            registry.insert(d);
        }
        let provenance =
            ProvenanceIndex::build(&registry.list().into_iter().cloned().collect::<Vec<_>>())
                .expect("fixture docs compile");
        Assessor::new(registry, provenance)
    }

    #[test]
    fn touched_doc_goes_fresh_to_stale_with_reasons() {
        let mut a = assessor(vec![doc(
            "arch",
            DocMode::Maintained,
            &["crates/tm-core/**"],
        )]);
        let changes = ChangeSet {
            changed_paths: vec!["crates/tm-core/src/lib.rs".to_string()],
            superseded_decisions: vec![],
        };
        let results = a.assess(&changes);
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.doc_id, "arch");
        assert_eq!(r.previous_state, DocState::Unverified);
        assert_eq!(r.new_state, DocState::Stale);
        assert!(!r.dismissed);
        assert_eq!(
            r.reasons,
            vec!["path crates/tm-core/src/lib.rs".to_string()]
        );
    }

    #[test]
    fn untouched_doc_does_not_appear_in_results() {
        let mut a = assessor(vec![doc(
            "arch",
            DocMode::Maintained,
            &["crates/tm-core/**"],
        )]);
        let changes = ChangeSet {
            changed_paths: vec!["crates/tm-scheduler/src/lib.rs".to_string()],
            superseded_decisions: vec![],
        };
        assert!(a.assess(&changes).is_empty());
    }

    #[test]
    fn already_stale_doc_is_not_reinvalidated() {
        let mut a = assessor(vec![doc(
            "arch",
            DocMode::Maintained,
            &["crates/tm-core/**"],
        )]);
        let changes = ChangeSet {
            changed_paths: vec!["crates/tm-core/src/lib.rs".to_string()],
            superseded_decisions: vec![],
        };
        let first = a.assess(&changes);
        assert_eq!(first[0].new_state, DocState::Stale);

        let second = a.assess(&changes);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].previous_state, DocState::Stale);
        assert_eq!(second[0].new_state, DocState::Stale);

        // No new `doc.invalidated` for the no-op thrash: `check` should still fail, but
        // `invalidation_events` over the second batch must be empty.
        let events = Assessor::invalidation_events(&second, a.registry());
        assert!(events.is_empty());
    }

    #[test]
    fn reconciling_doc_is_left_reconciling_not_reflagged_stale() {
        // A caller like `ops.rs::git_changeset` diffs an uncommitted edit against `HEAD` on
        // every invocation, with no memory of "already assessed this exact change" of its own.
        // A doc already `Reconciling` (a regeneration/review ticket is already open against it)
        // must not be pushed back to `Stale` -- and must not get a second `doc.invalidated` --
        // just because the same still-uncommitted edit is seen again.
        let mut a = assessor(vec![doc(
            "arch",
            DocMode::Maintained,
            &["crates/tm-core/**"],
        )]);
        a.registry_mut().get_mut("arch").unwrap().state = DocState::Reconciling;

        let changes = ChangeSet {
            changed_paths: vec!["crates/tm-core/src/lib.rs".to_string()],
            superseded_decisions: vec![],
        };
        let assessments = a.assess(&changes);
        assert_eq!(assessments.len(), 1);
        assert_eq!(assessments[0].previous_state, DocState::Reconciling);
        assert_eq!(assessments[0].new_state, DocState::Reconciling);
        assert_eq!(
            a.registry().get("arch").unwrap().state,
            DocState::Reconciling
        );

        let events = Assessor::invalidation_events(&assessments, a.registry());
        assert!(
            events.is_empty(),
            "a touched Reconciling doc must not produce a doc.invalidated"
        );
    }

    #[test]
    fn live_dismissal_suppresses_stale_and_is_consumed() {
        let mut a = assessor(vec![doc(
            "arch",
            DocMode::Maintained,
            &["crates/tm-core/**"],
        )]);
        a.dismiss(Dismissal {
            doc_id: "arch".to_string(),
            note: "checked, still accurate".to_string(),
            dismissed_by: ParticipantId::new("human:allie").expect("valid participant id fixture"),
            ts: ts(),
        });

        let changes = ChangeSet {
            changed_paths: vec!["crates/tm-core/src/lib.rs".to_string()],
            superseded_decisions: vec![],
        };
        let first = a.assess(&changes);
        assert_eq!(first[0].new_state, DocState::Unverified);
        assert!(first[0].dismissed);

        // The dismissal was consumed: a second, unrelated touch is not silently suppressed.
        let second = a.assess(&changes);
        assert_eq!(second[0].new_state, DocState::Stale);
        assert!(!second[0].dismissed);
    }

    #[test]
    fn decision_supersession_touches_doc_with_decision_reason() {
        let d = DecisionId::from_str("D-019").expect("valid decision id fixture");
        let mut a = assessor(vec![doc("policy", DocMode::Human, &["D-019"])]);
        let changes = ChangeSet {
            changed_paths: vec![],
            superseded_decisions: vec![d],
        };
        let results = a.assess(&changes);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].new_state, DocState::Stale);
        assert_eq!(results[0].reasons, vec!["decision D-019".to_string()]);
    }

    #[test]
    fn invalidation_events_skip_dismissed_and_thrash_results() {
        let mut a = assessor(vec![
            doc("arch", DocMode::Maintained, &["crates/tm-core/**"]),
            doc("api", DocMode::Generated, &["crates/tm-api/**"]),
        ]);
        a.dismiss(Dismissal {
            doc_id: "arch".to_string(),
            note: "fine".to_string(),
            dismissed_by: ParticipantId::new("human:allie").expect("valid participant id fixture"),
            ts: ts(),
        });
        let changes = ChangeSet {
            changed_paths: vec![
                "crates/tm-core/src/lib.rs".to_string(),
                "crates/tm-api/src/lib.rs".to_string(),
            ],
            superseded_decisions: vec![],
        };
        let results = a.assess(&changes);
        let events = Assessor::invalidation_events(&results, a.registry());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].path, "docs/api.md");
    }

    #[test]
    fn check_passes_when_no_doc_is_stale() {
        let a = assessor(vec![doc(
            "arch",
            DocMode::Maintained,
            &["crates/tm-core/**"],
        )]);
        assert!(a.check().is_ok());
    }

    #[test]
    fn check_fails_naming_every_stale_doc_id() {
        let mut a = assessor(vec![
            doc("arch", DocMode::Maintained, &["crates/tm-core/**"]),
            doc("api", DocMode::Generated, &["crates/tm-api/**"]),
        ]);
        let changes = ChangeSet {
            changed_paths: vec![
                "crates/tm-core/src/lib.rs".to_string(),
                "crates/tm-api/src/lib.rs".to_string(),
            ],
            superseded_decisions: vec![],
        };
        a.assess(&changes);
        let err = a.check().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api"));
        assert!(msg.contains("arch"));
    }

    #[test]
    fn human_docs_still_flow_through_assess_like_any_other_mode() {
        // The hard rule that a `Human` doc's *content* is never rewritten lives in
        // `crate::reconcile`; `assess` itself only tracks staleness state and must treat every
        // `DocMode` identically when deciding Fresh/Stale.
        let mut a = assessor(vec![doc("vision", DocMode::Human, &["crates/tm-core/**"])]);
        let changes = ChangeSet {
            changed_paths: vec!["crates/tm-core/src/lib.rs".to_string()],
            superseded_decisions: vec![],
        };
        let results = a.assess(&changes);
        assert_eq!(results[0].new_state, DocState::Stale);
    }
}
