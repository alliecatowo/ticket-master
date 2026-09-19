//! Idempotent effect receipts across resume (`SPEC.md` §21.5).
//!
//! Crash recovery restores execution; it does not undo effects. Every external effect an
//! executor can perform — a command, a push, a mirror write, a browser form submission — carries
//! an idempotency key derived from `(ticket, attempt, effect)` and records a receipt as an
//! artifact. On resume, an effect whose receipt already exists is not repeated.
//!
//! [`EffectKey::compute`] is the deterministic key; [`crate::store::Store::begin_effect`] is
//! journal-first (writes a `journaled` row before the caller does anything, so a crash between
//! "the external write happened" and "the receipt was recorded" always leaves a trace);
//! [`EffectGuard::complete`] marks the row `completed` with a receipt. A caller that finds
//! [`EffectGuard::already_completed`] true must not repeat the effect — it hands back the prior
//! receipt instead. A caller that finds [`EffectGuard::resumed`] true (a `journaled`/`failed` row
//! already existed, but not `completed`) is in the one genuinely ambiguous window: the effect may
//! or may not have actually happened externally before the crash. Each effect *kind* answers that
//! with its own `confirm()`-shaped recovery probe (e.g. `tm_mirror::Tracker::confirm`) rather than
//! one mechanism guessing on every kind's behalf — some effects (a GitHub issue push, a git push)
//! can be checked against the external system's own state; others (an arbitrary shell command)
//! cannot be checked at all and the documented, accepted behavior is to re-run.
//!
//! This one mechanism replaces three previously ad hoc, adapter-private idempotency schemes
//! (`docs/audit-2026-09-18-fable.md` A-05/B-11):
//! * `tm-mirror::github`'s in-memory `Mutex<BTreeMap<TicketId, u64>>` — lost entirely on process
//!   restart (not just on crash — every fresh `tm mirror push` invocation, restart or not, used to
//!   re-create rather than update).
//! * `tm-mirror::linear`'s `tm-id:<ticket>` label search — durable (lives in Linear itself) but
//!   adapter-private, not a mechanism any other crate could reuse.
//! * `tm-context::fingerprint`'s content-hash cache for `command::run` — keyed by *what ran*, not
//!   *(ticket, attempt, effect)*, so it answers "have I seen this exact command before" rather
//!   than "did this specific attempt's effect already happen" (cache-shaped, not receipt-shaped).

use tm_types::{ParticipantId, TicketId, Timestamp, TmError};

/// A deterministic idempotency key for one effect attempt: `blake3(ticket ‖ attempt ‖ kind ‖
/// canonical_args)`, hex-encoded. The identical effect attempted twice — same ticket, same
/// attempt, same effect kind, same canonicalized arguments — always produces the same key; that
/// equality is the entire idempotency guarantee, checked by
/// [`crate::store::Store::begin_effect`] before anything else happens.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EffectKey(String);

impl EffectKey {
    /// Compute the key for one effect attempt.
    ///
    /// `kind` is a short, stable, dotted tag naming the *kind* of effect (e.g.
    /// `"mirror.push:github"`, `"git.push"`, `"browser.form_submit"`) — not the
    /// [`tm_events::EventKind`] catalogue; effects are not events (see this module's own
    /// `effect.journaled`/`effect.completed`/`effect.failed` events, which record the receipt
    /// journal's own history, distinct from whatever event kind the effect itself might also
    /// produce). `canonical_args` must already be a stable, deterministic rendering of whatever
    /// distinguishes this attempt from a different one of the same kind (a content hash of the
    /// payload being pushed, a command's own cache key, ...) — this function does no
    /// canonicalization of its own and trusts the caller's.
    ///
    /// IMPL: build a canonical byte string — `ticket.as_str()`, then `attempt` (decimal), then
    /// `kind`, then `canonical_args`, each section separated by `"\u{1e}"` (ASCII record
    /// separator, the same delimiter `tm-context::fingerprint::cache_key` uses) so no field
    /// boundary is ambiguous — and hash it with blake3, returning `to_hex().to_string()`.
    /// Deterministic and total: no error cases.
    pub fn compute(ticket: &TicketId, attempt: u32, kind: &str, canonical_args: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(ticket.as_str().as_bytes());
        hasher.update(b"\x1e");
        hasher.update(attempt.to_string().as_bytes());
        hasher.update(b"\x1e");
        hasher.update(kind.as_bytes());
        hasher.update(b"\x1e");
        hasher.update(canonical_args.as_bytes());
        EffectKey(hasher.finalize().to_hex().to_string())
    }

    /// The hex-encoded key, as stored in `effects.key` and carried on `effect.journaled`'s
    /// payload.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Rebuild a key from its stored hex string (e.g. reading `effects.key` back out of SQLite).
    /// No validation beyond what [`crate::store::Store::effect_status`]'s lookup itself performs
    /// (a key that was never journaled simply matches no row).
    pub fn from_hex(hex: impl Into<String>) -> Self {
        EffectKey(hex.into())
    }
}

impl std::fmt::Display for EffectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One effect's lifecycle status, matching `effects.status` (`crate::schema`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectStatus {
    /// Journaled before the effect ran; not yet known to have completed. A row left in this
    /// state (no matching `effect.completed`/`effect.failed`) is the "effect ran, receipt lost"
    /// crash window `SPEC.md` §21.5 names.
    Journaled,
    /// The effect ran and its receipt was recorded.
    Completed,
    /// The effect was attempted and is known (not just presumed) to have failed. Not a terminal
    /// idempotency state the way `Completed` is: a later [`crate::store::Store::begin_effect`]
    /// call for the same key still reports [`EffectGuard::resumed`] and may retry.
    Failed,
}

impl EffectStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EffectStatus::Journaled => "journaled",
            EffectStatus::Completed => "completed",
            EffectStatus::Failed => "failed",
        }
    }

    pub(crate) fn parse(s: &str) -> tm_types::Result<Self> {
        match s {
            "journaled" => Ok(EffectStatus::Journaled),
            "completed" => Ok(EffectStatus::Completed),
            "failed" => Ok(EffectStatus::Failed),
            other => Err(TmError::storage(format!(
                "corrupt effects.status: {other:?}"
            ))),
        }
    }
}

/// A materialized `effects` row, as read back by [`crate::store::Store::effect_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    /// The idempotency key this row is keyed on.
    pub key: EffectKey,
    /// The ticket this effect was performed on behalf of.
    pub ticket: TicketId,
    /// The ticket's attempt count at the time this effect was journaled.
    pub attempt: u32,
    /// The effect kind tag (see [`EffectKey::compute`]'s `kind` parameter).
    pub kind: String,
    /// This effect's current lifecycle status.
    pub status: EffectStatus,
    /// The receipt recorded on completion, if any (an artifact id, external reference, or other
    /// durable pointer to what the effect produced).
    pub receipt_artifact: Option<String>,
    /// When this effect was journaled.
    pub started: Timestamp,
    /// When this effect reached `completed`/`failed`, if it has.
    pub completed: Option<Timestamp>,
}

/// A handle returned by [`crate::store::Store::begin_effect`].
///
/// Three outcomes, distinguished by [`EffectGuard::already_completed`]/[`EffectGuard::resumed`]:
/// * Neither set: no row existed for this key before this call: a fresh `journaled` row was just
///   written. The caller should perform the effect, then call [`EffectGuard::complete`].
/// * [`EffectGuard::already_completed`]: a `completed` row already existed. The caller must not
///   repeat the effect; [`EffectGuard::prior_receipt`] carries what was recorded the first time.
/// * [`EffectGuard::resumed`] (and not completed): a `journaled`/`failed` row already existed —
///   an earlier attempt began this exact effect and never reached `completed`. The caller should
///   try its effect kind's `confirm()`-shaped recovery probe before deciding whether to re-run,
///   to tell "the effect ran, the receipt was lost" apart from "the effect never ran".
#[derive(Debug, Clone)]
pub struct EffectGuard {
    key: EffectKey,
    ticket: TicketId,
    attempt: u32,
    kind: String,
    actor: ParticipantId,
    already_completed: bool,
    resumed: bool,
    prior_receipt: Option<String>,
}

impl EffectGuard {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        key: EffectKey,
        ticket: TicketId,
        attempt: u32,
        kind: String,
        actor: ParticipantId,
        already_completed: bool,
        resumed: bool,
        prior_receipt: Option<String>,
    ) -> Self {
        EffectGuard {
            key,
            ticket,
            attempt,
            kind,
            actor,
            already_completed,
            resumed,
            prior_receipt,
        }
    }

    /// The idempotency key this guard was opened for.
    pub fn key(&self) -> &EffectKey {
        &self.key
    }

    /// The ticket this effect belongs to.
    pub fn ticket(&self) -> &TicketId {
        &self.ticket
    }

    /// True if a `completed` row already existed for this exact key before this call — the
    /// effect must not be repeated. [`EffectGuard::prior_receipt`] carries whatever receipt was
    /// recorded the first time.
    pub fn already_completed(&self) -> bool {
        self.already_completed
    }

    /// True if a `journaled`/`failed` row already existed for this exact key before this call —
    /// i.e. this is not the first attempt to begin this effect. A caller whose effect kind has a
    /// `confirm()`-shaped recovery probe should call it before re-running, to detect "the effect
    /// ran, the receipt was lost" (a crash between the external write and `complete`) rather than
    /// blindly repeating a possibly non-idempotent external action. Always true when
    /// [`EffectGuard::already_completed`] is true.
    pub fn resumed(&self) -> bool {
        self.resumed
    }

    /// The receipt recorded the first time, when [`EffectGuard::already_completed`] is true.
    pub fn prior_receipt(&self) -> Option<&str> {
        self.prior_receipt.as_deref()
    }

    /// Mark this effect completed, recording `receipt_artifact` (an artifact id, external
    /// reference, or other durable pointer to what the effect produced). A no-op if
    /// [`EffectGuard::already_completed`] was already true when this guard was opened (idempotent
    /// to call on a guard that turned out to need no work).
    pub fn complete(
        &self,
        store: &crate::store::Store,
        receipt_artifact: Option<&str>,
    ) -> tm_types::Result<()> {
        if self.already_completed {
            return Ok(());
        }
        store.complete_effect(
            &self.key,
            &self.ticket,
            receipt_artifact,
            self.actor.clone(),
        )
    }

    /// Mark this effect failed (e.g. the external call returned a non-retryable error). Not a
    /// terminal idempotency state the way `complete` is — a later `begin_effect` call for the
    /// same key reports [`EffectGuard::resumed`] and may retry. A no-op if
    /// [`EffectGuard::already_completed`] was already true.
    pub fn fail(&self, store: &crate::store::Store, reason: &str) -> tm_types::Result<()> {
        if self.already_completed {
            return Ok(());
        }
        store.fail_effect(&self.key, &self.ticket, reason, self.actor.clone())
    }

    /// Same as [`EffectKey::compute`], the attempt/kind this guard was opened with — kept
    /// alongside the key so a caller that only holds the guard (not the original inputs) can
    /// still log/report what effect this was.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The attempt this guard was opened with.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(s: &str) -> TicketId {
        s.parse().expect("valid ticket id")
    }

    #[test]
    fn compute_is_deterministic() {
        let a = EffectKey::compute(&ticket("T-1"), 1, "git.push", "abc");
        let b = EffectKey::compute(&ticket("T-1"), 1, "git.push", "abc");
        assert_eq!(a, b);
    }

    #[test]
    fn compute_differs_on_ticket() {
        let a = EffectKey::compute(&ticket("T-1"), 1, "git.push", "abc");
        let b = EffectKey::compute(&ticket("T-2"), 1, "git.push", "abc");
        assert_ne!(a, b);
    }

    #[test]
    fn compute_differs_on_attempt() {
        let a = EffectKey::compute(&ticket("T-1"), 1, "git.push", "abc");
        let b = EffectKey::compute(&ticket("T-1"), 2, "git.push", "abc");
        assert_ne!(a, b);
    }

    #[test]
    fn compute_differs_on_kind() {
        let a = EffectKey::compute(&ticket("T-1"), 1, "git.push", "abc");
        let b = EffectKey::compute(&ticket("T-1"), 1, "git.commit", "abc");
        assert_ne!(a, b);
    }

    #[test]
    fn compute_differs_on_canonical_args() {
        let a = EffectKey::compute(&ticket("T-1"), 1, "git.push", "abc");
        let b = EffectKey::compute(&ticket("T-1"), 1, "git.push", "xyz");
        assert_ne!(a, b);
    }

    #[test]
    fn compute_has_no_field_boundary_ambiguity() {
        // "ab" + "c" as canonical_args vs "a" + "bc" as kind must not collide even though the
        // concatenated bytes without a delimiter would be identical.
        let a = EffectKey::compute(&ticket("T-1"), 1, "ab", "c");
        let b = EffectKey::compute(&ticket("T-1"), 1, "a", "bc");
        assert_ne!(a, b);
    }

    #[test]
    fn as_str_round_trips_through_from_hex() {
        let key = EffectKey::compute(&ticket("T-1"), 1, "git.push", "abc");
        let rebuilt = EffectKey::from_hex(key.as_str().to_string());
        assert_eq!(key, rebuilt);
    }

    #[test]
    fn status_round_trips_through_as_str_and_parse() {
        for status in [
            EffectStatus::Journaled,
            EffectStatus::Completed,
            EffectStatus::Failed,
        ] {
            let parsed = EffectStatus::parse(status.as_str()).expect("parse");
            assert_eq!(parsed.as_str(), status.as_str());
        }
    }

    #[test]
    fn status_parse_rejects_unknown() {
        assert!(EffectStatus::parse("bogus").is_err());
    }
}
