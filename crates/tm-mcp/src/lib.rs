//! Ticketmaster's Model Context Protocol adapter (`docs/audit-2026-09-18-fable.md` B-12,
//! `SPEC.md` §28).
//!
//! Two independent halves, sharing only [`protocol`]'s wire types:
//!
//! - **Client** ([`client`], [`capability`], [`transport`]): connects to an external MCP server
//!   over stdio or SSE, performs the real `initialize` handshake and `tools/list`, and wraps the
//!   result as one [`tm_types::CapabilityProvider`] per connected server
//!   ([`capability::McpClientCapability`]) — so `tm_agent::tools::ToolRegistry` (or any future
//!   caller) sees a remote server's tools exactly like it sees a built-in capability crate.
//! - **Server** ([`server`]): exposes one already-opened Ticketmaster project as a real MCP
//!   server over stdio, with `ticket_list`/`ticket_show`/`search_*`/`symbol_*` reads backed by
//!   [`tm_core::Store`] and `tm_codeintel::CodeIntel`, plus one write, `ticket_dispatch`, that
//!   creates and queues a worker ticket. `tm mcp` (in `tm-cli`) serves it to Claude Code with
//!   the scheduler running in-process.
//!
//! [`protocol`] is real JSON-RPC 2.0 (request/notification/response types, two wire framings —
//! see that module's doc comment for why there are two, not one). [`transport`] is the thin
//! async glue from those pure types onto an actual byte stream (a child process's stdio, an SSE
//! connection, or a test's in-memory pipe).
//!
//! # Scope
//!
//! Deliberately not attempted: MCP's resources/prompts/sampling primitives, tool-call streaming,
//! JSON-RPC batching, or a `tm mcp connect` verb. `tm mcp` lives in `tm-cli`, which depends on
//! this crate, not the other way round; this crate's own `[[bin]] tm-mcp-server` is the
//! standalone form (see `src/bin/tm-mcp-server.rs` and `server`'s module doc for why depending on
//! `tm-cli` from here was rejected).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod capability;
pub mod client;
pub mod protocol;
pub mod server;
pub mod transport;
