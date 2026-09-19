//! `xtask check-drift` — a narrow, advisory post-merge triage check.
//!
//! # What this catches, and honestly, what it does not
//!
//! This exists because of a real incident: commit `dba4e7f` ("fix(tm-context): update
//! section-count arithmetic for 10 sections") fixed two tests in
//! `crates/tm-context/src/tokens.rs` that silently assumed `SectionKind` still had 9 variants
//! after a merge (`2f86cfe`, B-10 + B-14) added a 10th (`Wiki`). Neither branch's diff touched
//! either test line, so git's own conflict detection had nothing to flag — the enum merged
//! cleanly, the tests kept compiling, and both were simply wrong until someone ran the full
//! suite and triaged two failures by hand.
//!
//! This check does not preempt that failure — the tests still fail in `cargo test`, exactly as
//! they should. What it does is turn "two confusing test failures somewhere in a 550-line file"
//! into a named cause in one command, run right after a merge: *this enum's variant count
//! changed in this file, and here is the one line shape most likely to still be assuming the old
//! count*.
//!
//! The one shape it catches precisely, with a low false-positive rate: an `assert`-style line
//! that is byte-for-byte unchanged between `base` and the working tree, names the drifted enum,
//! calls `.len()`, and compares against a literal equal to the *old* variant count — i.e. a
//! direct cardinality tripwire like `assert_eq!(SectionKind::PRIORITY_ORDER.len(), 9);`.
//!
//! What it deliberately does **not** attempt: the *other* test broken by the same real incident
//! (`budget_ledger_spend_order_preserves_priority`) hardcoded `88`, which is
//! `floor(800 / 9)` — a value three algebraic steps removed from the enum's cardinality, with no
//! `.len()` call and no enum name anywhere on its line. Catching that mechanically would require
//! evaluating the arithmetic relationship between a numeric literal and a count defined
//! elsewhere in the file, i.e. real dataflow/symbolic analysis, not a grep-shaped structural
//! check. Widening the pattern match to "any unchanged numeric literal in a drifted file" was
//! tried and rejected: on the real file that flags on the order of thirty lines, which is not
//! triage, it's a second thing to review. The honest fix for that broader class is a *convention*
//! (see `CLAUDE.md`): a test whose expected value is derived from a collection's cardinality
//! should compute the divisor from `X::PRIORITY_ORDER.len()` at test time instead of hardcoding
//! the quotient, so the value tracks the enum instead of needing to be remembered.
//!
//! This is intentionally **not** wired into `verify` or `hygiene`: it needs a `--base` (the
//! commit each parallel track branched from, or the pre-merge SHA) to mean anything, which only
//! the person doing the merge/triage knows. Run it by hand: `cargo run -p xtask -- check-drift
//! --base <sha>`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Every `.rs` path (relative to `root`) that differs between `base` and the current working
/// tree. Deliberately `git diff --name-only <base>` against the working tree, not `HEAD` — a
/// merge's own uncommitted-but-already-fixed-up state should still count as "current" for triage
/// purposes.
pub fn changed_rs_files(root: &Path, base: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["diff", "--name-only", base, "--", "*.rs"])
        .current_dir(root)
        .output()
        .context("failed to spawn `git diff --name-only`")?;
    if !output.status.success() {
        bail!(
            "git diff --name-only {base} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// A file's contents at `rev`, or `None` if it did not exist there (a newly added file has no
/// "before" state to drift-check against).
pub fn file_at_rev(root: &Path, rev: &str, relpath: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["show", &format!("{rev}:{relpath}")])
        .current_dir(root)
        .output()
        .context("failed to spawn `git show`")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

/// Counts each top-level `enum Name { ... }` definition's variants by tracking brace depth
/// line-by-line. Deliberately not a real parser: like `hygiene.rs`'s `TestRegionTracker`, this
/// trades perfect correctness (it can be fooled by, say, a doc comment containing the literal
/// text "enum Foo {") for something simple enough to trust. Enums are not nested in this
/// codebase, so tracking a single "currently open enum body" is sufficient.
pub fn extract_enum_variant_counts(src: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    let mut depth: i32 = 0;
    let mut enum_body: Option<(String, i32)> = None;
    let mut pending_name: Option<String> = None;

    for raw_line in src.lines() {
        let line = raw_line.trim();
        let body_open_before_line = enum_body.is_some();

        if enum_body.is_none() {
            if let Some(idx) = line.find("enum ") {
                let after = &line[idx + "enum ".len()..];
                let name: String = after
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    pending_name = Some(name);
                }
            }
        }

        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    if enum_body.is_none() {
                        if let Some(name) = pending_name.take() {
                            enum_body = Some((name, depth));
                        }
                    }
                }
                '}' => {
                    if let Some((name, body_depth)) = &enum_body {
                        if depth == *body_depth {
                            counts.entry(name.clone()).or_insert(0);
                            enum_body = None;
                        }
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }

        // Only a line that sat inside an *already open* enum body both before and after this
        // line's own braces (i.e. neither the `enum Name {` opener nor the closing `}`) is a
        // variant declaration.
        if body_open_before_line {
            if let Some((name, body_depth)) = &enum_body {
                if depth == *body_depth
                    && !line.is_empty()
                    && !line.starts_with("//")
                    && !line.starts_with("#[")
                {
                    *counts.entry(name.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    counts
}

/// True when `line` contains the decimal digits of `n` as a standalone token — not part of a
/// longer number, so an old count of `9` does not false-match inside `900` or `1900`.
fn contains_standalone_number(line: &str, n: usize) -> bool {
    let needle = n.to_string();
    let bytes = line.as_bytes();
    let nlen = needle.len();
    let mut start = 0;
    while let Some(pos) = line[start..].find(&needle) {
        let idx = start + pos;
        let before_ok = idx == 0 || !bytes[idx - 1].is_ascii_digit();
        let after_idx = idx + nlen;
        let after_ok = after_idx >= bytes.len() || !bytes[after_idx].is_ascii_digit();
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

/// The core heuristic: `(1-based line number in `new`, enum name, old variant count, line text)`
/// for every unchanged line in a file whose enum drifted that still reads as a direct cardinality
/// check against the old count (names the enum, calls `.len()`, compares to the old count).
pub fn find_stale_cardinality_asserts(old: &str, new: &str) -> Vec<(usize, String, usize, String)> {
    let old_counts = extract_enum_variant_counts(old);
    let new_counts = extract_enum_variant_counts(new);

    let changed: Vec<(String, usize)> = old_counts
        .iter()
        .filter_map(|(name, &old_count)| {
            new_counts
                .get(name)
                .filter(|&&new_count| new_count != old_count)
                .map(|_| (name.clone(), old_count))
        })
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }

    let old_lines: HashSet<&str> = old.lines().collect();
    let mut out = Vec::new();
    for (i, line) in new.lines().enumerate() {
        if !old_lines.contains(line) {
            continue; // this line was touched by whatever changed the enum; not the silent case
        }
        for (name, old_count) in &changed {
            if line.contains(&format!("{name}::"))
                && line.contains(".len()")
                && contains_standalone_number(line, *old_count)
            {
                out.push((i + 1, name.clone(), *old_count, line.trim().to_string()));
            }
        }
    }
    out
}

/// Runs the check against every changed `.rs` file between `base` and the working tree, and
/// returns human-readable warning lines (empty means clean).
pub fn run(root: &Path, base: &str) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    for relpath in changed_rs_files(root, base)? {
        let Some(old) = file_at_rev(root, base, &relpath)? else {
            continue; // new file: nothing to have drifted against
        };
        let Ok(new) = fs::read_to_string(root.join(&relpath)) else {
            continue; // deleted in the working tree
        };
        for (line_no, enum_name, old_count, line_text) in find_stale_cardinality_asserts(&old, &new)
        {
            warnings.push(format!(
                "{relpath}:{line_no}: possible stale cardinality assertion -- `{enum_name}`'s \
                 variant count changed elsewhere in this file since {base} (was {old_count}), but \
                 this unchanged line still asserts {old_count}: {line_text}"
            ));
        }
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_an_unchanged_len_assert_against_a_drifted_enum() {
        let old = "pub enum Fruit {\n    Apple,\n    Banana,\n    Cherry,\n}\n\n\
                    #[cfg(test)]\nmod tests {\n    #[test]\n    fn all_covers_every_kind() {\n        \
                    assert_eq!(Fruit::ALL.len(), 3);\n    }\n}\n";
        let new = "pub enum Fruit {\n    Apple,\n    Banana,\n    Cherry,\n    Date,\n}\n\n\
                    #[cfg(test)]\nmod tests {\n    #[test]\n    fn all_covers_every_kind() {\n        \
                    assert_eq!(Fruit::ALL.len(), 3);\n    }\n}\n";

        let hits = find_stale_cardinality_asserts(old, new);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].1, "Fruit");
        assert_eq!(hits[0].2, 3);
    }

    #[test]
    fn does_not_flag_when_the_enum_is_unchanged() {
        let src = "pub enum Fruit {\n    Apple,\n    Banana,\n}\n\n\
                   assert_eq!(Fruit::ALL.len(), 2);\n";
        assert!(find_stale_cardinality_asserts(src, src).is_empty());
    }

    #[test]
    fn does_not_flag_an_assertion_that_was_itself_updated() {
        let old =
            "pub enum Fruit {\n    Apple,\n    Banana,\n}\n\nassert_eq!(Fruit::ALL.len(), 2);\n";
        let new = "pub enum Fruit {\n    Apple,\n    Banana,\n    Cherry,\n}\n\nassert_eq!(Fruit::ALL.len(), 3);\n";
        assert!(find_stale_cardinality_asserts(old, new).is_empty());
    }

    #[test]
    fn extract_enum_variant_counts_ignores_doc_comments_and_attributes() {
        let src = "/// Doc comment mentioning Apple.\n#[derive(Debug)]\npub enum Fruit {\n    \
                    /// A doc comment on a variant.\n    #[deprecated]\n    Apple,\n    Banana,\n}\n";
        let counts = extract_enum_variant_counts(src);
        assert_eq!(counts.get("Fruit"), Some(&2));
    }

    /// Reproduces `crates/tm-context/src/tokens.rs` around the real incident: commit `cb6ef03`
    /// (9 `SectionKind` variants) versus `2f86cfe` (10, after the B-10/B-14 merge added `Wiki`
    /// without either branch's diff touching these test lines) -- the state commit `dba4e7f`
    /// later fixed by hand. Text below is trimmed but otherwise verbatim from those two commits.
    #[test]
    fn real_incident_flags_the_len_assert_but_honestly_not_the_derived_floor_division() {
        let old = r#"
pub enum SectionKind {
    Objective,
    Budget,
    Decisions,
    Dependencies,
    Retrieval,
    SymbolOutlines,
    GitHistory,
    PriorFailures,
    Conventions,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_order_covers_every_kind() {
        assert_eq!(SectionKind::PRIORITY_ORDER.len(), 9);
    }

    #[test]
    fn budget_ledger_spend_order_preserves_priority() {
        let budget = TokenBudget::even(800);
        let ledger = BudgetLedger::new(&budget);

        // Each section gets floor(800 / 9) = 88 tokens.
        for (i, &kind) in SectionKind::PRIORITY_ORDER.iter().enumerate() {
            let acct = &ledger.accounts[i];
            assert_eq!(acct.kind, kind);
            assert_eq!(acct.allotted, 88);
        }
    }
}
"#;
        let new = r#"
pub enum SectionKind {
    Objective,
    Budget,
    Decisions,
    Dependencies,
    Retrieval,
    Wiki,
    SymbolOutlines,
    GitHistory,
    PriorFailures,
    Conventions,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_order_covers_every_kind() {
        assert_eq!(SectionKind::PRIORITY_ORDER.len(), 9);
    }

    #[test]
    fn budget_ledger_spend_order_preserves_priority() {
        let budget = TokenBudget::even(900);
        let ledger = BudgetLedger::new(&budget);

        // Each section gets floor(800 / 9) = 88 tokens.
        for (i, &kind) in SectionKind::PRIORITY_ORDER.iter().enumerate() {
            let acct = &ledger.accounts[i];
            assert_eq!(acct.kind, kind);
            assert_eq!(acct.allotted, 88);
        }
    }
}
"#;

        let hits = find_stale_cardinality_asserts(old, new);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].1, "SectionKind");
        assert_eq!(hits[0].2, 9);
        assert!(hits[0].3.contains("PRIORITY_ORDER.len()"));

        // Honest limitation, locked in here rather than left as an unstated gap: the *other*
        // test broken by this same real incident hardcoded `88` (`floor(800 / 9)`), a value
        // three algebraic steps from the enum's cardinality with no `.len()` call and no enum
        // name on its line. This check does not and should not claim to catch that -- doing so
        // generically would require real dataflow analysis, not a grep-shaped structural check.
        assert!(
            !hits.iter().any(|h| h.3.contains("88")),
            "must not overclaim: the derived-arithmetic half of the incident is out of scope: {hits:?}"
        );
    }

    #[test]
    fn missing_base_revision_is_a_clean_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!(
            "xtask-drift-test-{}-{}",
            std::process::id(),
            line!()
        ));
        // Not a git repo at all: `changed_rs_files` must return an error, not panic.
        let _ = fs::create_dir_all(&dir);
        assert!(changed_rs_files(&dir, "nonexistent-ref").is_err());
    }
}
