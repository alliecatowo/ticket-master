//! The executor boundary (`SPEC.md` §24): whatever actually does a ticket's work, kept behind
//! one trait so the scheduler routes to `tm-agent`, an external harness or a human the same way.
//!
//! [`Executor`] is deliberately thin — `id`, `capabilities`, `execute`, `cancel` — because the
//! load-bearing idea lives in [`ExecutorTask`]: every executor receives the same compiled
//! context pack (§24.1), the ticket's already-attenuated [`tm_types::Authority`] and
//! [`tm_types::Budget`], and nothing else. `tm-core` cannot depend on `tm-context` (that crate
//! depends on this one), so [`ExecutorTask::context_pack`] carries the pack pre-rendered to
//! text by the caller rather than `tm_context::ContextPack` itself — the same shape an external
//! headless harness driven over stdio would need anyway.
//!
//! This module also owns the sandbox/return-scope machinery `SPEC.md` §24.3 describes:
//! authority is enforced on *our* side of the executor boundary. [`sandbox_for`] derives what an
//! executor may touch purely from an [`Authority`], and [`validate_return_scope`] checks a
//! produced diff against that scope before a dispatcher accepts it as a submission — a
//! dispatcher-level backstop that applies to every executor, not just ones that happen to
//! self-police (`tm-agent`'s `PatchEngine` already does, an external harness never will).

use std::collections::BTreeSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tm_types::{
    ArtifactId, Authority, Budget, DecisionId, ParticipantId, PatternSet, SessionId, Spend,
    TicketId,
};

use crate::ticket::FailureClass;

/// How expensive an executor is to run, coarse enough to route on without pricing every call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostClass {
    /// No marginal cost (a human, or a free-tier provider).
    Free,
    /// Cheap: small/fast models, local compute.
    Cheap,
    /// The common case: mid-tier models.
    Standard,
    /// Frontier models or otherwise expensive compute.
    Premium,
}

/// What an [`Executor`] can do, declared once and matched against a ticket's
/// [`crate::ticket::ExecutorRequirements`] before it is ever leased work (`SPEC.md` §24.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorCapabilities {
    /// Streams output as it is produced, rather than only returning a final result.
    pub streaming: bool,
    /// Can call tools (as opposed to being a single completion).
    pub tool_use: bool,
    /// Produces a patch/diff describing its change, rather than only side effects.
    pub patch_output: bool,
    /// Can suspend mid-run and wait on a human decision.
    pub interactive: bool,
    /// Accepts a compiled [`ExecutorTask::context_pack`] instead of discovering context itself.
    pub accepts_context_pack: bool,
    /// Runs inside a sandbox that can be constrained by [`sandbox_for`], rather than with the
    /// full authority of the host process.
    pub sandboxed: bool,
    /// The largest context window this executor can accept, in tokens, if bounded.
    pub max_context_tokens: Option<u64>,
    /// Coarse cost tier, for routing under `tm-provider`'s fabric (`SPEC.md` §24.4).
    pub cost_class: CostClass,
}

/// An opaque handle to one in-flight [`Executor::execute`] call, for [`Executor::cancel`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionHandle(String);

impl ExecutionHandle {
    /// Wrap an already-rendered handle id (executor-defined shape).
    pub fn new(id: impl Into<String>) -> Self {
        ExecutionHandle(id.into())
    }

    /// The rendered handle id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One unit of work handed to an [`Executor`] — the executor-agnostic sibling of
/// `tm_agent::outcome::AgentTask`. `tm-agent`'s `BuiltinExecutor` wraps this into a real
/// `AgentTask` (and a real `tm_context::ContextPack`) on the far side of the boundary; an
/// external harness gets `context_pack` handed over as-is.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutorTask {
    /// The ticket this task executes.
    pub ticket: TicketId,
    /// The role this task is being executed as (`SPEC.md` §24.2's routing dimension).
    pub role: tm_types::Role,
    /// The ticket's objective, verbatim, for executors that want it outside the rendered pack.
    pub objective: String,
    /// The compiled, bounded context pack, rendered to text (`SPEC.md` §24.1). Deterministic
    /// given the same ticket/view/codeintel state, per `tm_context::pack::compile`.
    pub context_pack: String,
    /// The authority this task's actions are checked against — already attenuated to the
    /// ticket, never the executor's own ceiling (`SPEC.md` §24.3: authority is enforced on our
    /// side of the boundary).
    pub authority: Authority,
    /// The budget this task's spend is checked against.
    pub budget: Budget,
    /// The pinned harness epoch this task's prompts/tool policy were assembled under.
    pub harness_epoch: u64,
    /// The participant id this task executes as (the lease holder), for attribution.
    pub actor: ParticipantId,
    /// The session this task executes inside, if one applies.
    pub session: Option<SessionId>,
}

/// Why an [`Executor::execute`] call did not produce a submission, using `tm-core`'s existing
/// closed failure vocabulary so a dispatcher can call `Store::record_failure` directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorFailure {
    /// The class of failure.
    pub class: FailureClass,
    /// Free-form detail (error message, denial reason, exhaustion detail).
    pub detail: String,
}

/// What an [`Executor::execute`] call produced — evidence, artifacts, a patch, usage, and (on
/// failure) a classified reason, but never a self-certification: verification stays a separate
/// step (`SPEC.md` §11), so there is no "verified" field here for an executor to set on itself.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutorOutcome {
    /// The ticket this outcome is for.
    pub ticket: TicketId,
    /// One-line human-readable submission summary, passed to `Store::submit` on success.
    pub summary: String,
    /// Artifacts already stored (e.g. command output, test runs) to attach as submission
    /// evidence.
    pub evidence: Vec<ArtifactId>,
    /// A unified diff describing the change, if the executor produced one as a single blob
    /// (typical of an external harness) rather than already-applied, individually-authorized
    /// edits (typical of `tm-agent`'s own `PatchEngine`). Validated by [`validate_return_scope`]
    /// before a dispatcher accepts it.
    pub patch: Option<String>,
    /// Token/dollar/time spend this run consumed.
    pub usage: Spend,
    /// Decisions recorded during the run that a verifier/auditor should be aware of.
    pub decisions: Vec<DecisionId>,
    /// `Some` when the run did not reach a submittable state; `None` means success.
    pub failure: Option<ExecutorFailure>,
}

impl ExecutorOutcome {
    /// True when this outcome represents a submission (i.e. [`ExecutorOutcome::failure`] is
    /// `None`).
    pub fn is_success(&self) -> bool {
        self.failure.is_none()
    }
}

/// Whatever actually does a ticket's work (`SPEC.md` §24.2): `tm-agent`'s own loop, an external
/// harness (`claude-code`, `codex`, ...), or a human. The scheduler routes to one via a
/// dispatcher (`tm-scheduler`'s `ExecutorDispatcher`) rather than assuming it is its own agent.
#[async_trait]
pub trait Executor: Send + Sync {
    /// A stable id for this executor (used to build the lease holder's `ParticipantId`, e.g.
    /// `agent:<id>/<ticket>`), distinct from the *role* it happens to be serving.
    fn id(&self) -> &str;

    /// What this executor can do, matched against a ticket's `ExecutorRequirements` before it
    /// is dispatched (refuse a mismatch rather than degrading quietly).
    fn capabilities(&self) -> ExecutorCapabilities;

    /// Run `task` to completion, returning an [`ExecutorOutcome`] — never a partial/suspended
    /// state; an executor that itself supports suspension (e.g. `tm-agent`'s approval flow)
    /// resolves it internally before returning.
    async fn execute(&self, task: ExecutorTask) -> tm_types::Result<ExecutorOutcome>;

    /// Best-effort cancellation of a previously-started [`Executor::execute`] call.
    async fn cancel(&self, handle: &ExecutionHandle) -> tm_types::Result<()>;
}

/// Filesystem read/write scope an executor's sandbox should be constrained to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsScope {
    /// Paths the sandbox may read.
    pub read: PatternSet,
    /// Paths the sandbox may write.
    pub write: PatternSet,
}

/// Network reach an executor's sandbox should be constrained to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetPolicy {
    /// Any host is reachable.
    pub arbitrary: bool,
    /// Documentation hosts (`NetworkAuthority::DOC_HOSTS`) are reachable.
    pub docs: bool,
    /// Additional explicitly-allowed hosts.
    pub allowlist: BTreeSet<String>,
}

/// The sandbox an executor should run inside, derived purely from an [`Authority`]
/// (`SPEC.md` §24.3). Never a second source of truth for what is permitted — [`sandbox_for`] is
/// a pure projection of the same `Authority` fields `Authority::permits` already checks, not a
/// reimplementation of the authority algebra.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sandbox {
    /// Filesystem scope.
    pub fs_scope: FsScope,
    /// Network policy.
    pub net_policy: NetPolicy,
    /// Command lines the sandbox may run (empty when `shell.enabled` is false).
    pub cmd_allow: PatternSet,
}

/// Derive the sandbox an executor should be constrained to from `authority`. Pure projection:
/// narrower authority always yields a narrower (or equally narrow) sandbox, never a broader one.
pub fn sandbox_for(authority: &Authority) -> Sandbox {
    Sandbox {
        fs_scope: FsScope {
            read: authority.repository.read.clone(),
            write: authority.repository.write.clone(),
        },
        net_policy: NetPolicy {
            arbitrary: authority.network.arbitrary,
            docs: authority.network.docs,
            allowlist: authority.network.allowlist.clone(),
        },
        cmd_allow: if authority.shell.enabled {
            authority.shell.allow.clone()
        } else {
            PatternSet::empty()
        },
    }
}

/// Why [`validate_return_scope`] refused a diff.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReturnScopeViolation {
    /// A path the diff touches is outside `write_scope`.
    #[error("diff touches {path}, outside the granted write scope")]
    OutOfScope {
        /// The offending path.
        path: String,
    },
    /// The diff was non-empty but no touched path could be identified (fails closed: an
    /// unparseable diff is never accepted as in-scope).
    #[error("diff could not be parsed to identify touched paths")]
    Unparseable,
}

/// Extract the paths a unified diff touches from its `+++ b/<path>` (or `diff --git a/.. b/..`)
/// headers. Not a full diff parser — just enough to name what a return-scope check needs, since
/// `tm-core` cannot depend on a real diff-parsing crate without depending on `tm-agent`/
/// `similar`.
fn touched_paths(diff: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let rest = rest.trim();
            if rest == "/dev/null" {
                continue;
            }
            let path = rest.strip_prefix("b/").unwrap_or(rest);
            if !path.is_empty() {
                paths.push(path.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some((_, b)) = rest.split_once(' ') {
                let path = b.trim().strip_prefix("b/").unwrap_or(b.trim());
                if !path.is_empty() {
                    paths.push(path.to_string());
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Check that every path `diff` touches is within `write_scope`, per `SPEC.md` §24.3's
/// "constrain it and check its output". An empty diff trivially passes (nothing was touched); a
/// non-empty diff whose touched paths cannot be identified fails closed rather than being
/// accepted on trust.
pub fn validate_return_scope(
    diff: &str,
    write_scope: &PatternSet,
) -> Result<Vec<String>, ReturnScopeViolation> {
    if diff.trim().is_empty() {
        return Ok(Vec::new());
    }
    let paths = touched_paths(diff);
    if paths.is_empty() {
        return Err(ReturnScopeViolation::Unparseable);
    }
    for path in &paths {
        if !write_scope.matches(path) {
            return Err(ReturnScopeViolation::OutOfScope { path: path.clone() });
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{GitAuthority, RepoAuthority, ShellAuthority};

    fn restricted_authority() -> Authority {
        Authority {
            repository: RepoAuthority {
                read: PatternSet::parse(["src/**"]).unwrap(),
                write: PatternSet::parse(["src/widget/**"]).unwrap(),
            },
            shell: ShellAuthority {
                enabled: true,
                allow: PatternSet::parse(["cargo test*"]).unwrap(),
                deny: PatternSet::empty(),
                pty: false,
            },
            git: GitAuthority::default(),
            ..Authority::none()
        }
    }

    #[test]
    fn sandbox_for_narrows_with_a_restricted_authority() {
        let root_sandbox = sandbox_for(&Authority::root());
        let scoped_sandbox = sandbox_for(&restricted_authority());

        assert!(scoped_sandbox.fs_scope.write.matches("src/widget/lib.rs"));
        assert!(!scoped_sandbox.fs_scope.write.matches("other/lib.rs"));
        assert!(root_sandbox.fs_scope.write.matches("other/lib.rs"));

        assert!(!scoped_sandbox.net_policy.arbitrary);
        assert!(root_sandbox.net_policy.arbitrary);

        assert!(scoped_sandbox.cmd_allow.matches_text("cargo test --all"));
        assert!(!scoped_sandbox.cmd_allow.matches_text("rm -rf /"));
    }

    #[test]
    fn sandbox_for_disabled_shell_yields_no_allowed_commands() {
        let authority = Authority::none();
        let sandbox = sandbox_for(&authority);
        assert!(sandbox.cmd_allow.is_empty());
    }

    #[test]
    fn validate_return_scope_accepts_a_diff_within_scope() {
        let write_scope = PatternSet::parse(["src/widget/**"]).unwrap();
        let diff = "diff --git a/src/widget/lib.rs b/src/widget/lib.rs\n\
                     --- a/src/widget/lib.rs\n\
                     +++ b/src/widget/lib.rs\n\
                     @@ -1 +1 @@\n\
                     -old\n\
                     +new\n";
        let touched = validate_return_scope(diff, &write_scope).expect("in scope");
        assert_eq!(touched, vec!["src/widget/lib.rs".to_string()]);
    }

    #[test]
    fn validate_return_scope_rejects_a_diff_outside_scope() {
        let write_scope = PatternSet::parse(["src/widget/**"]).unwrap();
        let diff = "diff --git a/src/other/lib.rs b/src/other/lib.rs\n\
                     --- a/src/other/lib.rs\n\
                     +++ b/src/other/lib.rs\n\
                     @@ -1 +1 @@\n\
                     -old\n\
                     +new\n";
        let err = validate_return_scope(diff, &write_scope).unwrap_err();
        assert_eq!(
            err,
            ReturnScopeViolation::OutOfScope {
                path: "src/other/lib.rs".to_string()
            }
        );
    }

    #[test]
    fn validate_return_scope_fails_closed_on_unparseable_diff() {
        let write_scope = PatternSet::parse(["src/widget/**"]).unwrap();
        let err = validate_return_scope("not a diff at all", &write_scope).unwrap_err();
        assert_eq!(err, ReturnScopeViolation::Unparseable);
    }

    #[test]
    fn validate_return_scope_accepts_an_empty_diff() {
        let write_scope = PatternSet::parse(["src/widget/**"]).unwrap();
        assert_eq!(validate_return_scope("", &write_scope), Ok(Vec::new()));
    }
}
