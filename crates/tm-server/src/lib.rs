//! Ticketmaster Server: one project world, many clients.
//!
//! `tm-server` is deliberately thin. No client — human UI, CLI, or agent — owns truth; truth is
//! [`tm_core::store::Store`] plus the durable, hash-chained event log underneath it
//! ([`tm_events::log::EventLog`]). This crate's entire job is to expose that state safely over
//! HTTP/SSE and to add exactly the multiplayer machinery `tm-core` has no opinion about:
//!
//! * [`state`] — [`state::AppState`], the one handle every route closes over, and
//!   [`state::ApiError`], the single place a [`tm_types::TmError`] becomes an HTTP status.
//! * [`auth`] — bind-address-aware authentication: open on loopback, bearer-token elsewhere.
//! * [`sse`] — the resumable event stream at `GET /events?from=<seq>`. Replay-then-live handover
//!   without a gap or a duplicate is this module's one hard problem.
//! * [`presence`] — who is looking at what, TTL-swept, sharing its collision substrate with path
//!   leases so humans and agents see the same picture.
//! * [`approvals`] — the durable object that blocks a requesting agent until a decision lands.
//! * [`routes`] — the axum router and every handler in `SPEC.md` §14.
//!
//! Sessions are **views**: a session pins a transcript and a harness epoch, but everything that
//! matters — decisions, artifacts, evidence, ticket transitions — is promoted to durable objects
//! before the session ever ends. A fresh worker with no transcript, replaying only durable
//! state, must be able to finish a ticket a deleted session started. See `SPEC.md` §14.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod approvals;
pub mod auth;
pub mod presence;
pub mod routes;
pub mod sse;
pub mod state;

pub use approvals::{ApprovalDecision, ApprovalRegistry, ApprovalRequest, ApprovalStatus};
pub use auth::{resolve_token, token_matches, AuthError, BindAddress};
pub use presence::{PathLeaseSummary, PresenceEntry, PresenceTable};
pub use routes::router;
pub use sse::sse_handler;
pub use state::{AppState, ServerError};
