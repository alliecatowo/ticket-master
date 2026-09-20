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
    out.extend(check_dot_tm_literals(root));
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
            let mut tracker = TestRegionTracker::new(path);
            for (i, line) in contents.lines().enumerate() {
                tracker.observe(line);
                // Determinism protects replay, which is a property of production code. A test
                // that measures elapsed wall time to prove `recv` blocks is not a replay hazard.
                if tracker.in_test() {
                    continue;
                }
                // A module doc comment stating "never call SystemTime::now" is the rule being
                // documented, not broken.
                if line.trim_start().starts_with("//") {
                    continue;
                }
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
            let mut tracker = TestRegionTracker::new(path);
            for (i, line) in lines.iter().enumerate() {
                tracker.observe(line);
                // The tests that assert the append-only triggers fire must issue the very
                // statements the triggers forbid. Flagging them would punish the proof.
                if tracker.in_test() {
                    continue;
                }
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
                let in_trigger = lines[lo..=hi]
                    .iter()
                    .any(|l| l.to_ascii_lowercase().contains("create trigger"));
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

/// Tracks whether a line sits inside test-only code.
///
/// Rust convention puts `#[cfg(test)] mod tests` at the end of a file, and brace counting is
/// unreliable here: a brace inside a char or string literal — `text.find('}')` — ends the region
/// early and leaks test code into the production checks. Treating everything from the first
/// `#[cfg(test)]` to end of file as test code is both simpler and harder to fool.
struct TestRegionTracker {
    in_test: bool,
}

impl TestRegionTracker {
    fn new(_path: &Path) -> Self {
        TestRegionTracker { in_test: false }
    }

    fn observe(&mut self, line: &str) {
        let t = line.trim_start();
        if t.starts_with("#[cfg(test)]")
            || t.starts_with("#[test]")
            || t.starts_with("#![cfg(test)]")
        {
            self.in_test = true;
        }
    }

    fn in_test(&self) -> bool {
        self.in_test
    }
}

/// Everything before a line's `//` comment, treating `://` as part of a URL rather than the
/// start of a comment — otherwise `"https://example.com"` truncates to `"https:"` and the URL
/// checks below stop seeing URLs at all.
fn strip_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' {
            if i > 0 && bytes[i - 1] == b':' {
                i += 2;
                continue;
            }
            return &line[..i];
        }
        i += 1;
    }
    line
}

/// (c) No network hosts reachable from test code: real client construction
/// against http(s) URLs, or reads of `ANTHROPIC_API_KEY`. Plain URL string
/// literals used as test fixture data are fine, as are comments mentioning
/// either.
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
                        // A comment *about* a key or a URL is not a use of one — several tests
                        // document that an env var is deliberately unset, which the substring
                        // match would otherwise read as a violation.
                        let code = strip_line_comment(line);

                        // Require an actual environment read, not a mention: the point of the
                        // rule is a test that depends on a real credential being present.
                        if code.contains("ANTHROPIC_API_KEY")
                            && (code.contains("var(")
                                || code.contains("var_os(")
                                || code.contains("set_var")
                                || code.contains("env!"))
                        {
                            violations.push(format!(
                                "{}:{}: test code reads ANTHROPIC_API_KEY (network dependency)",
                                path.display(),
                                i + 1
                            ));
                            continue;
                        }
                        let has_url = code.contains("http://") || code.contains("https://");
                        // `.get(`/`.post(` alone also match `HashMap::get` and `HeaderMap::get`,
                        // so require the URL to be the argument rather than merely on the same
                        // line as some `.get(`.
                        let has_client = code.contains("reqwest::Client")
                            || code.contains("Client::new")
                            || code.contains(".get(\"http")
                            || code.contains(".post(\"http");
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
    /// Messages that are not a justification, however long they are.
    const GENERIC: &[&str] = &[
        "unwrap",
        "should work",
        "should not fail",
        "failed",
        "error",
        "ok",
        "todo",
        "fixme",
        "must work",
        "no error",
    ];

    /// A message counts as documentation when it is long enough to say something and is not one
    /// of the stock non-answers above. It beats a comment: it survives into the panic output.
    fn message_justifies(msg: &str) -> bool {
        let lower = msg.trim().to_ascii_lowercase();
        lower.len() >= 24 && !GENERIC.iter().any(|g| lower == *g)
    }

    let mut violations = Vec::new();
    for src in crate_src_dirs(root) {
        walk_rs_files(&src, |path, contents| {
            let lines: Vec<&str> = contents.lines().collect();
            let mut tracker = TestRegionTracker::new(path);
            for (i, line) in lines.iter().enumerate() {
                tracker.observe(line);
                if tracker.in_test() {
                    continue;
                }
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }

                // A comment in the preceding window naming the invariant also satisfies the rule.
                let lo = i.saturating_sub(6);
                let documented = lines[lo..i].iter().any(|l| {
                    let lower = l.to_ascii_lowercase();
                    lower.contains("//") && (lower.contains("invariant") || lower.contains("safe:"))
                });

                if line.contains(".unwrap()") && !documented {
                    violations.push(format!(
                        "{}:{}: bare unwrap() outside tests; use `?` or expect() with a justification",
                        path.display(),
                        i + 1
                    ));
                }

                if let Some(rest) = line.split(".expect(\"").nth(1) {
                    let msg = rest.split('"').next().unwrap_or("");
                    if !documented && !message_justifies(msg) {
                        violations.push(format!(
                            "{}:{}: expect() outside tests whose message does not justify it: {:?}",
                            path.display(),
                            i + 1,
                            msg
                        ));
                    }
                }
            }
        });
    }
    violations
}

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

/// Files allowed to hardcode a `.tm` literal in production (pre-test-region) code, relative to
/// the workspace root, forward-slash separated. Every entry here is a documented, sanctioned
/// exception, not an oversight:
///
/// - `crates/tm-cli/src/project.rs` — the one place `docs/decisions/D-003-project-scope.md`
///   designates as the resolution path (`locate`, `open`, `tm_home`, `resolve_scope`,
///   `create_project_dir`): every other caller in the workspace is expected to go through an
///   already-opened `Project`'s `state_dir` field instead of rediscovering it.
/// - `crates/tm-core/src/store.rs` and `crates/tm-codeintel/src/api.rs` /
///   `crates/tm-codeintel/src/store.rs` — each owns exactly one pair of `open`/`open_with`
///   "old constructor" shims over an `_at`-suffixed one, kept for the repo-scoped
///   `<project_root>/.tm` layout callers used before D-003 split `root` from `state_dir`
///   (D-003's own "Context" section names this Phase 1-A work explicitly).
/// - `crates/tm-browser/src/managed.rs` — `~/.tm/browsers/<channel>-<version>/`, a real,
///   unrelated `$HOME`-based download cache that predates D-003 and has nothing to do with a
///   project's `state_dir` (D-003 itself cites this as the convention its own `$TM_HOME` layout
///   follows).
const DOT_TM_LITERAL_ALLOWLIST: &[&str] = &[
    "crates/tm-cli/src/project.rs",
    "crates/tm-core/src/store.rs",
    "crates/tm-codeintel/src/api.rs",
    "crates/tm-codeintel/src/store.rs",
    "crates/tm-browser/src/managed.rs",
];

/// (e) No new hardcoded `.tm` path-join or `".tm"` string literal outside
/// [`DOT_TM_LITERAL_ALLOWLIST`].
///
/// D-003 split a project's `root` (workspace) from its `state_dir`: `<root>/.tm` in repo scope,
/// `$TM_HOME/projects/<key>/` in global scope, entirely outside the workspace (see
/// `docs/decisions/D-003-project-scope.md`). A hand-rolled `.join(".tm")` — or an equivalent bare
/// `".tm"` string literal used to build one — anywhere outside the allowlist assumes the
/// pre-D-003 shape and silently misbehaves the moment it runs against a global-scope project:
/// exactly the class of regression this same session already found and fixed once, in
/// `resolve_scope`'s own `locate()` false-positiving on the real `$HOME/.tm`. New code should
/// take an already-opened `Project`'s `state_dir` field, or an `_at`-suffixed constructor that
/// accepts one directly, rather than rediscovering it.
///
/// Scoped to `crates/*/src` (not `tests/`) and skips test regions the same way every check above
/// does ([`TestRegionTracker`]): test fixtures across the workspace legitimately construct a
/// `.tm` directory by hand to drive the very shims [`DOT_TM_LITERAL_ALLOWLIST`] documents, and
/// flagging every one of them would be noise, not triage — the same reasoning `check-drift`'s own
/// module docs give for staying narrow. Matching the literal string `".tm"` exactly (not a
/// substring) means a longer, unrelated literal like `".tmp"` or a prose string that merely
/// mentions `.tm/workflows` in a user-facing message never matches.
pub fn check_dot_tm_literals(root: &Path) -> Vec<String> {
    let mut violations = Vec::new();
    for src in crate_src_dirs(root) {
        walk_rs_files(&src, |path, contents| {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if DOT_TM_LITERAL_ALLOWLIST
                .iter()
                .any(|allowed| relative == *allowed)
            {
                return;
            }
            let mut tracker = TestRegionTracker::new(path);
            for (i, line) in contents.lines().enumerate() {
                tracker.observe(line);
                if tracker.in_test() {
                    continue;
                }
                // Prose about the convention (including this very check's own doc comment, were
                // it ever copied elsewhere) is not the pattern being forbidden.
                let code = strip_line_comment(line);
                if code.contains("\".tm\"") {
                    violations.push(format!(
                        "{}:{}: hardcoded `.tm` literal outside the sanctioned D-003 resolution \
                         path — use an already-opened Project's `state_dir` (or an \
                         `_at`-suffixed constructor) instead of joining `.tm` directly (see \
                         docs/decisions/D-003-project-scope.md); if this call site is itself a \
                         sanctioned exception (a new back-compat shim alongside the ones \
                         tm-core/tm-codeintel already have), add its file to \
                         DOT_TM_LITERAL_ALLOWLIST in crates/xtask/src/hygiene.rs with a doc \
                         comment explaining why, rather than working around this check",
                        path.display(),
                        i + 1
                    ));
                }
            }
        });
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
        let dir =
            std::env::temp_dir().join(format!("xtask-hygiene-test-{}-{}", std::process::id(), id));
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
    fn network_in_tests_ignores_comments_mentioning_a_key_or_url() {
        // A comment documenting that a var is deliberately unset is not a read of it, and a
        // comment naming a host is not a request to it.
        let root = temp_root();
        write(
            &root.join("crates/tm-provider/src/lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn documented() {\n        // ANTHROPIC_API_KEY is not set in the test environment\n        // see https://api.example.com for the wire shape\n        assert!(true);\n    }\n}\n",
        );
        assert!(check_network_in_tests(&root).is_empty());
    }

    #[test]
    fn network_in_tests_ignores_map_get_on_a_line_holding_a_url() {
        // `.get(` also matches HashMap/HeaderMap lookups; only a URL passed *to* the call is a
        // network client construction.
        let root = temp_root();
        write(
            &root.join("crates/tm-provider/src/lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn headers() {\n        assert_eq!(headers.get(\"HTTP-Referer\").unwrap(), \"https://example.com\");\n    }\n}\n",
        );
        assert!(check_network_in_tests(&root).is_empty());
    }

    #[test]
    fn network_in_tests_still_flags_a_url_passed_to_get() {
        // The tightened rule must keep its teeth: a real request is still a violation.
        let root = temp_root();
        write(
            &root.join("crates/tm-provider/src/lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn hits_network() {\n        client.get(\"https://api.example.com/v1\").send();\n    }\n}\n",
        );
        assert_eq!(check_network_in_tests(&root).len(), 1);
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

    #[test]
    fn dot_tm_literal_flags_a_new_join_outside_the_allowlist() {
        let root = temp_root();
        write(
            &root.join("crates/tm-agent/src/executor.rs"),
            "fn state_dir(root: &std::path::Path) -> std::path::PathBuf { root.join(\".tm\") }\n",
        );
        let violations = check_dot_tm_literals(&root);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("crates/tm-agent/src/executor.rs:1"));
    }

    #[test]
    fn dot_tm_literal_flags_a_bare_string_literal_not_just_a_join_call() {
        let root = temp_root();
        write(
            &root.join("crates/tm-agent/src/executor.rs"),
            "fn is_state_dir_name(name: &str) -> bool { name == \".tm\" }\n",
        );
        assert_eq!(check_dot_tm_literals(&root).len(), 1);
    }

    #[test]
    fn dot_tm_literal_allows_the_sanctioned_resolution_path() {
        let root = temp_root();
        write(
            &root.join("crates/tm-cli/src/project.rs"),
            "pub fn open(root: &std::path::Path) -> std::path::PathBuf { root.join(\".tm\") }\n",
        );
        assert!(
            check_dot_tm_literals(&root).is_empty(),
            "project.rs is the D-003 resolution path and must stay exempt"
        );
    }

    #[test]
    fn dot_tm_literal_allows_the_documented_backcompat_shims() {
        let root = temp_root();
        write(
            &root.join("crates/tm-core/src/store.rs"),
            "pub fn open(root: &std::path::Path) -> std::path::PathBuf { root.join(\".tm\") }\n",
        );
        assert!(check_dot_tm_literals(&root).is_empty());

        let root2 = temp_root();
        write(
            &root2.join("crates/tm-codeintel/src/api.rs"),
            "pub fn open(root: &std::path::Path) -> std::path::PathBuf { root.join(\".tm\") }\n",
        );
        assert!(check_dot_tm_literals(&root2).is_empty());

        let root3 = temp_root();
        write(
            &root3.join("crates/tm-codeintel/src/store.rs"),
            "pub fn open(root: &std::path::Path) -> std::path::PathBuf { root.join(\".tm\") }\n",
        );
        assert!(check_dot_tm_literals(&root3).is_empty());
    }

    #[test]
    fn dot_tm_literal_allows_the_unrelated_browser_cache_dir() {
        let root = temp_root();
        write(
            &root.join("crates/tm-browser/src/managed.rs"),
            "fn install_dir(home: &std::path::Path) -> std::path::PathBuf { home.join(\".tm\").join(\"browsers\") }\n",
        );
        assert!(check_dot_tm_literals(&root).is_empty());
    }

    #[test]
    fn dot_tm_literal_ignores_test_regions_in_non_allowlisted_files() {
        // Fixtures across the workspace legitimately open a `.tm` dir by hand to drive the
        // sanctioned shims under test — flagging every one would be noise, not triage, the same
        // reasoning every other check in this file already applies via `TestRegionTracker`.
        let root = temp_root();
        write(
            &root.join("crates/tm-agent/src/tools.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn opens_a_fixture_project() {\n        let db_path = dir.path().join(\".tm\").join(\"project.db\");\n        let _ = db_path;\n    }\n}\n",
        );
        assert!(check_dot_tm_literals(&root).is_empty());
    }

    #[test]
    fn dot_tm_literal_ignores_comments_and_longer_unrelated_literals() {
        // A comment mentioning the convention is not the convention being violated, and a
        // longer, unrelated literal like ".tmp" must not match merely because it starts the
        // same way `.tm` does.
        let root = temp_root();
        write(
            &root.join("crates/tm-agent/src/executor.rs"),
            "// state lives under \".tm\" in repo scope, see D-003\nfn scratch_ext() -> &'static str { \".tmp\" }\n",
        );
        assert!(check_dot_tm_literals(&root).is_empty());
    }
}
