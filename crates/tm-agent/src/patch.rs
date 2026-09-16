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

use std::path::{Path, PathBuf};

use tm_types::{Authority, Result};

/// One requested edit. Every variant that touches existing content carries an
/// `expected_hash`: the blake3 hex hash of the content the caller last read, used to detect a
/// conflicting change made since.
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
        /// has never read (fresh-write intent, still checked against "must already exist").
        expected_hash: Option<String>,
    },
    /// Delete a file.
    Delete {
        /// Path relative to the project root.
        path: String,
        /// Hash of the content last observed at this path.
        expected_hash: Option<String>,
    },
    /// Replace a byte range within a file, leaving the rest untouched.
    RangeReplace {
        /// Path relative to the project root.
        path: String,
        /// Start byte offset, inclusive.
        byte_start: usize,
        /// End byte offset, exclusive.
        byte_end: usize,
        /// Replacement text for the range.
        replacement: String,
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
            | Edit::RangeReplace { path, .. } => path,
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
        /// Human-readable detail (e.g. "expected hash abc123, found def456", or "file no
        /// longer exists").
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
    // IMPL: (1) validate + resolve the path; (2) authority scope check first, before any I/O,
    // so a denied write never touches disk; (3) read current bytes (empty/absent as "no file"
    // for Create); (4) hash current content with `tm_core::artifact::hash_bytes` and compare to
    // `expected_hash`/existence, short-circuiting to `Conflict` on mismatch — the "never a
    // blind overwrite" invariant lives entirely in this comparison; (5) compute the new content
    // per variant (`RangeReplace` splices `replacement` into `[byte_start, byte_end)`, checked
    // against current content length first); (6) write atomically (write to a temp file in the
    // same directory, then rename, so a crash mid-write never leaves a partial file); (7) build
    // the unified diff via `similar::TextDiff::from_lines(...).unified_diff()` and a
    // `DiffSummary` from the same diff's change tally; (8) return the assembled `Patch`.
    pub fn apply(&self, edit: &Edit) -> PatchOutcome {
        todo!("resolve+scope-check the path, detect conflicts against expected_hash, write atomically, and build a similar-based unified diff and DiffSummary")
    }

    /// Whether `path` (relative to `self.root`) falls within the engine's write scope, without
    /// attempting any edit. Exposed so [`crate::tools::ToolRegistry`] can pre-flight a
    /// `fs.*`/`edit.*` tool call's `Action::WritePath` before even constructing an [`Edit`].
    // IMPL: join `path` to `self.root`, reject `..`/absolute components, then delegate to
    // `self.authority.repository.write.matches(path)`.
    pub fn in_write_scope(&self, path: &str) -> Result<bool> {
        todo!("resolve path safely and check tm_types::PatternSet::matches against the write scope")
    }
}
