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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    pub fn conservative() -> Self {
        Oversight {
            approval_required: ["git.merge", "git.push", "git.force_push", "project", "tickets.reopen"]
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
        if self.approval_required.iter().any(|c| class == *c || class.starts_with(&format!("{c}."))) {
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
        assert_eq!(Action::ReadPath { path: "a".into() }.class(), "repository.read");
        assert_eq!(Action::Git { op: GitOp::ForcePush }.class(), "git.force_push");
        assert_eq!(Action::Ticket { op: TicketOp::CreateChild }.class(), "tickets.create_children");
        assert_eq!(Action::Project { op: ProjectOp::CloseMilestone }.class(), "project.close_milestone");
    }

    #[test]
    fn oversight_escalates_by_prefix() {
        let o = Oversight::conservative();
        assert_eq!(
            o.review(&Action::Project { op: ProjectOp::ModifySpec }, Decision::Allow),
            Decision::NeedsApproval("project.modify_spec".into())
        );
        assert_eq!(o.review(&Action::Git { op: GitOp::Commit }, Decision::Allow), Decision::Allow);
        assert!(matches!(
            o.review(&Action::Git { op: GitOp::Push }, Decision::Allow),
            Decision::NeedsApproval(_)
        ));
    }

    #[test]
    fn oversight_never_softens_a_denial() {
        let o = Oversight::conservative();
        let denied = Decision::Deny("no write authority".into());
        assert_eq!(o.review(&Action::Git { op: GitOp::Push }, denied.clone()), denied);
    }

    #[test]
    fn large_spends_ask_first() {
        let o = Oversight::conservative();
        let small = Action::Spend { amount: Spend::dollars_micros(1_000_000) };
        let large = Action::Spend { amount: Spend::dollars_micros(11_000_000) };
        assert_eq!(o.review(&small, Decision::Allow), Decision::Allow);
        assert!(matches!(o.review(&large, Decision::Allow), Decision::NeedsApproval(_)));
    }

    #[test]
    fn autonomous_policy_asks_nothing() {
        let o = Oversight::autonomous();
        assert_eq!(o.review(&Action::Git { op: GitOp::ForcePush }, Decision::Allow), Decision::Allow);
    }
}
