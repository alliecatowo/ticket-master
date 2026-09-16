//! The two record shapes that flow through the log — [`EventDraft`] (what a caller proposes)
//! and [`Event`] (what actually landed, with `seq`/`ts`/`hash` assigned) — plus the pure
//! canonical-serialization and hash-chain functions that give every event its `hash`
//! (`SPEC.md` §3.1).
//!
//! Nothing here touches SQLite, the clock, or an id source: [`EventLog::append`] (`log.rs`)
//! is the only place that assigns `seq`, stamps `ts` from an injected `&dyn Clock`, and calls
//! into this module to compute `hash`. Keeping this module I/O-free makes the chain math
//! independently unit-testable.

use serde::Serialize;
use serde_json::Value;
use tm_types::{Id, ParticipantId, SessionId, Timestamp};

use crate::kind::EventKind;
use crate::payload::Payload;

/// The `hash` chained by the very first event in a project's log, standing in for "no
/// predecessor". `SPEC.md` §3.1: `hash = blake3(prev_hash || canonical_body)`.
pub const GENESIS_PREV_HASH: &str = "";

/// What a caller proposes to append. The log assigns `seq`, `ts` and `hash`; everything else
/// here is exactly what ends up on the row.
#[derive(Debug, Clone, PartialEq)]
pub struct EventDraft {
    /// Primary object this event is about; [`Id::none`] if there isn't one.
    pub subject: Id,
    /// Who produced this event.
    pub actor: ParticipantId,
    /// The session this event was produced inside, if any.
    pub session: Option<SessionId>,
    /// The `seq` of the event that caused this one, if any.
    pub causation: Option<u64>,
    /// A free-form id grouping every event in one logical operation.
    pub correlation: Option<String>,
    /// The typed payload; its [`Payload::kind`] becomes the row's `kind` column.
    pub payload: Payload,
}

impl EventDraft {
    /// Start a draft with no session, causation or correlation set.
    pub fn new(actor: ParticipantId, subject: Id, payload: Payload) -> Self {
        EventDraft {
            subject,
            actor,
            session: None,
            causation: None,
            correlation: None,
            payload,
        }
    }

    /// Attach the session this event was produced inside.
    pub fn with_session(mut self, session: SessionId) -> Self {
        self.session = Some(session);
        self
    }

    /// Attach the `seq` of the event that caused this one.
    pub fn with_causation(mut self, causation: u64) -> Self {
        self.causation = Some(causation);
        self
    }

    /// Attach a correlation id grouping this event with others in the same logical operation.
    pub fn with_correlation(mut self, correlation: impl Into<String>) -> Self {
        self.correlation = Some(correlation.into());
        self
    }

    /// The event kind this draft will land as, taken from its payload.
    pub fn kind(&self) -> EventKind {
        self.payload.kind()
    }
}

/// One committed row of the event log, exactly as read back from SQLite.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Total order within the project; gapless, assigned by SQLite `AUTOINCREMENT`.
    pub seq: u64,
    /// When this event was appended, per the log's injected clock.
    pub ts: Timestamp,
    /// The dotted event kind.
    pub kind: EventKind,
    /// Primary object this event is about; [`Id::none`] if there isn't one.
    pub subject: Id,
    /// Who produced this event.
    pub actor: ParticipantId,
    /// The session this event was produced inside, if any.
    pub session: Option<SessionId>,
    /// The `seq` of the event that caused this one, if any.
    pub causation: Option<u64>,
    /// A free-form id grouping every event in one logical operation.
    pub correlation: Option<String>,
    /// The typed payload.
    pub payload: Payload,
    /// `blake3(prev_hash || canonical_body)`, hex-encoded lowercase, chaining this event to the
    /// one before it (or to [`GENESIS_PREV_HASH`] if this is `seq == 1`).
    pub hash: String,
}

impl Event {
    /// Recompute this event's canonical body from its own fields (everything but `seq` and
    /// `hash`), for feeding back into [`chain_hash`] during [`crate::log::EventLog::verify_chain`].
    pub fn canonical_body(&self) -> tm_types::Result<Vec<u8>> {
        canonical_body(
            self.ts,
            self.kind,
            &self.subject,
            &self.actor,
            self.session.as_ref(),
            self.causation,
            self.correlation.as_deref(),
            &self.payload,
        )
    }

    /// True when `self.hash` is exactly `chain_hash(prev_hash, self.canonical_body())`.
    pub fn verifies_against(&self, prev_hash: &str) -> tm_types::Result<bool> {
        let body = self.canonical_body()?;
        let computed_hash = chain_hash(prev_hash, &body);
        Ok(computed_hash == self.hash)
    }
}

/// The exact field set and order hashed into every event's `hash`. Field order here is the
/// canonicalization: `serde`'s struct serialization follows declaration order, and nested
/// `payload` JSON keys sort lexicographically because `serde_json::Map` is a `BTreeMap` unless
/// the `preserve_order` feature is enabled (it is not, in this workspace) — so this struct's
/// `serde_json::to_vec` output is a deterministic function of its fields, independent of
/// insertion history or struct literal order at call sites.
#[derive(Serialize)]
struct CanonicalBody<'a> {
    ts: Timestamp,
    kind: EventKind,
    subject: &'a Id,
    actor: &'a ParticipantId,
    session: Option<&'a SessionId>,
    causation: Option<u64>,
    correlation: Option<&'a str>,
    payload: Value,
}

/// Build the canonical byte serialization of an event's body (everything hashed except the
/// previous event's hash), per `SPEC.md` §3.1.
#[allow(clippy::too_many_arguments)] // one param per hashed field; the field list is the contract (SPEC.md §3.1)
pub fn canonical_body(
    ts: Timestamp,
    kind: EventKind,
    subject: &Id,
    actor: &ParticipantId,
    session: Option<&SessionId>,
    causation: Option<u64>,
    correlation: Option<&str>,
    payload: &Payload,
) -> tm_types::Result<Vec<u8>> {
    let canonical = CanonicalBody {
        ts,
        kind,
        subject,
        actor,
        session,
        causation,
        correlation,
        payload: payload.to_json()?,
    };
    serde_json::to_vec(&canonical).map_err(Into::into)
}

/// Chain `body` onto `prev_hash`: `blake3(prev_hash.as_bytes() || body)`, hex-encoded lowercase.
///
/// `prev_hash` is [`GENESIS_PREV_HASH`] (the empty string) for the first event in a log.
pub fn chain_hash(prev_hash: &str, body: &[u8]) -> String {
    blake3::Hasher::new()
        .update(prev_hash.as_bytes())
        .update(body)
        .finalize()
        .to_hex()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{Id, ParticipantId, SessionId, Timestamp};

    #[test]
    fn genesis_prev_hash_is_empty() {
        assert_eq!(GENESIS_PREV_HASH, "");
    }

    #[test]
    fn chain_hash_is_deterministic() {
        let prev = "";
        let body = b"test body";
        let hash1 = chain_hash(prev, body);
        let hash2 = chain_hash(prev, body);
        assert_eq!(
            hash1, hash2,
            "chain_hash should produce deterministic results"
        );
    }

    #[test]
    fn chain_hash_with_genesis_prev_hash() {
        let body = b"first event";
        let hash = chain_hash(GENESIS_PREV_HASH, body);
        assert!(
            !hash.is_empty(),
            "chain_hash should produce non-empty output"
        );
        assert_eq!(
            hash.len(),
            64,
            "blake3 hex output is always 64 lowercase characters"
        );
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "chain_hash output should be lowercase hex"
        );
    }

    #[test]
    fn chain_hash_differs_for_different_bodies() {
        let prev = "";
        let body1 = b"body one";
        let body2 = b"body two";
        let hash1 = chain_hash(prev, body1);
        let hash2 = chain_hash(prev, body2);
        assert_ne!(
            hash1, hash2,
            "different bodies should produce different hashes"
        );
    }

    #[test]
    fn chain_hash_differs_for_different_prev_hashes() {
        let prev1 = "abc123";
        let prev2 = "def456";
        let body = b"same body";
        let hash1 = chain_hash(prev1, body);
        let hash2 = chain_hash(prev2, body);
        assert_ne!(
            hash1, hash2,
            "different prev_hash values should produce different hashes"
        );
    }

    #[test]
    fn chain_hash_with_empty_body() {
        let prev = "deadbeef";
        let body = b"";
        let hash = chain_hash(prev, body);
        assert_eq!(
            hash.len(),
            64,
            "hash of empty body should still be 64 characters"
        );
    }

    #[test]
    fn chain_hash_with_long_body() {
        let prev = "";
        let body: Vec<u8> = vec![42u8; 10000];
        let hash = chain_hash(prev, &body);
        assert_eq!(
            hash.len(),
            64,
            "hash of large body should still be 64 characters"
        );
    }

    #[test]
    fn chain_hash_concatenation_order_matters() {
        let body = b"test";
        let prev1 = "abc";
        let prev2 = "ab";
        let hash1 = chain_hash(prev1, body);
        let hash2 = chain_hash(prev2, body);
        assert_ne!(
            hash1, hash2,
            "prev_hash order in concatenation should matter"
        );
    }

    #[test]
    fn canonical_body_creates_valid_json() {
        let ts = Timestamp::EPOCH;
        let kind = crate::kind::EventKind::ProjectCreated;
        let subject = Id::none();
        let actor = ParticipantId::system();
        let payload = crate::payload::ProjectCreatedPayload {
            name: "test".to_string(),
            root: "/test".to_string(),
        };
        let payload_enum: Payload = payload.into();

        let result = canonical_body(ts, kind, &subject, &actor, None, None, None, &payload_enum);
        assert!(
            result.is_ok(),
            "canonical_body should succeed for valid inputs"
        );

        let body = result.unwrap();
        assert!(!body.is_empty(), "canonical body should not be empty");

        // Verify it's valid JSON
        let parsed: Result<serde_json::Value, _> = serde_json::from_slice(&body);
        assert!(parsed.is_ok(), "canonical body should be valid JSON");
    }

    #[test]
    fn canonical_body_includes_all_fields() {
        let ts = Timestamp::EPOCH;
        let kind = crate::kind::EventKind::ProjectCreated;
        let subject = Id::none();
        let actor = ParticipantId::system();
        let session = SessionId::new("S-123").ok();
        let correlation = "corr-id";
        let payload = crate::payload::ProjectCreatedPayload {
            name: "test".to_string(),
            root: "/test".to_string(),
        };
        let payload_enum: Payload = payload.into();

        let body = canonical_body(
            ts,
            kind,
            &subject,
            &actor,
            session.as_ref(),
            Some(42),
            Some(correlation),
            &payload_enum,
        )
        .unwrap();

        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let obj = json.as_object().unwrap();

        assert!(obj.contains_key("ts"), "canonical body should include ts");
        assert!(
            obj.contains_key("kind"),
            "canonical body should include kind"
        );
        assert!(
            obj.contains_key("subject"),
            "canonical body should include subject"
        );
        assert!(
            obj.contains_key("actor"),
            "canonical body should include actor"
        );
        assert!(
            obj.contains_key("session"),
            "canonical body should include session"
        );
        assert!(
            obj.contains_key("causation"),
            "canonical body should include causation"
        );
        assert!(
            obj.contains_key("correlation"),
            "canonical body should include correlation"
        );
        assert!(
            obj.contains_key("payload"),
            "canonical body should include payload"
        );
    }

    #[test]
    fn canonical_body_is_deterministic() {
        let ts = Timestamp::EPOCH;
        let kind = crate::kind::EventKind::ProjectCreated;
        let subject = Id::none();
        let actor = ParticipantId::system();
        let payload = crate::payload::ProjectCreatedPayload {
            name: "test".to_string(),
            root: "/test".to_string(),
        };
        let payload_enum: Payload = payload.into();

        let body1 =
            canonical_body(ts, kind, &subject, &actor, None, None, None, &payload_enum).unwrap();
        let body2 =
            canonical_body(ts, kind, &subject, &actor, None, None, None, &payload_enum).unwrap();

        assert_eq!(body1, body2, "canonical_body should be deterministic");
    }

    #[test]
    fn verifies_against_matches_valid_hash() {
        let ts = Timestamp::EPOCH;
        let kind = crate::kind::EventKind::ProjectCreated;
        let subject = Id::none();
        let actor = ParticipantId::system();
        let payload = crate::payload::ProjectCreatedPayload {
            name: "test".to_string(),
            root: "/test".to_string(),
        };
        let payload_enum: Payload = payload.into();

        let prev_hash = "abc123def456";
        let body =
            canonical_body(ts, kind, &subject, &actor, None, None, None, &payload_enum).unwrap();
        let hash = chain_hash(prev_hash, &body);

        let event = Event {
            seq: 1,
            ts,
            kind,
            subject,
            actor,
            session: None,
            causation: None,
            correlation: None,
            payload: payload_enum,
            hash,
        };

        let result = event.verifies_against(prev_hash);
        assert!(result.is_ok(), "verifies_against should not error");
        assert!(
            result.unwrap(),
            "event hash should verify against correct prev_hash"
        );
    }

    #[test]
    fn verifies_against_rejects_wrong_hash() {
        let ts = Timestamp::EPOCH;
        let kind = crate::kind::EventKind::ProjectCreated;
        let subject = Id::none();
        let actor = ParticipantId::system();
        let payload = crate::payload::ProjectCreatedPayload {
            name: "test".to_string(),
            root: "/test".to_string(),
        };
        let payload_enum: Payload = payload.into();

        let prev_hash = "abc123def456";
        let wrong_hash = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

        let event = Event {
            seq: 1,
            ts,
            kind,
            subject,
            actor,
            session: None,
            causation: None,
            correlation: None,
            payload: payload_enum,
            hash: wrong_hash.to_string(),
        };

        let result = event.verifies_against(prev_hash);
        assert!(
            result.is_ok(),
            "verifies_against should not error on wrong hash"
        );
        assert!(
            !result.unwrap(),
            "event hash should not verify against correct prev_hash"
        );
    }

    #[test]
    fn verifies_against_rejects_different_prev_hash() {
        let ts = Timestamp::EPOCH;
        let kind = crate::kind::EventKind::ProjectCreated;
        let subject = Id::none();
        let actor = ParticipantId::system();
        let payload = crate::payload::ProjectCreatedPayload {
            name: "test".to_string(),
            root: "/test".to_string(),
        };
        let payload_enum: Payload = payload.into();

        let prev_hash = "abc123def456";
        let body =
            canonical_body(ts, kind, &subject, &actor, None, None, None, &payload_enum).unwrap();
        let hash = chain_hash(prev_hash, &body);

        let event = Event {
            seq: 1,
            ts,
            kind,
            subject,
            actor,
            session: None,
            causation: None,
            correlation: None,
            payload: payload_enum,
            hash,
        };

        let different_prev = "different_prev_hash_value";
        let result = event.verifies_against(different_prev);
        assert!(result.is_ok(), "verifies_against should not error");
        assert!(
            !result.unwrap(),
            "event should not verify with different prev_hash"
        );
    }

    #[test]
    fn verifies_against_genesis() {
        let ts = Timestamp::EPOCH;
        let kind = crate::kind::EventKind::ProjectCreated;
        let subject = Id::none();
        let actor = ParticipantId::system();
        let payload = crate::payload::ProjectCreatedPayload {
            name: "test".to_string(),
            root: "/test".to_string(),
        };
        let payload_enum: Payload = payload.into();

        let body =
            canonical_body(ts, kind, &subject, &actor, None, None, None, &payload_enum).unwrap();
        let hash = chain_hash(GENESIS_PREV_HASH, &body);

        let event = Event {
            seq: 1,
            ts,
            kind,
            subject,
            actor,
            session: None,
            causation: None,
            correlation: None,
            payload: payload_enum,
            hash,
        };

        let result = event.verifies_against(GENESIS_PREV_HASH);
        assert!(
            result.is_ok(),
            "verifies_against should not error for genesis"
        );
        assert!(
            result.unwrap(),
            "first event should verify against GENESIS_PREV_HASH"
        );
    }

    #[test]
    fn event_canonical_body_roundtrip() {
        let ts = Timestamp::EPOCH;
        let kind = crate::kind::EventKind::ProjectCreated;
        let subject = Id::none();
        let actor = ParticipantId::system();
        let payload = crate::payload::ProjectCreatedPayload {
            name: "test".to_string(),
            root: "/test".to_string(),
        };
        let payload_enum: Payload = payload.into();

        let event = Event {
            seq: 1,
            ts,
            kind,
            subject: subject.clone(),
            actor: actor.clone(),
            session: None,
            causation: None,
            correlation: None,
            payload: payload_enum.clone(),
            hash: "dummy".to_string(),
        };

        // Event's canonical_body() should match standalone canonical_body()
        let from_event = event.canonical_body().unwrap();
        let standalone =
            canonical_body(ts, kind, &subject, &actor, None, None, None, &payload_enum).unwrap();

        assert_eq!(
            from_event, standalone,
            "Event::canonical_body() should match standalone function"
        );
    }

    #[test]
    fn chain_hash_produces_64_char_hex() {
        let hashes = vec![
            chain_hash("", b"test1"),
            chain_hash("prev", b"test2"),
            chain_hash(&"a".repeat(100), b"test3"),
            chain_hash("", b""),
        ];
        for hash in hashes {
            assert_eq!(hash.len(), 64, "blake3 hex should always be 64 characters");
            assert!(
                hash.chars().all(|c| c.is_ascii_hexdigit()),
                "output should be valid hex"
            );
            assert!(
                hash.chars().all(|c| !c.is_ascii_uppercase()),
                "output should be lowercase"
            );
        }
    }

    #[test]
    fn canonical_body_with_optional_fields() {
        let ts = Timestamp::EPOCH;
        let kind = crate::kind::EventKind::ProjectCreated;
        let subject = Id::none();
        let actor = ParticipantId::system();
        let payload = crate::payload::ProjectCreatedPayload {
            name: "test".to_string(),
            root: "/test".to_string(),
        };
        let payload_enum: Payload = payload.into();

        // With no optional fields
        let body_empty =
            canonical_body(ts, kind, &subject, &actor, None, None, None, &payload_enum).unwrap();

        // With all optional fields
        let session = SessionId::new("S-1").ok();
        let body_full = canonical_body(
            ts,
            kind,
            &subject,
            &actor,
            session.as_ref(),
            Some(123),
            Some("corr"),
            &payload_enum,
        )
        .unwrap();

        assert_ne!(
            body_empty, body_full,
            "canonical bodies should differ with optional fields"
        );
    }
}
