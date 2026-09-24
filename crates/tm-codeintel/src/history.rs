//! Owns git history as active knowledge: incremental ingest of commits, messages, changed
//! paths and hunk text via `git2`, plus history-aware queries — why a line looks the way it
//! does, searching messages/diffs, finding deleted implementations, and co-change frequency
//! between paths.
//!
//! Ingest is incremental like file indexing: commits already present (by SHA, unique in the
//! `commits` table) are not re-walked.

use std::collections::HashSet;
use std::sync::Arc;

use tm_types::{Clock, Result, TmError};

use crate::store::Store;

/// One commit that touched a path relevant to a [`WhyAnswer`] or search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSummary {
    /// Full commit SHA.
    pub sha: String,
    /// Author name.
    pub author: String,
    /// Commit timestamp, Unix seconds, as recorded in the commit.
    pub authored_at: i64,
    /// Commit message (full, including any trailers).
    pub message: String,
}

/// The answer to `why(path, line_range)`: the chain from a line range to the commits that
/// last touched it and their messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhyAnswer {
    /// Path queried.
    pub path: String,
    /// Line range queried, 1-based inclusive.
    pub line_start: u32,
    /// End of line range, 1-based inclusive.
    pub line_end: u32,
    /// Commits that last touched any line in the range, most recent first.
    pub commits: Vec<CommitSummary>,
}

/// A message/diff search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryHit {
    /// The matching commit.
    pub commit: CommitSummary,
    /// Path the match occurred in, if the match was within a diff hunk rather than the
    /// message body.
    pub path: Option<String>,
    /// Snippet of the matching text (message excerpt or hunk line).
    pub snippet: String,
}

/// A path/text pair describing something that used to exist but was removed, surfaced by
/// [`HistoryIndex::deleted`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedImplementation {
    /// Path the removed code lived in (may no longer exist in the working tree).
    pub path: String,
    /// Commit that removed it.
    pub commit: CommitSummary,
    /// The removed text (deleted lines from the hunk).
    pub removed_text: String,
}

/// How often two paths change together, from co-commit history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoChange {
    /// The other path.
    pub path: String,
    /// Number of commits in which both paths were touched together.
    pub co_commits: u64,
    /// Number of commits that touched the queried path at all (denominator for a ratio).
    pub total_commits_for_queried_path: u64,
}

/// Git history ingestion and query surface for one project's repository.
pub struct HistoryIndex {
    store: Arc<Store>,
    repo_root: std::path::PathBuf,
}

fn git_err(e: git2::Error) -> TmError {
    TmError::storage(format!("git2: {e}"))
}

fn row_to_commit(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommitSummary> {
    Ok(CommitSummary {
        sha: row.get("sha")?,
        author: row.get("author")?,
        authored_at: row.get("authored_at")?,
        message: row.get("message")?,
    })
}

impl HistoryIndex {
    /// A history index over `store`, reading commits from the git repository at `repo_root`.
    pub fn new(store: Arc<Store>, repo_root: impl Into<std::path::PathBuf>) -> Self {
        HistoryIndex {
            store,
            repo_root: repo_root.into(),
        }
    }

    /// Ingest any commits reachable from HEAD not already present in `commits` (by SHA),
    /// storing message, author, timestamp, and per-path hunk text/line counts in
    /// `commit_files`. `clock` is unused for commit timestamps (those come from the commit
    /// itself) but is available for any bookkeeping metadata this ingest records (e.g. last
    /// ingest time in `doc_meta`), keeping this module off the wall clock per the crate's
    /// determinism rule.
    pub fn ingest_incremental(&self, clock: &dyn Clock) -> Result<u64> {
        let repo = git2::Repository::open(&self.repo_root).map_err(git_err)?;

        let existing: HashSet<String> = {
            let conn = self.store.reader()?;
            let mut stmt = conn
                .prepare("SELECT sha FROM commits")
                .map_err(|e| TmError::storage(format!("prepare: {e}")))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| TmError::storage(format!("query: {e}")))?;
            let mut set = HashSet::new();
            for r in rows {
                set.insert(r.map_err(|e| TmError::storage(format!("row: {e}")))?);
            }
            set
        };

        let mut revwalk = repo.revwalk().map_err(git_err)?;
        revwalk.push_head().map_err(git_err)?;
        revwalk
            .set_sorting(git2::Sort::TOPOLOGICAL)
            .map_err(git_err)?;

        let mut new_commits: Vec<git2::Oid> = Vec::new();
        for oid in revwalk {
            let oid = oid.map_err(git_err)?;
            if !existing.contains(&oid.to_string()) {
                new_commits.push(oid);
            }
        }

        if new_commits.is_empty() {
            return Ok(0);
        }

        let conn = self.store.writer()?;
        let mut ingested = 0u64;
        for oid in &new_commits {
            let commit = repo.find_commit(*oid).map_err(git_err)?;
            let sha = commit.id().to_string();
            let author = commit.author().name().unwrap_or("").to_string();
            let authored_at = commit.time().seconds();
            let message = commit.message().unwrap_or("").to_string();

            let tree = commit.tree().map_err(git_err)?;
            let parent_tree = if commit.parent_count() > 0 {
                Some(commit.parent(0).map_err(git_err)?.tree().map_err(git_err)?)
            } else {
                None
            };
            let diff = repo
                .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)
                .map_err(git_err)?;

            let mut file_rows: Vec<(String, String, u32, u32)> = Vec::new();
            let num_deltas = diff.deltas().len();
            for idx in 0..num_deltas {
                let patch = git2::Patch::from_diff(&diff, idx).map_err(git_err)?;
                let Some(mut patch) = patch else { continue };
                let path = patch
                    .delta()
                    .new_file()
                    .path()
                    .or_else(|| patch.delta().old_file().path())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let (_context, additions, deletions) = patch.line_stats().map_err(git_err)?;
                let buf = patch.to_buf().map_err(git_err)?;
                let hunk_text = String::from_utf8_lossy(&buf).into_owned();
                file_rows.push((path, hunk_text, additions as u32, deletions as u32));
            }

            conn.execute(
                "INSERT INTO commits(sha, author, authored_at, message) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![sha, author, authored_at, message],
            )
            .map_err(|e| TmError::storage(format!("insert commit: {e}")))?;
            let commit_id = conn.last_insert_rowid();

            for (path, hunk_text, additions, deletions) in file_rows {
                conn.execute(
                    "INSERT INTO commit_files(commit_id, path, hunk_text, additions, deletions) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![commit_id, path, hunk_text, additions, deletions],
                )
                .map_err(|e| TmError::storage(format!("insert commit_files: {e}")))?;
            }

            ingested += 1;
        }

        Store::set_meta(&conn, "history.last_ingest", &clock.now().to_rfc3339())
            .map_err(|e| TmError::storage(format!("set_meta: {e}")))?;
        conn.execute_batch("COMMIT")
            .map_err(|e| TmError::storage(format!("commit txn: {e}")))?;

        Ok(ingested)
    }

    /// Blame `path` over `line_start..=line_end`, map the responsible commits, and return
    /// their messages, most recent first.
    pub fn why(&self, path: &str, line_start: u32, line_end: u32) -> Result<WhyAnswer> {
        if line_start == 0 || line_end < line_start {
            return Err(TmError::invariant(format!(
                "invalid line range {line_start}..={line_end} for {path}"
            )));
        }

        let repo = git2::Repository::open(&self.repo_root).map_err(git_err)?;
        let mut opts = git2::BlameOptions::new();
        opts.min_line(line_start as usize)
            .max_line(line_end as usize);
        let blame = repo
            .blame_file(std::path::Path::new(path), Some(&mut opts))
            .map_err(git_err)?;

        let mut shas: HashSet<String> = HashSet::new();
        for hunk in blame.iter() {
            shas.insert(hunk.final_commit_id().to_string());
        }

        let conn = self.store.reader()?;
        let mut commits: Vec<CommitSummary> = Vec::new();
        for sha in &shas {
            let mut stmt = conn
                .prepare("SELECT sha, author, authored_at, message FROM commits WHERE sha = ?1")
                .map_err(|e| TmError::storage(format!("prepare: {e}")))?;
            let mut rows = stmt
                .query_map(rusqlite::params![sha], row_to_commit)
                .map_err(|e| TmError::storage(format!("query: {e}")))?;
            if let Some(row) = rows.next() {
                commits.push(row.map_err(|e| TmError::storage(format!("row: {e}")))?);
            } else {
                // Not in the commits table yet (e.g. made after the last
                // ingest_incremental) — don't silently drop it, build the summary
                // straight from git2 instead.
                let oid = git2::Oid::from_str(sha).map_err(git_err)?;
                let commit = repo.find_commit(oid).map_err(git_err)?;
                commits.push(CommitSummary {
                    sha: commit.id().to_string(),
                    author: commit.author().name().unwrap_or("").to_string(),
                    authored_at: commit.time().seconds(),
                    message: commit.message().unwrap_or("").to_string(),
                });
            }
        }

        commits.sort_by_key(|c| std::cmp::Reverse(c.authored_at));

        Ok(WhyAnswer {
            path: path.to_string(),
            line_start,
            line_end,
            commits,
        })
    }

    /// Search commit messages and diff hunk text for `query` (substring match; case-insensitive),
    /// returning matches from both.
    pub fn search(&self, query: &str) -> Result<Vec<HistoryHit>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.store.reader()?;
        let pattern = format!("%{query}%");
        let mut hits: Vec<HistoryHit> = Vec::new();

        {
            let mut stmt = conn
                .prepare(
                    "SELECT sha, author, authored_at, message FROM commits WHERE message LIKE ?1",
                )
                .map_err(|e| TmError::storage(format!("prepare: {e}")))?;
            let rows = stmt
                .query_map(rusqlite::params![pattern], row_to_commit)
                .map_err(|e| TmError::storage(format!("query: {e}")))?;
            for row in rows {
                let commit = row.map_err(|e| TmError::storage(format!("row: {e}")))?;
                let snippet = excerpt(&commit.message, query);
                hits.push(HistoryHit {
                    commit,
                    path: None,
                    snippet,
                });
            }
        }

        {
            let mut stmt = conn
                .prepare(
                    "SELECT c.sha as sha, c.author as author, c.authored_at as authored_at, \
                     c.message as message, cf.path as path, cf.hunk_text as hunk_text \
                     FROM commit_files cf JOIN commits c ON c.id = cf.commit_id \
                     WHERE cf.hunk_text LIKE ?1",
                )
                .map_err(|e| TmError::storage(format!("prepare: {e}")))?;
            let rows = stmt
                .query_map(rusqlite::params![pattern], |row| {
                    let commit = row_to_commit(row)?;
                    let path: String = row.get("path")?;
                    let hunk_text: String = row.get("hunk_text")?;
                    Ok((commit, path, hunk_text))
                })
                .map_err(|e| TmError::storage(format!("query: {e}")))?;
            for row in rows {
                let (commit, path, hunk_text) =
                    row.map_err(|e| TmError::storage(format!("row: {e}")))?;
                let snippet = excerpt(&hunk_text, query);
                hits.push(HistoryHit {
                    commit,
                    path: Some(path),
                    snippet,
                });
            }
        }

        hits.sort_by_key(|h| std::cmp::Reverse(h.commit.authored_at));
        Ok(hits)
    }

    /// Find implementations matching `query` that were removed and never reintroduced: commits
    /// whose diff hunks contain deleted lines matching `query`, for paths where no later commit
    /// re-added matching text and the path either no longer exists or no longer contains it.
    pub fn deleted(&self, query: &str) -> Result<Vec<DeletedImplementation>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.store.reader()?;
        let needle = query.to_lowercase();

        let mut stmt = conn
            .prepare(
                "SELECT c.sha as sha, c.author as author, c.authored_at as authored_at, \
                 c.message as message, cf.path as path, cf.hunk_text as hunk_text \
                 FROM commit_files cf JOIN commits c ON c.id = cf.commit_id \
                 ORDER BY c.authored_at DESC",
            )
            .map_err(|e| TmError::storage(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let commit = row_to_commit(row)?;
                let path: String = row.get("path")?;
                let hunk_text: String = row.get("hunk_text")?;
                Ok((commit, path, hunk_text))
            })
            .map_err(|e| TmError::storage(format!("query: {e}")))?;

        let mut seen_paths: HashSet<String> = HashSet::new();
        let mut candidates: Vec<DeletedImplementation> = Vec::new();
        for row in rows {
            let (commit, path, hunk_text) =
                row.map_err(|e| TmError::storage(format!("row: {e}")))?;
            if seen_paths.contains(&path) {
                continue;
            }
            let removed_lines: Vec<&str> = hunk_text
                .lines()
                .filter(|l| l.starts_with('-') && !l.starts_with("---"))
                .filter(|l| l.to_lowercase().contains(&needle))
                .collect();
            if removed_lines.is_empty() {
                continue;
            }
            seen_paths.insert(path.clone());
            candidates.push(DeletedImplementation {
                path,
                commit,
                removed_text: removed_lines.join("\n"),
            });
        }

        let mut deleted = Vec::new();
        for candidate in candidates {
            let current_path = self.repo_root.join(&candidate.path);
            let still_present = std::fs::read_to_string(&current_path)
                .map(|content| content.to_lowercase().contains(&needle))
                .unwrap_or(false);
            if !still_present {
                deleted.push(candidate);
            }
        }

        Ok(deleted)
    }

    /// Co-change frequency between `path` and every other path it has ever been committed
    /// alongside, most frequent first.
    pub fn co_change(&self, path: &str) -> Result<Vec<CoChange>> {
        let conn = self.store.reader()?;

        let total_commits_for_queried_path: u64 =
            conn.query_row(
                "SELECT COUNT(*) FROM commit_files WHERE path = ?1",
                rusqlite::params![path],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| TmError::storage(format!("count: {e}")))? as u64;

        let mut stmt = conn
            .prepare(
                "SELECT cf2.path as other_path, COUNT(*) as co_commits \
                 FROM commit_files cf1 \
                 JOIN commit_files cf2 ON cf1.commit_id = cf2.commit_id AND cf2.path != cf1.path \
                 WHERE cf1.path = ?1 \
                 GROUP BY cf2.path \
                 ORDER BY co_commits DESC",
            )
            .map_err(|e| TmError::storage(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params![path], |row| {
                let other_path: String = row.get("other_path")?;
                let co_commits: i64 = row.get("co_commits")?;
                Ok((other_path, co_commits as u64))
            })
            .map_err(|e| TmError::storage(format!("query: {e}")))?;

        let mut out = Vec::new();
        for row in rows {
            let (other_path, co_commits) =
                row.map_err(|e| TmError::storage(format!("row: {e}")))?;
            out.push(CoChange {
                path: other_path,
                co_commits,
                total_commits_for_queried_path,
            });
        }

        Ok(out)
    }
}

/// Case-insensitive first-match excerpt of `haystack` around `needle`, for a search snippet.
fn excerpt(haystack: &str, needle: &str) -> String {
    let lower_haystack = haystack.to_lowercase();
    let lower_needle = needle.to_lowercase();
    if let Some(line) = haystack
        .lines()
        .find(|l| l.to_lowercase().contains(&lower_needle))
    {
        return line.trim().to_string();
    }
    if let Some(pos) = lower_haystack.find(&lower_needle) {
        let start = pos.saturating_sub(20);
        let end = (pos + needle.len() + 20).min(haystack.len());
        return haystack[start..end].trim().to_string();
    }
    haystack.trim().to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tm_types::FixedClock;

    use super::*;

    fn init_repo() -> (tempfile::TempDir, git2::Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        {
            let mut config = repo.config().unwrap();
            config.set_str("user.name", "Test Author").unwrap();
            config.set_str("user.email", "test@example.com").unwrap();
        }
        (dir, repo)
    }

    fn write_file(dir: &std::path::Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    fn commit_all(repo: &git2::Repository, message: &str, time: i64) -> git2::Oid {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig =
            git2::Signature::new("Test Author", "test@example.com", &git2::Time::new(time, 0))
                .unwrap();
        let parents: Vec<git2::Commit> = match repo.head() {
            Ok(head) => vec![head.peel_to_commit().unwrap()],
            Err(_) => vec![],
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
            .unwrap()
    }

    fn open_index(dir: &std::path::Path) -> HistoryIndex {
        let store = Arc::new(Store::open(dir).unwrap());
        HistoryIndex::new(store, dir)
    }

    #[test]
    fn ingest_incremental_stores_new_commits_and_is_idempotent() {
        let (dir, repo) = init_repo();
        write_file(dir.path(), "a.txt", "hello\nworld\n");
        commit_all(&repo, "add a.txt", 1_000);
        write_file(dir.path(), "b.txt", "second file\n");
        commit_all(&repo, "add b.txt", 2_000);

        let history = open_index(dir.path());
        let clock = FixedClock::epoch();
        let count = history.ingest_incremental(&clock).unwrap();
        assert_eq!(count, 2);

        let again = history.ingest_incremental(&clock).unwrap();
        assert_eq!(again, 0, "already-ingested commits are not re-walked");
    }

    #[test]
    fn ingest_incremental_handles_root_commit_diff_against_empty_tree() {
        let (dir, repo) = init_repo();
        write_file(dir.path(), "only.txt", "content\n");
        commit_all(&repo, "root commit", 500);

        let history = open_index(dir.path());
        let clock = FixedClock::epoch();
        let count = history.ingest_incremental(&clock).unwrap();
        assert_eq!(count, 1);

        let hits = history.search("root commit").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].commit.message.trim(), "root commit");
    }

    #[test]
    fn why_resolves_blame_to_the_commit_that_introduced_the_line() {
        let (dir, repo) = init_repo();
        write_file(dir.path(), "a.txt", "line one\nline two\n");
        commit_all(&repo, "add a.txt", 1_000);

        let history = open_index(dir.path());
        let clock = FixedClock::epoch();
        history.ingest_incremental(&clock).unwrap();

        let answer = history.why("a.txt", 1, 2).unwrap();
        assert_eq!(answer.path, "a.txt");
        assert_eq!(answer.commits.len(), 1);
        assert_eq!(answer.commits[0].message.trim(), "add a.txt");
    }

    #[test]
    fn why_includes_a_commit_made_after_the_last_ingest() {
        let (dir, repo) = init_repo();
        write_file(dir.path(), "a.txt", "line one\nline two\n");
        commit_all(&repo, "add a.txt", 1_000);

        let history = open_index(dir.path());
        let clock = FixedClock::epoch();
        history.ingest_incremental(&clock).unwrap();

        // A later commit that never went through ingest_incremental — its sha has no
        // row in the commits table.
        write_file(dir.path(), "a.txt", "line one\nline two changed\n");
        let later_oid = commit_all(&repo, "change line two", 2_000);

        let answer = history.why("a.txt", 2, 2).unwrap();
        assert_eq!(answer.commits.len(), 1);
        let commit = &answer.commits[0];
        assert_eq!(commit.sha, later_oid.to_string());
        assert_eq!(commit.author, "Test Author");
        assert_eq!(commit.message.trim(), "change line two");
    }

    #[test]
    fn why_rejects_an_empty_or_inverted_line_range() {
        let (dir, _repo) = init_repo();
        let history = open_index(dir.path());
        assert!(history.why("a.txt", 0, 1).is_err());
        assert!(history.why("a.txt", 5, 2).is_err());
    }

    #[test]
    fn search_finds_matches_in_both_messages_and_diff_hunks() {
        let (dir, repo) = init_repo();
        write_file(dir.path(), "a.txt", "unique_marker_text\n");
        commit_all(&repo, "unrelated message", 1_000);

        let history = open_index(dir.path());
        let clock = FixedClock::epoch();
        history.ingest_incremental(&clock).unwrap();

        let hits = history.search("unique_marker_text").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_deref(), Some("a.txt"));
    }

    #[test]
    fn search_is_case_insensitive() {
        let (dir, repo) = init_repo();
        write_file(dir.path(), "a.txt", "hello\n");
        commit_all(&repo, "Add Greeting", 1_000);

        let history = open_index(dir.path());
        let clock = FixedClock::epoch();
        history.ingest_incremental(&clock).unwrap();

        let hits = history.search("greeting").unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_returns_empty_for_an_empty_query() {
        let (dir, _repo) = init_repo();
        let history = open_index(dir.path());
        assert!(history.search("").unwrap().is_empty());
    }

    #[test]
    fn deleted_finds_removed_text_absent_from_current_file() {
        let (dir, repo) = init_repo();
        write_file(dir.path(), "a.txt", "fn legacy_impl() {}\n");
        commit_all(&repo, "add legacy_impl", 1_000);
        write_file(dir.path(), "a.txt", "fn new_impl() {}\n");
        commit_all(&repo, "remove legacy_impl", 2_000);

        let history = open_index(dir.path());
        let clock = FixedClock::epoch();
        history.ingest_incremental(&clock).unwrap();

        let results = history.deleted("legacy_impl").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "a.txt");
        assert_eq!(results[0].commit.message.trim(), "remove legacy_impl");
    }

    #[test]
    fn deleted_excludes_text_still_present_in_the_working_tree() {
        let (dir, repo) = init_repo();
        write_file(dir.path(), "a.txt", "fn still_here() {}\nfn temp() {}\n");
        commit_all(&repo, "add both", 1_000);
        write_file(dir.path(), "a.txt", "fn still_here() {}\n");
        commit_all(&repo, "remove temp", 2_000);

        let history = open_index(dir.path());
        let clock = FixedClock::epoch();
        history.ingest_incremental(&clock).unwrap();

        assert!(history.deleted("still_here").unwrap().is_empty());
        assert_eq!(history.deleted("temp").unwrap().len(), 1);
    }

    #[test]
    fn co_change_counts_commits_touching_both_paths() {
        let (dir, repo) = init_repo();
        write_file(dir.path(), "a.txt", "a1\n");
        write_file(dir.path(), "b.txt", "b1\n");
        commit_all(&repo, "add a and b together", 1_000);
        write_file(dir.path(), "a.txt", "a2\n");
        commit_all(&repo, "change only a", 2_000);

        let history = open_index(dir.path());
        let clock = FixedClock::epoch();
        history.ingest_incremental(&clock).unwrap();

        let co = history.co_change("a.txt").unwrap();
        assert_eq!(co.len(), 1);
        assert_eq!(co[0].path, "b.txt");
        assert_eq!(co[0].co_commits, 1);
        assert_eq!(co[0].total_commits_for_queried_path, 2);
    }

    #[test]
    fn co_change_is_empty_for_a_path_never_committed_with_others() {
        let (dir, repo) = init_repo();
        write_file(dir.path(), "solo.txt", "content\n");
        commit_all(&repo, "solo commit", 1_000);

        let history = open_index(dir.path());
        let clock = FixedClock::epoch();
        history.ingest_incremental(&clock).unwrap();

        assert!(history.co_change("solo.txt").unwrap().is_empty());
    }
}
