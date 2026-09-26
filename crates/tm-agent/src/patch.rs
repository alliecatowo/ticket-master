//! The patch engine: applies edits with conflict detection, never a blind overwrite.
//!
//! Every [`Edit`] carries the hash of the file content the caller last observed
//! ([`Edit::expected_hash`]); [`PatchEngine::apply`] refuses (returns [`PatchOutcome::Conflict`])
//! whenever the file's current on-disk hash doesn't match, rather than silently clobbering a
//! concurrent change. A successful apply always produces a [`Patch`] — a precise unified diff
//! plus before/after hashes — never just "the file changed, trust me". No apply is ever
//! attempted outside the write scope carried by the [`tm_types::Authority`] the engine was
//! built with: `PatchEngine` checks `Authority::repository.write` itself, in addition to
//! whatever gate [`crate::tools::ToolRegistry`] already applied before dispatch, so a bug in
//! the tool layer can't turn into an out-of-scope write.

use std::path::{Component, Path, PathBuf};

use similar::{ChangeTag, TextDiff};
use tm_core::artifact::hash_bytes;
use tm_types::{Authority, Result, TmError};

/// One requested edit. Every variant that touches existing content carries an
/// `expected_hash`: the blake3 hex hash ([`tm_core::artifact::hash_bytes`]) of the content the
/// caller last read, used to detect a conflicting change made since. The tool layer hands this
/// exact value to the model as the `hash` field of `fs.read`/`fs.read_range`/`fs.stat`, so a
/// model never has to (and never should) compute it itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Create a new file. Fails as a conflict if the path already exists.
    Create {
        /// Path relative to the project root.
        path: String,
        /// Full file content.
        content: String,
    },
    /// Overwrite a file's entire content.
    Write {
        /// Path relative to the project root.
        path: String,
        /// New full file content.
        content: String,
        /// Hash of the content last observed at this path; `None` only for a file the caller
        /// has never read (fresh-write intent, still checked against "must not already exist").
        expected_hash: Option<String>,
    },
    /// Delete a file.
    Delete {
        /// Path relative to the project root.
        path: String,
        /// Hash of the content last observed at this path.
        expected_hash: Option<String>,
    },
    /// Replace a byte range within a file, leaving the rest untouched. Byte offsets are the
    /// fallback anchor form (prefer [`Edit::TextReplace`]); when `old_text` is present it is
    /// checked against the file's current content at `[byte_start, byte_end)` before the
    /// replacement is applied, so a byte range computed against stale content is caught as a
    /// conflict instead of silently splicing at the wrong bytes.
    RangeReplace {
        /// Path relative to the project root.
        path: String,
        /// Start byte offset, inclusive.
        byte_start: usize,
        /// End byte offset, exclusive.
        byte_end: usize,
        /// Replacement text for the range.
        replacement: String,
        /// The text the caller believes occupies `[byte_start, byte_end)`. When present, it must
        /// match exactly or the edit is refused as a conflict rather than applied.
        old_text: Option<String>,
        /// Hash of the full file content last observed.
        expected_hash: Option<String>,
    },
    /// Replace an exact, unique run of text within a file, without the caller having to compute
    /// byte offsets at all — the preferred anchor form (mirrors Claude Code's own `Edit` tool).
    /// `old_text` must occur in the file's current content exactly once.
    TextReplace {
        /// Path relative to the project root.
        path: String,
        /// The exact text to find; must be present exactly once in the current content.
        old_text: String,
        /// The text to replace it with.
        new_text: String,
        /// Hash of the full file content last observed.
        expected_hash: Option<String>,
    },
}

impl Edit {
    /// The path this edit targets, regardless of variant.
    pub fn path(&self) -> &str {
        match self {
            Edit::Create { path, .. }
            | Edit::Write { path, .. }
            | Edit::Delete { path, .. }
            | Edit::RangeReplace { path, .. }
            | Edit::TextReplace { path, .. } => path,
        }
    }
}

/// A precise summary of what one applied edit changed, independent of the full diff text, for
/// callers that want counts without parsing the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSummary {
    /// Lines added.
    pub lines_added: usize,
    /// Lines removed.
    pub lines_removed: usize,
    /// Whether the edit created a new file.
    pub created: bool,
    /// Whether the edit deleted the file.
    pub deleted: bool,
}

/// The artifact-shaped record of a successful edit: a unified diff plus the hashes it moved
/// between. [`crate::agent_loop::AgentLoop`] stores this as a `tm_core::ArtifactKind::Patch`
/// artifact via `tm_core::Store::store_artifact`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    /// Path the patch touched.
    pub path: String,
    /// Unified diff text, `similar`-generated.
    pub unified_diff: String,
    /// Blake3 hex hash of the content before the edit (absent for a `Create`).
    pub hash_before: Option<String>,
    /// Blake3 hex hash of the content after the edit (absent for a `Delete`).
    pub hash_after: Option<String>,
    /// Precise change counts.
    pub summary: DiffSummary,
}

/// Why a [`PatchEngine::apply`] call did not produce an applied [`Patch`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    /// The edit's `expected_hash` didn't match the file's current on-disk hash (or existence),
    /// i.e. the file changed underneath the caller.
    #[error("conflict at {path}: {detail}")]
    Conflict {
        /// The path in conflict.
        path: String,
        /// Human-readable detail that also says how to recover (e.g. "expected_hash abc123 does
        /// not match the file's current content. ... Re-read the file with fs.read ...").
        detail: String,
    },
    /// The edit's path falls outside the engine's authority write scope.
    #[error("write to {path} is outside authority write scope")]
    OutOfScope {
        /// The out-of-scope path.
        path: String,
    },
    /// A byte range in a [`Edit::RangeReplace`] was invalid for the file's current content.
    #[error("invalid range [{byte_start}, {byte_end}) for {path}")]
    InvalidRange {
        /// The path.
        path: String,
        /// Requested start.
        byte_start: usize,
        /// Requested end.
        byte_end: usize,
    },
    /// The path escaped the project root (e.g. via `..` components) or wasn't valid UTF-8.
    #[error("invalid path: {0}")]
    InvalidPath(String),
    /// Underlying filesystem I/O failure.
    #[error("io error at {path}: {detail}")]
    Io {
        /// The path.
        path: String,
        /// Error detail.
        detail: String,
    },
}

/// The result of a single [`PatchEngine::apply`] call.
pub type PatchOutcome = std::result::Result<Patch, PatchError>;

/// Applies [`Edit`]s to files under a project root, gated by an [`Authority`]'s write scope.
pub struct PatchEngine {
    root: PathBuf,
    authority: Authority,
}

impl PatchEngine {
    /// Build an engine rooted at `root`, whose writes are checked against `authority`'s
    /// repository write scope.
    pub fn new(root: PathBuf, authority: Authority) -> Self {
        PatchEngine { root, authority }
    }

    /// The project root this engine writes under.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Apply one [`Edit`], producing a [`Patch`] on success.
    ///
    /// # Errors
    /// - [`PatchError::OutOfScope`] if `edit.path()` is not contained by
    ///   `self.authority.repository.write` (checked via `tm_types::PatternSet::matches`).
    /// - [`PatchError::InvalidPath`] if the path is absolute, contains `..` components, or is
    ///   not valid UTF-8 once joined to `self.root`.
    /// - [`PatchError::Conflict`] if the edit carries an `expected_hash` (or a `Create`/`Delete`
    ///   existence expectation) that doesn't match the file's current state on disk right now.
    /// - [`PatchError::InvalidRange`] for a [`Edit::RangeReplace`] whose byte range doesn't fall
    ///   on the current content's boundaries (`byte_start <= byte_end <= content.len()`).
    /// - [`PatchError::Io`] for any underlying read/write failure.
    pub fn apply(&self, edit: &Edit) -> PatchOutcome {
        let path = edit.path();
        let abs = self.resolve(path)?;
        if !self.authority.repository.write.matches(path) {
            return Err(PatchError::OutOfScope {
                path: path.to_string(),
            });
        }
        let existing = self.read_existing(&abs, path)?;

        match edit {
            Edit::Create { path: p, content } => {
                if existing.is_some() {
                    return Err(PatchError::Conflict {
                        path: p.clone(),
                        detail: CREATE_OVER_EXISTING.to_string(),
                    });
                }
                self.write_atomic(&abs, path, content.as_bytes())?;
                let diff = TextDiff::from_lines("", content.as_str());
                Ok(Patch {
                    path: p.clone(),
                    unified_diff: unified_diff_text(&diff, p),
                    hash_before: None,
                    hash_after: Some(hash_bytes(content.as_bytes())),
                    summary: diff_summary(&diff, true, false),
                })
            }
            Edit::Write {
                path: p,
                content,
                expected_hash,
            } => {
                check_expectation(p, existing.as_deref(), expected_hash)?;
                let before = existing.unwrap_or_default();
                self.write_atomic(&abs, path, content.as_bytes())?;
                let diff = TextDiff::from_lines(before.as_str(), content.as_str());
                Ok(Patch {
                    path: p.clone(),
                    unified_diff: unified_diff_text(&diff, p),
                    hash_before: Some(hash_bytes(before.as_bytes())),
                    hash_after: Some(hash_bytes(content.as_bytes())),
                    summary: diff_summary(&diff, false, false),
                })
            }
            Edit::Delete {
                path: p,
                expected_hash,
            } => {
                check_expectation(p, existing.as_deref(), expected_hash)?;
                let before = existing.ok_or_else(|| PatchError::Conflict {
                    path: p.clone(),
                    detail: NOTHING_TO_EDIT.to_string(),
                })?;
                self.remove_file(&abs, path)?;
                let diff = TextDiff::from_lines(before.as_str(), "");
                Ok(Patch {
                    path: p.clone(),
                    unified_diff: unified_diff_text(&diff, p),
                    hash_before: Some(hash_bytes(before.as_bytes())),
                    hash_after: None,
                    summary: diff_summary(&diff, false, true),
                })
            }
            Edit::RangeReplace {
                path: p,
                byte_start,
                byte_end,
                replacement,
                old_text,
                expected_hash,
            } => {
                check_expectation(p, existing.as_deref(), expected_hash)?;
                let before = existing.ok_or_else(|| PatchError::Conflict {
                    path: p.clone(),
                    detail: NOTHING_TO_EDIT.to_string(),
                })?;
                if *byte_start > *byte_end
                    || *byte_end > before.len()
                    || !before.is_char_boundary(*byte_start)
                    || !before.is_char_boundary(*byte_end)
                {
                    return Err(PatchError::InvalidRange {
                        path: p.clone(),
                        byte_start: *byte_start,
                        byte_end: *byte_end,
                    });
                }
                if let Some(expected) = old_text {
                    let actual = &before[*byte_start..*byte_end];
                    if actual != expected {
                        return Err(PatchError::Conflict {
                            path: p.clone(),
                            detail: format!(
                                "old_text does not match at that range: expected {expected:?} \
                                 but found {actual:?} at bytes [{byte_start}, {byte_end}) of \
                                 {p}. The file likely changed, or the offsets were computed \
                                 wrong. Re-read the file with fs.read and either recompute the \
                                 byte range or switch to edit.apply_patch's old_text/new_text \
                                 form, which needs no offsets at all."
                            ),
                        });
                    }
                }
                let mut after = String::with_capacity(
                    before.len() - (byte_end - byte_start) + replacement.len(),
                );
                after.push_str(&before[..*byte_start]);
                after.push_str(replacement);
                after.push_str(&before[*byte_end..]);
                self.write_atomic(&abs, path, after.as_bytes())?;
                let diff = TextDiff::from_lines(before.as_str(), after.as_str());
                Ok(Patch {
                    path: p.clone(),
                    unified_diff: unified_diff_text(&diff, p),
                    hash_before: Some(hash_bytes(before.as_bytes())),
                    hash_after: Some(hash_bytes(after.as_bytes())),
                    summary: diff_summary(&diff, false, false),
                })
            }
            Edit::TextReplace {
                path: p,
                old_text,
                new_text,
                expected_hash,
            } => {
                check_expectation(p, existing.as_deref(), expected_hash)?;
                let before = existing.ok_or_else(|| PatchError::Conflict {
                    path: p.clone(),
                    detail: NOTHING_TO_EDIT.to_string(),
                })?;
                let match_count = before.matches(old_text.as_str()).count();
                if match_count == 0 {
                    return Err(PatchError::Conflict {
                        path: p.clone(),
                        detail: format!(
                            "old_text was not found in {p}. Re-read the file with fs.read and \
                             copy the exact text to replace, including whitespace."
                        ),
                    });
                }
                if match_count > 1 {
                    return Err(PatchError::Conflict {
                        path: p.clone(),
                        detail: format!(
                            "old_text matches {match_count} times in {p}, but must match exactly \
                             once. Include more surrounding context in old_text to make it unique."
                        ),
                    });
                }
                let after = before.replacen(old_text.as_str(), new_text.as_str(), 1);
                self.write_atomic(&abs, path, after.as_bytes())?;
                let diff = TextDiff::from_lines(before.as_str(), after.as_str());
                Ok(Patch {
                    path: p.clone(),
                    unified_diff: unified_diff_text(&diff, p),
                    hash_before: Some(hash_bytes(before.as_bytes())),
                    hash_after: Some(hash_bytes(after.as_bytes())),
                    summary: diff_summary(&diff, false, false),
                })
            }
        }
    }

    /// Whether `path` (relative to `self.root`) falls within the engine's write scope, without
    /// attempting any edit. Exposed so [`crate::tools::ToolRegistry`] can pre-flight a
    /// `fs.*`/`edit.*` tool call's `Action::WritePath` before even constructing an [`Edit`].
    pub fn in_write_scope(&self, path: &str) -> Result<bool> {
        self.resolve(path)
            .map(|_| self.authority.repository.write.matches(path))
            .map_err(|e| TmError::Parse(e.to_string()))
    }

    /// Resolve `path` (relative to `self.root`) to an absolute path, rejecting anything that
    /// escapes the root or isn't representable as UTF-8 once joined.
    fn resolve(&self, path: &str) -> std::result::Result<PathBuf, PatchError> {
        let candidate = Path::new(path);
        if candidate.is_absolute()
            || candidate
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(PatchError::InvalidPath(format!(
                "{path} (project root '{}'; paths must be relative to the project root, with no \
                 `..` or absolute component)",
                self.root.display()
            )));
        }
        let joined = self.root.join(candidate);
        if joined.to_str().is_none() {
            return Err(PatchError::InvalidPath(path.to_string()));
        }
        Ok(joined)
    }

    /// Read the current content at `abs`, returning `None` when no file exists there yet.
    fn read_existing(
        &self,
        abs: &Path,
        path: &str,
    ) -> std::result::Result<Option<String>, PatchError> {
        match std::fs::read(abs) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| PatchError::Io {
                    path: path.to_string(),
                    detail: "file is not valid UTF-8".to_string(),
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(PatchError::Io {
                path: path.to_string(),
                detail: format!(
                    "{e} (project root '{}'; paths are relative to the project root)",
                    self.root.display()
                ),
            }),
        }
    }

    /// Write `bytes` to `abs` via a temp file in the same directory followed by a rename, so a
    /// crash mid-write never leaves a partial file behind.
    fn write_atomic(
        &self,
        abs: &Path,
        path: &str,
        bytes: &[u8],
    ) -> std::result::Result<(), PatchError> {
        let dir = abs.parent().ok_or_else(|| PatchError::Io {
            path: path.to_string(),
            detail: "target has no parent directory".to_string(),
        })?;
        std::fs::create_dir_all(dir).map_err(|e| PatchError::Io {
            path: path.to_string(),
            detail: e.to_string(),
        })?;
        let file_name = abs
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("patch-target");
        let tmp = dir.join(format!(".{file_name}.tmp-patch"));
        std::fs::write(&tmp, bytes).map_err(|e| PatchError::Io {
            path: path.to_string(),
            detail: e.to_string(),
        })?;
        std::fs::rename(&tmp, abs).map_err(|e| PatchError::Io {
            path: path.to_string(),
            detail: e.to_string(),
        })
    }

    /// Remove the file at `abs`.
    fn remove_file(&self, abs: &Path, path: &str) -> std::result::Result<(), PatchError> {
        std::fs::remove_file(abs).map_err(|e| PatchError::Io {
            path: path.to_string(),
            detail: e.to_string(),
        })
    }
}

/// Conflict detail for creating over a file that already exists. Every conflict detail below
/// says how to recover, not just what went wrong: the reader is a model, and a bare "hash
/// mismatch" once sent one hashing the file with sha256, md5 and sha1 for ~175k tokens trying to
/// guess the format, when the fix was one `fs.read` away.
const CREATE_OVER_EXISTING: &str = "file already exists. To change it, read it with fs.read and \
use edit.write_file or edit.apply_patch with the `hash` fs.read returns as expected_hash";

/// Conflict detail for deleting or range-editing a path with no file at it.
const NOTHING_TO_EDIT: &str = "file does not exist. Check the path with fs.list";

/// Check an edit's `expected_hash`/existence claim against what's actually on disk right now.
/// `None` means "I never observed this path", which is only consistent with the path not
/// currently existing; `Some(hash)` must match the current content's hash exactly.
fn check_expectation(
    path: &str,
    existing: Option<&str>,
    expected_hash: &Option<String>,
) -> std::result::Result<(), PatchError> {
    match (expected_hash, existing) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(PatchError::Conflict {
            path: path.to_string(),
            detail: "file already exists but no expected_hash was supplied. Read it with fs.read \
                     (or fs.stat) and pass the `hash` it returns as expected_hash"
                .to_string(),
        }),
        (Some(_), None) => Err(PatchError::Conflict {
            path: path.to_string(),
            detail: "file no longer exists, though an expected_hash was supplied. Check the path \
                     (fs.list); to create the file, use edit.create_file, or edit.write_file \
                     without expected_hash"
                .to_string(),
        }),
        (Some(expected), Some(content)) => {
            if &hash_bytes(content.as_bytes()) == expected {
                Ok(())
            } else {
                // The current hash is deliberately left out: handed it, a model can retry with
                // it without re-reading, and its stale byte offsets would then land on whatever
                // the file holds now. Re-reading is the only way to get it.
                Err(PatchError::Conflict {
                    path: path.to_string(),
                    detail: format!(
                        "expected_hash {expected} does not match the file's current content. The \
                         file changed since it was read, or the hash did not come from \
                         fs.read/fs.stat (it is tm's own content hash; never compute it \
                         yourself). Re-read the file with fs.read and retry with the `hash` it \
                         returns, working out any byte offsets against the fresh content"
                    ),
                })
            }
        }
    }
}

/// Tally a `similar` diff's changes into a [`DiffSummary`].
fn diff_summary<'a>(diff: &TextDiff<'a, 'a, 'a, str>, created: bool, deleted: bool) -> DiffSummary {
    let mut lines_added = 0;
    let mut lines_removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => lines_added += 1,
            ChangeTag::Delete => lines_removed += 1,
            ChangeTag::Equal => {}
        }
    }
    DiffSummary {
        lines_added,
        lines_removed,
        created,
        deleted,
    }
}

/// Render a `similar` diff as unified-diff text headed with `a/<path>` / `b/<path>`.
fn unified_diff_text<'a>(diff: &'a TextDiff<'a, 'a, 'a, str>, path: &str) -> String {
    diff.unified_diff()
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::{PatternSet, RepoAuthority};

    fn engine_with_scope(root: &Path, patterns: &[&str]) -> PatchEngine {
        let authority = Authority {
            repository: RepoAuthority {
                read: PatternSet::all(),
                write: PatternSet::parse(patterns.iter().map(|s| s.to_string())).unwrap(),
            },
            ..Authority::default()
        };
        PatchEngine::new(root.to_path_buf(), authority)
    }

    fn full_access_engine(root: &Path) -> PatchEngine {
        engine_with_scope(root, &["**"])
    }

    #[test]
    fn create_writes_a_new_file_and_reports_added_lines() {
        let dir = tempfile::tempdir().unwrap();
        let engine = full_access_engine(dir.path());
        let patch = engine
            .apply(&Edit::Create {
                path: "hello.txt".to_string(),
                content: "hi\nthere\n".to_string(),
            })
            .unwrap();
        assert_eq!(patch.hash_before, None);
        assert!(patch.summary.created);
        assert_eq!(patch.summary.lines_added, 2);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "hi\nthere\n"
        );
    }

    #[test]
    fn create_conflicts_when_the_file_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "existing").unwrap();
        let engine = full_access_engine(dir.path());
        let err = engine
            .apply(&Edit::Create {
                path: "hello.txt".to_string(),
                content: "new".to_string(),
            })
            .unwrap_err();
        assert!(matches!(err, PatchError::Conflict { .. }));
    }

    #[test]
    fn write_with_matching_hash_replaces_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "old\n").unwrap();
        let engine = full_access_engine(dir.path());
        let hash = hash_bytes(b"old\n");
        let patch = engine
            .apply(&Edit::Write {
                path: "f.txt".to_string(),
                content: "new\n".to_string(),
                expected_hash: Some(hash),
            })
            .unwrap();
        assert_eq!(patch.hash_after, Some(hash_bytes(b"new\n")));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn write_with_stale_hash_is_a_conflict_and_does_not_touch_disk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "current\n").unwrap();
        let engine = full_access_engine(dir.path());
        let err = engine
            .apply(&Edit::Write {
                path: "f.txt".to_string(),
                content: "new\n".to_string(),
                expected_hash: Some(hash_bytes(b"stale\n")),
            })
            .unwrap_err();
        assert!(matches!(err, PatchError::Conflict { .. }));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "current\n"
        );
    }

    #[test]
    fn write_with_no_hash_conflicts_if_the_file_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "current\n").unwrap();
        let engine = full_access_engine(dir.path());
        let err = engine
            .apply(&Edit::Write {
                path: "f.txt".to_string(),
                content: "new\n".to_string(),
                expected_hash: None,
            })
            .unwrap_err();
        assert!(matches!(err, PatchError::Conflict { .. }));
    }

    #[test]
    fn write_with_no_hash_creates_when_the_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let engine = full_access_engine(dir.path());
        let patch = engine
            .apply(&Edit::Write {
                path: "fresh.txt".to_string(),
                content: "hi\n".to_string(),
                expected_hash: None,
            })
            .unwrap();
        assert_eq!(patch.hash_before, Some(hash_bytes(b"")));
    }

    #[test]
    fn delete_removes_the_file_when_hash_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();
        let engine = full_access_engine(dir.path());
        let patch = engine
            .apply(&Edit::Delete {
                path: "gone.txt".to_string(),
                expected_hash: Some(hash_bytes(b"bye\n")),
            })
            .unwrap();
        assert!(patch.summary.deleted);
        assert_eq!(patch.hash_after, None);
        assert!(!dir.path().join("gone.txt").exists());
    }

    #[test]
    fn delete_of_a_missing_file_is_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let engine = full_access_engine(dir.path());
        let err = engine
            .apply(&Edit::Delete {
                path: "missing.txt".to_string(),
                expected_hash: None,
            })
            .unwrap_err();
        assert!(matches!(err, PatchError::Conflict { .. }));
    }

    #[test]
    fn range_replace_splices_into_the_middle_of_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("r.txt"), "abcdef").unwrap();
        let engine = full_access_engine(dir.path());
        let patch = engine
            .apply(&Edit::RangeReplace {
                path: "r.txt".to_string(),
                byte_start: 2,
                byte_end: 4,
                replacement: "XY".to_string(),
                old_text: None,
                expected_hash: Some(hash_bytes(b"abcdef")),
            })
            .unwrap();
        assert_eq!(patch.hash_after, Some(hash_bytes(b"abXYef")));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("r.txt")).unwrap(),
            "abXYef"
        );
    }

    /// Replays the T-2 dogfood shape: a byte range computed off by several bytes, with
    /// `old_text` supplied. Before this, the engine trusted the offsets blindly and spliced at
    /// the wrong place, corrupting an unrelated doc comment. Now it must refuse with a clear
    /// "old_text does not match" conflict instead of touching the file.
    #[test]
    fn range_replace_with_a_stale_offset_and_old_text_is_a_clear_conflict_not_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let content =
            "// a smell, not a regardless -- a 1x1 workflow is a smell, not a\nfn f() {}\n";
        std::fs::write(dir.path().join("doc.rs"), content).unwrap();
        let engine = full_access_engine(dir.path());
        // Off by several bytes from where "smell" actually is.
        let err = engine
            .apply(&Edit::RangeReplace {
                path: "doc.rs".to_string(),
                byte_start: 3,
                byte_end: 8,
                replacement: "thing".to_string(),
                old_text: Some("smell".to_string()),
                expected_hash: Some(hash_bytes(content.as_bytes())),
            })
            .unwrap_err();
        let PatchError::Conflict { detail, .. } = &err else {
            panic!("expected a conflict, got {err:?}");
        };
        assert!(
            detail.contains("old_text does not match at that range"),
            "{detail}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("doc.rs")).unwrap(),
            content,
            "the file must be untouched on a stale-offset conflict"
        );
    }

    #[test]
    fn range_replace_with_matching_old_text_applies_normally() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("r.txt"), "abcdef").unwrap();
        let engine = full_access_engine(dir.path());
        let patch = engine
            .apply(&Edit::RangeReplace {
                path: "r.txt".to_string(),
                byte_start: 2,
                byte_end: 4,
                replacement: "XY".to_string(),
                old_text: Some("cd".to_string()),
                expected_hash: Some(hash_bytes(b"abcdef")),
            })
            .unwrap();
        assert_eq!(patch.hash_after, Some(hash_bytes(b"abXYef")));
    }

    #[test]
    fn text_replace_with_a_unique_match_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let content = "def sub(a, b):\n    return a - b\n";
        std::fs::write(dir.path().join("calc.py"), content).unwrap();
        let engine = full_access_engine(dir.path());
        let patch = engine
            .apply(&Edit::TextReplace {
                path: "calc.py".to_string(),
                old_text: "a - b".to_string(),
                new_text: "b - a".to_string(),
                expected_hash: Some(hash_bytes(content.as_bytes())),
            })
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("calc.py")).unwrap(),
            "def sub(a, b):\n    return b - a\n"
        );
        assert_eq!(
            patch.hash_after,
            Some(hash_bytes(b"def sub(a, b):\n    return b - a\n"))
        );
    }

    #[test]
    fn text_replace_with_an_ambiguous_match_fails_with_the_match_count() {
        let dir = tempfile::tempdir().unwrap();
        let content = "foo\nfoo\n";
        std::fs::write(dir.path().join("dup.txt"), content).unwrap();
        let engine = full_access_engine(dir.path());
        let err = engine
            .apply(&Edit::TextReplace {
                path: "dup.txt".to_string(),
                old_text: "foo".to_string(),
                new_text: "bar".to_string(),
                expected_hash: Some(hash_bytes(content.as_bytes())),
            })
            .unwrap_err();
        let PatchError::Conflict { detail, .. } = &err else {
            panic!("expected a conflict, got {err:?}");
        };
        assert!(detail.contains("matches 2 times"), "{detail}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dup.txt")).unwrap(),
            content
        );
    }

    #[test]
    fn text_replace_with_no_match_fails_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let content = "hello\n";
        std::fs::write(dir.path().join("h.txt"), content).unwrap();
        let engine = full_access_engine(dir.path());
        let err = engine
            .apply(&Edit::TextReplace {
                path: "h.txt".to_string(),
                old_text: "goodbye".to_string(),
                new_text: "hi".to_string(),
                expected_hash: Some(hash_bytes(content.as_bytes())),
            })
            .unwrap_err();
        let PatchError::Conflict { detail, .. } = &err else {
            panic!("expected a conflict, got {err:?}");
        };
        assert!(detail.contains("was not found"), "{detail}");
    }

    #[test]
    fn range_replace_rejects_an_out_of_bounds_range() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("r.txt"), "abc").unwrap();
        let engine = full_access_engine(dir.path());
        let err = engine
            .apply(&Edit::RangeReplace {
                path: "r.txt".to_string(),
                byte_start: 1,
                byte_end: 10,
                replacement: "z".to_string(),
                old_text: None,
                expected_hash: Some(hash_bytes(b"abc")),
            })
            .unwrap_err();
        assert!(matches!(err, PatchError::InvalidRange { .. }));
    }

    #[test]
    fn range_replace_rejects_a_start_after_the_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("r.txt"), "abc").unwrap();
        let engine = full_access_engine(dir.path());
        let err = engine
            .apply(&Edit::RangeReplace {
                path: "r.txt".to_string(),
                byte_start: 2,
                byte_end: 1,
                replacement: "z".to_string(),
                old_text: None,
                expected_hash: Some(hash_bytes(b"abc")),
            })
            .unwrap_err();
        assert!(matches!(err, PatchError::InvalidRange { .. }));
    }

    #[test]
    fn apply_outside_the_write_scope_is_refused_before_any_io() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_with_scope(dir.path(), &["src/**"]);
        let err = engine
            .apply(&Edit::Create {
                path: "secrets.txt".to_string(),
                content: "nope".to_string(),
            })
            .unwrap_err();
        assert!(matches!(err, PatchError::OutOfScope { .. }));
        assert!(!dir.path().join("secrets.txt").exists());
    }

    #[test]
    fn apply_rejects_a_path_that_escapes_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let engine = full_access_engine(dir.path());
        let err = engine
            .apply(&Edit::Create {
                path: "../escape.txt".to_string(),
                content: "nope".to_string(),
            })
            .unwrap_err();
        assert!(matches!(err, PatchError::InvalidPath(_)));
        // The resolved project root is named in the message, not just the bare path — the
        // recovery hint an agent (or a human reading `tm run`'s output) needs to tell a
        // wrong-root mistake apart from a genuinely missing file.
        let root_display = engine.root().to_string_lossy().into_owned();
        assert!(
            err.to_string().contains(&root_display),
            "expected the project root in the error, got: {err}"
        );
    }

    #[test]
    fn apply_rejects_an_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let engine = full_access_engine(dir.path());
        let err = engine
            .apply(&Edit::Create {
                path: "/etc/passwd".to_string(),
                content: "nope".to_string(),
            })
            .unwrap_err();
        assert!(matches!(err, PatchError::InvalidPath(_)));
    }

    #[test]
    fn in_write_scope_reports_true_only_within_the_authority_scope() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_with_scope(dir.path(), &["src/**"]);
        assert!(engine.in_write_scope("src/lib.rs").unwrap());
        assert!(!engine.in_write_scope("docs/readme.md").unwrap());
    }

    #[test]
    fn in_write_scope_errors_on_an_escaping_path() {
        let dir = tempfile::tempdir().unwrap();
        let engine = full_access_engine(dir.path());
        assert!(engine.in_write_scope("../escape.txt").is_err());
    }

    #[test]
    fn unified_diff_names_the_touched_path() {
        let dir = tempfile::tempdir().unwrap();
        let engine = full_access_engine(dir.path());
        let patch = engine
            .apply(&Edit::Create {
                path: "named.txt".to_string(),
                content: "one\n".to_string(),
            })
            .unwrap();
        assert!(patch.unified_diff.contains("named.txt"));
        assert!(patch.unified_diff.contains("+one"));
    }
}
