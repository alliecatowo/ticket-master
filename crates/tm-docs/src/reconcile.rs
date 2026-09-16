//! Reconciliation (`SPEC.md` §9).
//!
//! This module is the one place the hard rule from `SPEC.md` §9 is enforced as code: **a
//! `Human` doc is never overwritten by the system, ever.** [`apply_regeneration`] is the single
//! function that would rewrite a doc's content on the system's behalf, and it hard-refuses
//! (`Err`, not a silent no-op) for anything that isn't [`DocMode::Generated`]. Every other doc
//! is reconciled the same way regardless of whether it is `Maintained` or `Human`: a review
//! ticket is opened, and only a human closing that ticket with an [`Attestation`] can move the
//! doc back to [`crate::registry::DocState::Fresh`].
//!
//! Ticket creation itself (authority, budget, executor requirements) is `tm-core::Store`'s job,
//! not this crate's — [`open_reconciliation`] produces the [`ReconciliationTicket`] record this
//! crate needs to track (which doc, which kind, which ticket id), given an already-allocated
//! [`tm_types::TicketId`] from the caller's [`tm_types::IdSource`].

use tm_core::TicketKind;
use tm_types::{ArtifactId, Clock, IdSource, ParticipantId, TicketId, Timestamp};

use crate::registry::{DocMode, DocRecord};

/// Why a reconciliation ticket was opened for a doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconciliationKind {
    /// [`DocMode::Generated`]: the system will regenerate the doc's content.
    Regeneration,
    /// [`DocMode::Maintained`] or [`DocMode::Human`]: a human must review and either update the
    /// doc themselves or attest that it is still accurate.
    Review,
}

impl ReconciliationKind {
    /// The [`ReconciliationKind`] appropriate for a doc's [`DocMode`].
    pub fn for_mode(mode: DocMode) -> Self {
        match mode {
            DocMode::Generated => ReconciliationKind::Regeneration,
            DocMode::Maintained | DocMode::Human => ReconciliationKind::Review,
        }
    }
}

/// A ticket opened to reconcile one stale doc, per `SPEC.md` §9's `tm docs reconcile [id]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationTicket {
    /// The doc being reconciled.
    pub doc_id: String,
    /// Whether this is a regeneration or a review.
    pub kind: ReconciliationKind,
    /// The `tm-core` ticket kind the caller should create ([`TicketKind::Work`] for both —
    /// reconciliation is ordinary work, not verification/audit/recovery).
    pub ticket_kind: TicketKind,
    /// The allocated ticket id.
    pub ticket: TicketId,
    /// When the ticket was opened.
    pub opened: Timestamp,
}

/// A human's evidence that a `Maintained`/`Human` doc's review ticket is resolved: either the
/// doc was updated by hand, or the human attests it is still accurate as written. Either way
/// this is itself evidence, per `SPEC.md` §9 ("a human closes with an attestation that is
/// itself evidence") — it is expected to be recorded as an [`tm_core::Evidence`] row against the
/// review ticket, not just applied in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attestation {
    /// The doc this attestation resolves.
    pub doc_id: String,
    /// The review ticket it closes.
    pub ticket: TicketId,
    /// Who attested.
    pub attested_by: ParticipantId,
    /// The human's note (why the doc is considered accurate, or what changed).
    pub note: String,
    /// Supporting evidence artifact (e.g. the diff that updated the doc), if any.
    pub evidence: Option<ArtifactId>,
    /// When the attestation was recorded.
    pub ts: Timestamp,
}

/// Why a reconciliation operation was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReconcileError {
    /// [`apply_regeneration`] was called on a doc that is not [`DocMode::Generated`]. The hard
    /// rule: the system may never rewrite a `Maintained` or `Human` doc's content, regardless of
    /// staleness or caller intent.
    #[error("doc {doc_id} is {mode:?}; the system may not rewrite its content, only open a review ticket")]
    NotSystemWritable {
        /// The doc that was refused.
        doc_id: String,
        /// Its actual mode.
        mode: DocMode,
    },
    /// An [`Attestation`] named a ticket that does not match the doc's open reconciliation
    /// ticket.
    #[error("attestation for doc {doc_id} names ticket {given}, but its open reconciliation ticket is {expected}")]
    TicketMismatch {
        /// The doc the attestation was for.
        doc_id: String,
        /// The ticket id the attestation named.
        given: TicketId,
        /// The ticket id actually open for this doc.
        expected: TicketId,
    },
}

impl From<ReconcileError> for tm_types::TmError {
    /// Every [`ReconcileError`] is a refusal to act, not a missing-object or storage failure —
    /// it always maps to [`tm_types::TmError::AuthorityDenied`] so `tm docs reconcile` and any
    /// caller attempting to bypass a `Human` doc's protection get a consistent, hard error.
    fn from(err: ReconcileError) -> Self {
        tm_types::TmError::AuthorityDenied(err.to_string())
    }
}

/// Open a reconciliation ticket for a stale (or explicitly reconciled-on-demand) doc, and move
/// it to [`crate::registry::DocState::Reconciling`].
// IMPL: `let kind = ReconciliationKind::for_mode(doc.mode);`, allocate the ticket id via
// `TicketId::new(ids.next(tm_types::IdKind::Ticket).as_str())?` (`IdSource::next` returns the
// untyped `Id`; `TicketId::new` validates and wraps it), `doc.state =
// DocState::Reconciling`. Building the actual `tm_core::Ticket` (objective text templated from
// `doc.id`/`kind`, authority scoped to `doc.path`, executor requirements) is the caller's job
// once it has this record — this function only produces the bookkeeping `tm-docs` itself needs.
pub fn open_reconciliation(
    doc: &mut DocRecord,
    ids: &dyn IdSource,
    clock: &dyn Clock,
) -> tm_types::Result<ReconciliationTicket> {
    todo!("allocate a ticket id, move the doc to Reconciling, see IMPL note above")
}

/// Accept a human attestation and return a `Maintained`/`Human` doc to [`crate::registry::DocState::Fresh`].
///
/// Valid for any [`DocMode`] (a `Generated` doc's regeneration ticket can also be closed this
/// way if a human did the regeneration by hand), but this is the *only* legal path back to
/// `Fresh` for `Maintained` and `Human` docs — there is no code path in this crate that sets
/// `Fresh` on such a doc without one.
// IMPL: `Err(ReconcileError::TicketMismatch { .. })` (via `?`/`.into()`) unless `doc.state ==
// DocState::Reconciling` and the caller can show `attestation.ticket` is the ticket that was
// opened for this doc (this function takes the doc alone, so the caller — which does have the
// `ReconciliationTicket` — is expected to have already checked `attestation.ticket ==
// open_ticket.ticket` before calling; this function's own job is just applying the transition).
// On success: `doc.state = DocState::Fresh; doc.last_verified = Some(attestation.ts);`.
pub fn accept_attestation(doc: &mut DocRecord, attestation: &Attestation) -> tm_types::Result<()> {
    todo!("validate and apply the attestation, returning the doc to Fresh, see IMPL note above")
}

/// Regenerate a `Generated` doc's content and return it to [`crate::registry::DocState::Fresh`]. The only
/// function in this crate that may move a doc to `Fresh` without a human [`Attestation`] — and
/// therefore the one hard-gated to [`DocMode::Generated`] only.
///
/// `content_hash` is a caller-supplied fingerprint (e.g. blake3 of the regenerated file) recorded
/// so a later `tm docs check` run can tell the regeneration actually landed on disk; this
/// function does not write the file itself.
// IMPL: `if doc.mode != DocMode::Generated { return Err(ReconcileError::NotSystemWritable { doc_id:
// doc.id.clone(), mode: doc.mode }.into()); }` — this check is the hard rule and must run first,
// unconditionally, before any state mutation. On success: `doc.state = DocState::Fresh;
// doc.last_verified = Some(ts);`. `content_hash` is accepted for the caller's/future
// implementation's bookkeeping even though this stub does not yet persist it anywhere.
pub fn apply_regeneration(
    doc: &mut DocRecord,
    content_hash: &str,
    ts: Timestamp,
) -> tm_types::Result<()> {
    todo!("hard-refuse non-Generated docs, else mark Fresh, see IMPL note above")
}
