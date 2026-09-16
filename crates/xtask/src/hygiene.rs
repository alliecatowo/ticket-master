//! Source-scanning hygiene checks for the Ticketmaster workspace.
//!
//! Each check walks the `crates/` tree (skipping paths that do not exist, since
//! sibling crates are still being written by other agents) and returns a list
//! of `file:line: message` violation strings. An empty vec means the check
//! passed.

use std::fs;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

/// Runs every hygiene check against `root` (the workspace root) and returns
/// the concatenation of all violations found.
pub fn run_all(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    out.extend(check_time_and_rand(root));
    out.extend(check_events_mutation(root));
    out.extend(check_network_in_tests(root));
    out.extend(check_unwrap_expect(root));
    out.extend(check_crate_descriptions(root));
    out
}

/// Walks every `.rs` file under `crates/*/src` (or `crates/*/tests`, for the
/// network check) and hands `(path, line_no_1_based, line_text)` to `visit`.
fn walk_rs_files(dir: &Path, mut visit: impl FnMut(&Path, &str)) {
    if !dir.exists() {
        return;
    }
    for entry in WalkBuilder::new(dir).hidden(false).build() {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(path) else {
            continue;
        };
        visit(path, &contents);
    }
}

/// Every crates/*/src directory, skipping crates that do not exist yet.
///
/// `xtask` itself is excluded: this file necessarily contains every pattern the checks search
/// for, as string literals, so scanning it would report the scanner as its own worst offender.
fn crate_src_dirs(root: &Path) -> Vec<PathBuf> {
    let crates_dir = root.join("crates");
    if !crates_dir.exists() {
        return Vec::new();
    }
    let mut dirs = Vec::new();
    if let Ok(entries) = fs::read_dir(&crates_dir) {
        for entry in entries.flatten() {
            if entry.file_name() == "xtask" {
                continue;
            }
            let src = entry.path().join("src");
            if src.exists() {
                dirs.push(src);
            }
        }
    }
    dirs
}

/// (a) No non-deterministic time/randomness sources outside the clock
/// substrate at `crates/tm-types/src/clock.rs`.
pub fn check_time_and_rand(root: &Path) -> Vec<String> {
    const FORBIDDEN: &[&str] = &[
        "SystemTime::now",
        "Instant::now",
        "rand::thread_rng",
        "rand::random",
        "uuid::new_v4",
    ];
    let exempt = root.join("crates/tm-types/src/clock.rs");
    let mut violations = Vec::new();
    for src in crate_src_dirs(root) {
        walk_rs_files(&src, |path, contents| {
            if path == exempt {
                return;
            }
            for (i, line) in contents.lines().enumerate() {
                for needle in FORBIDDEN {
                    if line.contains(needle) {
                        violations.push(format!(
                            "{}:{}: forbidden non-deterministic source `{}` outside tm-types/src/clock.rs",
                            path.display(),
                            i + 1,
                            needle
                        ));
                    }
                }
            }
        });
    }
    violations
}

/// (b) No UPDATE/DELETE against the `events` table (append-only ledger).
pub fn check_events_mutation(root: &Path) -> Vec<String> {
    let mut violations = Vec::new();
    for src in crate_src_dirs(root) {
        walk_rs_files(&src, |path, contents| {
            let lines: Vec<&str> = contents.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                // Prose about the rule is not the rule being broken.
                if trimmed.starts_with("//") {
                    continue;
                }
                let lower = line.to_ascii_lowercase();
                // A `BEFORE UPDATE ON events ... RAISE(ABORT, ...)` trigger is the mechanism that
                // enforces append-only storage, so it names the forbidden statement in order to
                // forbid it. A CREATE TRIGGER in the surrounding window is that declaration.
                let lo = i.saturating_sub(12);
                let hi = (i + 4).min(lines.len() - 1);
                let in_trigger =
                    lines[lo..=hi].iter().any(|l| l.to_ascii_lowercase().contains("create trigger"));
                if in_trigger {
                    continue;
                }
                if lower.contains("update events") || lower.contains("delete from events") {
                    violations.push(format!(
                        "{}:{}: mutating statement targets append-only `events` table",
                        path.display(),
                        i + 1
                    ));
                }
            }
        });
    }
    violations
}

/// Tracks whether we are currently inside a `#[cfg(test)]` block via a naive
/// brace-depth counter. Good enough for well-formatted source; not a real
/// parser.
struct TestRegionTracker {
    pending_cfg_test: bool,
    in_test_depth: i32,
    whole_file_is_test: bool,
}

impl TestRegionTracker {
    fn new(path: &Path) -> Self {
        let whole_file_is_test = path.components().any(|c| c.as_os_str() == "tests");
        Self {
            pending_cfg_test: false,
            in_test_depth: 0,
            whole_file_is_test,
        }
    }

    fn in_test(&self) -> bool {
        self.whole_file_is_test || self.in_test_depth > 0
    }

    fn observe(&mut self, line: &str) {
        if self.whole_file_is_test {
            return;
        }
        if line.contains("#[cfg(test)]") {
            self.pending_cfg_test = true;
        }
        if self.pending_cfg_test && self.in_test_depth == 0 {
            let opens = line.matches('{').count() as i32;
            if opens > 0 {
                self.in_test_depth += opens;
                self.in_test_depth -= line.matches('}').count() as i32;
                self.pending_cfg_test = false;
                return;
            }
        }
        if self.in_test_depth > 0 {
            self.in_test_depth += line.matches('{').count() as i32;
            self.in_test_depth -= line.matches('}').count() as i32;
            if self.in_test_depth < 0 {
                self.in_test_depth = 0;
            }
        }
    }
}

/// (c) No network hosts reachable from test code: real client construction
/// against http(s) URLs, or reads of `ANTHROPIC_API_KEY`. Plain URL string
/// literals used as test fixture data are fine.
pub fn check_network_in_tests(root: &Path) -> Vec<String> {
    let mut violations = Vec::new();
    let crates_dir = root.join("crates");
    if !crates_dir.exists() {
        return violations;
    }
    if let Ok(entries) = fs::read_dir(&crates_dir) {
        for entry in entries.flatten() {
            if entry.file_name() == "xtask" {
                continue;
            }
            let crate_dir = entry.path();
            for sub in ["src", "tests"] {
                let dir = crate_dir.join(sub);
                walk_rs_files(&dir, |path, contents| {
                    let mut tracker = TestRegionTracker::new(path);
                    for (i, line) in contents.lines().enumerate() {
                        tracker.observe(line);
                        if !tracker.in_test() {
                            continue;
                        }
                        if line.contains("ANTHROPIC_API_KEY") {
                            violations.push(format!(
                                "{}:{}: test code reads ANTHROPIC_API_KEY (network dependency)",
                                path.display(),
                                i + 1
                            ));
                            continue;
                        }
                        let has_url = line.contains("http://") || line.contains("https://");
                        let has_client = line.contains("reqwest")
                            || line.contains("Client::new")
                            || line.contains(".get(")
                            || line.contains(".post(");
                        if has_url && has_client {
                            violations.push(format!(
                                "{}:{}: test code constructs a network client against a real host",
                                path.display(),
                                i + 1
                            ));
                        }
                    }
                });
            }
        }
    }
    violations
}

/// (d) No `unwrap()`/`expect()` outside tests, unless the 3 lines above carry
/// a comment mentioning "invariant".
pub fn check_unwrap_expect(root: &Path) -> Vec<String> {
    let mut violations = Vec::new();
    for src in crate_src_dirs(root) {
        walk_rs_files(&src, |path, contents| {
            let mut tracker = TestRegionTracker::new(path);
            let lines: Vec<&str> = contents.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                tracker.observe(line);
                if tracker.in_test() {
                    continue;
                }
                if !(line.contains(".unwrap()") || line.contains(".expect(")) {
                    continue;
                }
                let window_start = i.saturating_sub(3);
                let allowed = lines[window_start..i].iter().any(|l| {
                    let lower = l.to_ascii_lowercase();
                    lower.contains("//") && lower.contains("invariant")
                });
                if allowed {
                    continue;
                }
                violations.push(format!(
                    "{}:{}: unwrap()/expect() outside tests without a documented invariant comment",
                    path.display(),
                    i + 1
                ));
            }
        });
    }
    violations
}

/// (e) Every crate under `crates/` declares a `description` in its
/// `Cargo.toml`.
pub fn check_crate_descriptions(root: &Path) -> Vec<String> {
    let mut violations = Vec::new();
    let crates_dir = root.join("crates");
    if !crates_dir.exists() {
        return violations;
    }
    let Ok(entries) = fs::read_dir(&crates_dir) else {
        return violations;
    };
    for entry in entries.flatten() {
        let manifest = entry.path().join("Cargo.toml");
        if !manifest.exists() {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&manifest) else {
            continue;
        };
        let has_description = contents
            .lines()
            .any(|l| l.trim_start().starts_with("description"));
        if !has_description {
            violations.push(format!(
                "{}:1: crate is missing a `description` field",
                manifest.display()
            ));
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("xtask-hygiene-test-{}-{}", std::process::id(), id));
        fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).expect("create parent");
        fs::write(path, contents).expect("write fixture");
    }

    #[test]
    fn time_and_rand_flags_violation_and_allows_clean() {
        let root = temp_root();
        write(
            &root.join("crates/tm-core/src/lib.rs"),
            "fn now() -> u64 { std::time::SystemTime::now(); 0 }\n",
        );
        let violations = check_time_and_rand(&root);
        assert_eq!(violations.len(), 1, "{violations:?}");

        let root2 = temp_root();
        write(
            &root2.join("crates/tm-core/src/lib.rs"),
            "fn now(clock: &dyn Clock) -> u64 { clock.now() }\n",
        );
        assert!(check_time_and_rand(&root2).is_empty());
    }

    #[test]
    fn time_and_rand_exempts_clock_substrate() {
        let root = temp_root();
        write(
            &root.join("crates/tm-types/src/clock.rs"),
            "fn real_now() -> std::time::SystemTime { std::time::SystemTime::now() }\n",
        );
        assert!(check_time_and_rand(&root).is_empty());
    }

    #[test]
    fn events_mutation_flags_violation_and_allows_clean() {
        let root = temp_root();
        write(
            &root.join("crates/tm-events/src/lib.rs"),
            "const SQL: &str = \"UPDATE events SET x = 1\";\n",
        );
        assert_eq!(check_events_mutation(&root).len(), 1);

        let root2 = temp_root();
        write(
            &root2.join("crates/tm-events/src/lib.rs"),
            "const SQL: &str = \"INSERT INTO events (id) VALUES (?)\";\n",
        );
        assert!(check_events_mutation(&root2).is_empty());
    }

    #[test]
    fn network_in_tests_flags_violation_and_allows_clean() {
        let root = temp_root();
        write(
            &root.join("crates/tm-provider/src/lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn hits_network() {\n        let c = reqwest::Client::new();\n        c.get(\"https://api.example.com\");\n    }\n}\n",
        );
        assert_eq!(check_network_in_tests(&root).len(), 1);

        let root2 = temp_root();
        write(
            &root2.join("crates/tm-provider/src/lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn fixture() {\n        let url = \"https://api.example.com/docs\";\n        assert!(url.starts_with(\"https\"));\n    }\n}\n",
        );
        assert!(check_network_in_tests(&root2).is_empty());
    }

    #[test]
    fn network_in_tests_flags_api_key_env_read() {
        let root = temp_root();
        write(
            &root.join("crates/tm-provider/tests/live.rs"),
            "#[test]\nfn uses_key() {\n    let key = std::env::var(\"ANTHROPIC_API_KEY\").unwrap();\n}\n",
        );
        assert_eq!(check_network_in_tests(&root).len(), 1);
    }

    #[test]
    fn unwrap_expect_flags_violation_and_allows_clean() {
        let root = temp_root();
        write(
            &root.join("crates/tm-core/src/lib.rs"),
            "fn f(x: Option<i32>) -> i32 { x.unwrap() }\n",
        );
        assert_eq!(check_unwrap_expect(&root).len(), 1);

        let root2 = temp_root();
        write(
            &root2.join("crates/tm-core/src/lib.rs"),
            "// invariant: parser guarantees this is Some\nfn f(x: Option<i32>) -> i32 { x.unwrap() }\n",
        );
        assert!(check_unwrap_expect(&root2).is_empty());
    }

    #[test]
    fn unwrap_expect_ignores_test_modules() {
        let root = temp_root();
        write(
            &root.join("crates/tm-core/src/lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn ok() {\n        let x: Option<i32> = Some(1);\n        x.unwrap();\n    }\n}\n",
        );
        assert!(check_unwrap_expect(&root).is_empty());
    }

    #[test]
    fn crate_descriptions_flags_missing_and_allows_present() {
        let root = temp_root();
        write(
            &root.join("crates/tm-nodesc/Cargo.toml"),
            "[package]\nname = \"tm-nodesc\"\nversion = \"0.1.0\"\n",
        );
        assert_eq!(check_crate_descriptions(&root).len(), 1);

        let root2 = temp_root();
        write(
            &root2.join("crates/tm-hasdesc/Cargo.toml"),
            "[package]\nname = \"tm-hasdesc\"\nversion = \"0.1.0\"\ndescription = \"has one\"\n",
        );
        assert!(check_crate_descriptions(&root2).is_empty());
    }

    #[test]
    fn missing_crates_dir_is_skipped_not_failed() {
        let root = temp_root();
        assert!(run_all(&root).is_empty());
    }
}
