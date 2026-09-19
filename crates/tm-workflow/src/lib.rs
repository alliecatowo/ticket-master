#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Workflow definitions: `SPEC.md` §25.
//!
//! A workflow definition is a reusable, parameterized *recipe* for a ticket subgraph -- e.g.
//! "review-change": lint -> test -> review -> merge -- distinct from `tm-genesis`'s one-shot
//! spec-to-graph compilation (§12, a frontier model inventing structure once) and from ad hoc
//! ticket creation (a human inventing structure by hand). Expanding a definition into tickets is
//! deterministic software: no inference is spent on structure that was already decided.
//!
//! Per §25.2, a definition compiles to a ticket subgraph -- it does not introduce a second
//! execution engine. The scheduler, leases, authority, budgets and verification all apply
//! unchanged to the tickets [`expand`] produces; this crate only decides what nodes and edges to
//! create.
//!
//! # Modules
//!
//! - [`def`]: [`def::WorkflowDef`] and its TOML wire shape, plus [`def::WorkflowDef::validate`]
//!   (definition-level checks: dangling node refs, dangling `for_each: FromOutput` targets, and
//!   §25.2's "a cycle without a `CycleBudget` is rejected at compile time" rule).
//! - [`template`]: the `{{param}}`/`{{item}}`/`{{item.field}}` substitution `def::NodeDef`
//!   objectives use, kept deliberately small rather than pulling in a templating crate or
//!   depending on `tm-templates` (see [`template`]'s own doc comment for why).
//! - [`expand`]: [`expand::expand`], the pure function turning a [`def::WorkflowDef`] plus
//!   concrete parameter values into a [`tm_genesis::compile::GraphCompilation`] proposal --
//!   deliberately the *same* type `tm-genesis` produces, so
//!   [`tm_genesis::compile::validate_graph`] validates a workflow expansion exactly the way it
//!   validates a genesis compilation, with no second validator to keep in sync.
//! - [`commit`]: [`commit::commit`], which commits an already-[`tm_genesis::compile::validate_graph`]-clean
//!   proposal to a [`tm_core::Store`] as one [`tm_core::Store::transaction`] -- so a
//!   partially-expanded workflow can never leave a half-created ticket graph behind on failure.

pub mod commit;
pub mod def;
pub mod expand;
pub mod template;

pub use commit::{commit, content_hash, ticket_has_settled, CommitOutcome};
pub use def::{
    BudgetSpec, ForEach, JoinDef, JoinKind, MergeStrategy, NodeDef, ParamDef, ParamKind,
    WorkflowDef,
};
pub use expand::{expand, expand_fan_out, node_id_of_ticket_ref};
