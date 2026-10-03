//! Workspace trust (direnv/mise style) for repo-committed configuration that makes `tm` run
//! code or relax review: `hooks.toml`, `acp.toml` and `oversight.toml`.
//!
//! A cloned repository can commit any of those files. Without a gate, `git clone evil && cd evil
//! && tm` would run its `session_start` hook (a shell command, with the user's environment) or
//! launch its `acp.toml` program, or silently loosen approvals. So these files are only loaded
//! when the user has explicitly run `tm trust` for this workspace *and* the files still hash to
//! what was trusted; any later edit (a `git pull`) puts the workspace back to untrusted.
//!
//! The trust list lives in the user's `$TM_HOME` (never in the repo), one line per workspace:
//! `<blake3 hex>  <canonical path>`.

use std::path::{Path, PathBuf};

use crate::error::{Result, TmError};

/// The repo-root files that are gated behind trust.
pub const GATED_FILES: [&str; 3] = ["hooks.toml", "acp.toml", "oversight.toml"];

/// Where the trust decisions are read from and written to.
#[derive(Debug, Clone)]
pub struct TrustPolicy {
    store: PathBuf,
    allow_all: bool,
}

/// How a workspace stands with respect to its gated files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustStatus {
    /// None of the gated files exist; nothing to trust.
    NoConfig,
    /// The gated files exist and match what the user trusted.
    Trusted,
    /// The gated files exist and were never trusted.
    Untrusted(Vec<String>),
    /// The gated files were trusted once but have changed since.
    Changed(Vec<String>),
}

impl TrustPolicy {
    /// The user's real policy: the list under `$TM_HOME` (default `$HOME/.tm`), or the file named
    /// by `TM_TRUST_FILE`. `TM_TRUST_ALL=1` (an explicit, user-set escape hatch for CI and
    /// automation; a repository cannot set environment variables) trusts everything.
    pub fn from_env() -> Self {
        let non_empty = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let store = non_empty("TM_TRUST_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = non_empty("TM_HOME")
                    .map(PathBuf::from)
                    .or_else(|| non_empty("HOME").map(|h| PathBuf::from(h).join(".tm")))
                    .unwrap_or_else(|| PathBuf::from(".tm"));
                home.join("trusted-workspaces")
            });
        TrustPolicy {
            store,
            allow_all: non_empty("TM_TRUST_ALL").is_some_and(|v| v == "1"),
        }
    }

    /// A policy reading and writing `store`.
    pub fn with_store(store: impl Into<PathBuf>) -> Self {
        TrustPolicy {
            store: store.into(),
            allow_all: false,
        }
    }

    /// A policy that trusts everything (tests, `TM_TRUST_ALL`).
    pub fn allow_all() -> Self {
        TrustPolicy {
            store: PathBuf::new(),
            allow_all: true,
        }
    }

    /// The gated files present in `root`.
    pub fn present_files(root: &Path) -> Vec<String> {
        GATED_FILES
            .iter()
            .filter(|f| root.join(f).exists())
            .map(|f| (*f).to_string())
            .collect()
    }

    /// Hash of the gated files' names and contents; `None` when there are none.
    pub fn fingerprint(root: &Path) -> Option<String> {
        let present = Self::present_files(root);
        if present.is_empty() {
            return None;
        }
        let mut hasher = blake3::Hasher::new();
        for name in &present {
            let bytes = std::fs::read(root.join(name)).unwrap_or_default();
            hasher.update(name.as_bytes());
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
        Some(hasher.finalize().to_hex().to_string())
    }

    fn key(root: &Path) -> String {
        std::fs::canonicalize(root)
            .unwrap_or_else(|_| root.to_path_buf())
            .display()
            .to_string()
    }

    fn entries(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(&self.store)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once("  "))
            .map(|(h, p)| (h.to_string(), p.to_string()))
            .collect()
    }

    /// Where `root` stands.
    pub fn status(&self, root: &Path) -> TrustStatus {
        let Some(current) = Self::fingerprint(root) else {
            return TrustStatus::NoConfig;
        };
        if self.allow_all {
            return TrustStatus::Trusted;
        }
        let key = Self::key(root);
        match self.entries().into_iter().find(|(_, p)| *p == key) {
            Some((hash, _)) if hash == current => TrustStatus::Trusted,
            Some(_) => TrustStatus::Changed(Self::present_files(root)),
            None => TrustStatus::Untrusted(Self::present_files(root)),
        }
    }

    /// Ok when `file` is absent from `root` or the workspace is trusted; otherwise an error
    /// telling the user what to review and which command to run.
    pub fn require(&self, root: &Path, file: &str) -> Result<()> {
        if !root.join(file).exists() {
            return Ok(());
        }
        match self.status(root) {
            TrustStatus::NoConfig | TrustStatus::Trusted => Ok(()),
            TrustStatus::Untrusted(files) | TrustStatus::Changed(files) => {
                Err(TmError::AuthorityDenied(format!(
                    "{file} in {} is not trusted ({} can run commands or change approvals). \
                     Review it, then run `tm trust` in that project to allow it.",
                    root.display(),
                    files.join(", ")
                )))
            }
        }
    }

    /// Record the current gated files of `root` as trusted, replacing any earlier entry. Returns
    /// the files that were trusted.
    pub fn trust(&self, root: &Path) -> Result<Vec<String>> {
        let Some(hash) = Self::fingerprint(root) else {
            return Ok(Vec::new());
        };
        let key = Self::key(root);
        let mut entries: Vec<_> = self
            .entries()
            .into_iter()
            .filter(|(_, p)| *p != key)
            .collect();
        entries.push((hash, key));
        self.write(&entries)?;
        Ok(Self::present_files(root))
    }

    /// Remove `root` from the trust list. Returns whether it was there.
    pub fn revoke(&self, root: &Path) -> Result<bool> {
        let key = Self::key(root);
        let before = self.entries();
        let after: Vec<_> = before.iter().filter(|(_, p)| *p != key).cloned().collect();
        let removed = after.len() != before.len();
        if removed {
            self.write(&after)?;
        }
        Ok(removed)
    }

    fn write(&self, entries: &[(String, String)]) -> Result<()> {
        if let Some(parent) = self.store.parent() {
            std::fs::create_dir_all(parent).map_err(|e| TmError::Io(e.to_string()))?;
        }
        let body: String = entries.iter().map(|(h, p)| format!("{h}  {p}\n")).collect();
        std::fs::write(&self.store, body).map_err(|e| TmError::Io(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, TrustPolicy) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("mkdir");
        let policy = TrustPolicy::with_store(dir.path().join("trust"));
        (dir, root, policy)
    }

    #[test]
    fn no_gated_files_means_nothing_to_trust() {
        let (_d, root, policy) = setup();
        assert_eq!(policy.status(&root), TrustStatus::NoConfig);
        assert!(policy.require(&root, "hooks.toml").is_ok());
    }

    #[test]
    fn committed_hooks_are_refused_until_trusted_and_again_after_a_change() {
        let (_d, root, policy) = setup();
        let hooks = root.join("hooks.toml");
        std::fs::write(&hooks, "session_start = []\n").expect("write");
        assert!(matches!(policy.status(&root), TrustStatus::Untrusted(_)));
        assert!(policy.require(&root, "hooks.toml").is_err());
        // A file that isn't there is never refused, even in an untrusted workspace.
        assert!(policy.require(&root, "acp.toml").is_ok());

        assert_eq!(policy.trust(&root).expect("trust"), vec!["hooks.toml"]);
        assert_eq!(policy.status(&root), TrustStatus::Trusted);
        assert!(policy.require(&root, "hooks.toml").is_ok());

        // A `git pull` that edits the file revokes trust.
        std::fs::write(&hooks, "session_start = [{command=[\"sh\"]}]\n").expect("rewrite");
        assert!(matches!(policy.status(&root), TrustStatus::Changed(_)));
        assert!(policy.require(&root, "hooks.toml").is_err());
    }

    #[test]
    fn adding_a_second_gated_file_invalidates_trust_and_revoke_works() {
        let (_d, root, policy) = setup();
        std::fs::write(root.join("oversight.toml"), "").expect("write");
        policy.trust(&root).expect("trust");
        std::fs::write(root.join("acp.toml"), "").expect("write");
        assert!(policy.require(&root, "acp.toml").is_err());
        policy.trust(&root).expect("trust again");
        assert!(policy.require(&root, "acp.toml").is_ok());
        assert!(policy.revoke(&root).expect("revoke"));
        assert!(policy.require(&root, "acp.toml").is_err());
    }

    #[test]
    fn trusting_one_workspace_does_not_trust_another() {
        let (d, root, policy) = setup();
        let other = d.path().join("other");
        std::fs::create_dir_all(&other).expect("mkdir");
        std::fs::write(root.join("hooks.toml"), "x").expect("write");
        std::fs::write(other.join("hooks.toml"), "x").expect("write");
        policy.trust(&root).expect("trust");
        assert!(policy.require(&other, "hooks.toml").is_err());
    }
}
