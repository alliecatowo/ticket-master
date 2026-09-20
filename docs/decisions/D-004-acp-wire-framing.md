# D-004 — ACP wire framing, and how `tm-acp` plugs into the executor registry

**Status:** accepted · **Date:** 2026-09-19 · **Supersedes:** nothing

## Context

`docs/audit-2026-09-18-fable.md` B-12 asks for `crates/tm-acp`: a JSON-RPC 2.0-over-stdio client
that lets `tm-scheduler` dispatch a ticket to an external Agent Client Protocol (ACP) agent (the
protocol Zed uses to drive headless coding agents), plus a server half exposing a Ticketmaster
project as an ACP agent. Two things needed settling before writing any wire code: what framing
ACP actually uses over stdio, and how a new `tm_core::Executor` impl plugs into
`tm_scheduler::dispatch::ExecutorRegistry` without inventing a second registration mechanism.

## Decision

**Framing: newline-delimited JSON (ndjson) — one compact JSON value per line — not
`Content-Length:`-prefixed framing like LSP.** Confirmed against `agentclientprotocol.com`'s
protocol docs and cross-checked against independent ACP implementer write-ups (Rust/Elixir/Go
clients discussing the same transport), not merely assumed. The downloaded
`agentclientprotocol/agent-client-protocol` `schema.json` is honestly silent on this — it defines
message *shapes* only, never the transport framing — so the schema alone would not have settled
it; the docs/implementer-consensus evidence is what did. `crates/tm-acp/src/jsonrpc.rs` implements
ndjson accordingly (`read_line`/`write_message`), with this caveat stated again in that module's
own doc comment so a future session re-reading only the code still gets the honest provenance.

**Registration: an optional `acp.toml` at the project root, read by
`crates/tm-cli/src/dispatch.rs::optional_acp_executor`, naming one external agent's `command`
argv and the `Role` it serves.** When present, `AcpExecutor` is registered into the same
`ExecutorRegistry::register(role, executor)` call `BuiltinExecutor` already uses for every role —
replacing `BuiltinExecutor` for exactly that one role, the same "later registration for a role
wins" mechanism the registry already had. When `acp.toml` is absent, `build_dispatcher` behaves
identically to before this crate existed: `BuiltinExecutor` still serves every role. This mirrors
`crate::drive::optional_browser_wiring`'s existing shape (`browser.toml` absent ⇒
`BrowserWiring: None` ⇒ no browser tools registered) applied to a whole executor instead of one
capability provider, deliberately kept simpler than `browser.toml`'s provider-fallback-chain shape
since B-12's scope is one external agent as one more role-routed adapter, not a fallback chain.

**Permission mapping fails closed, in both directions.** `crates/tm-acp/src/permission.rs` maps an
ACP `ToolCallUpdate` to a `tm_types::Action` and checks it against the task's own
`Authority::permits` — but only ever *selects* an option the agent itself offered in
`RequestPermissionRequest::options`; it never fabricates an `optionId`, and among options of the
wanted polarity it prefers `_once` over `_always` (an `_always` grant would let the remote agent
skip a later re-check of the same authority class for calls this process never sees again). A tool
call this module cannot confidently map to a known `Action` — an unrecognized `ToolKind`, missing
`locations`, or an unrecognized `rawInput` shape — is denied, never defaulted to allowed.

## Why

Getting the framing wrong isn't a cosmetic bug: a client speaking Content-Length frames against a
real ndjson-speaking agent (or vice versa) never completes a single round trip, and the failure
mode (hung reads, not a clean parse error) is exactly the kind of thing that looks like a
subprocess-spawning bug rather than a protocol mismatch. Verifying against the real spec's own
docs plus independent implementations, rather than defaulting to "ACP looks LSP-shaped so it's
probably Content-Length," is what this decision protects future readers from re-discovering the
hard way.

The registration shape was chosen to satisfy the task's explicit constraint — plug into B-04's
existing dispatch mechanism "following that existing pattern exactly rather than inventing a new
registration mechanism" — literally: `ExecutorRegistry` only ever knew how to map `Role ->
Arc<dyn Executor>`, so an `acp.toml`-driven override of one role's executor is the only shape that
doesn't require a scheduler-level change.

The permission fail-closed rules exist because this is the one seam where a bug is not "a test
fails" but "an external, un-vetted binary's file/shell/network access silently exceeds what a
ticket's own Authority ever granted it." Both directions of that mistake are real: auto-approving
degrades to no sandbox at all; auto-denying everything makes the whole client pointless. Mapping
through the same `Authority::permits` this codebase's own executors are checked against, with a
conjunction (not a single check) over every location a tool call touches, keeps this one authority
algebra as the only source of truth instead of a second, ACP-specific permission model growing
next to it.

## What this costs, stated plainly

- `tm-acp`'s client (`crates/tm-acp/src/client.rs`) and server
  (`crates/tm-acp/src/server.rs`) halves are both real and tested (unit tests for JSON-RPC framing
  in isolation, plus two integration tests: a raw hand-rolled client driving the real server over
  a pipe, and the real client driving a hand-rolled fake agent that calls back mid-turn to prove
  the connection is genuinely multiplexed, not "write then read one line") — but this is
  deliberately not full ACP spec coverage. `authenticate`, `session/load`/`session/resume`/
  `session/cancel`, MCP-server wiring for a launched agent, and every `SessionUpdate` variant
  besides the three `*_message_chunk`s are left for later, each addable behind the existing
  `RequestHandler`/`AgentBackend` seams without protocol-layer rework.
- `ProjectAgentBackend` (the server's `AgentBackend`) answers a prompt with a live summary of the
  project's tickets, not a real coding-agent turn of its own — it does not itself call an LLM. A
  richer backend that drives `tm-agent`'s own loop per ACP prompt is future work behind the same
  trait.
- One `AcpServer` instance serves exactly one connection (a second `attach` call would silently
  misdirect its `session/update` notifications to the first connection's outbound handle) — a real
  constraint, not just an implementation gap, though it matches how a real ACP agent subprocess
  only ever has one stdin/stdout pair for its lifetime anyway.
- The permission-mapping heuristics for `execute`/`fetch` tool calls (`crate::permission::
  extract_argv`/`extract_url`) recognize only the shapes this crate has seen documented
  (`{"command": [...]}` or `{"command": "..."}`; `{"url": "..."}`) — a real external agent whose
  tool schema differs will have those specific calls denied rather than silently miscategorized,
  which is the intended fail-closed behavior, but it does mean broader `rawInput` shape coverage
  is a real, expected follow-up as real agents are tested against this client.
