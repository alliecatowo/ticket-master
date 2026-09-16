//! The authority algebra.
//!
//! Ticketmaster delegates authority, not prompts. Authority is explicit, inspectable,
//! delegatable, attenuating, revocable and auditable; if executor A delegates to B then
//! `authority(B) ⊆ authority(A)`, always. A child cannot manufacture powers its parent never
//! possessed (`SPEC.md` §4.4).

use crate::action::{Action, Decision, GitOp, ProjectOp, TicketOp};
use crate::budget::Budget;
use crate::pattern::PatternSet;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Denials a grantor imposes are inherited by everyone it delegates to; see
/// [`Authority::with_inherited_denials`].
///
/// A delegation was refused, with every reason it was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("requested authority exceeds the grantor's: {}", .reasons.join("; "))]
pub struct AuthorityDenied {
    /// Every way in which the request exceeded what the grantor holds.
    pub reasons: Vec<String>,
}

/// Repository read and write scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoAuthority {
    /// Paths that may be read.
    #[serde(default)]
    pub read: PatternSet,
    /// Paths that may be written.
    #[serde(default)]
    pub write: PatternSet,
}

/// Git operations that are permitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitAuthority {
    /// May create commits.
    #[serde(default)]
    pub commit: bool,
    /// May create or switch branches.
    #[serde(default)]
    pub branch: bool,
    /// May merge.
    #[serde(default)]
    pub merge: bool,
    /// May push.
    #[serde(default)]
    pub push: bool,
    /// May force-push.
    #[serde(default)]
    pub force: bool,
}

/// Ticket-graph powers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketAuthority {
    /// May create child tickets.
    #[serde(default)]
    pub create_children: bool,
    /// May delegate children to other executors.
    #[serde(default)]
    pub delegate_children: bool,
    /// May modify sibling tickets.
    #[serde(default)]
    pub modify_siblings: bool,
    /// May close tickets.
    #[serde(default)]
    pub close: bool,
    /// May cancel tickets.
    #[serde(default)]
    pub cancel: bool,
    /// May reopen closed tickets.
    #[serde(default)]
    pub reopen: bool,
}

/// Durable project-configuration powers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectAuthority {
    /// May edit the specification.
    #[serde(default)]
    pub modify_spec: bool,
    /// May edit the vision.
    #[serde(default)]
    pub modify_vision: bool,
    /// May add, remove or re-scope milestones.
    #[serde(default)]
    pub modify_milestones: bool,
    /// May close a milestone.
    #[serde(default)]
    pub close_milestone: bool,
    /// May reopen a milestone.
    #[serde(default)]
    pub reopen_milestone: bool,
    /// May change harness configuration.
    #[serde(default)]
    pub modify_harness: bool,
}

/// Network reach.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAuthority {
    /// May fetch documentation hosts.
    #[serde(default)]
    pub docs: bool,
    /// May fetch anything.
    #[serde(default)]
    pub arbitrary: bool,
    /// Additional permitted hosts.
    #[serde(default)]
    pub allowlist: BTreeSet<String>,
}

impl NetworkAuthority {
    /// Hosts always reachable when `docs` is granted.
    pub const DOC_HOSTS: [&'static str; 6] = [
        "docs.rs",
        "doc.rust-lang.org",
        "developer.mozilla.org",
        "pkg.go.dev",
        "docs.python.org",
        "crates.io",
    ];

    fn host_of(url: &str) -> Option<&str> {
        let rest = url.split("://").nth(1).unwrap_or(url);
        let host = rest.split('/').next()?;
        let host = host.rsplit('@').next()?;
        Some(host.split(':').next().unwrap_or(host))
    }

    /// True when this authority permits fetching `url`.
    pub fn permits_url(&self, url: &str) -> bool {
        if self.arbitrary {
            return true;
        }
        let Some(host) = Self::host_of(url) else {
            return false;
        };
        if self.docs && Self::DOC_HOSTS.contains(&host) {
            return true;
        }
        self.allowlist.iter().any(|h| h == host || host.ends_with(&format!(".{h}")))
    }
}

/// Shell reach.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellAuthority {
    /// Whether commands may be run at all.
    #[serde(default)]
    pub enabled: bool,
    /// Command lines that are permitted.
    #[serde(default)]
    pub allow: PatternSet,
    /// Command lines that are refused even when allowed above.
    #[serde(default)]
    pub deny: PatternSet,
}

impl ShellAuthority {
    /// Render argv the way [`ShellAuthority::allow`] and `deny` patterns are matched against it.
    pub fn command_line(command: &[String]) -> String {
        command.join(" ")
    }

    /// True when this authority permits running `command`.
    pub fn permits(&self, command: &[String]) -> bool {
        if !self.enabled || command.is_empty() {
            return false;
        }
        let line = Self::command_line(command);
        if self.deny.matches_text(&line) || self.deny.matches_text(&command[0]) {
            return false;
        }
        self.allow.matches_text(&line) || self.allow.matches_text(&command[0])
    }
}

/// Concurrency ceilings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceAuthority {
    /// Maximum simultaneous workers beneath this authority.
    #[serde(default)]
    pub max_workers: u32,
    /// Maximum simultaneous commands beneath this authority.
    #[serde(default)]
    pub max_concurrent_commands: u32,
}

/// Everything an executor is permitted to do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authority {
    /// Repository scope.
    #[serde(default)]
    pub repository: RepoAuthority,
    /// Git powers.
    #[serde(default)]
    pub git: GitAuthority,
    /// Ticket-graph powers.
    #[serde(default)]
    pub tickets: TicketAuthority,
    /// Project-configuration powers.
    #[serde(default)]
    pub project: ProjectAuthority,
    /// Network reach.
    #[serde(default)]
    pub network: NetworkAuthority,
    /// Shell reach.
    #[serde(default)]
    pub shell: ShellAuthority,
    /// Concurrency ceilings.
    #[serde(default)]
    pub resources: ResourceAuthority,
    /// Spending ceiling.
    #[serde(default)]
    pub budget: Budget,
}

/// `parent >= child` for a boolean power: a child may only hold `true` where the parent does.
fn bool_ok(parent: bool, child: bool) -> bool {
    parent || !child
}

impl Authority {
    /// Full authority. Held by the project itself and never leased out wholesale.
    pub fn root() -> Self {
        Authority {
            repository: RepoAuthority { read: PatternSet::all(), write: PatternSet::all() },
            git: GitAuthority { commit: true, branch: true, merge: true, push: true, force: true },
            tickets: TicketAuthority {
                create_children: true,
                delegate_children: true,
                modify_siblings: true,
                close: true,
                cancel: true,
                reopen: true,
            },
            project: ProjectAuthority {
                modify_spec: true,
                modify_vision: true,
                modify_milestones: true,
                close_milestone: true,
                reopen_milestone: true,
                modify_harness: true,
            },
            network: NetworkAuthority { docs: true, arbitrary: true, allowlist: BTreeSet::new() },
            shell: ShellAuthority {
                enabled: true,
                allow: PatternSet::all(),
                deny: PatternSet::empty(),
            },
            resources: ResourceAuthority {
                max_workers: u32::MAX,
                max_concurrent_commands: u32::MAX,
            },
            budget: Budget::unlimited(),
        }
    }

    /// No authority at all. This is also [`Authority::default`].
    pub fn none() -> Self {
        Authority::default()
    }

    /// `requested`, with every shell denial this authority imposes folded in.
    ///
    /// Denials are inherited rather than restated: a delegation request that simply does not
    /// mention `rm -rf` is not trying to re-enable it, so attenuation adds the grantor's
    /// denials instead of refusing the request.
    pub fn with_inherited_denials(&self, requested: &Authority) -> Authority {
        let mut out = requested.clone();
        out.shell.deny = out.shell.deny.union(&self.shell.deny);
        out
    }

    /// Read-only exploration: read anything, change nothing.
    pub fn read_only() -> Self {
        Authority { repository: RepoAuthority { read: PatternSet::all(), ..Default::default() }, ..Authority::none() }
    }

    /// True when `other` is entirely contained by `self`.
    pub fn contains(&self, other: &Authority) -> bool {
        self.containment_failures(other).is_empty()
    }

    /// Every way in which `other` exceeds `self`. Empty means contained.
    pub fn containment_failures(&self, other: &Authority) -> Vec<String> {
        let mut out = Vec::new();
        if !other.repository.read.is_subset_of(&self.repository.read) {
            out.push(format!(
                "repository.read {:?} is not within {:?}",
                other.repository.read, self.repository.read
            ));
        }
        if !other.repository.write.is_subset_of(&self.repository.write) {
            out.push(format!(
                "repository.write {:?} is not within {:?}",
                other.repository.write, self.repository.write
            ));
        }
        for (name, p, c) in [
            ("git.commit", self.git.commit, other.git.commit),
            ("git.branch", self.git.branch, other.git.branch),
            ("git.merge", self.git.merge, other.git.merge),
            ("git.push", self.git.push, other.git.push),
            ("git.force", self.git.force, other.git.force),
            ("tickets.create_children", self.tickets.create_children, other.tickets.create_children),
            (
                "tickets.delegate_children",
                self.tickets.delegate_children,
                other.tickets.delegate_children,
            ),
            ("tickets.modify_siblings", self.tickets.modify_siblings, other.tickets.modify_siblings),
            ("tickets.close", self.tickets.close, other.tickets.close),
            ("tickets.cancel", self.tickets.cancel, other.tickets.cancel),
            ("tickets.reopen", self.tickets.reopen, other.tickets.reopen),
            ("project.modify_spec", self.project.modify_spec, other.project.modify_spec),
            ("project.modify_vision", self.project.modify_vision, other.project.modify_vision),
            (
                "project.modify_milestones",
                self.project.modify_milestones,
                other.project.modify_milestones,
            ),
            ("project.close_milestone", self.project.close_milestone, other.project.close_milestone),
            (
                "project.reopen_milestone",
                self.project.reopen_milestone,
                other.project.reopen_milestone,
            ),
            ("project.modify_harness", self.project.modify_harness, other.project.modify_harness),
            ("network.docs", self.network.docs, other.network.docs),
            ("network.arbitrary", self.network.arbitrary, other.network.arbitrary),
            ("shell.enabled", self.shell.enabled, other.shell.enabled),
        ] {
            if !bool_ok(p, c) {
                out.push(format!("{name} is not held by the grantor"));
            }
        }
        if !self.network.arbitrary {
            for host in &other.network.allowlist {
                let covered = self.network.allowlist.contains(host)
                    || (self.network.docs && NetworkAuthority::DOC_HOSTS.contains(&host.as_str()));
                if !covered {
                    out.push(format!("network host {host} is not permitted by the grantor"));
                }
            }
        }
        if !other.shell.allow.is_subset_of(&self.shell.allow) {
            out.push("shell.allow exceeds the grantor's".to_string());
        }
        // A disabled shell cannot run anything, so it cannot violate a denial regardless of
        // what its (irrelevant) deny list says; only an enabled child must inherit every
        // denial its grantor imposes.
        if other.shell.enabled && !self.shell.deny.is_subset_of(&other.shell.deny) {
            out.push("shell.deny drops a restriction the grantor imposes".to_string());
        }
        if other.resources.max_workers > self.resources.max_workers {
            out.push(format!(
                "resources.max_workers {} exceeds {}",
                other.resources.max_workers, self.resources.max_workers
            ));
        }
        if other.resources.max_concurrent_commands > self.resources.max_concurrent_commands {
            out.push(format!(
                "resources.max_concurrent_commands {} exceeds {}",
                other.resources.max_concurrent_commands, self.resources.max_concurrent_commands
            ));
        }
        if !self.budget.contains(&other.budget) {
            out.push("budget exceeds the grantor's remaining budget".to_string());
        }
        out
    }

    /// The greatest authority contained by both.
    pub fn intersect(&self, other: &Authority) -> Authority {
        Authority {
            repository: RepoAuthority {
                read: self.repository.read.intersect(&other.repository.read),
                write: self.repository.write.intersect(&other.repository.write),
            },
            git: GitAuthority {
                commit: self.git.commit && other.git.commit,
                branch: self.git.branch && other.git.branch,
                merge: self.git.merge && other.git.merge,
                push: self.git.push && other.git.push,
                force: self.git.force && other.git.force,
            },
            tickets: TicketAuthority {
                create_children: self.tickets.create_children && other.tickets.create_children,
                delegate_children: self.tickets.delegate_children && other.tickets.delegate_children,
                modify_siblings: self.tickets.modify_siblings && other.tickets.modify_siblings,
                close: self.tickets.close && other.tickets.close,
                cancel: self.tickets.cancel && other.tickets.cancel,
                reopen: self.tickets.reopen && other.tickets.reopen,
            },
            project: ProjectAuthority {
                modify_spec: self.project.modify_spec && other.project.modify_spec,
                modify_vision: self.project.modify_vision && other.project.modify_vision,
                modify_milestones: self.project.modify_milestones && other.project.modify_milestones,
                close_milestone: self.project.close_milestone && other.project.close_milestone,
                reopen_milestone: self.project.reopen_milestone && other.project.reopen_milestone,
                modify_harness: self.project.modify_harness && other.project.modify_harness,
            },
            network: NetworkAuthority {
                docs: self.network.docs && other.network.docs,
                arbitrary: self.network.arbitrary && other.network.arbitrary,
                allowlist: self
                    .network
                    .allowlist
                    .intersection(&other.network.allowlist)
                    .cloned()
                    .collect(),
            },
            shell: ShellAuthority {
                enabled: self.shell.enabled && other.shell.enabled,
                allow: self.shell.allow.intersect(&other.shell.allow),
                deny: self.shell.deny.union(&other.shell.deny),
            },
            resources: ResourceAuthority {
                max_workers: self.resources.max_workers.min(other.resources.max_workers),
                max_concurrent_commands: self
                    .resources
                    .max_concurrent_commands
                    .min(other.resources.max_concurrent_commands),
            },
            budget: self.budget.intersect(&other.budget),
        }
    }

    /// Grant `requested` if and only if this authority already holds it.
    ///
    /// Attenuation never widens: the result is always contained by `self`.
    pub fn attenuate(&self, requested: &Authority) -> Result<Authority, AuthorityDenied> {
        let granted = self.with_inherited_denials(requested);
        let reasons = self.containment_failures(&granted);
        if reasons.is_empty() {
            Ok(granted)
        } else {
            Err(AuthorityDenied { reasons })
        }
    }

    /// Grant as much of `requested` as this authority holds, silently dropping the rest.
    ///
    /// Used where partial delegation is wanted instead of refusal.
    pub fn attenuate_lossy(&self, requested: &Authority) -> Authority {
        self.intersect(requested)
    }

    /// Whether `action` is permitted. Oversight may escalate an `Allow` afterwards.
    pub fn permits(&self, action: &Action) -> Decision {
        let deny = |m: String| Decision::Deny(m);
        match action {
            Action::ReadPath { path } => {
                if self.repository.read.matches(path) {
                    Decision::Allow
                } else {
                    deny(format!("no read authority for {path}"))
                }
            }
            Action::WritePath { path } => {
                if self.repository.write.matches(path) {
                    Decision::Allow
                } else {
                    deny(format!("no write authority for {path}"))
                }
            }
            Action::RunCommand { command } => {
                if self.shell.permits(command) {
                    Decision::Allow
                } else {
                    deny(format!("no shell authority for `{}`", ShellAuthority::command_line(command)))
                }
            }
            Action::Git { op } => {
                let ok = match op {
                    GitOp::Commit => self.git.commit,
                    GitOp::Branch => self.git.branch,
                    GitOp::Merge => self.git.merge,
                    GitOp::Push => self.git.push,
                    GitOp::ForcePush => self.git.force,
                };
                if ok {
                    Decision::Allow
                } else {
                    deny(format!("no git.{} authority", op.as_str()))
                }
            }
            Action::NetFetch { url } => {
                if self.network.permits_url(url) {
                    Decision::Allow
                } else {
                    deny(format!("no network authority for {url}"))
                }
            }
            Action::Ticket { op } => {
                let ok = match op {
                    TicketOp::CreateChild => self.tickets.create_children,
                    TicketOp::Delegate => self.tickets.delegate_children,
                    TicketOp::ModifySibling => self.tickets.modify_siblings,
                    TicketOp::Close => self.tickets.close,
                    TicketOp::Cancel => self.tickets.cancel,
                    TicketOp::Reopen => self.tickets.reopen,
                };
                if ok {
                    Decision::Allow
                } else {
                    deny(format!("no tickets.{} authority", op.as_str()))
                }
            }
            Action::Project { op } => {
                let ok = match op {
                    ProjectOp::ModifySpec => self.project.modify_spec,
                    ProjectOp::ModifyVision => self.project.modify_vision,
                    ProjectOp::ModifyMilestones => self.project.modify_milestones,
                    ProjectOp::CloseMilestone => self.project.close_milestone,
                    ProjectOp::ReopenMilestone => self.project.reopen_milestone,
                    ProjectOp::ModifyHarness => self.project.modify_harness,
                };
                if ok {
                    Decision::Allow
                } else {
                    deny(format!("no project.{} authority", op.as_str()))
                }
            }
            Action::Spend { amount } => match self.budget.check(*amount) {
                Ok(()) => Decision::Allow,
                Err(e) => deny(e.to_string()),
            },
            Action::SpawnWorker { live } => {
                if *live <= self.resources.max_workers {
                    Decision::Allow
                } else {
                    deny(format!(
                        "worker ceiling {} would be exceeded",
                        self.resources.max_workers
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Spend;

    fn scoped() -> Authority {
        Authority {
            repository: RepoAuthority {
                read: PatternSet::all(),
                write: PatternSet::parse(["src/auth/**", "tests/auth/**"]).unwrap(),
            },
            git: GitAuthority { commit: true, ..Default::default() },
            tickets: TicketAuthority {
                create_children: true,
                delegate_children: true,
                ..Default::default()
            },
            network: NetworkAuthority { docs: true, ..Default::default() },
            shell: ShellAuthority {
                enabled: true,
                allow: PatternSet::parse(["cargo*", "git*"]).unwrap(),
                deny: PatternSet::parse(["*rm -rf*"]).unwrap(),
            },
            resources: ResourceAuthority { max_workers: 4, max_concurrent_commands: 2 },
            budget: Budget::new(180_000, 3_000_000, 3_600),
            ..Authority::none()
        }
    }

    #[test]
    fn root_contains_everything_and_none_contains_nothing_but_itself() {
        assert!(Authority::root().contains(&scoped()));
        assert!(Authority::root().contains(&Authority::root()));
        assert!(Authority::none().contains(&Authority::none()));
        assert!(!Authority::none().contains(&scoped()));
        assert!(!scoped().contains(&Authority::root()));
    }

    #[test]
    fn a_child_cannot_widen_the_repository_scope() {
        let parent = scoped();
        let greedy = Authority {
            repository: RepoAuthority { read: PatternSet::all(), write: PatternSet::all() },
            ..Authority::none()
        };
        let err = parent.attenuate(&greedy).unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("repository.write")));

        let ok = Authority {
            repository: RepoAuthority {
                read: PatternSet::parse(["src/**"]).unwrap(),
                write: PatternSet::parse(["src/auth/tokens.rs"]).unwrap(),
            },
            ..Authority::none()
        };
        assert!(parent.attenuate(&ok).is_ok());
    }

    #[test]
    fn a_child_cannot_manufacture_a_boolean_power() {
        let parent = scoped();
        let greedy = Authority {
            git: GitAuthority { commit: true, push: true, ..Default::default() },
            ..Authority::none()
        };
        let err = parent.attenuate(&greedy).unwrap_err();
        assert_eq!(err.reasons, vec!["git.push is not held by the grantor"]);
    }

    #[test]
    fn a_child_cannot_drop_a_shell_denial() {
        let parent = scoped();
        let silent = Authority {
            shell: ShellAuthority {
                enabled: true,
                allow: PatternSet::parse(["cargo*"]).unwrap(),
                deny: PatternSet::empty(),
            },
            ..Authority::none()
        };
        // A request that simply omits the denial inherits it rather than being refused.
        let granted = parent.attenuate(&silent).unwrap();
        assert!(granted.shell.deny.matches_text("cargo run -- rm -rf /"));
        assert!(!granted.shell.permits(&["cargo".into(), "run".into(), "--".into(), "rm -rf /".into()]));

        // And containment on its own still treats a dropped denial as an escalation.
        assert!(!parent.contains(&silent));
        assert!(parent
            .containment_failures(&silent)
            .iter()
            .any(|r| r.contains("shell.deny")));
    }

    #[test]
    fn the_default_authority_grants_nothing() {
        let a = Authority::default();
        assert_eq!(a, Authority::none());
        assert!(!a.permits(&Action::ReadPath { path: "any".into() }).is_allowed());
        assert!(!a.permits(&Action::Spend { amount: Spend::tokens(1) }).is_allowed());
        assert!(!a.permits(&Action::SpawnWorker { live: 1 }).is_allowed());
        assert!(Authority::root().contains(&a));
    }

    #[test]
    fn delegation_chains_attenuate_monotonically() {
        let a = Authority::root();
        let b = a.attenuate(&scoped()).unwrap();
        let c_req = Authority {
            repository: RepoAuthority {
                read: PatternSet::parse(["src/**"]).unwrap(),
                write: PatternSet::parse(["src/auth/**"]).unwrap(),
            },
            shell: ShellAuthority {
                enabled: true,
                allow: PatternSet::parse(["cargo*"]).unwrap(),
                deny: PatternSet::parse(["*rm -rf*"]).unwrap(),
            },
            resources: ResourceAuthority { max_workers: 1, max_concurrent_commands: 1 },
            budget: Budget::new(1_000, 1_000, 60),
            ..Authority::none()
        };
        let c = b.attenuate(&c_req).unwrap();
        assert!(a.contains(&b) && b.contains(&c) && a.contains(&c));
    }

    #[test]
    fn budget_delegation_respects_what_is_left() {
        let mut parent = scoped();
        parent.budget.try_spend(Spend::tokens(179_000)).unwrap();
        let child = Authority { budget: Budget::new(2_000, 0, 0), ..Authority::none() };
        assert!(parent.attenuate(&child).is_err());
        let smaller = Authority { budget: Budget::new(1_000, 0, 0), ..Authority::none() };
        assert!(parent.attenuate(&smaller).is_ok());
    }

    #[test]
    fn permits_gates_each_action_kind() {
        let a = scoped();
        assert!(a.permits(&Action::ReadPath { path: "docs/x.md".into() }).is_allowed());
        assert!(a.permits(&Action::WritePath { path: "src/auth/mod.rs".into() }).is_allowed());
        assert!(!a.permits(&Action::WritePath { path: "src/parser/mod.rs".into() }).is_allowed());
        assert!(a
            .permits(&Action::RunCommand { command: vec!["cargo".into(), "test".into()] })
            .is_allowed());
        assert!(!a
            .permits(&Action::RunCommand { command: vec!["curl".into(), "evil".into()] })
            .is_allowed());
        assert!(a.permits(&Action::Git { op: GitOp::Commit }).is_allowed());
        assert!(!a.permits(&Action::Git { op: GitOp::Merge }).is_allowed());
        assert!(a.permits(&Action::Ticket { op: TicketOp::CreateChild }).is_allowed());
        assert!(!a.permits(&Action::Project { op: ProjectOp::ModifySpec }).is_allowed());
        assert!(a.permits(&Action::SpawnWorker { live: 4 }).is_allowed());
        assert!(!a.permits(&Action::SpawnWorker { live: 5 }).is_allowed());
    }

    #[test]
    fn shell_denials_beat_allowances() {
        let a = scoped();
        let cmd = vec!["cargo".into(), "run".into(), "--".into(), "rm -rf /".into()];
        assert!(!a.permits(&Action::RunCommand { command: cmd }).is_allowed());
    }

    #[test]
    fn network_reach_is_host_scoped() {
        let a = scoped();
        assert!(a.permits(&Action::NetFetch { url: "https://docs.rs/serde".into() }).is_allowed());
        assert!(!a.permits(&Action::NetFetch { url: "https://evil.example/x".into() }).is_allowed());

        let mut b = Authority::none();
        b.network.allowlist.insert("example.com".into());
        assert!(b.permits(&Action::NetFetch { url: "https://api.example.com/v1".into() }).is_allowed());
        assert!(!b.permits(&Action::NetFetch { url: "https://notexample.com/v1".into() }).is_allowed());
    }

    #[test]
    fn spending_beyond_the_ceiling_is_denied() {
        let a = scoped();
        assert!(a.permits(&Action::Spend { amount: Spend::tokens(1_000) }).is_allowed());
        assert!(!a.permits(&Action::Spend { amount: Spend::tokens(1_000_000) }).is_allowed());
    }

    #[test]
    fn intersect_is_contained_by_both_sides() {
        let a = scoped();
        let b = Authority {
            repository: RepoAuthority {
                read: PatternSet::parse(["src/**"]).unwrap(),
                write: PatternSet::parse(["src/auth/**", "src/parser/**"]).unwrap(),
            },
            git: GitAuthority { commit: true, merge: true, ..Default::default() },
            shell: ShellAuthority {
                enabled: true,
                allow: PatternSet::parse(["cargo*"]).unwrap(),
                deny: PatternSet::parse(["*sudo*"]).unwrap(),
            },
            resources: ResourceAuthority { max_workers: 8, max_concurrent_commands: 1 },
            budget: Budget::new(50_000, 1_000_000, 600),
            ..Authority::none()
        };
        let i = a.intersect(&b);
        assert!(a.contains(&i), "{:?}", a.containment_failures(&i));
        assert!(b.contains(&i), "{:?}", b.containment_failures(&i));
        assert_eq!(i.resources.max_workers, 4);
        assert!(!i.git.merge);
        assert!(i.git.commit);
    }

    #[test]
    fn lossy_attenuation_never_exceeds_the_grantor() {
        let parent = scoped();
        let greedy = Authority::root();
        let granted = parent.attenuate_lossy(&greedy);
        assert!(parent.contains(&granted), "{:?}", parent.containment_failures(&granted));
    }

    #[test]
    fn authority_round_trips_through_json() {
        let a = scoped();
        let s = serde_json::to_string(&a).unwrap();
        let back: Authority = serde_json::from_str(&s).unwrap();
        assert_eq!(a, back);
    }
}
