//! Cache keying inputs for [`crate::command`]: a repository dirty-state fingerprint, the
//! environment-variable allowlist, and declared input paths, combined with blake3 into one
//! stable cache key.
//!
//! Per `SPEC.md` §8.2: `key = blake3(argv, cwd, env allowlist, repo dirty-state fingerprint,
//! declared inputs)`. Everything here is a pure function of already-gathered data
//! ([`cache_key`], [`RepoFingerprint::digest`]) except the [`GitInspector`] trait, which is
//! the injected I/O seam so the pure key math stays unit-testable against fixed inputs.

use std::collections::BTreeMap;
use std::path::Path;

use tm_types::Result;

/// A snapshot of repository state sufficient to detect "did anything relevant change since
/// the last run", without requiring a clean working tree: tracked *and* dirty files both
/// contribute, keyed by path with their current content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFingerprint {
    /// The current `HEAD` commit sha (or a sentinel for a repo with no commits).
    pub head: String,
    /// Every tracked file's path mapped to the blake3 hex hash of its current on-disk
    /// contents (so uncommitted edits change the digest, unlike hashing `HEAD`'s tree alone).
    pub file_hashes: BTreeMap<String, String>,
}

impl RepoFingerprint {
    /// Collapse this fingerprint into one stable digest string.
    ///
    /// IMPL: blake3 over a canonical byte encoding — `head` followed by every `(path, hash)`
    /// pair from `file_hashes` in `BTreeMap` (i.e. path-sorted) order, each field
    /// newline-separated so no ambiguous concatenation across entries. Return the lowercase
    /// hex digest. Deterministic and total: no error cases.
    pub fn digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();

        // Hash the HEAD commit.
        hasher.update(self.head.as_bytes());
        hasher.update(b"\n");

        // Hash each (path, hash) pair in sorted order (BTreeMap guarantees this).
        for (path, hash) in &self.file_hashes {
            hasher.update(path.as_bytes());
            hasher.update(b"=");
            hasher.update(hash.as_bytes());
            hasher.update(b"\n");
        }

        hasher.finalize().to_hex().to_string()
    }
}

/// The environment variables a [`crate::command::CommandSpec`] declares as relevant to its
/// output, by name. Only these are read and folded into the cache key — the full environment
/// is never hashed, both for stability (unrelated env churn must not bust the cache) and to
/// avoid keying on secrets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvAllowlist(pub Vec<String>);

impl EnvAllowlist {
    /// Build an allowlist from an iterator of variable names.
    pub fn new(names: impl IntoIterator<Item = String>) -> Self {
        EnvAllowlist(names.into_iter().collect())
    }

    /// Read this allowlist's variables via `lookup` (injected rather than reading
    /// `std::env` directly, so key computation stays a pure function of its inputs in tests),
    /// returning only the names that were actually set, sorted by name.
    ///
    /// IMPL: for each name in `self.0`, call `lookup(name)`; keep `Some` results, discard
    /// `None`; collect into a `BTreeMap` (sorts by construction). No error cases.
    pub fn snapshot(&self, lookup: &dyn Fn(&str) -> Option<String>) -> BTreeMap<String, String> {
        let mut result = BTreeMap::new();
        for name in &self.0 {
            if let Some(value) = lookup(name) {
                result.insert(name.clone(), value);
            }
        }
        result
    }
}

/// One input path a [`crate::command::CommandSpec`] declares it reads, beyond the repository
/// fingerprint (e.g. a generated file outside version control, or a config path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredInput {
    /// The path, relative to the project root or absolute.
    pub path: String,
    /// The blake3 hex hash of the path's contents at declaration time.
    pub hash: String,
}

/// Everything [`cache_key`] hashes together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKeyInputs {
    /// The command's argv, in order.
    pub argv: Vec<String>,
    /// The command's working directory.
    pub cwd: String,
    /// Allowlisted environment variables actually set, name to value, sorted by name (see
    /// [`EnvAllowlist::snapshot`]).
    pub env: BTreeMap<String, String>,
    /// The repository dirty-state fingerprint at the time of the call.
    pub repo_fingerprint: RepoFingerprint,
    /// Declared input paths and their content hashes.
    pub declared_inputs: Vec<DeclaredInput>,
}

/// Combine `inputs` into one stable, lowercase-hex blake3 cache key.
///
/// IMPL: build a canonical byte string — argv entries newline-joined, then `cwd`, then `env`
/// as sorted `name=value` lines (already sorted since `env` is a `BTreeMap`), then
/// `repo_fingerprint.digest()`, then `declared_inputs` sorted by `path` as `path=hash` lines —
/// each section separated by a distinct delimiter (e.g. `"\u{1e}"`, ASCII record separator) so
/// no field boundary is ambiguous. Hash the whole byte string with blake3, return
/// `to_hex().to_string()`. Deterministic and total: no error cases.
pub fn cache_key(inputs: &CacheKeyInputs) -> String {
    let mut hasher = blake3::Hasher::new();

    // Argv entries newline-joined.
    for arg in &inputs.argv {
        hasher.update(arg.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(b"\x1e"); // ASCII record separator

    // Current working directory.
    hasher.update(inputs.cwd.as_bytes());
    hasher.update(b"\x1e");

    // Environment as sorted name=value lines (BTreeMap guarantees sorted order).
    for (name, value) in &inputs.env {
        hasher.update(name.as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(b"\x1e");

    // Repository fingerprint digest.
    hasher.update(inputs.repo_fingerprint.digest().as_bytes());
    hasher.update(b"\x1e");

    // Declared inputs sorted by path.
    let mut declared_sorted: Vec<_> = inputs.declared_inputs.iter().collect();
    declared_sorted.sort_by(|a, b| a.path.cmp(&b.path));

    for input in declared_sorted {
        hasher.update(input.path.as_bytes());
        hasher.update(b"=");
        hasher.update(input.hash.as_bytes());
        hasher.update(b"\n");
    }

    hasher.finalize().to_hex().to_string()
}

/// The I/O seam for reading repository state: implemented against `git` (shelling out or via
/// a library) by the binary that wires this crate up; a fake implementation stands in for it
/// in unit tests so [`repo_fingerprint`]'s composition logic is testable without a real repo.
pub trait GitInspector {
    /// The current `HEAD` commit sha for the repository at `project_root`.
    fn head_sha(&self, project_root: &Path) -> Result<String>;

    /// Every tracked file's path (relative to `project_root`), regardless of dirty state.
    fn tracked_files(&self, project_root: &Path) -> Result<Vec<String>>;
}

/// Build a [`RepoFingerprint`] for `project_root` using `git` to enumerate tracked files and
/// `hash_file` to hash each one's current on-disk contents (so edits show up even though the
/// file list itself is unchanged).
///
/// IMPL: call `git.head_sha(project_root)`, then `git.tracked_files(project_root)`; for each
/// tracked path, call `hash_file(project_root, path)` and collect into `file_hashes`. Error
/// cases: propagate `GitInspector` errors; propagate `hash_file` errors (e.g. a tracked file
/// deleted on disk since `git` last saw it -- callers should treat that as `TmError::Io`).
/// Invariant: `file_hashes` covers exactly the paths `tracked_files` returned, no more, no
/// fewer.
pub fn repo_fingerprint(
    project_root: &Path,
    git: &dyn GitInspector,
    hash_file: &dyn Fn(&Path, &str) -> Result<String>,
) -> Result<RepoFingerprint> {
    let head = git.head_sha(project_root)?;
    let tracked_files = git.tracked_files(project_root)?;

    let mut file_hashes = BTreeMap::new();
    for path in tracked_files {
        let hash = hash_file(project_root, &path)?;
        file_hashes.insert(path, hash);
    }

    Ok(RepoFingerprint { head, file_hashes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_fingerprint_digest_empty() {
        let fp = RepoFingerprint {
            head: "abc123".to_string(),
            file_hashes: BTreeMap::new(),
        };
        let digest = fp.digest();
        assert!(!digest.is_empty());
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn repo_fingerprint_digest_single_file() {
        let mut file_hashes = BTreeMap::new();
        file_hashes.insert("file.txt".to_string(), "hash1".to_string());

        let fp = RepoFingerprint {
            head: "abc123".to_string(),
            file_hashes,
        };
        let digest = fp.digest();
        assert!(!digest.is_empty());
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn repo_fingerprint_digest_multiple_files_sorted() {
        let mut file_hashes = BTreeMap::new();
        file_hashes.insert("z_file.txt".to_string(), "hash_z".to_string());
        file_hashes.insert("a_file.txt".to_string(), "hash_a".to_string());
        file_hashes.insert("m_file.txt".to_string(), "hash_m".to_string());

        let fp = RepoFingerprint {
            head: "abc123".to_string(),
            file_hashes,
        };
        let digest = fp.digest();
        assert!(!digest.is_empty());
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn repo_fingerprint_digest_deterministic() {
        let mut file_hashes = BTreeMap::new();
        file_hashes.insert("file1.txt".to_string(), "hash1".to_string());
        file_hashes.insert("file2.txt".to_string(), "hash2".to_string());

        let fp = RepoFingerprint {
            head: "abc123".to_string(),
            file_hashes: file_hashes.clone(),
        };

        let digest1 = fp.digest();
        let fp2 = RepoFingerprint {
            head: "abc123".to_string(),
            file_hashes,
        };
        let digest2 = fp2.digest();

        assert_eq!(digest1, digest2);
    }

    #[test]
    fn repo_fingerprint_digest_different_head() {
        let file_hashes = BTreeMap::new();

        let fp1 = RepoFingerprint {
            head: "abc123".to_string(),
            file_hashes: file_hashes.clone(),
        };
        let fp2 = RepoFingerprint {
            head: "def456".to_string(),
            file_hashes,
        };

        assert_ne!(fp1.digest(), fp2.digest());
    }

    #[test]
    fn repo_fingerprint_digest_different_file_hashes() {
        let mut hashes1 = BTreeMap::new();
        hashes1.insert("file.txt".to_string(), "hash1".to_string());

        let mut hashes2 = BTreeMap::new();
        hashes2.insert("file.txt".to_string(), "hash2".to_string());

        let fp1 = RepoFingerprint {
            head: "abc123".to_string(),
            file_hashes: hashes1,
        };
        let fp2 = RepoFingerprint {
            head: "abc123".to_string(),
            file_hashes: hashes2,
        };

        assert_ne!(fp1.digest(), fp2.digest());
    }

    #[test]
    fn env_allowlist_holds_configured_names() {
        let allow = EnvAllowlist::new(["TM_ROLE".to_string(), "TM_SESSION".to_string()]);
        assert_eq!(allow.0.len(), 2);
    }

    #[test]
    fn env_allowlist_snapshot_empty() {
        let allow = EnvAllowlist::new([]);
        let snapshot = allow.snapshot(&|_| None);
        assert!(snapshot.is_empty());
    }

    #[test]
    fn env_allowlist_snapshot_all_present() {
        let allow = EnvAllowlist::new(["VAR1".to_string(), "VAR2".to_string()]);
        let snapshot = allow.snapshot(&|name| match name {
            "VAR1" => Some("value1".to_string()),
            "VAR2" => Some("value2".to_string()),
            _ => None,
        });
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot.get("VAR1").map(|s| s.as_str()), Some("value1"));
        assert_eq!(snapshot.get("VAR2").map(|s| s.as_str()), Some("value2"));
    }

    #[test]
    fn env_allowlist_snapshot_partial() {
        let allow = EnvAllowlist::new(["VAR1".to_string(), "VAR2".to_string(), "VAR3".to_string()]);
        let snapshot = allow.snapshot(&|name| match name {
            "VAR1" => Some("value1".to_string()),
            "VAR3" => Some("value3".to_string()),
            _ => None,
        });
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot.get("VAR1").map(|s| s.as_str()), Some("value1"));
        assert_eq!(snapshot.get("VAR3").map(|s| s.as_str()), Some("value3"));
        assert!(!snapshot.contains_key("VAR2"));
    }

    #[test]
    fn env_allowlist_snapshot_sorted() {
        let allow = EnvAllowlist::new([
            "Z_VAR".to_string(),
            "A_VAR".to_string(),
            "M_VAR".to_string(),
        ]);
        let snapshot = allow.snapshot(&|name| Some(format!("{}_value", name)));
        let keys: Vec<_> = snapshot.keys().collect();
        assert_eq!(
            keys,
            vec![
                &"A_VAR".to_string(),
                &"M_VAR".to_string(),
                &"Z_VAR".to_string()
            ]
        );
    }

    #[test]
    fn cache_key_deterministic() {
        let mut file_hashes = BTreeMap::new();
        file_hashes.insert("file.txt".to_string(), "abcd1234".to_string());

        let mut env = BTreeMap::new();
        env.insert("VAR1".to_string(), "val1".to_string());

        let inputs = CacheKeyInputs {
            argv: vec!["cargo".to_string(), "build".to_string()],
            cwd: "/project".to_string(),
            env: env.clone(),
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes: file_hashes.clone(),
            },
            declared_inputs: vec![],
        };

        let key1 = cache_key(&inputs);
        let key2 = cache_key(&inputs);

        assert_eq!(key1, key2);
    }

    #[test]
    fn cache_key_different_argv() {
        let mut file_hashes = BTreeMap::new();
        file_hashes.insert("file.txt".to_string(), "abcd1234".to_string());

        let env = BTreeMap::new();

        let inputs1 = CacheKeyInputs {
            argv: vec!["cargo".to_string(), "build".to_string()],
            cwd: "/project".to_string(),
            env: env.clone(),
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes: file_hashes.clone(),
            },
            declared_inputs: vec![],
        };

        let inputs2 = CacheKeyInputs {
            argv: vec!["cargo".to_string(), "test".to_string()],
            cwd: "/project".to_string(),
            env,
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes,
            },
            declared_inputs: vec![],
        };

        assert_ne!(cache_key(&inputs1), cache_key(&inputs2));
    }

    #[test]
    fn cache_key_different_cwd() {
        let mut file_hashes = BTreeMap::new();
        file_hashes.insert("file.txt".to_string(), "abcd1234".to_string());

        let env = BTreeMap::new();
        let argv = vec!["cargo".to_string(), "build".to_string()];

        let inputs1 = CacheKeyInputs {
            argv: argv.clone(),
            cwd: "/project1".to_string(),
            env: env.clone(),
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes: file_hashes.clone(),
            },
            declared_inputs: vec![],
        };

        let inputs2 = CacheKeyInputs {
            argv,
            cwd: "/project2".to_string(),
            env,
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes,
            },
            declared_inputs: vec![],
        };

        assert_ne!(cache_key(&inputs1), cache_key(&inputs2));
    }

    #[test]
    fn cache_key_different_env() {
        let mut file_hashes = BTreeMap::new();
        file_hashes.insert("file.txt".to_string(), "abcd1234".to_string());

        let mut env1 = BTreeMap::new();
        env1.insert("VAR1".to_string(), "value1".to_string());

        let mut env2 = BTreeMap::new();
        env2.insert("VAR1".to_string(), "value2".to_string());

        let argv = vec!["cargo".to_string(), "build".to_string()];

        let inputs1 = CacheKeyInputs {
            argv: argv.clone(),
            cwd: "/project".to_string(),
            env: env1,
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes: file_hashes.clone(),
            },
            declared_inputs: vec![],
        };

        let inputs2 = CacheKeyInputs {
            argv,
            cwd: "/project".to_string(),
            env: env2,
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes,
            },
            declared_inputs: vec![],
        };

        assert_ne!(cache_key(&inputs1), cache_key(&inputs2));
    }

    #[test]
    fn cache_key_different_repo_state() {
        let mut file_hashes1 = BTreeMap::new();
        file_hashes1.insert("file.txt".to_string(), "hash1".to_string());

        let mut file_hashes2 = BTreeMap::new();
        file_hashes2.insert("file.txt".to_string(), "hash2".to_string());

        let env = BTreeMap::new();
        let argv = vec!["cargo".to_string(), "build".to_string()];

        let inputs1 = CacheKeyInputs {
            argv: argv.clone(),
            cwd: "/project".to_string(),
            env: env.clone(),
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes: file_hashes1,
            },
            declared_inputs: vec![],
        };

        let inputs2 = CacheKeyInputs {
            argv,
            cwd: "/project".to_string(),
            env,
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes: file_hashes2,
            },
            declared_inputs: vec![],
        };

        assert_ne!(cache_key(&inputs1), cache_key(&inputs2));
    }

    #[test]
    fn cache_key_different_declared_inputs() {
        let file_hashes = BTreeMap::new();
        let env = BTreeMap::new();
        let argv = vec!["cargo".to_string(), "build".to_string()];

        let declared1 = vec![DeclaredInput {
            path: "input.txt".to_string(),
            hash: "hash1".to_string(),
        }];

        let declared2 = vec![DeclaredInput {
            path: "input.txt".to_string(),
            hash: "hash2".to_string(),
        }];

        let inputs1 = CacheKeyInputs {
            argv: argv.clone(),
            cwd: "/project".to_string(),
            env: env.clone(),
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes: file_hashes.clone(),
            },
            declared_inputs: declared1,
        };

        let inputs2 = CacheKeyInputs {
            argv,
            cwd: "/project".to_string(),
            env,
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes,
            },
            declared_inputs: declared2,
        };

        assert_ne!(cache_key(&inputs1), cache_key(&inputs2));
    }

    #[test]
    fn cache_key_declared_inputs_sorted() {
        let file_hashes = BTreeMap::new();
        let env = BTreeMap::new();
        let argv = vec!["cargo".to_string()];

        let declared1 = vec![
            DeclaredInput {
                path: "z.txt".to_string(),
                hash: "hz".to_string(),
            },
            DeclaredInput {
                path: "a.txt".to_string(),
                hash: "ha".to_string(),
            },
        ];

        let declared2 = vec![
            DeclaredInput {
                path: "a.txt".to_string(),
                hash: "ha".to_string(),
            },
            DeclaredInput {
                path: "z.txt".to_string(),
                hash: "hz".to_string(),
            },
        ];

        let inputs1 = CacheKeyInputs {
            argv: argv.clone(),
            cwd: "/project".to_string(),
            env: env.clone(),
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes: file_hashes.clone(),
            },
            declared_inputs: declared1,
        };

        let inputs2 = CacheKeyInputs {
            argv,
            cwd: "/project".to_string(),
            env,
            repo_fingerprint: RepoFingerprint {
                head: "abc123".to_string(),
                file_hashes,
            },
            declared_inputs: declared2,
        };

        assert_eq!(cache_key(&inputs1), cache_key(&inputs2));
    }

    struct FakeGitInspector {
        head: std::cell::RefCell<Option<Result<String>>>,
        tracked: std::cell::RefCell<Option<Result<Vec<String>>>>,
    }

    impl FakeGitInspector {
        fn new(head: Result<String>, tracked: Result<Vec<String>>) -> Self {
            Self {
                head: std::cell::RefCell::new(Some(head)),
                tracked: std::cell::RefCell::new(Some(tracked)),
            }
        }
    }

    impl GitInspector for FakeGitInspector {
        fn head_sha(&self, _: &Path) -> Result<String> {
            self.head
                .borrow_mut()
                .take()
                .expect("head_sha called at most once per test")
        }

        fn tracked_files(&self, _: &Path) -> Result<Vec<String>> {
            self.tracked
                .borrow_mut()
                .take()
                .expect("tracked_files called at most once per test")
        }
    }

    #[test]
    fn repo_fingerprint_happy_path() {
        let git = FakeGitInspector::new(
            Ok("abc123".to_string()),
            Ok(vec!["file1.txt".to_string(), "file2.txt".to_string()]),
        );

        let hash_calls = std::cell::RefCell::new(0);
        let hash_file = |_: &Path, path: &str| -> Result<String> {
            let mut calls = hash_calls.borrow_mut();
            *calls += 1;
            Ok(format!("hash_{}", path))
        };

        let result = repo_fingerprint(Path::new("/project"), &git, &hash_file).unwrap();
        assert_eq!(result.head, "abc123");
        assert_eq!(result.file_hashes.len(), 2);
        assert_eq!(
            result.file_hashes.get("file1.txt").map(|s| s.as_str()),
            Some("hash_file1.txt")
        );
        assert_eq!(
            result.file_hashes.get("file2.txt").map(|s| s.as_str()),
            Some("hash_file2.txt")
        );
    }

    #[test]
    fn repo_fingerprint_git_head_error() {
        let git = FakeGitInspector::new(
            Err(tm_types::TmError::Io("head error".to_string())),
            Ok(vec![]),
        );

        let hash_file = |_: &Path, _: &str| -> Result<String> { Ok("hash".to_string()) };
        let result = repo_fingerprint(Path::new("/project"), &git, &hash_file);
        assert!(result.is_err());
    }

    #[test]
    fn repo_fingerprint_git_tracked_error() {
        let git = FakeGitInspector::new(
            Ok("abc123".to_string()),
            Err(tm_types::TmError::Io("tracked error".to_string())),
        );

        let hash_file = |_: &Path, _: &str| -> Result<String> { Ok("hash".to_string()) };
        let result = repo_fingerprint(Path::new("/project"), &git, &hash_file);
        assert!(result.is_err());
    }

    #[test]
    fn repo_fingerprint_hash_file_error() {
        let git = FakeGitInspector::new(Ok("abc123".to_string()), Ok(vec!["file.txt".to_string()]));

        let hash_file = |_: &Path, _: &str| -> Result<String> {
            Err(tm_types::TmError::Io("hash error".to_string()))
        };

        let result = repo_fingerprint(Path::new("/project"), &git, &hash_file);
        assert!(result.is_err());
    }

    #[test]
    fn repo_fingerprint_empty_tracked_files() {
        let git = FakeGitInspector::new(Ok("abc123".to_string()), Ok(vec![]));

        let hash_file = |_: &Path, _: &str| -> Result<String> {
            panic!("should not be called");
        };

        let result = repo_fingerprint(Path::new("/project"), &git, &hash_file).unwrap();
        assert_eq!(result.head, "abc123");
        assert!(result.file_hashes.is_empty());
    }
}
