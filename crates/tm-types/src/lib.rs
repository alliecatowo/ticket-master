//! Shared primitives for Ticketmaster.
//!
//! Everything in this crate is deterministic and dependency-light: identifiers, the injected
//! clock/id sources that make the rest of the system reproducible, the error root, path
//! patterns, the authority algebra, and budgets.
//!
//! See `SPEC.md` §2 and §4.4.

#![forbid(unsafe_code)]
#![recursion_limit = "1024"]
#![warn(missing_docs)]

pub mod action;
pub mod authority;
pub mod budget;
pub mod clock;
pub mod error;
pub mod id;
pub mod pattern;
pub mod predicate;
pub mod role;
pub mod time_;

pub use action::{Action, Decision, GitOp, Oversight, ProjectOp, TicketOp};
pub use authority::{
    Authority, AuthorityDenied, GitAuthority, NetworkAuthority, ProjectAuthority, RepoAuthority,
    ResourceAuthority, ShellAuthority, TicketAuthority,
};
pub use budget::{Budget, BudgetError, Spend};
pub use clock::{Clock, CounterIds, FixedClock, IdSource, SystemClock, TestIds};
pub use error::{Result, TmError};
pub use id::{
    ArtifactId, DecisionId, Id, IdKind, LeaseId, MilestoneId, ParticipantId, SessionId, TicketId,
};
pub use pattern::{PathPattern, PatternSet};
pub use predicate::{Predicate, PredicateOutcome};
pub use role::{Role, Tolerance};
pub use time_::Timestamp;
