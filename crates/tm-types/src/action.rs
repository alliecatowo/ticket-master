//! Gated actions and the oversight policy that can escalate them to a human.
//!
//! Human control is shaped like authority, not like a stream of confirmation dialogs
//! (`SPEC.md` §4.4, and the product brief's "oversight is authority-shaped").

use crate::budget::Spend;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Something an executor wants to do that authority governs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Read a repository path.
    ReadPath {
        /// Repository-relative path.
        path: String,
    },
    /// Write a repository path.
    WritePath {
        /// Repository-relative path.
        path: String,
    },
    /// Run a command.
    RunCommand {
        /// argv, not a shell string.
        command: Vec<String>,
    },
    /// Perform a git operation.
    Git {
        /// Which operation.
        op: GitOp,
    },
    /// Fetch a URL.
    NetFetch {
        /// The absolute URL.
        url: String,
    },
    /// Change the ticket graph.
    Ticket {
        /// Which operation.
        op: TicketOp,
    },
    /// Change durable project configuration.
    Project {
        /// Which operation.
        op: ProjectOp,
    },
    /// Consume budget.
    Spend {
        /// The amount.
        amount: Spend,
    },
    /// Start another worker.
    SpawnWorker {
        /// How many workers would then be live under this authority.
        live: u32,
    },
    /// Navigate a browser session to a URL (`SPEC.md` §19.4). Gated by the same
    /// `Authority.network` mechanism [`Action::NetFetch`] uses — browser navigation and network
    /// fetch are the same authority concern (`NetworkAuthority::permits_url`) — with one
    /// carve-out: `about:`/`data:` URLs reach no network origin at all, so they need no network
    /// grant. `docs/audit-2026-09-18-fable.md` B-02: every non-navigate `tm-browser` tool
    /// (click/type/eval/snapshot/...) also maps to this variant with an `about:blank` url, since
    /// those tools act on a page whose origin was already checked when `navigate` put the
    /// session there, and `to_action` is a pure function of `(tool, input)` with no session
    /// state to consult for "what origin is this session currently on". See
    /// `crates/tm-browser/src/capability.rs` for exactly which tools take which path.
    BrowserNavigate {
        /// The absolute URL being navigated to, or `"about:blank"` for a same-page interaction
        /// tool that reaches no new origin.
        url: String,
    },
    /// Synthesize mouse/keyboard input into the real desktop (`SPEC.md` §20.5). Gated by
    /// `Authority.computer.input`. §20.5's default steady-state oversight puts this behind
    /// approval; `oversight.toml` loading (audit M-16) is not implemented yet, so today this
    /// gates admission only — the approval escalation itself is future work.
    ComputerInput,
    /// Capture the desktop: a screenshot or an accessibility-tree snapshot (`SPEC.md` §20.5).
    /// Gated by `Authority.computer.capture`.
    ComputerCapture,
    /// Read or write the system clipboard (`SPEC.md` §20.5). Gated by
    /// `Authority.computer.clipboard`. Same M-16 caveat as [`Action::ComputerInput`]: default
    /// oversight is supposed to put this behind approval, but there is no oversight loader yet.
    ComputerClipboard,
    /// Spawn an interactive process inside a pseudo-terminal (`SPEC.md` §22.2/§22.4,
    /// `docs/audit-2026-09-18-fable.md` B-16). Gated by `Authority.shell` exactly like
    /// [`Action::RunCommand`] — SPEC §22.4: "`pty.spawn` is governed by the same
    /// `shell.allow`/`shell.deny` patterns as `RunCommand`" — a pty session can run arbitrary
    /// commands just as `shell.run` can, so this is a distinct variant only so `tm-pty`'s
    /// `pty.spawn` tool has its own class string (`pty.spawn`, not `shell.run`) for oversight
    /// and event attribution, not because the gate itself differs.
    PtySpawn {
        /// argv, not a shell string, matching [`Action::RunCommand`].
        command: Vec<String>,
    },
    /// Synthesize input (text or a key chord) into an already-spawned `tm-pty` session
    /// (`SPEC.md` §22.4). A *distinct* action class from [`Action::PtySpawn`]/
    /// [`Action::RunCommand`], per SPEC §22.4's own reasoning: "an agent that can synthesize
    /// keystrokes into a live interactive process can answer a destructive confirmation
    /// prompt" — a materially larger attack surface than one bounded, argv-checked command, so
    /// it needs its own approval class and its own authority grant
    /// ([`crate::authority::ShellAuthority::pty`]) rather than riding along on `shell.enabled`.
    PtySend,
    /// Observe or manage an already-spawned `tm-pty` session without injecting input:
    /// `pty.screen`, `pty.diff`, `pty.resize`, `pty.wait_exit`. Gated by `Authority.shell.enabled`
    /// alone — these tools take only a session id, not a command, so unlike
    /// [`Action::PtySpawn`] there is no argv for `to_action` to check (it is a pure function of
    /// `(tool, input)` with no session state to recover the command that was originally
    /// spawned), and unlike [`Action::PtySend`] they inject no new input into the child. SPEC
    /// §22.4 does not name a class for these explicitly; this is the nearest-existing-bucket
    /// choice, same convention as `tm-computer`'s window-management tools joining
    /// `ComputerInput`.
    PtyControl,
}

/// Git operations, ordered by blast radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitOp {
    /// Create a commit.
    Commit,
    /// Create or switch a branch.
    Branch,
    /// Merge a branch.
    Merge,
    /// Push to a remote.
    Push,
    /// Force-push to a remote.
    ForcePush,
}

/// Ticket-graph operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketOp {
    /// Create a child ticket.
    CreateChild,
    /// Delegate a child to another executor.
    Delegate,
    /// Modify a sibling ticket.
    ModifySibling,
    /// Close a ticket.
    Close,
    /// Cancel a ticket.
    Cancel,
    /// Reopen a closed ticket.
    Reopen,
}

/// Durable project-configuration operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectOp {
    /// Edit the specification.
    ModifySpec,
    /// Edit the vision.
    ModifyVision,
    /// Add, remove or re-scope milestones.
    ModifyMilestones,
    /// Close a milestone.
    CloseMilestone,
    /// Reopen a closed milestone.
    ReopenMilestone,
    /// Change harness configuration.
    ModifyHarness,
}

impl Action {
    /// A stable dotted class name, used by oversight policy and by events.
    pub fn class(&self) -> String {
        match self {
            Action::ReadPath { .. } => "repository.read".into(),
            Action::WritePath { .. } => "repository.write".into(),
            Action::RunCommand { .. } => "shell.run".into(),
            Action::Git { op } => format!("git.{}", op.as_str()),
            Action::NetFetch { .. } => "network.fetch".into(),
            Action::Ticket { op } => format!("tickets.{}", op.as_str()),
            Action::Project { op } => format!("project.{}", op.as_str()),
            Action::Spend { .. } => "budget.spend".into(),
            Action::SpawnWorker { .. } => "resources.spawn_worker".into(),
            Action::BrowserNavigate { .. } => "browser.navigate".into(),
            Action::ComputerInput => "computer.input".into(),
            Action::ComputerCapture => "computer.capture".into(),
            Action::ComputerClipboard => "computer.clipboard".into(),
            Action::PtySpawn { .. } => "pty.spawn".into(),
            Action::PtySend => "pty.send".into(),
            Action::PtyControl => "pty.control".into(),
        }
    }
}

impl GitOp {
    /// The name used in class strings and configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            GitOp::Commit => "commit",
            GitOp::Branch => "branch",
            GitOp::Merge => "merge",
            GitOp::Push => "push",
            GitOp::ForcePush => "force_push",
        }
    }
}

impl TicketOp {
    /// The name used in class strings and configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            TicketOp::CreateChild => "create_children",
            TicketOp::Delegate => "delegate_children",
            TicketOp::ModifySibling => "modify_siblings",
            TicketOp::Close => "close",
            TicketOp::Cancel => "cancel",
            TicketOp::Reopen => "reopen",
        }
    }
}

impl ProjectOp {
    /// The name used in class strings and configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectOp::ModifySpec => "modify_spec",
            ProjectOp::ModifyVision => "modify_vision",
            ProjectOp::ModifyMilestones => "modify_milestones",
            ProjectOp::CloseMilestone => "close_milestone",
            ProjectOp::ReopenMilestone => "reopen_milestone",
            ProjectOp::ModifyHarness => "modify_harness",
        }
    }
}

/// The verdict on an attempted action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Proceed.
    Allow,
    /// Refuse, with the reason the executor is shown.
    Deny(String),
    /// Proceed only after a human approves, with the scope being approved.
    NeedsApproval(String),
}

impl Decision {
    /// True only for [`Decision::Allow`].
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

/// Which action classes a human wants to be asked about.
///
/// `deny_unknown_fields`: this is the whole body of a human-edited `oversight.toml`
/// (`crates/tm-cli/src/dispatch.rs`'s `load_oversight`), a security control, not a lenient
/// display-only config — a typo'd key (`approval_requird`) must be a parse error, not a silently
/// empty policy that leaves a human believing an action class is gated when it is not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Oversight {
    /// Action classes that require approval. A prefix matches its whole subtree, so
    /// `git` covers `git.push`.
    #[serde(default)]
    pub approval_required: BTreeSet<String>,
    /// Spending more than this many micros in one action requires approval.
    #[serde(default)]
    pub spend_over_micros: Option<u64>,
}

impl Oversight {
    /// Nothing requires approval.
    pub fn autonomous() -> Self {
        Oversight::default()
    }

    /// The default steady-state policy: irreversible or outward-facing actions ask first.
    ///
    /// `computer.input`/`computer.clipboard` are here per `SPEC.md` §20.5 ("an agent that can
    /// synthesize keystrokes into whatever window has focus is strictly more dangerous than one
    /// that can write files inside a scoped path"). `pty.send` joins them per the same
    /// reasoning, restated for a pty by `SPEC.md` §22.4: an agent that can synthesize keystrokes
    /// into a live interactive process can answer a destructive confirmation prompt.
    ///
    /// `crates/tm-cli/src/dispatch.rs`'s `load_oversight` parses this policy from `oversight.toml`
    /// at a project's root (falling back to [`Oversight::default`] when absent), and
    /// `tm_agent::agent_loop::AgentLoop::drive` is the real effect boundary that calls
    /// [`Oversight::review`] on it before a tool call dispatches — see `docs/decisions/D-009-oversight-policy-wiring.md`
    /// (`docs/audit-2026-09-18-fable.md` M-16, now closed).
    pub fn conservative() -> Self {
        Oversight {
            approval_required: [
                "git.merge",
                "git.push",
                "git.force_push",
                "project",
                "tickets.reopen",
                "computer.input",
                "computer.clipboard",
                "pty.send",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            spend_over_micros: Some(10_000_000),
        }
    }

    /// Escalate an otherwise-allowed action to a human when policy says so.
    ///
    /// A denial is never softened into an approval request: authority is checked first and
    /// oversight can only make the answer stricter.
    pub fn review(&self, action: &Action, base: Decision) -> Decision {
        if !base.is_allowed() {
            return base;
        }
        let class = action.class();
        if self
            .approval_required
            .iter()
            .any(|c| class == *c || class.starts_with(&format!("{c}.")))
        {
            return Decision::NeedsApproval(class);
        }
        if let (Action::Spend { amount }, Some(limit)) = (action, self.spend_over_micros) {
            if amount.dollars_micros > limit {
                return Decision::NeedsApproval(format!("spend over {limit} micros"));
            }
        }
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_are_stable_dotted_names() {
        assert_eq!(
            Action::ReadPath { path: "a".into() }.class(),
            "repository.read"
        );
        assert_eq!(
            Action::Git {
                op: GitOp::ForcePush
            }
            .class(),
            "git.force_push"
        );
        assert_eq!(
            Action::Ticket {
                op: TicketOp::CreateChild
            }
            .class(),
            "tickets.create_children"
        );
        assert_eq!(
            Action::Project {
                op: ProjectOp::CloseMilestone
            }
            .class(),
            "project.close_milestone"
        );
        assert_eq!(
            Action::BrowserNavigate {
                url: "https://example.com".into()
            }
            .class(),
            "browser.navigate"
        );
        assert_eq!(Action::ComputerInput.class(), "computer.input");
        assert_eq!(Action::ComputerCapture.class(), "computer.capture");
        assert_eq!(Action::ComputerClipboard.class(), "computer.clipboard");
        assert_eq!(
            Action::PtySpawn {
                command: vec!["bash".into()]
            }
            .class(),
            "pty.spawn"
        );
        assert_eq!(Action::PtySend.class(), "pty.send");
        assert_eq!(Action::PtyControl.class(), "pty.control");
    }

    #[test]
    fn conservative_oversight_escalates_pty_send() {
        let o = Oversight::conservative();
        assert!(matches!(
            o.review(&Action::PtySend, Decision::Allow),
            Decision::NeedsApproval(_)
        ));
        assert_eq!(
            o.review(&Action::PtyControl, Decision::Allow),
            Decision::Allow
        );
    }

    #[test]
    fn conservative_oversight_escalates_computer_input_and_clipboard() {
        let o = Oversight::conservative();
        assert!(matches!(
            o.review(&Action::ComputerInput, Decision::Allow),
            Decision::NeedsApproval(_)
        ));
        assert!(matches!(
            o.review(&Action::ComputerClipboard, Decision::Allow),
            Decision::NeedsApproval(_)
        ));
        assert_eq!(
            o.review(&Action::ComputerCapture, Decision::Allow),
            Decision::Allow
        );
    }

    #[test]
    fn oversight_escalates_by_prefix() {
        let o = Oversight::conservative();
        assert_eq!(
            o.review(
                &Action::Project {
                    op: ProjectOp::ModifySpec
                },
                Decision::Allow
            ),
            Decision::NeedsApproval("project.modify_spec".into())
        );
        assert_eq!(
            o.review(&Action::Git { op: GitOp::Commit }, Decision::Allow),
            Decision::Allow
        );
        assert!(matches!(
            o.review(&Action::Git { op: GitOp::Push }, Decision::Allow),
            Decision::NeedsApproval(_)
        ));
    }

    #[test]
    fn oversight_never_softens_a_denial() {
        let o = Oversight::conservative();
        let denied = Decision::Deny("no write authority".into());
        assert_eq!(
            o.review(&Action::Git { op: GitOp::Push }, denied.clone()),
            denied
        );
    }

    #[test]
    fn large_spends_ask_first() {
        let o = Oversight::conservative();
        let small = Action::Spend {
            amount: Spend::dollars_micros(1_000_000),
        };
        let large = Action::Spend {
            amount: Spend::dollars_micros(11_000_000),
        };
        assert_eq!(o.review(&small, Decision::Allow), Decision::Allow);
        assert!(matches!(
            o.review(&large, Decision::Allow),
            Decision::NeedsApproval(_)
        ));
    }

    #[test]
    fn autonomous_policy_asks_nothing() {
        let o = Oversight::autonomous();
        assert_eq!(
            o.review(
                &Action::Git {
                    op: GitOp::ForcePush
                },
                Decision::Allow
            ),
            Decision::Allow
        );
    }

    #[test]
    fn oversight_rejects_an_unknown_field_instead_of_silently_ignoring_it() {
        // `deny_unknown_fields`, type-level: `oversight.toml`'s loader
        // (`crates/tm-cli/src/dispatch.rs::load_oversight`) relies on this to fail a misspelled
        // key loudly rather than parse into an empty, all-autonomous policy.
        let err = serde_json::from_str::<Oversight>(r#"{"aproval_required": ["git.push"]}"#)
            .expect_err("a misspelled key must not deserialize");
        let _ = err; // exact message is serde_json's own, not this crate's to assert on
    }
}
