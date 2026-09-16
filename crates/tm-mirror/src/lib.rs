//! Ticketmaster Mirror: bidirectional projection of the authoritative internal graph onto
//! external task trackers (GitHub Issues, Linear, Jira, GitLab) — never the other direction.
//!
//! The internal graph is authoritative. External trackers are projections of it: convenient
//! surfaces for humans and integrations, never a second orchestration database. Three rules
//! keep that true end to end:
//!
//! * [`projection::ProjectionPolicy`] decides which tickets surface at all — machine-only kinds
//!   (`Verification`, `Audit`, `Recovery`) never do — and degrades deliberately, recording every
//!   degradation on the mirror link rather than applying it silently, when an adapter's
//!   [`tracker::TrackerCapabilities`] can't represent the real graph shape (checklist rollup
//!   where there's no native parent/child; nearest-state mapping where states aren't arbitrary).
//! * [`sync::SyncEngine`] never writes ticket state directly from an inbound change. Pulled
//!   changes are translated into [`tm_events::EventDraft`]s for a fixed, semantically meaningful
//!   allowlist (status hints, assignment, comments, priority, human-created issues becoming
//!   `Draft` tickets); Ticketmaster's own transition/authority machinery decides whether to act
//!   on the hint. Push is idempotent: syncing twice changes nothing the second time.
//! * On divergence, [`sync::ConflictField::owner`] is the single fixed rule: Ticketmaster wins
//!   orchestration fields, the external system wins human-presentation fields.
//!
//! [`github`], [`linear`], [`jira`] and [`gitlab`] are the shipped adapters, selected and
//! configured via [`config::MirrorConfig`] (`mirror.toml`); credentials are always named
//! environment variables, never stored in the file. [`tracker::NullTracker`] is the no-op
//! default and [`tracker::RecordingTracker`] is the test double every adapter's round-trip
//! tests run against, so no test in this crate touches the network.
//!
//! See `SPEC.md` §13.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod github;
pub mod gitlab;
pub mod jira;
pub mod linear;
pub mod projection;
pub mod sync;
pub mod tracker;

pub use config::{AdapterConfig, AdapterKind, CredentialEnv, MirrorConfig, ProjectionOverrides};
pub use github::GitHubTracker;
pub use gitlab::GitLabTracker;
pub use jira::JiraTracker;
pub use linear::LinearTracker;
pub use projection::{ChecklistItem, Degradation, Projection, ProjectionPolicy};
pub use sync::{ConflictField, FieldOwner, InboundAction, MirrorLink, SyncEngine};
pub use tracker::{
    ExternalChange, ExternalChangeKind, ExternalRef, NullTracker, RecordingTracker, Tracker,
    TrackerCapabilities,
};
