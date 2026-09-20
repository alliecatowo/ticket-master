//! The scheduler: the property that makes the whole system testable.
//!
//! [`plan::plan`] is a **pure function**: `(&SchedulerView, Timestamp, &SchedulingPolicy) ->
//! Vec<SchedulerAction>`. Identical state plus identical `now` plus identical policy produce an
//! identical action list, every time, with ties broken deterministically by ticket id. No
//! inference, no wall clock, no randomness — see `SPEC.md` §5.
//!
//! The crate is split so the pure core stays trivially unit-testable and the I/O-touching driver
//! stays thin:
//! - [`policy`] — the tunables: tick interval, lease TTL, ordering weights, Ignition/SteadyState
//!   relaxations, harness capacity share.
//! - [`select`] — deterministic ranking of `Ready` tickets against executor requirements.
//! - [`admission`] — worker/command ceilings, provider availability gating, harness capacity cap.
//! - [`retry`] — exponential backoff, per-`FailureClass` retryability, cycle-budget accounting,
//!   escalation.
//! - [`plan`] — composes the above into [`plan::SchedulerAction`] output.
//! - [`driver`] — [`driver::SchedulerLoop`]: applies planned actions through `tm-core::Store`
//!   inside transactions, ticking on an interval and on event notification, deterministic under
//!   an injected `Clock`.
//!
//! Time comes only from an injected `&dyn tm_types::Clock`; ids only from an injected
//! `&dyn tm_types::IdSource`. Never `SystemTime::now`, `Instant::now`, or `rand::thread_rng`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod admission;
pub mod dispatch;
pub mod driver;
pub mod plan;
pub mod policy;
pub mod retry;
pub mod select;
pub mod snapshot;

pub use admission::{AdmissionDecision, AdmissionGate, AdmissionRefusal};
pub use dispatch::{ContextPackSource, DispatchError, ExecutorDispatcher, ExecutorRegistry};
pub use driver::{SchedulerLoop, SchedulerLoopEvent};
pub use plan::{plan, SchedulerAction};
pub use policy::{IgnitionRelaxations, OrderingWeights, SchedulingMode, SchedulingPolicy};
pub use retry::{EscalationReason, RetryDecision, RetryOutcome};
pub use select::{
    capabilities_satisfy, select_next, CapabilityMismatch, ExecutorMatch, SelectionError,
};
pub use snapshot::{capture_workspace_snapshot, WorkspaceSnapshot};
