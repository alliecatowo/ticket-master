//! The provider fabric.
//!
//! Tickets ask for capability **roles** (`coder.fast`, `reviewer.semantic`, ...), never model
//! names. This crate owns the mapping from a role to a concrete `(provider, model)` candidate,
//! under live quota, health and price constraints, plus the two [`Provider`] implementations the
//! rest of the workspace calls through: a real Anthropic client and a deterministic mock every
//! test uses instead of the network.
//!
//! The routing decision itself ([`route::route`]) is a pure function of [`state::FabricState`]
//! plus the request: no I/O, no wall clock, fully unit-testable. Quota exhaustion is routable
//! state, not an exception — see [`route::RouteDecision`].
//!
//! See `SPEC.md` §6.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod anthropic;
pub mod fabric;
pub mod mock;
pub mod providers;
pub mod role_config;
pub mod route;
pub mod state;
pub mod types;
pub mod wire_names;

pub use anthropic::AnthropicProvider;
pub use fabric::{Fabric, Provider, ProviderRecord};
pub use mock::MockProvider;
pub use providers::compat::DevPassProvider;
pub use providers::registry::Registry;
pub use providers::{
    Availability, Capabilities, EnvVarRequirement, LocalProbe, ProviderInfo, LOCAL_PROVIDER_IDS,
};
pub use role_config::{RoleCandidate, RoleConfigError, RoleTable};
pub use route::{route, Need, RouteDecision};
pub use state::{
    Breaker, BreakerState, CandidateKey, CandidateState, FabricEvent, FabricState, LedgerEntry,
};
pub use types::{
    Candidate, Completion, CompletionRequest, ContentBlock, EmbedRequest, Embeddings, Message,
    MessageRole, ModelId, ProviderError, StopReason, ToolDef, Usage,
};
pub use wire_names::WireNames;
