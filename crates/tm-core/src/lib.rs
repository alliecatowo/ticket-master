//! Ticketmaster Core: the authoritative project state and the deterministic transitions over it.
//!
//! This is the keystone crate. `tm-scheduler`, `tm-context`, `tm-docs`, `tm-harness`, `tm-agent`,
//! `tm-genesis`, `tm-mirror`, `tm-server` and `tm-cli` all build their view of a project on the
//! API defined here, so the public surface must be coherent, complete and hard to misuse.
//!
//! Core is boring by design: no inference happens here. Every transition this crate performs is
//! a pure function of project state (`machine.rs`, `graph.rs`, `lease.rs::expire_due`). Every row
//! of every materialized table (`schema.rs`) is derivable by replaying the event log from `seq 0`
//! through the single `apply` function in `materialize.rs` — the live write path and
//! `Store::rebuild()`'s replay path are the same code, so they can never drift. A state change
//! and the events that record it commit together, in one SQLite transaction, via the [`tm_events`]
//! `Tx` handle.
//!
//! See `SPEC.md` §4.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod artifact;
pub mod budget;
pub mod decision;
pub mod graph;
pub mod invariants;
pub mod lease;
pub mod machine;
pub mod materialize;
pub mod milestone;
pub mod schema;
pub mod store;
pub mod ticket;
pub mod view;

pub use artifact::{Artifact, ArtifactKind, ArtifactStorage, Evidence, EvidenceKind};
pub use budget::{BudgetScope, ExhaustedScope, record_usage};
pub use decision::{Decision, DecisionRecord};
pub use graph::{DependencyEdge, GraphView};
pub use invariants::{check_invariants, Violation};
pub use lease::{Lease, ReversionAction};
pub use machine::{can_lease, is_live, is_terminal, transition, InvalidTransition};
pub use materialize::{apply, replay};
pub use milestone::{Milestone, MilestoneState};
pub use store::Store;
pub use ticket::{
    ContextRef, CycleBudget, DependencyKind, ExecutorRequirements, FailureClass, FailureRecord,
    ResourceClaim, ResourceMode, RetryPolicy, Ticket, TicketKind, TicketState, Trigger,
    VerificationPolicy,
};
pub use view::{ProjectView, SchedulerView};
