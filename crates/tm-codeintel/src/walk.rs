//! Owns repository walking: enumerating files a project wants indexed, detecting their
//! language, skipping binaries and oversized files, and deciding which files changed since
//! the last index run.
//!
//! Walking respects `.gitignore` and an additional project-local `.tmignore`, both handled by
//! the `ignore` crate's standard override chain. This module produces [`FileRecord`]s only —
//! it never opens `index.db` itself; [`store`](crate::store) owns persistence.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use tm_types::Result;

/// A file's classification and change fingerprint as seen by the walker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    /// Path relative to the project root, using `/` separators regardless of platform.
    pub path: String,
    /// BLAKE3 hash of the file's raw bytes.
    pub blake3: String,
    /// Size in bytes.
    pub size: u64,
    /// Detected language, or `None` for unrecognized/binary content.
    pub lang: Option<Language>,
    /// Modification time as reported by the filesystem, in Unix seconds.
    ///
    /// This is metadata read from `fs::metadata`, not wall-clock time drawn by this crate, so
    /// it does not violate the injected-`Clock` rule; it is never used to decide re-indexing
    /// (blake3 does that), only surfaced for display and staleness heuristics upstream.
    pub mtime: i64,
}

/// Languages this crate can chunk syntax-aware and extract symbols from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    /// Rust source (`.rs`).
    Rust,
    /// Go source (`.go`).
    Go,
    /// TypeScript source (`.ts`).
    TypeScript,
    /// TypeScript with JSX (`.tsx`).
    Tsx,
    /// JavaScript source (`.js`, `.jsx`, `.mjs`, `.cjs`).
    JavaScript,
    /// Python source (`.py`).
    Python,
    /// Any other recognized-but-unparsed text extension (markdown, toml, json, ...).
    Other,
}

impl Language {
    /// Classify by file extension (case-insensitive). Returns `None` when the extension is
    /// absent or unrecognized, in which case the caller should treat the file as opaque text
    /// (still indexable for exact/semantic search) or, if it looks binary, skip it entirely.
    pub fn from_extension(ext: &str) -> Option<Language> {
        match ext.to_lowercase().as_str() {
            "rs" => Some(Language::Rust),
            "go" => Some(Language::Go),
            "ts" => Some(Language::TypeScript),
            "tsx" => Some(Language::Tsx),
            "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
            "py" => Some(Language::Python),
            "md" | "toml" | "json" | "yaml" | "yml" | "txt" | "proto" | "sql" | "sh" | "css"
            | "html" => Some(Language::Other),
            _ => None,
        }
    }

    /// Whether this language has a tree-sitter grammar wired up in [`symbols`](crate::symbols)
    /// and [`chunk`](crate::chunk) for syntax-aware splitting.
    pub fn has_grammar(self) -> bool {
        !matches!(self, Language::Other)
    }
}

/// Ceiling on file size considered for indexing. Files larger than this are recorded (so
/// they are not re-scanned every run) but their content is never chunked or embedded.
pub const MAX_INDEXABLE_BYTES: u64 = 2 * 1024 * 1024;

/// Number of leading bytes sniffed to decide whether a file is binary.
pub const BINARY_SNIFF_BYTES: usize = 8192;

/// Walks a project's working tree, honoring `.gitignore` and `.tmignore`.
pub struct RepoWalker {
    root: PathBuf,
}

impl RepoWalker {
    /// Build a walker rooted at `root`, the project's **workspace** root -- the directory
    /// actually walked/indexed, which in global scope (D-003) has no `.tm/` in it at all; the
    /// index this walk feeds lives under the project's `state_dir`, a separate path (see
    /// [`crate::api::CodeIntel::open_at`]), not necessarily anywhere under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        RepoWalker { root: root.into() }
    }

    /// The project root this walker scans.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Walk the tree and produce a [`FileRecord`] for every non-ignored, non-binary file,
    /// including those over [`MAX_INDEXABLE_BYTES`] (flagged via `lang: None` when content is
    /// unreadable, but still counted).
    ///
    /// Returns records in a stable order (lexicographic by `path`) so callers can diff two
    /// walks without an intermediate sort.
    pub fn walk(&self) -> Result<Vec<FileRecord>> {
        let mut builder = WalkBuilder::new(&self.root);
        builder
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
            .standard_filters(true)
            // Honor `.gitignore` even when `root` isn't inside an actual `.git` repository
            // (e.g. a project scaffold that hasn't run `git init` yet, or a test fixture).
            .require_git(false);
        // `.tmignore` is optional; a missing file is not an error, so any `Some(err)` here is
        // only surfaced as a warning rather than aborting the walk.
        if let Some(err) = builder.add_ignore(self.root.join(".tmignore")) {
            tracing::warn!("failed to load .tmignore: {}", err);
        }
        let walker = builder.build();

        let mut records = Vec::new();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    if err
                        .io_error()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
                    {
                        tracing::warn!("permission denied while walking: {}", err);
                        continue;
                    }
                    let message = err.to_string();
                    return Err(err
                        .into_io_error()
                        .unwrap_or_else(|| std::io::Error::other(message))
                        .into());
                }
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            let path = entry.path();
            let relative_path = match path.strip_prefix(&self.root) {
                Ok(p) => p,
                Err(_) => continue,
            };

            // Sniff for binary content by reading up to BINARY_SNIFF_BYTES.
            let mut sniff_buffer = vec![0u8; BINARY_SNIFF_BYTES];
            let sniff_bytes = match fs::File::open(path) {
                Ok(mut file) => {
                    use std::io::Read;
                    match file.read(&mut sniff_buffer) {
                        Ok(n) => {
                            sniff_buffer.truncate(n);
                            n
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                            tracing::warn!("permission denied reading {}: {}", path.display(), e);
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    tracing::warn!("permission denied opening {}: {}", path.display(), e);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };

            // Skip if binary (contains NUL byte).
            if sniff_buffer[..sniff_bytes].contains(&0u8) {
                continue;
            }

            // Read full content and hash.
            let contents = match fs::read(path) {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    tracing::warn!(
                        "permission denied reading full file {}: {}",
                        path.display(),
                        e
                    );
                    continue;
                }
                Err(e) => return Err(e.into()),
            };

            let blake3_hash = blake3::hash(&contents).to_hex().to_string();
            let size = contents.len() as u64;

            // Get metadata for mtime.
            let metadata = match fs::metadata(path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    tracing::warn!("permission denied stat {}: {}", path.display(), e);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };

            let mtime = metadata
                .modified()
                .ok()
                .and_then(|st| st.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            // Classify language by extension.
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let lang = Language::from_extension(ext);

            // Convert path to forward-slash relative format.
            let relative_str = relative_path.to_string_lossy().replace('\\', "/");

            records.push(FileRecord {
                path: relative_str,
                blake3: blake3_hash,
                size,
                lang,
                mtime,
            });
        }

        // Sort by path for stable ordering.
        records.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(records)
    }

    /// Compare a fresh walk against previously stored records (keyed by path) and partition
    /// into added, modified (blake3 differs) and removed paths. `previous` need not be sorted.
    ///
    /// Pure function of its inputs — no I/O — so it is unit-testable without touching disk.
    pub fn diff(current: &[FileRecord], previous: &[FileRecord]) -> ChangeSet {
        // Build a map from previous records for O(1) lookup by path.
        let mut previous_map: BTreeMap<&str, &str> = BTreeMap::new();
        for record in previous {
            previous_map.insert(record.path.as_str(), record.blake3.as_str());
        }

        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut seen_paths = std::collections::HashSet::new();

        // Classify current records.
        for record in current {
            seen_paths.insert(record.path.as_str());
            match previous_map.get(record.path.as_str()) {
                None => {
                    // Path not in previous -> added
                    added.push(record.clone());
                }
                Some(&prev_hash) => {
                    if prev_hash != record.blake3.as_str() {
                        // Hash differs -> modified
                        modified.push(record.clone());
                    }
                    // Otherwise unchanged, skip
                }
            }
        }

        // Find removed paths: in previous_map but not in current.
        let mut removed = Vec::new();
        for (path, _hash) in previous_map.iter() {
            if !seen_paths.contains(path) {
                removed.push(path.to_string());
            }
        }

        ChangeSet {
            added,
            modified,
            removed,
        }
    }
}

/// The result of comparing two walks: what an incremental re-index needs to touch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    /// Paths present now but not before.
    pub added: Vec<FileRecord>,
    /// Paths present in both but whose blake3 changed.
    pub modified: Vec<FileRecord>,
    /// Paths present before but not now.
    pub removed: Vec<String>,
}

impl ChangeSet {
    /// Whether nothing changed between the two walks.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_from_extension_rust() {
        assert_eq!(Language::from_extension("rs"), Some(Language::Rust));
        assert_eq!(Language::from_extension("RS"), Some(Language::Rust));
        assert_eq!(Language::from_extension("Rs"), Some(Language::Rust));
    }

    #[test]
    fn language_from_extension_go() {
        assert_eq!(Language::from_extension("go"), Some(Language::Go));
        assert_eq!(Language::from_extension("GO"), Some(Language::Go));
    }

    #[test]
    fn language_from_extension_typescript() {
        assert_eq!(Language::from_extension("ts"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("TS"), Some(Language::TypeScript));
    }

    #[test]
    fn language_from_extension_tsx() {
        assert_eq!(Language::from_extension("tsx"), Some(Language::Tsx));
        assert_eq!(Language::from_extension("TSX"), Some(Language::Tsx));
    }

    #[test]
    fn language_from_extension_javascript() {
        assert_eq!(Language::from_extension("js"), Some(Language::JavaScript));
        assert_eq!(Language::from_extension("jsx"), Some(Language::JavaScript));
        assert_eq!(Language::from_extension("mjs"), Some(Language::JavaScript));
        assert_eq!(Language::from_extension("cjs"), Some(Language::JavaScript));
        assert_eq!(Language::from_extension("JS"), Some(Language::JavaScript));
    }

    #[test]
    fn language_from_extension_python() {
        assert_eq!(Language::from_extension("py"), Some(Language::Python));
        assert_eq!(Language::from_extension("PY"), Some(Language::Python));
    }

    #[test]
    fn language_from_extension_other() {
        assert_eq!(Language::from_extension("md"), Some(Language::Other));
        assert_eq!(Language::from_extension("toml"), Some(Language::Other));
        assert_eq!(Language::from_extension("json"), Some(Language::Other));
        assert_eq!(Language::from_extension("yaml"), Some(Language::Other));
        assert_eq!(Language::from_extension("yml"), Some(Language::Other));
        assert_eq!(Language::from_extension("txt"), Some(Language::Other));
        assert_eq!(Language::from_extension("proto"), Some(Language::Other));
        assert_eq!(Language::from_extension("sql"), Some(Language::Other));
        assert_eq!(Language::from_extension("sh"), Some(Language::Other));
        assert_eq!(Language::from_extension("css"), Some(Language::Other));
        assert_eq!(Language::from_extension("html"), Some(Language::Other));
    }

    #[test]
    fn language_from_extension_unknown() {
        assert_eq!(Language::from_extension("unknown"), None);
        assert_eq!(Language::from_extension("xyz"), None);
        assert_eq!(Language::from_extension(""), None);
        assert_eq!(Language::from_extension("bin"), None);
    }

    #[test]
    fn language_has_grammar() {
        assert!(Language::Rust.has_grammar());
        assert!(Language::Go.has_grammar());
        assert!(Language::TypeScript.has_grammar());
        assert!(Language::Tsx.has_grammar());
        assert!(Language::JavaScript.has_grammar());
        assert!(Language::Python.has_grammar());
        assert!(!Language::Other.has_grammar());
    }

    #[test]
    fn diff_empty_inputs() {
        let changes = RepoWalker::diff(&[], &[]);
        assert!(changes.is_empty());
        assert_eq!(changes.added.len(), 0);
        assert_eq!(changes.modified.len(), 0);
        assert_eq!(changes.removed.len(), 0);
    }

    #[test]
    fn diff_no_changes() {
        let records = vec![
            FileRecord {
                path: "file1.rs".to_string(),
                blake3: "hash1".to_string(),
                size: 100,
                lang: Some(Language::Rust),
                mtime: 1000,
            },
            FileRecord {
                path: "file2.rs".to_string(),
                blake3: "hash2".to_string(),
                size: 200,
                lang: Some(Language::Rust),
                mtime: 2000,
            },
        ];
        let changes = RepoWalker::diff(&records, &records);
        assert!(changes.is_empty());
    }

    #[test]
    fn diff_added_files() {
        let previous = vec![FileRecord {
            path: "file1.rs".to_string(),
            blake3: "hash1".to_string(),
            size: 100,
            lang: Some(Language::Rust),
            mtime: 1000,
        }];

        let current = vec![
            FileRecord {
                path: "file1.rs".to_string(),
                blake3: "hash1".to_string(),
                size: 100,
                lang: Some(Language::Rust),
                mtime: 1000,
            },
            FileRecord {
                path: "file2.rs".to_string(),
                blake3: "hash2".to_string(),
                size: 200,
                lang: Some(Language::Rust),
                mtime: 2000,
            },
        ];

        let changes = RepoWalker::diff(&current, &previous);
        assert_eq!(changes.added.len(), 1);
        assert_eq!(changes.added[0].path, "file2.rs");
        assert!(changes.modified.is_empty());
        assert!(changes.removed.is_empty());
    }

    #[test]
    fn diff_modified_files() {
        let previous = vec![FileRecord {
            path: "file1.rs".to_string(),
            blake3: "hash1_old".to_string(),
            size: 100,
            lang: Some(Language::Rust),
            mtime: 1000,
        }];

        let current = vec![FileRecord {
            path: "file1.rs".to_string(),
            blake3: "hash1_new".to_string(),
            size: 150,
            lang: Some(Language::Rust),
            mtime: 2000,
        }];

        let changes = RepoWalker::diff(&current, &previous);
        assert!(changes.added.is_empty());
        assert_eq!(changes.modified.len(), 1);
        assert_eq!(changes.modified[0].path, "file1.rs");
        assert_eq!(changes.modified[0].blake3, "hash1_new");
        assert!(changes.removed.is_empty());
    }

    #[test]
    fn diff_removed_files() {
        let previous = vec![
            FileRecord {
                path: "file1.rs".to_string(),
                blake3: "hash1".to_string(),
                size: 100,
                lang: Some(Language::Rust),
                mtime: 1000,
            },
            FileRecord {
                path: "file2.rs".to_string(),
                blake3: "hash2".to_string(),
                size: 200,
                lang: Some(Language::Rust),
                mtime: 2000,
            },
        ];

        let current = vec![FileRecord {
            path: "file1.rs".to_string(),
            blake3: "hash1".to_string(),
            size: 100,
            lang: Some(Language::Rust),
            mtime: 1000,
        }];

        let changes = RepoWalker::diff(&current, &previous);
        assert!(changes.added.is_empty());
        assert!(changes.modified.is_empty());
        assert_eq!(changes.removed.len(), 1);
        assert_eq!(changes.removed[0], "file2.rs");
    }

    #[test]
    fn diff_mixed_changes() {
        let previous = vec![
            FileRecord {
                path: "added_then_removed.rs".to_string(),
                blake3: "hash_old".to_string(),
                size: 100,
                lang: Some(Language::Rust),
                mtime: 1000,
            },
            FileRecord {
                path: "modified.rs".to_string(),
                blake3: "hash_mod_old".to_string(),
                size: 200,
                lang: Some(Language::Rust),
                mtime: 2000,
            },
            FileRecord {
                path: "unchanged.rs".to_string(),
                blake3: "hash_unchanged".to_string(),
                size: 300,
                lang: Some(Language::Rust),
                mtime: 3000,
            },
        ];

        let current = vec![
            FileRecord {
                path: "modified.rs".to_string(),
                blake3: "hash_mod_new".to_string(),
                size: 220,
                lang: Some(Language::Rust),
                mtime: 2500,
            },
            FileRecord {
                path: "unchanged.rs".to_string(),
                blake3: "hash_unchanged".to_string(),
                size: 300,
                lang: Some(Language::Rust),
                mtime: 3000,
            },
            FileRecord {
                path: "newly_added.rs".to_string(),
                blake3: "hash_new".to_string(),
                size: 50,
                lang: Some(Language::Rust),
                mtime: 4000,
            },
        ];

        let changes = RepoWalker::diff(&current, &previous);
        assert_eq!(changes.added.len(), 1);
        assert_eq!(changes.added[0].path, "newly_added.rs");
        assert_eq!(changes.modified.len(), 1);
        assert_eq!(changes.modified[0].path, "modified.rs");
        assert_eq!(changes.removed.len(), 1);
        assert_eq!(changes.removed[0], "added_then_removed.rs");
    }

    #[test]
    fn diff_unsorted_previous() {
        let previous = vec![
            FileRecord {
                path: "z_file.rs".to_string(),
                blake3: "hashz".to_string(),
                size: 100,
                lang: Some(Language::Rust),
                mtime: 1000,
            },
            FileRecord {
                path: "a_file.rs".to_string(),
                blake3: "hasha".to_string(),
                size: 200,
                lang: Some(Language::Rust),
                mtime: 2000,
            },
        ];

        let current = vec![
            FileRecord {
                path: "a_file.rs".to_string(),
                blake3: "hasha".to_string(),
                size: 200,
                lang: Some(Language::Rust),
                mtime: 2000,
            },
            FileRecord {
                path: "m_file.rs".to_string(),
                blake3: "hashm".to_string(),
                size: 300,
                lang: Some(Language::Rust),
                mtime: 3000,
            },
        ];

        let changes = RepoWalker::diff(&current, &previous);
        assert_eq!(changes.added.len(), 1);
        assert_eq!(changes.added[0].path, "m_file.rs");
        assert!(changes.modified.is_empty());
        assert_eq!(changes.removed.len(), 1);
        assert_eq!(changes.removed[0], "z_file.rs");
    }

    #[test]
    fn repo_walker_creation() {
        let walker = RepoWalker::new("/tmp");
        assert_eq!(walker.root(), Path::new("/tmp"));
    }

    #[test]
    fn walk_empty_directory() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let walker = RepoWalker::new(tempdir.path());
        let records = walker.walk().expect("walk should succeed");
        assert!(records.is_empty());
    }

    #[test]
    fn walk_single_file() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let file_path = tempdir.path().join("test.rs");
        std::fs::write(&file_path, b"fn main() {}").expect("write file");

        let walker = RepoWalker::new(tempdir.path());
        let records = walker.walk().expect("walk should succeed");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, "test.rs");
        assert_eq!(records[0].lang, Some(Language::Rust));
        assert_eq!(records[0].size, 12);
        assert!(!records[0].blake3.is_empty());
    }

    #[test]
    fn walk_multiple_languages() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        std::fs::write(tempdir.path().join("main.rs"), b"rust code").expect("write");
        std::fs::write(tempdir.path().join("main.go"), b"go code").expect("write");
        std::fs::write(tempdir.path().join("main.py"), b"python code").expect("write");
        std::fs::write(tempdir.path().join("README.md"), b"markdown").expect("write");

        let walker = RepoWalker::new(tempdir.path());
        let records = walker.walk().expect("walk should succeed");

        assert_eq!(records.len(), 4);
        // Should be sorted
        assert_eq!(records[0].path, "README.md");
        assert_eq!(records[0].lang, Some(Language::Other));
        assert_eq!(records[1].path, "main.go");
        assert_eq!(records[1].lang, Some(Language::Go));
        assert_eq!(records[2].path, "main.py");
        assert_eq!(records[2].lang, Some(Language::Python));
        assert_eq!(records[3].path, "main.rs");
        assert_eq!(records[3].lang, Some(Language::Rust));
    }

    #[test]
    fn walk_skips_binary_files() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let binary_path = tempdir.path().join("binary.bin");
        let mut binary_data = vec![0u8; 100];
        binary_data[50] = 0u8; // NUL byte
        std::fs::write(&binary_path, &binary_data).expect("write binary");

        let text_path = tempdir.path().join("text.txt");
        std::fs::write(&text_path, b"plain text").expect("write text");

        let walker = RepoWalker::new(tempdir.path());
        let records = walker.walk().expect("walk should succeed");

        // Binary file should be skipped, only text file present
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, "text.txt");
    }

    #[test]
    fn walk_respects_forward_slash_paths() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let subdir = tempdir.path().join("subdir");
        std::fs::create_dir(&subdir).expect("create subdir");
        std::fs::write(subdir.join("nested.rs"), b"code").expect("write");

        let walker = RepoWalker::new(tempdir.path());
        let records = walker.walk().expect("walk should succeed");

        assert_eq!(records.len(), 1);
        // Path should use forward slashes even on Windows
        assert_eq!(records[0].path, "subdir/nested.rs");
    }

    #[test]
    fn walk_sorted_output() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        for name in &["z.rs", "a.rs", "m.rs", "b.rs"] {
            std::fs::write(tempdir.path().join(name), b"code").expect("write");
        }

        let walker = RepoWalker::new(tempdir.path());
        let records = walker.walk().expect("walk should succeed");

        assert_eq!(records.len(), 4);
        assert_eq!(records[0].path, "a.rs");
        assert_eq!(records[1].path, "b.rs");
        assert_eq!(records[2].path, "m.rs");
        assert_eq!(records[3].path, "z.rs");
    }

    #[test]
    fn walk_blake3_consistency() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let content = b"consistent content";
        std::fs::write(tempdir.path().join("file.txt"), content).expect("write");

        let walker = RepoWalker::new(tempdir.path());
        let records1 = walker.walk().expect("first walk");
        let records2 = walker.walk().expect("second walk");

        assert_eq!(records1.len(), 1);
        assert_eq!(records2.len(), 1);
        assert_eq!(records1[0].blake3, records2[0].blake3);
    }

    #[test]
    fn walk_large_file() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let large_content = vec![b'x'; (MAX_INDEXABLE_BYTES + 1024) as usize];
        std::fs::write(tempdir.path().join("large.rs"), &large_content).expect("write");

        let walker = RepoWalker::new(tempdir.path());
        let records = walker.walk().expect("walk should succeed");

        // Large files should still be emitted with size > MAX_INDEXABLE_BYTES
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, "large.rs");
        assert!(records[0].size > MAX_INDEXABLE_BYTES);
        assert_eq!(records[0].lang, Some(Language::Rust));
    }

    #[test]
    fn walk_unknown_extension() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        std::fs::write(tempdir.path().join("file.unknown"), b"content").expect("write");

        let walker = RepoWalker::new(tempdir.path());
        let records = walker.walk().expect("walk should succeed");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].lang, None);
    }

    #[test]
    fn walk_respects_gitignore() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        std::fs::write(tempdir.path().join(".gitignore"), b"ignored.rs\n").expect("write");
        std::fs::write(tempdir.path().join("ignored.rs"), b"code").expect("write");
        std::fs::write(tempdir.path().join("included.rs"), b"code").expect("write");

        let walker = RepoWalker::new(tempdir.path());
        let records = walker.walk().expect("walk should succeed");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, "included.rs");
    }

    #[test]
    fn walk_respects_tmignore() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        std::fs::write(tempdir.path().join(".tmignore"), b"tmignored.rs\n").expect("write");
        std::fs::write(tempdir.path().join("tmignored.rs"), b"code").expect("write");
        std::fs::write(tempdir.path().join("included.rs"), b"code").expect("write");

        let walker = RepoWalker::new(tempdir.path());
        let records = walker.walk().expect("walk should succeed");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, "included.rs");
    }

    #[test]
    fn changeset_is_empty() {
        let empty = ChangeSet {
            added: vec![],
            modified: vec![],
            removed: vec![],
        };
        assert!(empty.is_empty());

        let with_added = ChangeSet {
            added: vec![FileRecord {
                path: "test.rs".to_string(),
                blake3: "hash".to_string(),
                size: 100,
                lang: Some(Language::Rust),
                mtime: 1000,
            }],
            modified: vec![],
            removed: vec![],
        };
        assert!(!with_added.is_empty());
    }
}
