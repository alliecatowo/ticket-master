//! The event log: the single source of durable truth for a Ticketmaster project.
//!
//! Everything else in Ticketmaster — project state, indexes, docs — is a materialized view
//! derived by replaying this log. It lives in SQLite at `<project>/.tm/project.db`, opened in
//! WAL mode: one serialized write connection plus any number of concurrent read connections
//! (`schema.rs`). Events are append-only, enforced by SQLite triggers that `RAISE` on
//! `UPDATE`/`DELETE`, not just by application convention (`schema.rs`). Each event's `hash`
//! chains the previous one — `blake3(prev_hash || canonical_body)` — so tampering is detectable
//! via [`log::EventLog::verify_chain`] (`event.rs`).
//!
//! The catalogue of event kinds is closed (`kind.rs`); each kind has exactly one typed payload
//! shape (`payload.rs`). The critical API for the rest of the system is [`log::Tx`]: it lets
//! `tm-core` commit a state change and the events that describe it in one SQLite transaction, so
//! materialized state and the log can never disagree after a crash.
//!
//! See `SPEC.md` §3.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod event;
pub mod kind;
pub mod log;
pub mod payload;
pub mod schema;
pub mod stream;

pub use event::{canonical_body, chain_hash, Event, EventDraft, GENESIS_PREV_HASH};
pub use kind::{EventCategory, EventKind};
pub use log::{ChainReport, EventLog, Tx};
pub use payload::Payload;
pub use schema::SCHEMA_VERSION;
pub use stream::{EventHub, EventStream};
