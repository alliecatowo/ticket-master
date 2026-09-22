//! The Ticketmaster coding agent: a provider-agnostic, authority-gated tool-using loop.
//!
//! This is the thing that makes `tm` worth using with no swarm at all (`SPEC.md` §11). An
//! [`agent_loop::AgentLoop`] takes an [`outcome::AgentTask`] — a ticket, its compiled context
//! pack, the authority it may exercise, and the budget it may spend — and drives a
//! provider-fabric completion loop: every model-issued tool call is checked against
//! `tm_types::Authority::permits` before it runs, a denial comes back to the model as a tool
//! result rather than a crash, approval-required actions suspend the loop until a human or
//! higher authority decides, and the budget is checked between steps so exhaustion stops
//! cleanly rather than mid-edit.
//!
//! Non-negotiables this crate exists to enforce (`SPEC.md` §11):
//! - Every tool call is gated by [`tm_types::Authority::permits`]; see [`tools::ToolRegistry`],
//!   which dispatches through whatever [`tm_types::CapabilityProvider`]s it was built with
//!   (`docs/audit-2026-09-18-fable.md` A-01).
//! - Approval-required actions suspend and resume via `approval.requested`/`approval.decided`.
//! - Edits go through [`patch::PatchEngine`] with conflict detection, never blind overwrites.
//! - Budget is checked before each provider call; exhaustion stops between steps, never
//!   mid-edit.
//! - The agent that produced a change may not audit it (enforced upstream in `tm-core`, but
//!   this crate never offers a tool that would let a worker mark its own submission verified).
//!
//! Every test in this crate drives `tm_provider::MockProvider`: no network call is ever made
//! from a test.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod agent_loop;
pub mod executor;
pub mod hooks;
pub mod outcome;
pub mod patch;
pub mod prompt;
mod pruning;
pub mod session;
pub mod skill_capability;
pub mod tools;

pub use agent_loop::AgentLoop;
pub use executor::{
    BrowserWiring, BuiltinExecutor, ComputerWiring, HumanApprovalSink, HumanDecision,
    HumanExecutor, StoreArtifactSink,
};
pub use hooks::{HookConfig, HookDecision, HookEntry};
pub use outcome::{
    AgentOutcome, AgentTask, Conversation, ConversationTurn, EvidenceBundle, StepRecord,
};
pub use patch::{Edit, PatchEngine, PatchOutcome};
pub use prompt::{render_system_prompt, render_task_prompt};
pub use session::{DurablePromotion, Session};
pub use skill_capability::SkillCapability;
pub use tools::{BuiltinCapability, ToolCall, ToolOutcome, ToolRegistry};
