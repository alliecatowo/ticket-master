//! Ticketmaster Core: the authoritative project state and the deterministic transitions over it.
//!
//! This is the keystone crate. `tm-scheduler`, `tm-context`, `tm-docs`, `tm-harness`, `tm-agent`,
//! `tm-genesis`, `tm-mirror`, `tm-server` and `tm-cli` all build on the API defined here, so the
//! public surface must stay coherent, complete and hard to misuse.
//!
//! Core is deliberately boring: no inference happens here. Every transition this crate performs
//! is a pure function of project state. The load-bearing invariants:
//!
//! * Every row of every materialized table ([`schema`]) is derivable by replaying the event log
//!   from `seq` 0. [`store::Store::rebuild`] drops the views and replays; [`materialize::apply`]
//!   is the *only* function that turns an event into a state change, shared by the live path and
//!   the replay path.
//! * A state change and the events that record it commit in the same SQLite transaction, via the
//!   [`tm_events::log::Tx`] handle `tm-events` exposes.
//! * Ticket transition legality is a pure total function over `(state, trigger)`, see
//!   [`machine::transition`].
//! * A worker never certifies itself: [`invariants`] enforces that the auditor differs from the
//!   executor, and that a submission carries evidence.
//! * A dead worker cannot block the project: [`lease::expire_due`] is a pure sweep that reverts
//!   authority and returns the ticket to `Ready` with an attempt counted.
//! * Cycles in the dependency graph ([`graph`]) are legal only when every edge in the cycle is a
//!   `Loop` edge carrying a [`ticket::CycleBudget`]; everything else must be acyclic.
//!
//! See `SPEC.md` §4.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod artifact;
pub mod budget;
pub mod decision;
pub mod executor;
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
pub use budget::{BudgetLedger, BudgetScope, ExhaustedScope};
pub use decision::{Decision, DecisionStore};
pub use executor::{
    sandbox_for, validate_return_scope, CostClass, ExecutionHandle, Executor, ExecutorCapabilities,
    ExecutorFailure, ExecutorOutcome, ExecutorTask, FsScope, NetPolicy, ReturnScopeViolation,
    Sandbox,
};
pub use graph::{CycleViolation, DependencyEdge, DependencyGraph};
pub use invariants::{check_invariants, Violation};
pub use lease::{Lease, LeaseStore, ReversionAction};
pub use machine::{InvalidTransition, TransitionTable};
pub use materialize::{apply, replay};
pub use milestone::{Milestone, MilestoneState, MilestoneStore};
pub use schema::drop_views;
pub use store::{DocRow, HarnessEpochRow, MirrorLinkRow, MirrorSyncDirection, Store, StoreTx};
pub use ticket::{
    ContextRef, DependencyKind, ExecutorRequirements, FailureClass, FailureRecord, ResourceClaim,
    ResourceMode, RetryPolicy, Ticket, TicketKind, TicketState, Trigger, VerificationPolicy,
};
pub use view::{ProjectView, SchedulerView};
