//! `hooks.toml`: `PreToolUse`/`PostToolUse`/`UserPromptSubmit`/`SessionStart`/`Stop` handlers
//! (`docs/audit-2026-09-18-fable.md` M-04), matching the event vocabulary this repo's own
//! `CLAUDE.md` already documents Claude Code itself using for hooks.
//!
//! **Shell-only, not "shell or in-process"** — the audit's own phrasing names both as acceptable
//! and calls out picking the smaller cut explicitly. An in-process hook would need a Rust trait
//! object (or a scripting-language embed) configured from `hooks.toml`, which is real added
//! surface for a feature this workspace has zero callers of yet; a shell hook is one
//! `std::process::Command`-shaped contract, testable with `/bin/sh -c '...'`, and covers the
//! stated use cases (a linter gate, a policy check, a notification) without a new plugin
//! mechanism. See `docs/decisions/D-013-hooks-agents-skills.md` for the full tradeoff.
//!
//! # Where each event fires
//!
//! `PreToolUse`/`PostToolUse` are evaluated inside [`crate::tools::ToolRegistry::dispatch`],
//! before/after a tool call — see that method. `UserPromptSubmit`/`SessionStart`/`Stop` are not
//! tool-scoped at all; `tm-cli`'s `AgentSession` (`crates/tm-cli/src/agent.rs`) calls
//! [`HookConfig::evaluate_user_prompt_submit`]/[`HookConfig::run_session_start`]/
//! [`HookConfig::run_stop`] at its own lifecycle points (turn start, session start, turn end).
//!
//! # Wire contract
//!
//! A hook process receives one JSON object on stdin: `{"event": "<Name>", ...fields...}`
//! (`tool`/`input` for `PreToolUse`/`PostToolUse`, `prompt` for `UserPromptSubmit`, `session`
//! and — where applicable — `ticket` on all of them). It may write nothing (silently treated as
//! `allow`), or a JSON object on stdout:
//! `{"decision": "allow" | "deny" | "rewrite", "reason": "...", "updated_input": <object>,
//! "updated_prompt": "..."}` (`updated_input` for a tool-call rewrite, `updated_prompt` for a
//! `UserPromptSubmit` rewrite; `reason` is shown as the denial detail).
//!
//! **Fail-closed**: a hook that cannot be spawned, times out, or exits non-zero is treated as
//! `deny` (see [`run_hook_entry`]) — a broken hook script blocks the calls it's configured
//! against rather than silently granting them, matching this workspace's deny-by-default
//! `Authority` model. `PostToolUse`/`SessionStart`/`Stop` do not consume a decision at all (the
//! tool call, or the turn, has already happened); they run for side effects only, and a failure
//! is logged, never surfaced as a denial of something already completed.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tm_types::{CallContext, SessionId};

/// The `hooks.toml` filename, resolved relative to a project's root — mirrors
/// `tm-cli::dispatch::OVERSIGHT_TOML_FILENAME`'s convention for `oversight.toml`.
pub const HOOKS_TOML_FILENAME: &str = "hooks.toml";

/// How long [`run_hook_entry`] waits for one hook process before treating it as failed (and
/// therefore denied — see this module's doc comment). Generous enough for a real lint/policy
/// script, short enough that a hung hook cannot stall an agent turn indefinitely.
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// One configured hook: which command to run, and (for `PreToolUse`/`PostToolUse` only) which
/// tool names it applies to.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookEntry {
    /// Which tool name this hook applies to; `None` or `Some("*")` matches every tool. Ignored
    /// by non-tool-scoped events (`UserPromptSubmit`/`SessionStart`/`Stop`).
    #[serde(default)]
    pub matcher: Option<String>,
    /// argv: `command[0]` is spawned directly (not through a shell) with `command[1..]` as its
    /// arguments. Use `["/bin/sh", "-c", "..."]` for an inline script.
    pub command: Vec<String>,
}

impl HookEntry {
    /// Whether this entry applies to `tool` (only meaningful for `PreToolUse`/`PostToolUse`;
    /// callers for other events don't call this).
    fn matches(&self, tool: &str) -> bool {
        match self.matcher.as_deref() {
            None | Some("*") => true,
            Some(pattern) => pattern == tool,
        }
    }
}

/// The full parsed `hooks.toml`: zero or more [`HookEntry`] per event name. Every field defaults
/// to empty, so a project with no `hooks.toml` (or an empty one) runs exactly as it did before
/// this feature existed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct HookConfig {
    /// Runs before a tool call reaches `Authority::permits`; may deny, rewrite the call's
    /// input, or allow.
    #[serde(default)]
    pub pre_tool_use: Vec<HookEntry>,
    /// Runs after a tool call completes (observational only — see this module's doc comment).
    #[serde(default)]
    pub post_tool_use: Vec<HookEntry>,
    /// Runs before a submitted prompt starts a turn; may deny, rewrite the prompt text, or
    /// allow.
    #[serde(default)]
    pub user_prompt_submit: Vec<HookEntry>,
    /// Runs once per session, at session start (observational only).
    #[serde(default)]
    pub session_start: Vec<HookEntry>,
    /// Runs when a turn ends (observational only).
    #[serde(default)]
    pub stop: Vec<HookEntry>,
}

/// A hook's effect on the call/prompt it was evaluated against.
#[derive(Debug, Clone, PartialEq)]
pub enum HookDecision {
    /// No hook objected; proceed with (possibly already-rewritten) input unchanged.
    Allow,
    /// A hook refused this call/prompt; `String` is shown as the denial detail.
    Deny(String),
    /// A hook replaced the input/prompt; the replacement is the final value only if no
    /// subsequent hook denies it.
    Rewrite(Value),
}

/// A single hook process's raw stdout, parsed. Every field optional: an empty or non-JSON
/// stdout on a successful exit parses to "no opinion" ([`HookDecision::Allow`]).
#[derive(Debug, Default, Deserialize)]
struct HookResponse {
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    updated_input: Option<Value>,
    #[serde(default)]
    updated_prompt: Option<String>,
}

/// Load `<root>/hooks.toml`, or [`HookConfig::default`] (every list empty) when the file does
/// not exist.
///
/// # Errors
/// `TmError::Io` if the file exists but cannot be read; `TmError::Parse` if it exists but is not
/// valid `hooks.toml` (including an unknown top-level key, `deny_unknown_fields` — a typo like
/// `pre_tool_us` must fail loudly, not silently configure nothing, mirroring
/// `tm-cli::dispatch::load_oversight`'s same choice for `oversight.toml`).
pub fn load_hooks_toml(root: &Path) -> tm_types::Result<HookConfig> {
    let path = root.join(HOOKS_TOML_FILENAME);
    if !path.exists() {
        return Ok(HookConfig::default());
    }
    let source = std::fs::read_to_string(&path)
        .map_err(|e| tm_types::TmError::Io(format!("reading {}: {e}", path.display())))?;
    toml::from_str(&source)
        .map_err(|e| tm_types::TmError::parse(format!("{}: {e}", path.display())))
}

impl HookConfig {
    /// Run every `pre_tool_use` entry matching `tool`, in configured order, threading a
    /// rewritten input through subsequent entries and short-circuiting on the first deny.
    /// Called from [`crate::tools::ToolRegistry::dispatch`], before `Authority::permits`.
    pub async fn evaluate_pre_tool_use(
        &self,
        tool: &str,
        input: &Value,
        ctx: &CallContext<'_>,
    ) -> HookDecision {
        let mut current = input.clone();
        let mut rewritten = false;
        for entry in self.pre_tool_use.iter().filter(|e| e.matches(tool)) {
            let envelope = tool_envelope("PreToolUse", tool, &current, ctx);
            match run_hook_entry(entry, &envelope).await {
                HookDecision::Deny(reason) => return HookDecision::Deny(reason),
                HookDecision::Rewrite(v) => {
                    current = v;
                    rewritten = true;
                }
                HookDecision::Allow => {}
            }
        }
        if rewritten {
            HookDecision::Rewrite(current)
        } else {
            HookDecision::Allow
        }
    }

    /// Run every `post_tool_use` entry matching `tool`, for side effects only — the tool call
    /// already completed, so no decision from these entries changes anything; a hook failure is
    /// swallowed (not surfaced as a tool error) after being logged.
    pub async fn run_post_tool_use(
        &self,
        tool: &str,
        input: &Value,
        outcome_summary: &str,
        ctx: &CallContext<'_>,
    ) {
        for entry in self.post_tool_use.iter().filter(|e| e.matches(tool)) {
            let mut envelope = tool_envelope("PostToolUse", tool, input, ctx);
            envelope["outcome"] = json!(outcome_summary);
            if let HookDecision::Deny(reason) = run_hook_entry(entry, &envelope).await {
                tracing::warn!(
                    tool,
                    hook = ?entry.command,
                    reason,
                    "PostToolUse hook reported failure (ignored: the tool call already completed)"
                );
            }
        }
    }

    /// Run every `user_prompt_submit` entry, in configured order, threading a rewritten prompt
    /// through subsequent entries and short-circuiting on the first deny. Called from
    /// `tm-cli`'s `AgentSession` before a submitted prompt starts a turn.
    pub async fn evaluate_user_prompt_submit(
        &self,
        prompt: &str,
        session: &SessionId,
    ) -> HookDecision {
        let mut current = Value::String(prompt.to_string());
        let mut rewritten = false;
        for entry in &self.user_prompt_submit {
            let envelope = json!({
                "event": "UserPromptSubmit",
                "prompt": current,
                "session": session.as_str(),
            });
            match run_hook_entry(entry, &envelope).await {
                HookDecision::Deny(reason) => return HookDecision::Deny(reason),
                HookDecision::Rewrite(v) => {
                    current = v;
                    rewritten = true;
                }
                HookDecision::Allow => {}
            }
        }
        if rewritten {
            HookDecision::Rewrite(current)
        } else {
            HookDecision::Allow
        }
    }

    /// Run every `session_start` entry for side effects only.
    pub async fn run_session_start(&self, session: &SessionId) {
        for entry in &self.session_start {
            let envelope = json!({"event": "SessionStart", "session": session.as_str()});
            let _ = run_hook_entry(entry, &envelope).await;
        }
    }

    /// Run every `stop` entry for side effects only.
    pub async fn run_stop(&self, session: &SessionId, ticket: Option<&str>) {
        for entry in &self.stop {
            let envelope = json!({"event": "Stop", "session": session.as_str(), "ticket": ticket});
            let _ = run_hook_entry(entry, &envelope).await;
        }
    }
}

/// Build the stdin envelope for a `PreToolUse`/`PostToolUse` hook.
fn tool_envelope(event: &str, tool: &str, input: &Value, ctx: &CallContext<'_>) -> Value {
    json!({
        "event": event,
        "tool": tool,
        "input": input,
        "ticket": ctx.ticket.map(|t| t.as_str()),
        "session": ctx.session.as_str(),
    })
}

/// Spawn one hook process, write `envelope` to its stdin as JSON, and interpret its outcome.
/// Never returns an `Err`-shaped failure to its caller: every execution problem (spawn failure,
/// non-zero exit, timeout) becomes [`HookDecision::Deny`] (see this module's doc comment on
/// fail-closed behavior); a clean exit with empty or non-JSON stdout is [`HookDecision::Allow`].
async fn run_hook_entry(entry: &HookEntry, envelope: &Value) -> HookDecision {
    let Some(program) = entry.command.first() else {
        // An entry with an empty `command` has nothing to run; treat as no opinion rather than
        // failing every call it's matched against over a config mistake with no security
        // implication either way.
        return HookDecision::Allow;
    };

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&entry.command[1..]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return HookDecision::Deny(format!(
                "hook `{}` failed to start: {e}",
                entry.command.join(" ")
            ));
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let payload = serde_json::to_vec(envelope).unwrap_or_default();
        // A hook that closes stdin early (never reads it) is not this call's problem to
        // diagnose; the write failing just means the hook didn't want its input, not that the
        // hook itself failed.
        let _ = stdin.write_all(&payload).await;
        drop(stdin);
    }

    let output = match tokio::time::timeout(HOOK_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return HookDecision::Deny(format!("hook `{}` failed: {e}", entry.command.join(" ")));
        }
        Err(_) => {
            return HookDecision::Deny(format!(
                "hook `{}` timed out after {:?}",
                entry.command.join(" "),
                HOOK_TIMEOUT
            ));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let reason = if stderr.is_empty() {
            format!(
                "hook `{}` exited with status {}",
                entry.command.join(" "),
                output.status
            )
        } else {
            stderr
        };
        return HookDecision::Deny(reason);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return HookDecision::Allow;
    }

    let parsed: HookResponse = match serde_json::from_str(trimmed) {
        Ok(p) => p,
        // A hook that exits 0 but doesn't emit the structured contract on stdout has no
        // opinion, not a failure — e.g. a script whose only job is a side effect (a log line, a
        // notification).
        Err(_) => return HookDecision::Allow,
    };

    match parsed.decision.as_deref() {
        Some("deny") => HookDecision::Deny(
            parsed
                .reason
                .unwrap_or_else(|| "denied by hook".to_string()),
        ),
        Some("rewrite") => {
            if let Some(v) = parsed.updated_input {
                HookDecision::Rewrite(v)
            } else if let Some(s) = parsed.updated_prompt {
                HookDecision::Rewrite(Value::String(s))
            } else {
                HookDecision::Allow
            }
        }
        _ => HookDecision::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{Authority, FixedClock, ParticipantId, TestIds, TicketId};

    fn test_ctx_pieces() -> (
        Authority,
        TicketId,
        SessionId,
        ParticipantId,
        FixedClock,
        TestIds,
    ) {
        let authority = Authority::root();
        let ticket: TicketId = "T-1".parse().expect("valid ticket id");
        let session: SessionId = "S-1".parse().expect("valid session id");
        let actor: ParticipantId = "agent:test/worker".parse().expect("valid participant id");
        let clock = FixedClock::epoch();
        let ids = TestIds::new();
        (authority, ticket, session, actor, clock, ids)
    }

    /// `entry.command = ["/bin/sh", "-c", <script>]` — no temp file, no executable bit, no
    /// portability question across the platforms this repo's tests run on.
    fn sh_hook(script: &str) -> HookEntry {
        HookEntry {
            matcher: None,
            command: vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
        }
    }

    #[test]
    fn load_hooks_toml_defaults_to_empty_without_a_hooks_toml() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let config = load_hooks_toml(dir.path()).expect("no error");
        assert!(config.pre_tool_use.is_empty());
        assert!(config.post_tool_use.is_empty());
    }

    #[test]
    fn load_hooks_toml_parses_pre_tool_use_entries() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join(HOOKS_TOML_FILENAME),
            "[[pre_tool_use]]\nmatcher = \"shell.run\"\ncommand = [\"/bin/sh\", \"-c\", \"exit 0\"]\n",
        )
        .expect("write hooks.toml");

        let config = load_hooks_toml(dir.path()).expect("parses");
        assert_eq!(config.pre_tool_use.len(), 1);
        assert_eq!(config.pre_tool_use[0].matcher.as_deref(), Some("shell.run"));
    }

    #[test]
    fn load_hooks_toml_rejects_an_unknown_top_level_key() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join(HOOKS_TOML_FILENAME), "pre_tool_us = []\n")
            .expect("write hooks.toml");

        assert!(load_hooks_toml(dir.path()).is_err());
    }

    #[tokio::test]
    async fn evaluate_pre_tool_use_with_no_matching_entries_allows() {
        let (authority, ticket, session, actor, clock, ids) = test_ctx_pieces();
        let ctx = CallContext {
            authority: &authority,
            ticket: Some(&ticket),
            session: &session,
            actor: &actor,
            clock: &clock,
            ids: &ids,
            root: Path::new("/tmp"),
        };
        let config = HookConfig::default();
        let decision = config
            .evaluate_pre_tool_use("shell.run", &json!({"argv": ["echo", "hi"]}), &ctx)
            .await;
        assert_eq!(decision, HookDecision::Allow);
    }

    /// The task's own required-test bar: a real `hooks.toml` `PreToolUse` hook that denies a
    /// specific tool call actually blocks it.
    #[tokio::test]
    async fn evaluate_pre_tool_use_deny_hook_blocks_the_call() {
        let (authority, ticket, session, actor, clock, ids) = test_ctx_pieces();
        let ctx = CallContext {
            authority: &authority,
            ticket: Some(&ticket),
            session: &session,
            actor: &actor,
            clock: &clock,
            ids: &ids,
            root: Path::new("/tmp"),
        };
        let mut config = HookConfig::default();
        config.pre_tool_use.push(sh_hook(
            r#"printf '{"decision":"deny","reason":"blocked by policy"}'"#,
        ));

        let decision = config
            .evaluate_pre_tool_use("shell.run", &json!({"argv": ["rm", "-rf", "/"]}), &ctx)
            .await;
        assert_eq!(
            decision,
            HookDecision::Deny("blocked by policy".to_string())
        );
    }

    /// ...and one that allows does not.
    #[tokio::test]
    async fn evaluate_pre_tool_use_allow_hook_does_not_block() {
        let (authority, ticket, session, actor, clock, ids) = test_ctx_pieces();
        let ctx = CallContext {
            authority: &authority,
            ticket: Some(&ticket),
            session: &session,
            actor: &actor,
            clock: &clock,
            ids: &ids,
            root: Path::new("/tmp"),
        };
        let mut config = HookConfig::default();
        config
            .pre_tool_use
            .push(sh_hook(r#"printf '{"decision":"allow"}'"#));

        let decision = config
            .evaluate_pre_tool_use("shell.run", &json!({"argv": ["echo", "hi"]}), &ctx)
            .await;
        assert_eq!(decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn evaluate_pre_tool_use_rewrite_hook_replaces_input() {
        let (authority, ticket, session, actor, clock, ids) = test_ctx_pieces();
        let ctx = CallContext {
            authority: &authority,
            ticket: Some(&ticket),
            session: &session,
            actor: &actor,
            clock: &clock,
            ids: &ids,
            root: Path::new("/tmp"),
        };
        let mut config = HookConfig::default();
        config.pre_tool_use.push(sh_hook(
            r#"printf '{"decision":"rewrite","updated_input":{"argv":["echo","safe"]}}'"#,
        ));

        let decision = config
            .evaluate_pre_tool_use("shell.run", &json!({"argv": ["echo", "hi"]}), &ctx)
            .await;
        assert_eq!(
            decision,
            HookDecision::Rewrite(json!({"argv": ["echo", "safe"]}))
        );
    }

    #[tokio::test]
    async fn evaluate_pre_tool_use_matcher_only_applies_to_the_named_tool() {
        let (authority, ticket, session, actor, clock, ids) = test_ctx_pieces();
        let ctx = CallContext {
            authority: &authority,
            ticket: Some(&ticket),
            session: &session,
            actor: &actor,
            clock: &clock,
            ids: &ids,
            root: Path::new("/tmp"),
        };
        let mut config = HookConfig::default();
        config.pre_tool_use.push(HookEntry {
            matcher: Some("git.commit".to_string()),
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf '{"decision":"deny","reason":"no commits"}'"#.to_string(),
            ],
        });

        let decision = config
            .evaluate_pre_tool_use("shell.run", &json!({"argv": ["echo", "hi"]}), &ctx)
            .await;
        assert_eq!(
            decision,
            HookDecision::Allow,
            "non-matching tool must not be denied"
        );
    }

    #[tokio::test]
    async fn a_nonzero_exit_denies_fail_closed() {
        let (authority, ticket, session, actor, clock, ids) = test_ctx_pieces();
        let ctx = CallContext {
            authority: &authority,
            ticket: Some(&ticket),
            session: &session,
            actor: &actor,
            clock: &clock,
            ids: &ids,
            root: Path::new("/tmp"),
        };
        let mut config = HookConfig::default();
        config.pre_tool_use.push(sh_hook("echo 'boom' >&2; exit 1"));

        let decision = config
            .evaluate_pre_tool_use("shell.run", &json!({"argv": ["echo", "hi"]}), &ctx)
            .await;
        assert_eq!(decision, HookDecision::Deny("boom".to_string()));
    }

    #[tokio::test]
    async fn a_missing_command_denies_fail_closed() {
        let (authority, ticket, session, actor, clock, ids) = test_ctx_pieces();
        let ctx = CallContext {
            authority: &authority,
            ticket: Some(&ticket),
            session: &session,
            actor: &actor,
            clock: &clock,
            ids: &ids,
            root: Path::new("/tmp"),
        };
        let mut config = HookConfig::default();
        config.pre_tool_use.push(HookEntry {
            matcher: None,
            command: vec!["/definitely/not/a/real/binary-xyz".to_string()],
        });

        let decision = config
            .evaluate_pre_tool_use("shell.run", &json!({"argv": ["echo", "hi"]}), &ctx)
            .await;
        assert!(matches!(decision, HookDecision::Deny(_)));
    }

    #[tokio::test]
    async fn user_prompt_submit_deny_hook_blocks_the_prompt() {
        let session: SessionId = "S-1".parse().expect("valid session id");
        let mut config = HookConfig::default();
        config.user_prompt_submit.push(sh_hook(
            r#"printf '{"decision":"deny","reason":"no thanks"}'"#,
        ));

        let decision = config
            .evaluate_user_prompt_submit("delete everything", &session)
            .await;
        assert_eq!(decision, HookDecision::Deny("no thanks".to_string()));
    }

    #[tokio::test]
    async fn user_prompt_submit_rewrite_hook_replaces_the_prompt() {
        let session: SessionId = "S-1".parse().expect("valid session id");
        let mut config = HookConfig::default();
        config.user_prompt_submit.push(sh_hook(
            r#"printf '{"decision":"rewrite","updated_prompt":"safer prompt"}'"#,
        ));

        let decision = config
            .evaluate_user_prompt_submit("original prompt", &session)
            .await;
        assert_eq!(
            decision,
            HookDecision::Rewrite(Value::String("safer prompt".to_string()))
        );
    }

    #[tokio::test]
    async fn session_start_and_stop_run_without_panicking_when_configured() {
        let session: SessionId = "S-1".parse().expect("valid session id");
        let mut config = HookConfig::default();
        config.session_start.push(sh_hook("exit 0"));
        config.stop.push(sh_hook("exit 0"));

        config.run_session_start(&session).await;
        config.run_stop(&session, Some("T-1")).await;
    }

    #[tokio::test]
    async fn post_tool_use_failure_is_logged_not_propagated() {
        let (authority, ticket, session, actor, clock, ids) = test_ctx_pieces();
        let ctx = CallContext {
            authority: &authority,
            ticket: Some(&ticket),
            session: &session,
            actor: &actor,
            clock: &clock,
            ids: &ids,
            root: Path::new("/tmp"),
        };
        let mut config = HookConfig::default();
        config.post_tool_use.push(sh_hook("exit 1"));

        // Must not panic or otherwise surface the failure to the caller.
        config
            .run_post_tool_use(
                "shell.run",
                &json!({"argv": ["echo", "hi"]}),
                "completed",
                &ctx,
            )
            .await;
    }
}
