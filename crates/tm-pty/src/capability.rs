//! [`PtyCapability`]: tm-pty's [`tm_types::CapabilityProvider`] impl
//! (`docs/audit-2026-09-18-fable.md` B-16, `SPEC.md` §22).
//!
//! Wraps [`crate::session::PtySession`]'s methods as dotted `pty.*` tools, following
//! `tm_browser::capability::BrowserCapability`/`tm_computer::capability::ComputerCapability`'s
//! shape exactly: [`SessionRegistry`] holds one session per `(ticket, session)` key
//! (`CallContext` carries no lease id today, so this is the same closest-available substitute
//! those two tracks use) as `Arc<tokio::sync::Mutex<Option<PtySession>>>`, so concurrent calls
//! against the *same* session serialize while calls against *different* sessions run
//! independently, and a session can be torn down by value under the lock via `Option::take`.
//!
//! Unlike browser/computer, a pty session is not lazily launched on first use — there is no
//! sensible default "argv" to launch on behalf of a caller who has not said what to run. `pty.spawn`
//! is the one tool that creates a session ([`SessionRegistry::spawn`], erroring on a key that is
//! already live); every other tool looks one up ([`SessionRegistry::get`], erroring on a key that
//! is not).
//!
//! # Action mapping (`SPEC.md` §22.4)
//!
//! - `pty.spawn` → [`tm_types::Action::PtySpawn`], gated by `Authority.shell` exactly like
//!   `Action::RunCommand` — a pty session can run arbitrary commands just as `shell.run` can.
//! - `pty.write`/`pty.send`/`pty.key` → [`tm_types::Action::PtySend`], gated by the *distinct*
//!   `Authority.shell.pty` grant: SPEC §22.4 draws this line because a live session can receive
//!   arbitrary keystrokes into an already-running process (answering a destructive confirmation
//!   prompt, driving a REPL that itself runs further commands), a materially larger attack
//!   surface than one bounded, argv-checked command.
//! - `pty.screen`/`pty.diff`/`pty.resize`/`pty.expect`/`pty.wait_exit`/`pty.record` →
//!   [`tm_types::Action::PtyControl`], gated by `Authority.shell.enabled` alone: these tools take
//!   only a session id (`to_action` is a pure function of `(tool, input)` with no session state
//!   to recover the argv `pty.spawn` used), and none of them injects new input into the child.
//!
//! # Session teardown
//!
//! [`SessionRegistry::close`]/[`SessionRegistry::close_all`] are the explicit teardown this
//! module provides. `SPEC.md` §22.4's "Sessions carry the lease's TTL and are killed on expiry —
//! process group and all" is only half implemented here, for the same reason
//! `tm_browser::capability`/`tm_computer::capability` document for their own tracks: nothing in
//! this workspace yet fires a callback when a lease expires (`tm-core::Store::expire_leases`
//! emits `ticket.lease_expired`, but no subscriber wakes a `SessionRegistry` — B-16's own audit
//! brief and the browser/computer tracks' reports all confirm that hook does not exist anywhere
//! in this workspace, not merely here). What *is* real: [`SessionRegistry::close`] and
//! [`SessionRegistry::close_all`] call [`crate::session::PtySession::kill_process_group`] before
//! dropping a session, so an owning executor that calls `close_all` on every ordinary dispatch
//! return (completion, an in-band failure, or an `Err` the loop returns) bounds a session's
//! lifetime by its dispatch — the same real-world proxy for "the lease that authorized it" the
//! browser/computer tracks use, and the same caveat: not unwind-safe (a panic, or the dispatched
//! task's future being dropped/cancelled before it resolves, skips this — there is no `Drop` impl
//! or `catch_unwind` here), and not wired to true lease-expiry yet.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tm_types::{
    Action, AuthorityRequirement, CallContext, CapabilityProvider, Clock, CostClass, Result,
    SessionId, TicketId, TmError, ToolSchema,
};
use tokio::sync::Mutex as AsyncMutex;

use crate::session::{ArtifactSink, PtyExpectOutcome, PtySession};

/// Default terminal size for a `pty.spawn` call that does not specify one — 80x24 is the
/// conventional default terminal size (and matches `crates/tm-tui`'s own `Harness::new` default
/// dimensions in its tests).
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

fn missing(field: &str) -> TmError {
    TmError::parse(format!("missing or malformed field `{field}`"))
}

fn get_str<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
    input
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| missing(field))
}

fn get_string_array(input: &Value, field: &str) -> Result<Vec<String>> {
    let arr = input
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| missing(field))?;
    arr.iter()
        .map(|v| v.as_str().map(str::to_string).ok_or_else(|| missing(field)))
        .collect()
}

/// Resolve `env_allowlist` (a list of variable *names*, not values) against this process's real
/// environment, mirroring `crates/tm-cli/src/agent.rs`'s `ProcessCommandExecutor::execute`
/// exactly: the caller names which variables the child may see, but the *value* always comes
/// from the trusted host environment, never from the tool call's input. Accepting arbitrary
/// caller-supplied values here (an `env: {"PATH": "/tmp/evil"}`-shaped input) would let an agent
/// spoof an allowlisted variable's content, not just choose which variables pass through —
/// exactly the confusion `env_clear()` plus an allowlist exists to prevent.
fn resolve_env(input: &Value) -> Vec<(String, String)> {
    input
        .get("env_allowlist")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|name| std::env::var(name).ok().map(|v| (name.to_string(), v)))
                .collect()
        })
        .unwrap_or_default()
}

fn get_u16_or(input: &Value, field: &str, default: u16) -> u16 {
    input
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(default)
}

fn get_u64_or(input: &Value, field: &str, default: u64) -> u64 {
    input.get(field).and_then(Value::as_u64).unwrap_or(default)
}

/// `ctx.root`-relative `cwd`, matching `crates/tm-agent/src/tools.rs`'s own `resolve_cwd`
/// exactly (`root.join(rel)`, defaulting to `"."`) — the same unclamped join `shell.run` uses
/// today (`docs/audit-2026-09-18-fable.md` P-01 tracks tightening that; this module matches the
/// existing behavior rather than independently tightening or loosening it for pty sessions).
fn resolve_cwd(root: &std::path::Path, input: &Value) -> PathBuf {
    let rel = input.get("cwd").and_then(Value::as_str).unwrap_or(".");
    root.join(rel)
}

fn screen_json(lines: Vec<String>) -> Value {
    json!({ "lines": lines })
}

fn expect_outcome_json(outcome: PtyExpectOutcome) -> Value {
    json!({
        "matched": outcome.matched(),
        "lines": outcome.screen(),
    })
}

/// A key identifying one live pty session. `CallContext` does not carry a lease id, so
/// `(ticket, session)` is the closest available substitute, matching
/// `tm_browser::capability::SessionRegistry`/`tm_computer::capability::SessionRegistry`'s same
/// choice.
type SessionKey = (TicketId, SessionId);

/// Explicitly spawns and tears down one [`PtySession`] per `(ticket, session)` key. See this
/// module's doc comment for why this does not lazily launch on first use the way browser/computer
/// sessions do.
pub struct SessionRegistry {
    clock: Arc<dyn Clock>,
    sink: Arc<dyn ArtifactSink>,
    sessions: AsyncMutex<HashMap<SessionKey, Arc<AsyncMutex<Option<PtySession>>>>>,
}

impl SessionRegistry {
    /// Build an empty registry over an already-constructed clock/artifact-sink pair.
    pub fn new(clock: Arc<dyn Clock>, sink: Arc<dyn ArtifactSink>) -> Self {
        SessionRegistry {
            clock,
            sink,
            sessions: AsyncMutex::new(HashMap::new()),
        }
    }

    /// Spawn a new session for `(ctx.ticket, ctx.session)`. Errors with
    /// [`TmError::Conflict`] if a session is already live for that key — a caller that wants a
    /// fresh session must [`SessionRegistry::close`] the old one first, the same way a real
    /// terminal cannot be reused by a second, unrelated login without logging the first one out.
    #[allow(clippy::too_many_arguments)]
    async fn spawn(
        &self,
        ctx: &CallContext<'_>,
        argv: &[String],
        cwd: Option<&std::path::Path>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        let key = (ctx.ticket.clone(), ctx.session.clone());
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&key) {
            return Err(TmError::conflict(format!(
                "a pty session is already live for ticket {} session {}",
                ctx.ticket, ctx.session
            )));
        }
        let session = PtySession::spawn(argv, cwd, env, cols, rows, Arc::clone(&self.clock))?;
        sessions.insert(key, Arc::new(AsyncMutex::new(Some(session))));
        Ok(())
    }

    /// Look up the session for `(ctx.ticket, ctx.session)`. Errors with [`TmError::NotFound`] if
    /// none has been spawned (or it was already closed).
    async fn get(&self, ctx: &CallContext<'_>) -> Result<Arc<AsyncMutex<Option<PtySession>>>> {
        let key = (ctx.ticket.clone(), ctx.session.clone());
        let sessions = self.sessions.lock().await;
        sessions.get(&key).cloned().ok_or_else(|| {
            TmError::not_found("pty_session", format!("{}/{}", ctx.ticket, ctx.session))
        })
    }

    /// Explicitly tear down the session for `(ticket, session)`, if one is live: kills its
    /// process group ([`PtySession::kill_process_group`]) before dropping it. A no-op if no
    /// session is live for that key.
    pub async fn close(&self, ticket: &TicketId, session: &SessionId) -> Result<()> {
        let mut sessions = self.sessions.lock().await;
        if let Some(slot) = sessions.remove(&(ticket.clone(), session.clone())) {
            let mut guard = slot.lock().await;
            if let Some(mut s) = guard.take() {
                let _ = s.kill_process_group();
            }
        }
        Ok(())
    }

    /// Tear down every session this registry currently holds — the same "called by the owning
    /// executor on every ordinary dispatch return" stand-in for lease-expiry teardown
    /// `tm_browser::capability::SessionRegistry::close_all`/
    /// `tm_computer::capability::SessionRegistry::close_all` use; see this module's doc comment.
    pub async fn close_all(&self) -> Result<()> {
        let mut sessions = self.sessions.lock().await;
        for (_, slot) in sessions.drain() {
            let mut guard = slot.lock().await;
            if let Some(mut s) = guard.take() {
                let _ = s.kill_process_group();
            }
        }
        Ok(())
    }

    /// How many sessions are currently live. Test/diagnostic use.
    pub async fn live_count(&self) -> usize {
        self.sessions.lock().await.len()
    }
}

struct ToolSpec {
    name: &'static str,
    description: &'static str,
    input_schema: fn() -> Value,
    cost: CostClass,
    requires: AuthorityRequirement,
}

fn schema_none() -> Value {
    json!({"type": "object", "properties": {}})
}
fn schema_spawn() -> Value {
    json!({
        "type": "object",
        "properties": {
            "argv": {"type": "array", "items": {"type": "string"}},
            "cwd": {"type": "string"},
            "env_allowlist": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Names of environment variables to pass through from the host \
                    (their real values, never a caller-supplied override) — e.g. [\"PATH\"]."
            },
            "cols": {"type": "integer"},
            "rows": {"type": "integer"}
        },
        "required": ["argv"]
    })
}
fn schema_write() -> Value {
    json!({
        "type": "object",
        "properties": {"data_base64": {"type": "string"}},
        "required": ["data_base64"]
    })
}
fn schema_send() -> Value {
    json!({
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"]
    })
}
fn schema_key() -> Value {
    json!({
        "type": "object",
        "properties": {"chord": {"type": "string"}},
        "required": ["chord"]
    })
}
fn schema_resize() -> Value {
    json!({
        "type": "object",
        "properties": {"cols": {"type": "integer"}, "rows": {"type": "integer"}},
        "required": ["cols", "rows"]
    })
}
fn schema_expect() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string"},
            "timeout_ms": {"type": "integer"}
        },
        "required": ["pattern"]
    })
}

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "pty.spawn",
        description: "Spawn a command inside a new pseudo-terminal session.",
        input_schema: schema_spawn,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::Shell,
    },
    ToolSpec {
        name: "pty.write",
        description: "Write raw base64-encoded bytes to the session, as if typed.",
        input_schema: schema_write,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::PtySend,
    },
    ToolSpec {
        name: "pty.send",
        description: "Write literal text to the session, as if typed.",
        input_schema: schema_send,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::PtySend,
    },
    ToolSpec {
        name: "pty.key",
        description: "Press a named key or chord, e.g. \"enter\", \"ctrl+c\", \"down down enter\".",
        input_schema: schema_key,
        cost: CostClass::Mutating,
        requires: AuthorityRequirement::PtySend,
    },
    ToolSpec {
        name: "pty.screen",
        description: "The session's rendered screen right now, as plain text lines.",
        input_schema: schema_none,
        cost: CostClass::Cheap,
        requires: AuthorityRequirement::Shell,
    },
    ToolSpec {
        name: "pty.diff",
        description: "What changed on screen since the last pty.diff call.",
        input_schema: schema_none,
        cost: CostClass::Cheap,
        requires: AuthorityRequirement::Shell,
    },
    ToolSpec {
        name: "pty.resize",
        description: "Resize the session's terminal, exercising reflow.",
        input_schema: schema_resize,
        cost: CostClass::Moderate,
        requires: AuthorityRequirement::Shell,
    },
    ToolSpec {
        name: "pty.expect",
        description: "Block until the screen contains `pattern` or `timeout_ms` elapses.",
        input_schema: schema_expect,
        cost: CostClass::Moderate,
        requires: AuthorityRequirement::Shell,
    },
    ToolSpec {
        name: "pty.wait_exit",
        description: "Block until the child process exits, returning its exit status.",
        input_schema: schema_none,
        cost: CostClass::Moderate,
        requires: AuthorityRequirement::Shell,
    },
    ToolSpec {
        name: "pty.record",
        description: "Store the session's asciicast recording as a ticket artifact.",
        input_schema: schema_none,
        cost: CostClass::Moderate,
        requires: AuthorityRequirement::Shell,
    },
];

fn pty_action(tool: &str, input: &Value) -> Result<Action> {
    match tool {
        "pty.spawn" => Ok(Action::PtySpawn {
            command: get_string_array(input, "argv")?,
        }),
        "pty.write" | "pty.send" | "pty.key" => Ok(Action::PtySend),
        "pty.screen" | "pty.diff" | "pty.resize" | "pty.expect" | "pty.wait_exit"
        | "pty.record" => Ok(Action::PtyControl),
        other => Err(TmError::parse(format!("unknown tool `{other}`"))),
    }
}

/// tm-pty's [`CapabilityProvider`]: every `SPEC.md` §22.2 `pty.*` tool over [`SessionRegistry`].
pub struct PtyCapability {
    sessions: SessionRegistry,
}

impl PtyCapability {
    /// Build the pty capability over an already-constructed [`SessionRegistry`].
    pub fn new(sessions: SessionRegistry) -> Self {
        PtyCapability { sessions }
    }

    /// Passthrough to [`SessionRegistry::close_all`] — see
    /// `tm_browser::capability::BrowserCapability::close_all`'s doc comment for why this exists
    /// alongside the trait-object registration rather than only on `SessionRegistry`.
    pub async fn close_all(&self) -> Result<()> {
        self.sessions.close_all().await
    }
}

#[async_trait::async_trait]
impl CapabilityProvider for PtyCapability {
    fn id(&self) -> &str {
        "pty"
    }

    fn tools(&self) -> Vec<ToolSchema> {
        TOOLS
            .iter()
            .map(|t| ToolSchema {
                name: t.name,
                description: t.description,
                input_schema: (t.input_schema)(),
                cost: t.cost,
                requires: t.requires.clone(),
            })
            .collect()
    }

    fn to_action(&self, tool: &str, input: &Value) -> Result<Action> {
        pty_action(tool, input)
    }

    fn requires(&self) -> AuthorityRequirement {
        // Coarse capability-level gate: reachable at all only with some shell reach, exactly
        // like `RunCommand`'s own gate — see this module's doc comment.
        AuthorityRequirement::Shell
    }

    async fn invoke(&self, tool: &str, input: Value, ctx: &CallContext<'_>) -> Result<Value> {
        if !TOOLS.iter().any(|t| t.name == tool) {
            return Err(TmError::parse(format!("unknown tool `{tool}`")));
        }

        if tool == "pty.spawn" {
            let argv = get_string_array(&input, "argv")?;
            let cwd = resolve_cwd(ctx.root, &input);
            let env = resolve_env(&input);
            let cols = get_u16_or(&input, "cols", DEFAULT_COLS);
            let rows = get_u16_or(&input, "rows", DEFAULT_ROWS);
            self.sessions
                .spawn(ctx, &argv, Some(cwd.as_path()), &env, cols, rows)
                .await?;
            return Ok(json!({"spawned": true, "cols": cols, "rows": rows}));
        }

        let slot = self.sessions.get(ctx).await?;
        let mut guard = slot.lock().await;
        let s = guard.as_mut().ok_or_else(|| {
            TmError::invariant("pty session for this ticket/session was already closed")
        })?;

        match tool {
            "pty.write" => {
                let b64 = get_str(&input, "data_base64")?;
                let bytes = base64_decode(b64)?;
                s.write(&bytes)?;
                Ok(json!({"written": bytes.len()}))
            }
            "pty.send" => {
                let text = get_str(&input, "text")?;
                s.send(text)?;
                Ok(json!({"sent": text}))
            }
            "pty.key" => {
                let chord = get_str(&input, "chord")?;
                s.key(chord)?;
                Ok(json!({"pressed": chord}))
            }
            "pty.screen" => Ok(screen_json(s.screen())),
            "pty.diff" => {
                let changed = s.diff();
                Ok(json!({
                    "changed": changed
                        .into_iter()
                        .map(|(row, line)| json!({"row": row, "line": line}))
                        .collect::<Vec<_>>()
                }))
            }
            "pty.resize" => {
                let cols = get_u16_or(&input, "cols", DEFAULT_COLS);
                let rows = get_u16_or(&input, "rows", DEFAULT_ROWS);
                s.resize(cols, rows)?;
                Ok(json!({"resized": true, "cols": cols, "rows": rows}))
            }
            "pty.expect" => {
                let pattern = get_str(&input, "pattern")?;
                let timeout_ms = get_u64_or(&input, "timeout_ms", 5_000);
                let outcome = s.expect(pattern, Duration::from_millis(timeout_ms));
                Ok(expect_outcome_json(outcome))
            }
            "pty.wait_exit" => {
                let status = s.wait_exit()?;
                Ok(json!({
                    "success": status.success(),
                    "exit_code": status.exit_code(),
                    "signal": status.signal(),
                }))
            }
            "pty.record" => {
                let id = s.record(self.sessions.sink.as_ref())?;
                Ok(json!({
                    "artifact": id.as_str(),
                    "truncated": s.recording_truncated(),
                }))
            }
            other => Err(TmError::parse(format!("unknown tool `{other}`"))),
        }
    }
}

/// Decode a standard (padded or unpadded) base64 string, mirroring
/// `crates/tm-browser/src/session.rs`'s own hand-rolled decoder (this crate has no base64
/// dependency either, and pty input is small enough that pulling one in for this alone is not
/// worth it).
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut reverse = [255u8; 256];
    for (i, &b) in ALPHABET.iter().enumerate() {
        reverse[b as usize] = i as u8;
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut chunk = [0u8; 4];
    let mut chunk_len = 0usize;
    for b in input.bytes() {
        if b.is_ascii_whitespace() || b == b'=' {
            continue;
        }
        let v = reverse[b as usize];
        if v == 255 {
            return Err(TmError::parse("invalid base64 input"));
        }
        chunk[chunk_len] = v;
        chunk_len += 1;
        if chunk_len == 4 {
            out.push((chunk[0] << 2) | (chunk[1] >> 4));
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
            out.push((chunk[2] << 6) | chunk[3]);
            chunk_len = 0;
        }
    }
    match chunk_len {
        0 => {}
        2 => out.push((chunk[0] << 2) | (chunk[1] >> 4)),
        3 => {
            out.push((chunk[0] << 2) | (chunk[1] >> 4));
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
        }
        _ => return Err(TmError::parse("invalid base64 input length")),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{Authority, Decision, FixedClock, ParticipantId, SystemClock};

    struct FakeSink;
    impl ArtifactSink for FakeSink {
        fn store(&self, _bytes: &[u8], _content_type: &str) -> Result<tm_types::ArtifactId> {
            tm_types::ArtifactId::new("ART-000000000001")
        }
    }

    fn test_registry() -> SessionRegistry {
        SessionRegistry::new(Arc::new(SystemClock), Arc::new(FakeSink))
    }

    fn test_ctx<'a>(
        authority: &'a Authority,
        ticket: &'a TicketId,
        session: &'a SessionId,
        actor: &'a ParticipantId,
        clock: &'a FixedClock,
        ids: &'a tm_types::TestIds,
        root: &'a std::path::Path,
    ) -> CallContext<'a> {
        CallContext {
            authority,
            ticket,
            session,
            actor,
            clock,
            ids,
            root,
        }
    }

    #[test]
    fn every_tool_has_a_unique_name_and_a_real_schema() {
        let cap = PtyCapability::new(test_registry());
        let tools = cap.tools();
        assert_eq!(tools.len(), TOOLS.len());
        let unique: std::collections::BTreeSet<&str> = tools.iter().map(|t| t.name).collect();
        assert_eq!(unique.len(), tools.len());
        for t in &tools {
            assert!(t.name.starts_with("pty."));
            assert!(t.input_schema.is_object());
        }
    }

    #[test]
    fn action_mapping_covers_every_tool_and_uses_the_right_bucket() {
        let cap = PtyCapability::new(test_registry());
        for t in TOOLS {
            let sample_input = if t.name == "pty.spawn" {
                json!({"argv": ["echo", "hi"]})
            } else {
                json!({})
            };
            let action = cap.to_action(t.name, &sample_input).unwrap();
            match t.name {
                "pty.spawn" => assert!(matches!(action, Action::PtySpawn { .. })),
                "pty.write" | "pty.send" | "pty.key" => assert_eq!(action, Action::PtySend),
                _ => assert_eq!(action, Action::PtyControl),
            }
        }
    }

    #[test]
    fn pty_send_tools_are_denied_without_the_distinct_pty_grant() {
        let cap = PtyCapability::new(test_registry());
        let mut shell_only = Authority::none();
        shell_only.shell.enabled = true;
        shell_only.shell.allow = tm_types::PatternSet::all();

        let send_action = cap.to_action("pty.send", &json!({})).unwrap();
        assert!(matches!(
            shell_only.permits(&send_action),
            Decision::Deny(_)
        ));

        let mut with_pty = shell_only.clone();
        with_pty.shell.pty = true;
        assert_eq!(with_pty.permits(&send_action), Decision::Allow);
    }

    #[test]
    fn provider_requires_shell() {
        let cap = PtyCapability::new(test_registry());
        assert!(!cap.requires().admits(&Authority::none()));
        let mut with_shell = Authority::none();
        with_shell.shell.enabled = true;
        with_shell.shell.allow = tm_types::PatternSet::all();
        assert!(cap.requires().admits(&with_shell));
    }

    #[tokio::test]
    async fn calling_a_tool_before_spawn_reports_not_found() {
        let cap = PtyCapability::new(test_registry());
        let authority = Authority::root();
        let ticket: TicketId = "T-1".parse().unwrap();
        let session: SessionId = "S-1".parse().unwrap();
        let actor: ParticipantId = "agent:test/worker".parse().unwrap();
        let clock = FixedClock::epoch();
        let ids = tm_types::TestIds::new();
        let root = std::env::temp_dir();
        let ctx = test_ctx(&authority, &ticket, &session, &actor, &clock, &ids, &root);

        let err = cap.invoke("pty.screen", json!({}), &ctx).await.unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }

    #[tokio::test]
    async fn spawn_then_screen_then_close_round_trips() {
        let cap = PtyCapability::new(test_registry());
        let authority = Authority::root();
        let ticket: TicketId = "T-1".parse().unwrap();
        let session: SessionId = "S-1".parse().unwrap();
        let actor: ParticipantId = "agent:test/worker".parse().unwrap();
        let clock = FixedClock::epoch();
        let ids = tm_types::TestIds::new();
        let root = std::env::temp_dir();
        let ctx = test_ctx(&authority, &ticket, &session, &actor, &clock, &ids, &root);

        cap.invoke(
            "pty.spawn",
            json!({"argv": ["echo", "hi-from-capability"], "env_allowlist": ["PATH"]}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(cap.sessions.live_count().await, 1);

        // Spawning again for the same key must conflict rather than silently replacing.
        let conflict = cap
            .invoke(
                "pty.spawn",
                json!({"argv": ["echo", "again"], "env_allowlist": ["PATH"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(conflict, TmError::Conflict(_)));

        let expect_result = cap
            .invoke(
                "pty.expect",
                json!({"pattern": "hi-from-capability", "timeout_ms": 5000}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(expect_result["matched"], true);

        let record_result = cap.invoke("pty.record", json!({}), &ctx).await.unwrap();
        assert_eq!(record_result["artifact"], "ART-000000000001");

        cap.close_all().await.unwrap();
        assert_eq!(cap.sessions.live_count().await, 0);
    }

    #[test]
    fn base64_round_trips() {
        let encoded_hi = "aGk="; // "hi"
        assert_eq!(base64_decode(encoded_hi).unwrap(), b"hi");
        assert!(base64_decode("not valid base64!!").is_err());
    }
}
