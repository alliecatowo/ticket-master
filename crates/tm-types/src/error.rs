//! The shared error root.

use std::fmt;

/// Convenience alias used across the workspace.
pub type Result<T, E = TmError> = std::result::Result<T, E>;

/// The root error type. Crate-local error types convert into this at layer boundaries.
#[derive(Debug, thiserror::Error)]
pub enum TmError {
    /// The referenced object does not exist.
    #[error("not found: {kind} {id}")]
    NotFound {
        /// What kind of object was looked up.
        kind: &'static str,
        /// The identifier that was looked up.
        id: String,
    },

    /// The operation conflicts with existing state (duplicate, concurrent writer, live lease).
    #[error("conflict: {0}")]
    Conflict(String),

    /// A state machine transition was not legal.
    #[error("invalid transition: {0}")]
    InvalidTransition(String),

    /// The actor lacked the authority for the attempted action.
    #[error("authority denied: {0}")]
    AuthorityDenied(String),

    /// A budget would have gone negative.
    #[error("budget exhausted: {0}")]
    BudgetExhausted(String),

    /// The lease used for this operation is no longer live.
    #[error("lease expired: {0}")]
    LeaseExpired(String),

    /// Persistence failure.
    #[error("storage: {0}")]
    Storage(String),

    /// Provider (model/API) failure.
    #[error("provider: {0}")]
    Provider(String),

    /// Filesystem or process I/O failure.
    #[error("io: {0}")]
    Io(String),

    /// Malformed input that could not be parsed.
    #[error("parse: {0}")]
    Parse(String),

    /// An internal invariant was violated. This always indicates a bug, never bad user input.
    #[error("invariant violated: {0}")]
    Invariant(String),
}

impl TmError {
    /// Build a [`TmError::NotFound`].
    pub fn not_found(kind: &'static str, id: impl fmt::Display) -> Self {
        TmError::NotFound { kind, id: id.to_string() }
    }

    /// Build a [`TmError::Conflict`].
    pub fn conflict(msg: impl fmt::Display) -> Self {
        TmError::Conflict(msg.to_string())
    }

    /// Build a [`TmError::Storage`].
    pub fn storage(msg: impl fmt::Display) -> Self {
        TmError::Storage(msg.to_string())
    }

    /// Build a [`TmError::Invariant`].
    pub fn invariant(msg: impl fmt::Display) -> Self {
        TmError::Invariant(msg.to_string())
    }

    /// Build a [`TmError::Parse`].
    pub fn parse(msg: impl fmt::Display) -> Self {
        TmError::Parse(msg.to_string())
    }

    /// The process exit code this error maps to, per `SPEC.md` §15.
    pub fn exit_code(&self) -> i32 {
        match self {
            TmError::AuthorityDenied(_) => 3,
            TmError::BudgetExhausted(_) => 4,
            TmError::Invariant(_) => 5,
            _ => 1,
        }
    }
}

impl From<std::io::Error> for TmError {
    fn from(e: std::io::Error) -> Self {
        TmError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for TmError {
    fn from(e: serde_json::Error) -> Self {
        TmError::Parse(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_spec() {
        assert_eq!(TmError::AuthorityDenied("x".into()).exit_code(), 3);
        assert_eq!(TmError::BudgetExhausted("x".into()).exit_code(), 4);
        assert_eq!(TmError::invariant("x").exit_code(), 5);
        assert_eq!(TmError::conflict("x").exit_code(), 1);
    }

    #[test]
    fn not_found_renders_kind_and_id() {
        assert_eq!(TmError::not_found("ticket", "T-1").to_string(), "not found: ticket T-1");
    }
}
