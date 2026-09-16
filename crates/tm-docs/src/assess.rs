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
    ///   rule: no second `doc.invalidated` for a flag that is already raised.
    /// - A touched doc with a live [`Dismissal`] (recorded via [`Assessor::dismiss`]) transitions
    ///   to [`DocState::Unverified`] instead of `Stale`, is reported with `dismissed: true`, and
    ///   the dismissal is then consumed (removed from `self.dismissals`) so it does not silently
    ///   suppress a *future*, unrelated staleness flag.
    /// - Otherwise the doc transitions `Fresh`/`Unverified` -> `Stale`.
    // IMPL: `let touched = self.provenance.docs_touched(changes);` then for each `doc_id` in
    // `touched` (iterate in sorted order — `BTreeSet` already gives that — for determinism):
    // look up the record via `self.registry.get(doc_id)` (skip silently, or debug-assert, if the
    // provenance index references a doc the registry no longer has — that is a build/registry
    // desync, not something `assess` should panic over); compute `reasons` by re-checking which
    // `changed_paths`/`superseded_decisions` this doc's `CompiledProvenance` actually matched
    // (needed for the human-readable reason strings; `ProvenanceIndex` does not expose this
    // directly today, so either add a per-doc lookup here or thread it through
    // `docs_touched`/`CompiledProvenance` — implementer's call); apply the transition rules
    // above via `self.registry.get_mut(doc_id)`; push the `DocAssessment`.
    pub fn assess(&mut self, changes: &ChangeSet) -> Vec<DocAssessment> {
        todo!("resolve touched docs and apply the staleness transition rules, see IMPL note above")
    }

    /// Build the `doc.invalidated` payloads for a batch of [`DocAssessment`]s: one per doc that
    /// actually transitioned *into* `Stale` this call (i.e. `previous_state != Stale &&
    /// new_state == Stale`) — a no-op `Stale -> Stale` thrash-guard result does not get a second
    /// event, and a dismissed result (`Unverified`) does not get one either.
    // IMPL: filter `assessments` for `previous_state != DocState::Stale && new_state ==
    // DocState::Stale`, map to `tm_events::payload::DocInvalidatedPayload { path: <doc's path>,
    // reason: assessment.reasons.join("; ") }`. This needs the doc's `path`, not just its id —
    // take a `&DocRegistry` (post-assessment state is fine, path never changes) as a second
    // argument to resolve it.
    pub fn invalidation_events(
        assessments: &[DocAssessment],
        registry: &DocRegistry,
    ) -> Vec<tm_events::payload::DocInvalidatedPayload> {
        todo!("emit doc.invalidated for docs that newly became Stale, see IMPL note above")
    }

    /// The predicate behind `tm docs check`: `Ok(())` when no doc is `Stale`, otherwise
    /// `Err(TmError::invariant(..))` naming every stale doc id. The CLI turns this `Result` into
    /// the process exit code via `TmError::exit_code()`.
    // IMPL: collect `self.registry.list()` filtered to `DocState::Stale`, sorted by id; if
    // empty, `Ok(())`; else `Err(TmError::invariant(format!("stale docs: {ids}")))` with `ids`
    // being a comma-joined list.
    pub fn check(&self) -> tm_types::Result<()> {
        todo!("fail with every stale doc id named, see IMPL note above")
    }
}
