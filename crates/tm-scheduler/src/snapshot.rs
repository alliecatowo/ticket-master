//! A per-turn, `git stash`-style workspace snapshot (`docs/decisions/D-008-ticket-checkpoint-fork.md`).
//!
//! [`capture_workspace_snapshot`] is the primitive [`crate::dispatch::report_outcome`] calls
//! right after a turn produces a real patch: `git stash create` builds a commit object holding
//! the current index/working-tree state relative to `HEAD` *without* touching either (unlike
//! `git stash push`, which is why this shells out to the porcelain command rather than using the
//! `git2` bindings already in this workspace — see the decision doc for why `git2` cannot do
//! this directly). The resulting commit is otherwise unreachable from any ref the moment the
//! `git stash create` process exits, so this also pins it under `refs/tm/snapshots/<sha>` —
//! without that, the next `git gc` could reap it before anyone ever reads it back.
//!
//! Deliberately infallible from the caller's perspective ([`Option`], never [`Result`]): a
//! snapshot is auxiliary to the turn that produced it, never load-bearing for the ticket's own
//! submission, so any failure here (no `git` on `$PATH`, not a git repository, a detached-HEAD
//! edge case) is logged and treated as "nothing to capture," never surfaced as an error that
//! could turn a successful turn into a recorded failure.

use std::path::Path;
use std::process::Command;

/// One captured workspace snapshot: a stash commit's sha, the ref pinning it against `git gc`,
/// and the `HEAD` it was taken relative to (best-effort; `None` if `git rev-parse HEAD` itself
/// failed, e.g. a brand-new repository with no commits yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    /// The `git stash create` commit's sha.
    pub sha: String,
    /// The ref this sha was pinned under (`refs/tm/snapshots/<sha>`), so it survives `git gc`.
    pub git_ref: String,
    /// `git rev-parse HEAD` at capture time, if it succeeded.
    pub base_head: Option<String>,
}

/// Run `git stash create` in `repo_root`, pin the result under `refs/tm/snapshots/<sha>`, and
/// return it — or `None` if there was nothing to capture (a clean working tree, `repo_root` is
/// not a git repository, or `git` itself could not be run). See this module's doc comment for
/// why failure here is swallowed rather than propagated.
pub fn capture_workspace_snapshot(repo_root: &Path) -> Option<WorkspaceSnapshot> {
    let stash = match Command::new("git")
        .args(["stash", "create"])
        .current_dir(repo_root)
        .output()
    {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            tracing::warn!(
                stderr = %String::from_utf8_lossy(&out.stderr),
                "git stash create did not succeed; skipping workspace snapshot"
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not run `git`; skipping workspace snapshot");
            return None;
        }
    };

    let sha = String::from_utf8_lossy(&stash.stdout).trim().to_string();
    if sha.is_empty() {
        // A clean working tree: `git stash create` succeeds with empty stdout rather than
        // erroring, so this is the common "nothing changed this turn" case, not a failure.
        return None;
    }

    let git_ref = format!("refs/tm/snapshots/{sha}");
    match Command::new("git")
        .args(["update-ref", &git_ref, &sha])
        .current_dir(repo_root)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            tracing::warn!(
                %sha, %git_ref, ?status,
                "git update-ref did not succeed; snapshot commit is unpinned and may be gc'd"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e, %sha, %git_ref,
                "could not run `git update-ref`; snapshot commit is unpinned and may be gc'd"
            );
        }
    }

    let base_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());

    Some(WorkspaceSnapshot {
        sha,
        git_ref,
        base_head,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A tempdir git repo with an initial commit, user identity configured so `git stash create`
    /// (which authors a real commit under the hood) never fails for lack of one.
    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        fs::write(root.join("a.txt"), "one\n").expect("write a.txt");
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "initial"]);
        dir
    }

    #[test]
    fn clean_working_tree_captures_nothing() {
        let dir = init_repo();
        assert_eq!(capture_workspace_snapshot(dir.path()), None);
    }

    #[test]
    fn not_a_git_repository_captures_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(capture_workspace_snapshot(dir.path()), None);
    }

    #[test]
    fn modified_tracked_file_is_captured_as_a_real_commit_and_pinned() {
        let dir = init_repo();
        let root = dir.path();
        fs::write(root.join("a.txt"), "two\n").expect("modify a.txt");

        let snapshot = capture_workspace_snapshot(root).expect("snapshot captured");
        assert_eq!(
            snapshot.git_ref,
            format!("refs/tm/snapshots/{}", snapshot.sha)
        );
        assert!(snapshot.base_head.is_some(), "HEAD exists in this repo");

        // The stash commit is a real, readable git object...
        let cat_file = Command::new("git")
            .args(["cat-file", "-t", &snapshot.sha])
            .current_dir(root)
            .output()
            .expect("git cat-file");
        assert!(cat_file.status.success());
        assert_eq!(String::from_utf8_lossy(&cat_file.stdout).trim(), "commit");

        // ...and the ref actually resolves to it, so it survives a `git gc`.
        let rev_parse = Command::new("git")
            .args(["rev-parse", &snapshot.git_ref])
            .current_dir(root)
            .output()
            .expect("git rev-parse");
        assert!(rev_parse.status.success());
        assert_eq!(
            String::from_utf8_lossy(&rev_parse.stdout).trim(),
            snapshot.sha
        );

        // `git stash create` never touches the working tree, so the modification is still there
        // uncommitted, exactly as it was before capture.
        let contents = fs::read_to_string(root.join("a.txt")).expect("read a.txt");
        assert_eq!(contents, "two\n");
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(root)
            .output()
            .expect("git status");
        assert!(
            !String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "working tree should still show the uncommitted modification"
        );
    }

    #[test]
    fn untracked_file_alone_is_not_captured() {
        // `git stash create` (like plain `git stash push`) never captures untracked files unless
        // told to — a real, documented limitation (see this module's doc comment and D-008),
        // reproduced here so a future change to that default is caught by a failing test rather
        // than silently changing what a snapshot actually protects.
        let dir = init_repo();
        fs::write(dir.path().join("new.txt"), "untracked\n").expect("write new.txt");
        assert_eq!(capture_workspace_snapshot(dir.path()), None);
    }
}
