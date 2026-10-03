//! Source-scanning hygiene checks for the Ticketmaster workspace.
//!
//! Each check walks the `crates/` tree (skipping paths that do not exist, since
//! sibling crates are still being written by other agents) and returns a list
//! of `file:line: message` violation strings. An empty vec means the check
//! passed.

use std::collections::HashSet;
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
    out.extend(check_decision_doc_references(root));
    out.extend(check_cli_help_jargon(root));
    out.extend(check_spec_refs_in_user_strings(root));
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
    // `index_root_and_state_dir` mirrors `Project::code_intel`'s own `.tm` resolution for a `tm
    // run --worktree` checkout's exec root, which is deliberately never itself doctored/
    // initialized as a `Project` (so it has no `Project::state_dir` to take): see that
    // function's own doc comment in dispatch.rs (`critic-worktree-exec-root-indexing`).
    "crates/tm-cli/src/dispatch.rs",
    // `$TM_HOME` (default `$HOME/.tm`) is the user-level home, not a project state dir: the
    // trust list lives there, same convention as `project::tm_home`.
    "crates/tm-types/src/trust.rs",
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

/// Every decision number that has a real `docs/decisions/D-NNN-*.md` file, as the zero-padded
/// 3-digit number alone (`"008"`, `"014"`) — matching just the `D-NNN` prefix, since the rest of
/// the filename (the slug) varies and this check only needs to know the *number* is real, not
/// that a caller spelled the slug correctly (see [`check_decision_doc_references`]'s "Scope and
/// why it's shaped this way" for the gap that leaves open).
fn existing_decision_numbers(decisions_dir: &Path) -> HashSet<String> {
    let mut numbers = HashSet::new();
    let Ok(entries) = fs::read_dir(decisions_dir) else {
        return numbers;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(rest) = name.strip_prefix("D-") {
            let digits: String = rest.chars().take(3).collect();
            if digits.len() == 3 && digits.bytes().all(|b| b.is_ascii_digit()) {
                numbers.insert(digits);
            }
        }
    }
    numbers
}

/// True for an ASCII letter, digit, or underscore — the "this position is part of a longer word"
/// test [`decision_id_tokens_in_line`] uses on both sides of a candidate `D-NNN` match.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Every `D-NNN` token in `line`: exactly `D-` followed by 3 ASCII digits, not part of a longer
/// digit run (so `D-000000000001`, a `DecisionId` fixture value elsewhere in this workspace, is
/// not misread as containing `D-000`) and not immediately preceded by a letter, digit, or
/// underscore (so a hypothetical `WEIRD-001` doesn't misread as `D-001`).
///
/// Hand-rolled rather than pulling in the `regex` crate, matching `drift.rs`'s own
/// `contains_standalone_number`. Byte-indexed scanning of a `&str` is safe here even though the
/// line may contain multi-byte UTF-8 (this repo's prose uses em-dashes freely): a UTF-8
/// continuation byte always has its high bit set, so it can never equal the ASCII byte values
/// (`b'D'`, `b'-'`, an ASCII digit) this loop compares against — meaning every position where a
/// comparison succeeds is necessarily already a real `char` boundary, and slicing 5 bytes forward
/// from `D` through 3 confirmed-ASCII digits always lands on another one.
fn decision_id_tokens_in_line(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 5 <= bytes.len() {
        if bytes[i] == b'D' && bytes[i + 1] == b'-' {
            let before_ok = i == 0 || !is_word_byte(bytes[i - 1]);
            let digits_ok = bytes[i + 2..i + 5].iter().all(u8::is_ascii_digit);
            if before_ok && digits_ok {
                let after_ok = i + 5 >= bytes.len() || !bytes[i + 5].is_ascii_digit();
                if after_ok {
                    out.push(line[i..i + 5].to_string());
                    i += 5;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// `SPEC.md`, `CLAUDE.md`, and every `.md` file under `docs/` except `docs/wiki/**` — generated
/// output (`tm wiki generate`, `SPEC.md` §26/B-14), not authored content, regenerated wholesale on
/// its own cadence from live project state rather than hand-maintained. A stale cross-reference
/// there is a generator-input problem (or an as-yet-unregenerated page), not this check's problem;
/// see `CLAUDE.md`'s "Navigation" section for the same distinction stated for humans.
fn doc_files_to_scan(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for top in ["SPEC.md", "CLAUDE.md"] {
        let p = root.join(top);
        if p.exists() {
            files.push(p);
        }
    }
    let docs_dir = root.join("docs");
    if docs_dir.exists() {
        for entry in WalkBuilder::new(&docs_dir).hidden(false).build() {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if relative.starts_with("docs/wiki/") {
                continue;
            }
            files.push(path.to_path_buf());
        }
    }
    files
}

/// Files where a `D-NNN`-shaped token with no matching `docs/decisions/D-NNN-*.md` file is a
/// known, deliberate illustrative example, not a broken cross-reference — keyed by
/// `(repo-root-relative path, exact token)`.
///
/// This codebase's own domain model has a *second*, unrelated `DecisionId` type
/// (`crates/tm-types/src/id.rs`'s `IdKind::Decision`: prefix `D-`, 3-digit padding) for a
/// `Store`-backed ticket-decision entity that has nothing to do with an architecture decision
/// record under `docs/decisions/` — the two collide in string shape, not by design (see
/// `docs/backlog.md`'s "Open decision: `docs/decisions/*.md` vs. `tm-wiki`'s `Decision` model",
/// which names this exact ambiguity as unresolved). [`TestRegionTracker`] already keeps every
/// quoted `DecisionId` fixture value out of this check's `.rs` scan — as of this check being
/// written, `DecisionId::new("D-001")`-shaped construction only appears in `#[cfg(test)]` code
/// across this workspace. The five entries below are the sole exceptions found outside test
/// regions, and all five illustrate the identical `derived_from` free-text shape: `SPEC.md` §9's
/// `derived_from = [..., "D-019", "D-027", ...]` worked example, echoed by §2.1's ID-format table
/// and §26.3's wiki-staleness example using the same two sample values; `docs/backlog.md` quoting
/// `SPEC.md`'s own `D-027` example back in its retelling; and `crates/tm-docs/src/provenance.rs`'s
/// module doc comment (production code, not a test) illustrating the same shape with the same two
/// ids. Confirmed by exhaustively scanning this workspace's current `D-\d{3}` occurrences before
/// picking this design: these five are the *only* ones outside test regions with no real matching
/// file.
const DECISION_ID_EXAMPLE_ALLOWLIST: &[(&str, &str)] = &[
    ("SPEC.md", "D-019"),
    ("SPEC.md", "D-027"),
    ("docs/backlog.md", "D-027"),
    ("crates/tm-docs/src/provenance.rs", "D-019"),
    ("crates/tm-docs/src/provenance.rs", "D-027"),
];

/// Appends one violation per `D-NNN` token in `text` that resolves to neither a real decision
/// number nor an allowlisted illustrative example.
fn record_decision_ref_violations(
    path: &Path,
    relative: &str,
    line_no: usize,
    text: &str,
    real_numbers: &HashSet<String>,
    violations: &mut Vec<String>,
) {
    for token in decision_id_tokens_in_line(text) {
        let number = &token[2..5];
        if real_numbers.contains(number) {
            continue;
        }
        if DECISION_ID_EXAMPLE_ALLOWLIST
            .iter()
            .any(|(f, t)| *f == relative && *t == token)
        {
            continue;
        }
        violations.push(format!(
            "{}:{}: reference to `{token}` but no docs/decisions/{token}-*.md file exists — \
             renamed/renumbered without updating this cross-reference, or a typo? If this is a \
             deliberate illustrative example rather than a document cross-reference, add it to \
             DECISION_ID_EXAMPLE_ALLOWLIST in crates/xtask/src/hygiene.rs with a doc comment \
             explaining why, rather than working around this check",
            path.display(),
            line_no
        ));
    }
}

/// (f) Every `D-NNN` reference — in Rust source (outside test regions) under `crates/*/src` and
/// `crates/*/tests`, and in `SPEC.md`/`CLAUDE.md`/`docs/**.md` (excluding the generated
/// `docs/wiki/`) — must have a matching `docs/decisions/D-NNN-*.md` file.
///
/// # Why this exists
///
/// A real, repeated incident this session: parallel background-agent tracks, each in its own git
/// worktree, independently picked "the next free `docs/decisions/D-NNN-*.md` number" by listing
/// their own worktree's directory — blind to a sibling track building concurrently in a different
/// worktree. Five real collisions resulted (D-004, D-008 ×2, D-010 ×2, D-012), each requiring a
/// human/orchestrator to renumber a file by hand and grep-and-fix every cross-reference across the
/// tree — and that manual grep-and-fix step itself missed five bare `D-008` mentions in source
/// comments (`crates/tm-cli/src/dispatch.rs`, `crates/tm-cli/src/otel.rs`), caught only later, by
/// luck, during an unrelated wiki-regeneration spot-check. This check makes that mechanical: a
/// reference to a decision number with no matching file on disk is flagged the same way any other
/// structural-correctness hygiene violation is, rather than relying on someone remembering to
/// grep for it by hand.
///
/// # Scope and why it's shaped this way
///
/// Matches just the `D-NNN` *number* against `docs/decisions/`'s real file list, not the full
/// slug — a bare `D-008`, or even a full `docs/decisions/D-008-wrong-slug.md` path naming the
/// wrong topic, is not flagged as long as *some* `D-008-*.md` file exists, since verifying the
/// slug text matches the author's intent would need understanding what the reference is about,
/// not just whether the number is real. That is a real, known gap: this check alone would not
/// catch `docs/decisions/D-008-opentelemetry-tracing.md` naming the wrong topic (the real
/// OpenTelemetry decision is `D-010`; `D-008` is `ticket-checkpoint-fork`) as long as `D-008`
/// resolves to *some* file. It is deliberately narrow and mechanical, matching the brief this
/// check was written against: "does this number that's referenced actually exist as a real
/// decision doc" — no more.
///
/// `.rs` scanning skips test regions via [`TestRegionTracker`], the same one-way latch every other
/// check in this file already uses. Closer to `check_dot_tm_literals`'s reasoning than the
/// determinism/mutation checks' (a test fixture constructing a domain value by hand is not the
/// thing being checked for, rather than "the test must issue the forbidden operation to prove it's
/// forbidden"): a `DecisionId::new("D-001")`-shaped fixture value in a test asserting the ID
/// parser's behavior is not a cross-reference to a decision *document*, and every current instance
/// of that shape in this workspace lives in test code — see [`DECISION_ID_EXAMPLE_ALLOWLIST`]'s
/// doc comment for the (small, enumerated) exceptions outside test regions. `SPEC.md`/
/// `CLAUDE.md`/`docs/**.md` have no test-region concept and are scanned in full.
pub fn check_decision_doc_references(root: &Path) -> Vec<String> {
    let real_numbers = existing_decision_numbers(&root.join("docs/decisions"));
    let mut violations = Vec::new();

    let crates_dir = root.join("crates");
    if let Ok(entries) = fs::read_dir(&crates_dir) {
        for entry in entries.flatten() {
            if entry.file_name() == "xtask" {
                continue;
            }
            let crate_dir = entry.path();
            for sub in ["src", "tests"] {
                let dir = crate_dir.join(sub);
                walk_rs_files(&dir, |path, contents| {
                    let relative = path
                        .strip_prefix(root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let mut tracker = TestRegionTracker::new(path);
                    for (i, line) in contents.lines().enumerate() {
                        tracker.observe(line);
                        if tracker.in_test() {
                            continue;
                        }
                        record_decision_ref_violations(
                            path,
                            &relative,
                            i + 1,
                            line,
                            &real_numbers,
                            &mut violations,
                        );
                    }
                });
            }
        }
    }

    for doc_path in doc_files_to_scan(root) {
        let Ok(contents) = fs::read_to_string(&doc_path) else {
            continue;
        };
        let relative = doc_path
            .strip_prefix(root)
            .unwrap_or(&doc_path)
            .to_string_lossy()
            .replace('\\', "/");
        for (i, line) in contents.lines().enumerate() {
            record_decision_ref_violations(
                &doc_path,
                &relative,
                i + 1,
                line,
                &real_numbers,
                &mut violations,
            );
        }
    }

    violations
}

/// True when `text` contains an identifier of the shape `tm_<lowercase-letters>::` (e.g.
/// `tm_core::`, `tm_mirror::`) — a crate-internal module path, meaningless to a CLI user reading
/// `--help`. Hand-rolled rather than pulling in `regex`. Unlike [`decision_id_tokens_in_line`]
/// this compares byte slices (`bytes[i..i + 3] == b"tm_"`), not `&str` slices: `args.rs`'s prose
/// help text is full of multi-byte characters (em dashes), and slicing a `&str` at a byte offset
/// that isn't a char boundary panics, whereas slicing the underlying `&[u8]` never can — it's
/// just bytes, so there is no boundary to violate.
///
/// Deliberately narrow to match this check's own spec (`tm_[a-z]+::`): a multi-segment identifier
/// like `tm_agent_loop::` is not matched, since the underscore after `agent` breaks the run of
/// `[a-z]` before the `::` this looks for immediately afterward. That mirrors every occurrence
/// actually found in `args.rs` (`tm_core::`, `tm_mirror::`) rather than over-generalizing.
fn contains_tm_module_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i..].starts_with(b"tm_") {
            let mut j = i + 3;
            while j < bytes.len() && bytes[j].is_ascii_lowercase() {
                j += 1;
            }
            if j > i + 3 && bytes[j..].starts_with(b"::") {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// (g) `crates/tm-cli/src/args.rs`'s `///` doc comments (clap's source for `--help` text) must
/// read like product copy, not implementation notes: no `D-NNN` decision reference, no
/// `crates/`-rooted path, no `docs/decisions` path, and no `tm_<mod>::` module path. A CLI user
/// asking `tm project --help` has no reason to know this project keeps decision docs, how its
/// source tree is laid out, or what its internal crates are named — that context belongs in a
/// plain `//` comment for maintainers instead, right above or in place of the `///` line.
///
/// Scoped to this one file, deliberately: every other crate's `///` doc comments feed `rustdoc`,
/// not a terminal a human reads live, so the same jargon there is merely internal documentation,
/// not user-facing copy. Widening this to every crate would flag legitimate rustdoc cross-references
/// (`[crate::project::resolve_scope]`-shaped links are exactly the right tool in a doc comment
/// that isn't clap-derived help text) as if they were the same mistake.
pub fn check_cli_help_jargon(root: &Path) -> Vec<String> {
    let path = root.join("crates/tm-cli/src/args.rs");
    let Ok(contents) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut violations = Vec::new();
    let mut tracker = TestRegionTracker::new(&path);
    for (i, line) in contents.lines().enumerate() {
        tracker.observe(line);
        // A `///` doc comment inside `#[cfg(test)] mod tests` documents a test fixture, not
        // something clap ever surfaces to a real `tm --help` caller.
        if tracker.in_test() {
            continue;
        }
        let trimmed = line.trim_start();
        if !trimmed.starts_with("///") {
            continue;
        }
        let text = &trimmed[3..];
        let mut reasons = Vec::new();
        if !decision_id_tokens_in_line(text).is_empty() {
            reasons.push("a `D-NNN` decision reference");
        }
        if text.contains("crates/") {
            reasons.push("a `crates/`-rooted path");
        }
        if text.contains("docs/decisions") {
            reasons.push("a `docs/decisions` path");
        }
        if contains_tm_module_path(text) {
            reasons.push("a `tm_*::` module path");
        }
        if text.contains("SPEC.md") && text.contains('§') {
            reasons.push("a SPEC section reference");
        }
        if !reasons.is_empty() {
            violations.push(format!(
                "{}:{}: user-facing help text contains {} — rewrite this `///` line in plain \
                 language a CLI user would understand, and move any implementation rationale to \
                 a plain `//` comment instead",
                path.display(),
                i + 1,
                reasons.join(" and ")
            ));
        }
    }
    violations
}

/// User-facing strings and clap help must not expose internal SPEC section references.
pub fn check_spec_refs_in_user_strings(root: &Path) -> Vec<String> {
    let mut violations = Vec::new();
    for src in crate_src_dirs(root) {
        walk_rs_files(&src, |path, contents| {
            let mut tracker = TestRegionTracker::new(path);
            for (i, line) in contents.lines().enumerate() {
                tracker.observe(line);
                if tracker.in_test() || path.ends_with("crates/tm-cli/src/args.rs") {
                    continue;
                }
                let code = strip_line_comment(line);
                if code.contains("SPEC.md") && code.contains('§') && code.contains('"') {
                    violations.push(format!(
                        "{}:{}: user-facing string contains a SPEC section reference — rewrite in plain language",
                        path.display(), i + 1
                    ));
                }
            }
        });
    }
    violations.extend(check_cli_help_jargon_spec_refs(root));
    violations
}

fn check_cli_help_jargon_spec_refs(root: &Path) -> Vec<String> {
    // Reuse the established args.rs-only doc-comment scope; the combined check above covers
    // string literals in all other production source files.
    let path = root.join("crates/tm-cli/src/args.rs");
    let Ok(contents) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    contents.lines().enumerate().filter_map(|(i, line)| {
        let trimmed = line.trim_start();
        (trimmed.starts_with("///") && trimmed.contains("SPEC.md") && trimmed.contains('§'))
            .then(|| format!("{}:{}: user-facing help text contains a SPEC section reference — rewrite in plain language", path.display(), i + 1))
    }).collect()
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

    #[test]
    fn decision_doc_references_flags_the_real_d008_incident_shape() {
        // Reproduces the exact real failure this check exists for: a source comment references a
        // decision number that got renumbered elsewhere without this cross-reference following
        // it. `docs/decisions/` here only has D-009 (the renamed target), not D-008.
        let root = temp_root();
        write(
            &root.join("docs/decisions/D-009-oversight-policy-wiring.md"),
            "# D-009 — oversight.toml\n",
        );
        write(
            &root.join("crates/tm-cli/src/dispatch.rs"),
            "/// The human-authored approval policy, per docs/decisions/D-008-oversight-policy-wiring.md.\nfn f() {}\n",
        );
        let violations = check_decision_doc_references(&root);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("crates/tm-cli/src/dispatch.rs:1"));
        assert!(violations[0].contains("D-008"));
    }

    #[test]
    fn decision_doc_references_allows_a_reference_with_a_matching_file() {
        let root = temp_root();
        write(
            &root.join("docs/decisions/D-003-project-scope.md"),
            "# D-003 — Project scope\n",
        );
        write(
            &root.join("crates/tm-cli/src/project.rs"),
            "// see docs/decisions/D-003-project-scope.md for the resolution order\nfn f() {}\n",
        );
        assert!(check_decision_doc_references(&root).is_empty());
    }

    #[test]
    fn decision_doc_references_ignores_decisionid_fixtures_in_test_regions() {
        // `DecisionId::new("D-999")`-shaped fixtures in tests are exercising the *domain* id
        // parser (`crates/tm-types/src/id.rs`'s `IdKind::Decision`), not referencing a decision
        // *document* — this must not be flagged even though no D-999 decision doc exists.
        let root = temp_root();
        write(
            &root.join("crates/tm-types/src/id.rs"),
            "pub struct DecisionId;\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn parses() {\n        assert_eq!(DecisionId::new(\"D-999\").unwrap().number(), Some(999));\n    }\n}\n",
        );
        assert!(check_decision_doc_references(&root).is_empty());
    }

    #[test]
    fn decision_doc_references_flags_a_stale_reference_in_markdown_docs() {
        let root = temp_root();
        write(
            &root.join("CLAUDE.md"),
            "See docs/decisions/D-999-nonexistent.md for details.\n",
        );
        let violations = check_decision_doc_references(&root);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("CLAUDE.md:1"));
    }

    #[test]
    fn decision_doc_references_allows_the_documented_illustrative_examples() {
        // SPEC.md's own ID-format table and `derived_from` worked example use D-019/D-027 as
        // sample values for the unrelated `DecisionId` domain type, not as cross-references —
        // see DECISION_ID_EXAMPLE_ALLOWLIST's doc comment.
        let root = temp_root();
        write(
            &root.join("SPEC.md"),
            "| Decision | `D-<n>` | `D-019` |\n\nderived_from = [\"crates/tm-core/src/**\", \"D-019\", \"D-027\"]\n",
        );
        assert!(check_decision_doc_references(&root).is_empty());
    }

    #[test]
    fn decision_doc_references_skips_generated_wiki_docs() {
        let root = temp_root();
        write(
            &root.join("docs/wiki/decisions.md"),
            "Stale reference to D-999 here, but this page is generated output.\n",
        );
        assert!(check_decision_doc_references(&root).is_empty());
    }

    #[test]
    fn decision_doc_references_scans_crate_tests_dirs_too() {
        let root = temp_root();
        write(
            &root.join("crates/tm-cli/tests/integration.rs"),
            "// exercises the flow docs/decisions/D-999-nonexistent.md describes\nfn f() {}\n",
        );
        let violations = check_decision_doc_references(&root);
        assert_eq!(violations.len(), 1, "{violations:?}");
    }

    #[test]
    fn decision_doc_references_allows_its_own_self_titled_file() {
        // A decision doc's own title line (`# D-005 — ...`) is itself a `D-NNN` match, but it
        // trivially resolves since the file it's found in is the match — no special-casing
        // needed, it falls out of `existing_decision_numbers` reading the same directory.
        let root = temp_root();
        write(
            &root.join("docs/decisions/D-005-devpass-default-provider.md"),
            "# D-005 — DevPass as the default provider\n",
        );
        assert!(check_decision_doc_references(&root).is_empty());
    }

    #[test]
    fn decision_id_tokens_in_line_respects_digit_run_and_word_boundaries() {
        // "D-000000000001" is a real `DecisionId` fixture shape elsewhere in this workspace
        // (`crates/tm-genesis/src/stages.rs`) and must not be misread as containing `D-000`; a
        // hypothetical "WEIRD-001" must not be misread as `D-001` either.
        assert!(decision_id_tokens_in_line("D-000000000001").is_empty());
        assert!(decision_id_tokens_in_line("see WEIRD-001 over there").is_empty());
        assert_eq!(
            decision_id_tokens_in_line("(D-014) and D-008."),
            vec!["D-014".to_string(), "D-008".to_string()]
        );
    }

    #[test]
    fn cli_help_jargon_flags_decision_refs_crate_paths_and_module_paths() {
        let root = temp_root();
        write(
            &root.join("crates/tm-cli/src/args.rs"),
            "/// Force a thing (D-002, \"Terminal surface quality bar\").\n\
             /// See docs/decisions/D-012-run-worktree-isolation.md for details.\n\
             /// The subset of `tm_core::ticket::TicketState` a user can filter on.\n\
             /// Lives under crates/tm-core/src/foo.rs.\n\
             pub struct Foo;\n",
        );
        let violations = check_cli_help_jargon(&root);
        assert_eq!(violations.len(), 4, "{violations:?}");
        assert!(violations[0].contains(":1:") && violations[0].contains("D-NNN"));
        assert!(violations[1].contains(":2:") && violations[1].contains("docs/decisions"));
        assert!(violations[2].contains(":3:") && violations[2].contains("tm_*::"));
        assert!(violations[3].contains(":4:") && violations[3].contains("`crates/`-rooted"));
    }

    #[test]
    fn cli_help_jargon_allows_plain_help_text() {
        let root = temp_root();
        write(
            &root.join("crates/tm-cli/src/args.rs"),
            "/// Open the tickets view (or print tickets with --json).\n\
             // Rationale: D-019 named this the claude-agents-parity view; not user-facing.\n\
             pub struct Foo;\n",
        );
        assert!(check_cli_help_jargon(&root).is_empty());
    }

    #[test]
    fn cli_help_jargon_only_scans_args_rs() {
        let root = temp_root();
        write(
            &root.join("crates/tm-cli/src/other.rs"),
            "/// See docs/decisions/D-012-run-worktree-isolation.md for details.\n",
        );
        assert!(check_cli_help_jargon(&root).is_empty());
    }

    #[test]
    fn cli_help_jargon_ignores_doc_comments_inside_the_test_module() {
        // A `///` above a helper inside `#[cfg(test)] mod tests` never reaches a real `tm --help`
        // caller — it documents a fixture, not a clap item.
        let root = temp_root();
        write(
            &root.join("crates/tm-cli/src/args.rs"),
            "/// Open the tickets view (or print tickets with --json).\n\
             pub struct Foo;\n\n\
             #[cfg(test)]\n\
             mod tests {\n\
             \x20\x20\x20\x20/// See docs/decisions/D-012-run-worktree-isolation.md (D-012).\n\
             \x20\x20\x20\x20fn helper() {}\n\
             }\n",
        );
        assert!(check_cli_help_jargon(&root).is_empty());
    }

    #[test]
    fn contains_tm_module_path_never_panics_on_multibyte_help_text() {
        // `args.rs`'s real help text is full of em dashes; slicing a `&str` at a non-char-boundary
        // byte offset panics, so this must compare `&[u8]`, not `&str`, to stay panic-free.
        assert!(contains_tm_module_path(
            "— matching `tm_mirror::CredentialEnv`'s contract —"
        ));
        assert!(!contains_tm_module_path("— nothing to see here —"));
    }

    #[test]
    fn contains_tm_module_path_matches_single_segment_only() {
        assert!(contains_tm_module_path(
            "see `tm_core::ticket::TicketState`"
        ));
        assert!(contains_tm_module_path(
            "matching `tm_mirror::CredentialEnv`"
        ));
        assert!(!contains_tm_module_path("no module path here"));
        // Deliberately narrow: an underscore between segments breaks the match (see
        // `contains_tm_module_path`'s own doc comment for why that's the intended scope).
        assert!(!contains_tm_module_path("tm_agent_loop::Foo"));
    }

    #[test]
    fn spec_refs_in_user_strings_flags_literals_and_args_help_but_allows_plain_text() {
        let root = temp_root();
        write(
            &root.join("crates/tm-cli/src/drive.rs"),
            "fn message() { let _ = \"see SPEC.md §1\"; }\n",
        );
        write(
            &root.join("crates/tm-cli/src/args.rs"),
            "/// See SPEC.md §1 for details.\nfn help() {}\n",
        );
        let violations = check_spec_refs_in_user_strings(&root);
        assert_eq!(violations.len(), 2, "{violations:?}");

        let clean = temp_root();
        write(
            &clean.join("crates/tm-cli/src/drive.rs"),
            "fn message() { let _ = \"See the user guide for details.\"; }\n",
        );
        assert!(check_spec_refs_in_user_strings(&clean).is_empty());
    }
}
