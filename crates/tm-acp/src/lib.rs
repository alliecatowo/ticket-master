//! Agent Client Protocol (ACP) adapter — `SPEC.md` §28, `docs/audit-2026-09-18-fable.md` B-12.
//!
//! ACP is the JSON-RPC 2.0-over-stdio protocol Zed (and other editors/agents) use to let an
//! editor drive an external, headless coding agent. This crate implements both directions:
//!
//! - [`client::AcpClient`]: connects *to* an external ACP agent as a subprocess, speaks
//!   `initialize` -> `session/new` -> `session/prompt`, and answers the agent's
//!   `session/request_permission` callbacks from this codebase's own [`tm_types::Authority`]
//!   (via [`permission::evaluate`]) rather than auto-approving or auto-denying every tool call.
//!   [`executor::AcpExecutor`] wraps this as the `acp` [`tm_core::executor::Executor`] — one
//!   more adapter `tm-scheduler`'s `ExecutorDispatcher` can route a ticket to, alongside
//!   `tm_agent::BuiltinExecutor`/`HumanExecutor` (`SPEC.md` §24.3).
//! - [`server::AcpServer`]: exposes a Ticketmaster project *as* an ACP agent, so Zed (or any
//!   other ACP client) can connect and hold a real conversation against live project state
//!   ([`server::ProjectAgentBackend`] answers from a real `tm_core::Store::view()`, not a
//!   canned string).
//!
//! # Scope
//!
//! This is a real client and server for the three core methods plus the one permission
//! callback and one streamed-reply notification a working turn needs — not full ACP spec
//! coverage. Left out on purpose, each independently addable later behind the same
//! [`connection::RequestHandler`]/[`server::AgentBackend`] seams without protocol-layer rework:
//! `authenticate`, `session/load`/`session/resume`/`session/cancel`, MCP server wiring for a
//! launched agent (a sibling crate, `tm-mcp`, is where an MCP *client* belongs), non-text
//! [`protocol::ContentBlock`] variants (image/audio/resource — they still round-trip losslessly
//! via `#[serde(flatten)]`, just aren't interpreted), and every [`protocol::SessionUpdate`]
//! variant besides the three `*_message_chunk`s (classified as
//! [`protocol::SessionUpdate::Unknown`] rather than causing a parse failure).
//!
//! # Wire framing: newline-delimited JSON, not Content-Length-prefixed
//!
//! See [`jsonrpc`]'s module doc comment for the full justification and the caveat about what
//! the downloaded schema does and does not establish about framing; the short version is that
//! ACP's own docs and cross-checked implementer write-ups describe ndjson (one compact JSON
//! value per line), unlike LSP's `Content-Length:` prefixing, so that is what this crate speaks.
//!
//! # Architecture
//!
//! [`jsonrpc`] is pure message-shape/framing code with no ACP-specific knowledge, unit-tested in
//! isolation. [`protocol`] is the ACP wire types (real schema field names/casing, not guessed —
//! see its own doc comment). [`connection`] is a multiplexed request/response/notification
//! dispatcher over an arbitrary `AsyncRead`/`AsyncWrite` pair — the piece that lets either side
//! answer an inbound request while its own outbound call is still pending, which a naive
//! "write, then read one line" loop cannot do (see its module doc comment for the deadlock this
//! avoids). [`permission`] is pure `ToolCallUpdate -> Action -> Authority::permits ->
//! PermissionOption` mapping logic, unit-tested against a real `Authority` with no connection
//! involved at all. [`client`] and [`server`] each wire one `RequestHandler` implementation
//! (respectively answering the agent's callbacks, and answering the client's session calls) on
//! top of [`connection`]. [`executor`] adapts [`client::AcpClient`] to `tm_core::Executor`.

pub mod client;
pub mod connection;
pub mod error;
pub mod executor;
pub mod jsonrpc;
pub mod permission;
pub mod protocol;
pub mod server;

pub use client::AcpClient;
pub use error::AcpError;
pub use executor::{AcpAgentConfig, AcpExecutor};
pub use server::{AcpServer, AgentBackend, ProjectAgentBackend};
