//! [`CapabilityProvider`]: the polymorphic tool-registration seam `docs/audit-2026-09-18-fable.md`
//! A-01 names. A capability crate (the in-tree builtin tool set, and — in a later change, not
//! this one — `tm-browser`/`tm-computer`/a PTY/an MCP client) implements this trait once and is
//! registered by whatever binary assembles a [`crate::Authority`]-gated tool surface; nothing
//! else has to change for a new capability to become reachable by an agent.
//!
//! This lives in `tm-types`, not `tm-agent`, on purpose: a capability crate (e.g. `tm-browser`)
//! must be able to implement [`CapabilityProvider`] without depending on `tm-agent`, and
//! `tm-agent` in turn depends on nothing but this trait plus whatever providers a binary hands
//! it. Concretely this means [`CallContext`] cannot carry a `&CodeIntel`/`&Store`/`&PatchEngine`
//! — those crates sit *above* `tm-types` in the dependency graph — so a provider that needs them
//! (the builtin one does) owns them itself, constructed once, rather than receiving them fresh
//! on every call. [`CallContext`] only carries what's expressible at this layer and genuinely
//! provider-agnostic: authority, identity and injected time/id sources, plus the project root
//! every filesystem-touching capability (builtin `fs.*`/`edit.*` today, browser downloads or
//! computer screenshots tomorrow) needs.
//!
//! The shape follows `tm_mirror::Tracker` (`crates/tm-mirror/src/tracker.rs`), the closest
//! existing convention in this codebase per A-03: a `&str`-returning identity method, a
//! declared-capabilities method (here, `tools()`), async I/O methods, and `tm_types::Result`
//! rather than a bespoke error enum.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::action::{Action, GitOp, TicketOp};
use crate::authority::Authority;
use crate::clock::{Clock, IdSource};
use crate::error::Result;
use crate::id::{ParticipantId, SessionId, TicketId};

/// How expensive one tool call is, coarse enough to route/budget on without pricing every call
/// (moved here from `tm-agent::tools` so a `ToolSchema` can carry it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostClass {
    /// Pure in-memory read, effectively free (`fs.stat`, `symbol.outline`, ...).
    Free,
    /// A bounded local read or index query (`fs.read`, `search.exact`, ...).
    Cheap,
    /// A recomputation over the codebase or a subprocess (`search.semantic`, `build.run`, ...).
    Moderate,
    /// A write, a shell command with side effects, or anything that mutates project state.
    Mutating,
    /// Suspends the loop for external (human) input.
    Blocking,
}

/// A structural, argument-independent precondition on an [`Authority`], used to decide whether a
/// tool (or a whole capability) is worth advertising at all — `SPEC.md` §30.1: "A ticket whose
/// authority disallows shell does not receive shell tool definitions ... no schema in the
/// context at all", not merely a tool that would be denied at call time.
///
/// This is deliberately a small composable tree rather than a closure so it stays
/// `Debug`/`Clone`/inspectable (a capability's declared requirement is itself worth logging and
/// testing), and so a future capability can build a requirement out of the same primitives a
/// current one uses without reaching into `tm-agent` for a helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityRequirement {
    /// Always admitted, independent of `authority`. Used by a provider whose *tools*
    /// individually carry the real per-tool requirement (see `ToolSchema::requires`) and which
    /// therefore has no single coarse precondition of its own — e.g. the builtin capability,
    /// whose 39 tools span nearly every bucket below.
    Always,
    /// Admitted iff `authority.repository.read` is nonempty.
    RepoRead,
    /// Admitted iff `authority.repository.write` is nonempty.
    RepoWrite,
    /// Admitted iff shell execution is enabled and at least one command line is allowed.
    Shell,
    /// Admitted iff `authority.git` holds the named operation.
    Git(GitOp),
    /// Admitted iff `authority.tickets` holds the named operation.
    Ticket(TicketOp),
    /// Admitted iff `authority.network` permits reaching anything at all (docs, arbitrary, or a
    /// nonempty allowlist). Covers `browser.navigate` and every other `tm-browser` tool
    /// (`docs/audit-2026-09-18-fable.md` B-02 — see `crates/tm-browser/src/capability.rs`).
    Network,
    /// Admitted iff `authority.computer.input` is granted (`SPEC.md` §20.5).
    ComputerInput,
    /// Admitted iff `authority.computer.capture` is granted (`SPEC.md` §20.5).
    ComputerCapture,
    /// Admitted iff `authority.computer.clipboard` is granted (`SPEC.md` §20.5).
    ComputerClipboard,
    /// Admitted iff every inner requirement is.
    All(Vec<AuthorityRequirement>),
    /// Admitted iff at least one inner requirement is.
    Any(Vec<AuthorityRequirement>),
}

impl AuthorityRequirement {
    /// Whether `authority` structurally satisfies this requirement. See the type's doc comment:
    /// this approximates [`Authority::permits`] at listing time, before any call input exists.
    pub fn admits(&self, authority: &Authority) -> bool {
        match self {
            AuthorityRequirement::Always => true,
            AuthorityRequirement::RepoRead => !authority.repository.read.is_empty(),
            AuthorityRequirement::RepoWrite => !authority.repository.write.is_empty(),
            AuthorityRequirement::Shell => {
                authority.shell.enabled && !authority.shell.allow.is_empty()
            }
            AuthorityRequirement::Git(op) => match op {
                GitOp::Commit => authority.git.commit,
                GitOp::Branch => authority.git.branch,
                GitOp::Merge => authority.git.merge,
                GitOp::Push => authority.git.push,
                GitOp::ForcePush => authority.git.force,
            },
            AuthorityRequirement::Ticket(op) => match op {
                TicketOp::CreateChild => authority.tickets.create_children,
                TicketOp::Delegate => authority.tickets.delegate_children,
                TicketOp::ModifySibling => authority.tickets.modify_siblings,
                TicketOp::Close => authority.tickets.close,
                TicketOp::Cancel => authority.tickets.cancel,
                TicketOp::Reopen => authority.tickets.reopen,
            },
            AuthorityRequirement::Network => {
                authority.network.docs
                    || authority.network.arbitrary
                    || !authority.network.allowlist.is_empty()
            }
            AuthorityRequirement::ComputerInput => authority.computer.input,
            AuthorityRequirement::ComputerCapture => authority.computer.capture,
            AuthorityRequirement::ComputerClipboard => authority.computer.clipboard,
            AuthorityRequirement::All(reqs) => reqs.iter().all(|r| r.admits(authority)),
            AuthorityRequirement::Any(reqs) => reqs.iter().any(|r| r.admits(authority)),
        }
    }
}

/// One tool a [`CapabilityProvider`] contributes: its wire name, schema and gating metadata.
///
/// `name`/`description`/`input_schema`/`cost` match `docs/audit-2026-09-18-fable.md` A-01's
/// sketch exactly. `requires` is an addition the sketch's `ToolSchema` didn't list: A-01 gives
/// each *provider* a single coarse [`AuthorityRequirement`] ("the Authority slice needed to be
/// admitted at all"), but the existing admit-by-authority filter this replaces
/// (`tm-agent::tools::ToolSpec::required_by`, landed pre-audit) gates *per tool* — `git.commit`
/// needs `authority.git.commit` specifically, not just "some git power" — and three of its tests
/// assert exact admitted-tool counts that a provider-wide gate cannot reproduce. Composing
/// `provider.requires()` (coarse: is this capability reachable at all) with
/// `ToolSchema::requires` (fine: is this specific tool) preserves that behavior exactly while
/// still giving a future capability like `tm-browser` a cheap whole-capability gate to declare.
#[derive(Debug, Clone)]
pub struct ToolSchema {
    /// The dotted wire name, e.g. `"browser.click"` or `"fs.read"`.
    pub name: &'static str,
    /// Shown to the model to decide when to call this tool.
    pub description: &'static str,
    /// JSON Schema for this tool's input.
    pub input_schema: Value,
    /// This tool's cost tier.
    pub cost: CostClass,
    /// The structural precondition on top of the provider's own [`CapabilityProvider::requires`]
    /// (see this struct's doc comment).
    pub requires: AuthorityRequirement,
}

/// Everything [`CapabilityProvider::invoke`] needs that is genuinely provider-agnostic:
/// identity, authority and injected time/id sources, and the project root. Deliberately does
/// *not* carry provider-specific service handles (a `Store`, a `SessionRegistry`, ...) — those
/// belong on the provider itself, constructed once, the same way `BuiltinCapability` owns an
/// `Arc<tm_core::Store>` rather than receiving one here.
pub struct CallContext<'a> {
    /// The authority this call was already checked against.
    pub authority: &'a Authority,
    /// The ticket this call happens on behalf of.
    pub ticket: &'a TicketId,
    /// The session this call happens inside.
    pub session: &'a SessionId,
    /// Who/what is issuing this call, for event/evidence attribution.
    pub actor: &'a ParticipantId,
    /// Injected clock; a provider never reads the wall clock directly.
    pub clock: &'a dyn Clock,
    /// Injected id source; a provider never mints ids itself.
    pub ids: &'a dyn IdSource,
    /// The project root, for any capability that touches the filesystem (builtin `fs.*`/
    /// `edit.*` today; a future browser download directory or computer screenshot directory
    /// tomorrow).
    pub root: &'a Path,
}

/// One pluggable capability: a named, authority-gated, independently registerable source of
/// tools. `docs/audit-2026-09-18-fable.md` A-01's target shape; see this module's doc comment
/// for why it lives here rather than in `tm-agent`.
///
/// Implementors must be `Send + Sync` (held behind `Arc<dyn CapabilityProvider>` in a registry
/// shared across concurrent dispatches) and object-safe (no generic methods), so a registry can
/// hold a homogeneous `Vec<Arc<dyn CapabilityProvider>>` of however many capabilities a binary
/// assembled.
#[async_trait::async_trait]
pub trait CapabilityProvider: Send + Sync {
    /// A slug identifying this capability, e.g. `"builtin"`, `"browser"`, `"computer"`, `"pty"`,
    /// or `"mcp:<server>"`.
    fn id(&self) -> &str;

    /// Every tool this capability contributes. Called once per registry construction (a
    /// registry caches the result; a provider does not need to memoize it itself), so returning
    /// a fresh `Vec` built from `'static` data each call is fine.
    fn tools(&self) -> Vec<ToolSchema>;

    /// Pure: map a call's input to the [`Action`] `Authority::permits` gates it on. Never
    /// performs I/O or consults ambient state — the same contract
    /// `tm-agent::tools::ToolSpec::to_action`'s doc comment already established, now at the
    /// provider level instead of one function pointer per tool.
    fn to_action(&self, tool: &str, input: &Value) -> Result<Action>;

    /// The slice of [`Authority`] this capability needs to be admitted at all (`SPEC.md` §30.1).
    /// A worker whose granted authority does not satisfy this gets *no schema* for any of this
    /// provider's tools, composed with (not replacing) each tool's own
    /// [`ToolSchema::requires`] — see [`ToolSchema`]'s doc comment.
    fn requires(&self) -> AuthorityRequirement;

    /// Execute one already-authorized call. Async because a future browser/computer/pty
    /// provider's calls are I/O-bound; the builtin provider's implementation simply has no
    /// await point.
    async fn invoke(&self, tool: &str, input: Value, ctx: &CallContext<'_>) -> Result<Value>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::{GitAuthority, NetworkAuthority, RepoAuthority, ShellAuthority};
    use crate::pattern::PatternSet;
    use std::collections::BTreeSet;

    fn authority_with(f: impl FnOnce(&mut Authority)) -> Authority {
        let mut a = Authority::default();
        f(&mut a);
        a
    }

    #[test]
    fn always_admits_the_empty_authority() {
        assert!(AuthorityRequirement::Always.admits(&Authority::default()));
    }

    #[test]
    fn repo_read_requires_a_nonempty_read_set() {
        assert!(!AuthorityRequirement::RepoRead.admits(&Authority::default()));
        let a = authority_with(|a| {
            a.repository = RepoAuthority {
                read: PatternSet::parse(["src/**"]).unwrap(),
                write: PatternSet::empty(),
            }
        });
        assert!(AuthorityRequirement::RepoRead.admits(&a));
    }

    #[test]
    fn shell_requires_enabled_and_a_nonempty_allowlist() {
        let enabled_no_allow = authority_with(|a| {
            a.shell = ShellAuthority {
                enabled: true,
                allow: PatternSet::empty(),
                deny: PatternSet::empty(),
            }
        });
        assert!(!AuthorityRequirement::Shell.admits(&enabled_no_allow));

        let enabled_with_allow = authority_with(|a| {
            a.shell = ShellAuthority {
                enabled: true,
                allow: PatternSet::parse(["echo *"]).unwrap(),
                deny: PatternSet::empty(),
            }
        });
        assert!(AuthorityRequirement::Shell.admits(&enabled_with_allow));
    }

    #[test]
    fn git_op_checks_the_matching_bool() {
        let a = authority_with(|a| {
            a.git = GitAuthority {
                commit: true,
                branch: false,
                merge: false,
                push: false,
                force: false,
            }
        });
        assert!(AuthorityRequirement::Git(GitOp::Commit).admits(&a));
        assert!(!AuthorityRequirement::Git(GitOp::Branch).admits(&a));
    }

    #[test]
    fn network_admits_on_docs_arbitrary_or_allowlist() {
        assert!(!AuthorityRequirement::Network.admits(&Authority::default()));
        let docs = authority_with(|a| {
            a.network = NetworkAuthority {
                docs: true,
                arbitrary: false,
                allowlist: BTreeSet::new(),
            }
        });
        assert!(AuthorityRequirement::Network.admits(&docs));
    }

    #[test]
    fn computer_requirements_check_the_matching_authority_field() {
        let mut a = Authority::default();
        a.computer.input = true;
        assert!(AuthorityRequirement::ComputerInput.admits(&a));
        assert!(!AuthorityRequirement::ComputerCapture.admits(&a));
        assert!(!AuthorityRequirement::ComputerClipboard.admits(&a));
    }

    #[test]
    fn all_and_any_compose() {
        let a = authority_with(|a| {
            a.git = GitAuthority {
                commit: true,
                branch: false,
                merge: false,
                push: false,
                force: false,
            }
        });
        assert!(!AuthorityRequirement::All(vec![
            AuthorityRequirement::Git(GitOp::Commit),
            AuthorityRequirement::Git(GitOp::Branch),
        ])
        .admits(&a));
        assert!(AuthorityRequirement::Any(vec![
            AuthorityRequirement::Git(GitOp::Commit),
            AuthorityRequirement::Git(GitOp::Branch),
        ])
        .admits(&a));
    }
}
