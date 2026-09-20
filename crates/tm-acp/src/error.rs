//! This crate's own error type. Kept separate from `tm_types::TmError` because most of what can
//! go wrong here (a malformed wire payload, a wedged subprocess, a protocol version mismatch) is
//! specific to speaking ACP over a JSON-RPC connection, not a Ticketmaster-domain failure; call
//! sites that need a `tm_types::Result` (e.g. `crate::executor::AcpExecutor::execute`) convert
//! at the boundary via `.to_string()`, the same ad hoc mapping `tm-cli`'s own dispatch code uses
//! for infrastructure errors that don't fit `TmError`'s closed vocabulary cleanly.

use crate::connection::ConnError;

/// Everything that can go wrong building or driving an ACP connection.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// A JSON-RPC call/notification failed (timeout, remote error, or the connection closed).
    #[error(transparent)]
    Connection(#[from] ConnError),
    /// Spawning or otherwise doing I/O with the agent process failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A request/response payload failed to (de)serialize.
    #[error("failed to (de)serialize a JSON-RPC payload: {0}")]
    Json(#[from] serde_json::Error),
    /// The agent's `initialize` response named a protocol version newer than this crate
    /// supports (see `crate::protocol::PROTOCOL_VERSION`'s doc comment on negotiation).
    #[error(
        "the agent negotiated protocol version {found}, which this client (max {max}) does not support"
    )]
    UnsupportedProtocolVersion {
        /// The version the agent responded with.
        found: u16,
        /// The maximum version this client supports.
        max: u16,
    },
    /// The configuration handed to [`crate::client::AcpClient::spawn`] was not usable (e.g. an
    /// empty command).
    #[error("invalid ACP agent configuration: {0}")]
    InvalidConfig(String),
}
