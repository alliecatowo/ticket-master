//! Path patterns and the containment tests the authority algebra is built on.
//!
//! Two questions matter here, and they fail in opposite directions on purpose
//! (`SPEC.md` §2.4, §4.5):
//!
//! * [`PatternSet::is_subset_of`] backs authority attenuation. It may answer `false` for a set
//!   that is in fact contained — costing a delegation that would have been safe — but it must
//!   never answer `true` for one that is not.
//! * [`PatternSet::overlaps`] backs resource-conflict detection. It may report a conflict that
//!   could not actually occur — costing some parallelism — but it must never miss a real one.

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A single glob over repository-relative paths.
///
/// `*` matches within one path segment, `**` matches any number of segments, `?` matches one
/// character.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PathPattern(String);

impl PathPattern {
    /// Build a pattern, validating that it compiles as a glob.
    pub fn new(s: impl Into<String>) -> Result<Self, crate::error::TmError> {
        let s = normalize(&s.into());
        compile_glob(&s)
            .map_err(|e| crate::error::TmError::parse(format!("bad path pattern: {e}")))?;
        Ok(PathPattern(s))
    }

    /// The pattern source.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Segments, split on `/`.
    fn segments(&self) -> Vec<&str> {
        self.0.split('/').filter(|s| !s.is_empty()).collect()
    }

    /// The leading run of characters containing no wildcard.
    fn literal_prefix(&self) -> &str {
        let end = self.0.find(['*', '?', '[']).unwrap_or(self.0.len());
        &self.0[..end]
    }

    /// True when every path this pattern matches is also matched by `other`.
    ///
    /// Conservative: a `false` result means "not provably contained".
    pub fn implied_by(&self, other: &PathPattern) -> bool {
        segments_imply(&other.segments(), &self.segments())
    }

    /// True unless the two patterns are provably disjoint.
    pub fn may_overlap(&self, other: &PathPattern) -> bool {
        let (a, b) = (self.literal_prefix(), other.literal_prefix());
        let shorter = a.len().min(b.len());
        a[..shorter] == b[..shorter]
    }
}

impl fmt::Display for PathPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for PathPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PathPattern({})", self.0)
    }
}

/// Compile one glob with `*` confined to a single path segment.
///
/// `globset`'s default lets `*` cross `/`, which would quietly make `tests/*.rs` cover
/// `tests/deep/x.rs` — far too generous for a write scope.
fn compile_glob(pattern: &str) -> Result<Glob, globset::Error> {
    GlobBuilder::new(pattern).literal_separator(true).build()
}

fn normalize(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("./").unwrap_or(s);
    let s = s.strip_prefix('/').unwrap_or(s);
    s.to_string()
}

/// `q` implies `p`: every path matched by `p` is matched by `q`.
fn segments_imply(q: &[&str], p: &[&str]) -> bool {
    if p.is_empty() {
        return q.iter().all(|s| *s == "**");
    }
    let Some((qh, qt)) = q.split_first() else {
        return false;
    };
    if *qh == "**" {
        // Absorb zero segments, or absorb one segment of `p` and try again.
        return segments_imply(qt, p) || segments_imply(q, &p[1..]);
    }
    let (ph, pt) = p.split_first().expect("p is non-empty");
    if *ph == "**" {
        // A bounded pattern cannot cover an unbounded one.
        return false;
    }
    segment_implies(qh.as_bytes(), ph.as_bytes()) && segments_imply(qt, pt)
}

/// Character-level implication inside one path segment.
fn segment_implies(q: &[u8], p: &[u8]) -> bool {
    match q.split_first() {
        None => p.is_empty(),
        Some((b'*', qt)) => {
            segment_implies(qt, p) || (!p.is_empty() && segment_implies(q, &p[1..]))
        }
        Some((b'?', qt)) => match p.split_first() {
            Some((pc, pt)) if *pc != b'*' => segment_implies(qt, pt),
            _ => false,
        },
        Some((qc, qt)) => match p.split_first() {
            Some((pc, pt)) if pc == qc => segment_implies(qt, pt),
            _ => false,
        },
    }
}

/// A set of path patterns, matched as a union.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PatternSet {
    patterns: Vec<PathPattern>,
}

impl PatternSet {
    /// The empty set: matches nothing.
    pub fn empty() -> Self {
        PatternSet::default()
    }

    /// The universal set: matches every path.
    pub fn all() -> Self {
        PatternSet { patterns: vec![PathPattern("**".to_string())] }
    }

    /// Build from pattern sources.
    pub fn parse<I, S>(items: I) -> Result<Self, crate::error::TmError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut patterns = Vec::new();
        for it in items {
            patterns.push(PathPattern::new(it)?);
        }
        patterns.sort();
        patterns.dedup();
        Ok(PatternSet { patterns })
    }

    /// The patterns in this set.
    pub fn patterns(&self) -> &[PathPattern] {
        &self.patterns
    }

    /// True when the set contains no patterns.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// True when some pattern in the set matches `path`.
    pub fn matches(&self, path: impl AsRef<str>) -> bool {
        let path = normalize(path.as_ref());
        match self.compiled() {
            Some(set) => set.is_match(&path),
            None => false,
        }
    }

    /// True when some pattern matches `text` treated as free text rather than a path.
    ///
    /// Command lines are not paths: `*rm -rf*` has to be able to span the `/` in `rm -rf /`,
    /// so this matcher lets `*` cross separators where [`PatternSet::matches`] does not.
    pub fn matches_text(&self, text: impl AsRef<str>) -> bool {
        let text = text.as_ref();
        self.patterns.iter().any(|p| {
            Glob::new(p.as_str()).map(|g| g.compile_matcher().is_match(text)).unwrap_or(false)
        })
    }

    fn compiled(&self) -> Option<GlobSet> {
        if self.patterns.is_empty() {
            return None;
        }
        let mut b = GlobSetBuilder::new();
        for p in &self.patterns {
            b.add(compile_glob(p.as_str()).ok()?);
            // `src/**` should also cover the directory itself when it is named directly.
            if let Some(stripped) = p.as_str().strip_suffix("/**") {
                if let Ok(g) = compile_glob(stripped) {
                    b.add(g);
                }
            }
        }
        b.build().ok()
    }

    /// True when every path matched by `self` is provably matched by `other`.
    pub fn is_subset_of(&self, other: &PatternSet) -> bool {
        self.patterns.iter().all(|p| other.patterns.iter().any(|q| p.implied_by(q)))
    }

    /// The union of two sets.
    pub fn union(&self, other: &PatternSet) -> PatternSet {
        let mut patterns = self.patterns.clone();
        patterns.extend(other.patterns.iter().cloned());
        patterns.sort();
        patterns.dedup();
        PatternSet { patterns }
    }

    /// A conservative intersection: the patterns of `self` provably contained in `other`, plus
    /// the patterns of `other` provably contained in `self`.
    pub fn intersect(&self, other: &PatternSet) -> PatternSet {
        let mut patterns: Vec<PathPattern> = self
            .patterns
            .iter()
            .filter(|p| other.patterns.iter().any(|q| p.implied_by(q)))
            .cloned()
            .collect();
        patterns.extend(
            other
                .patterns
                .iter()
                .filter(|p| self.patterns.iter().any(|q| p.implied_by(q)))
                .cloned(),
        );
        patterns.sort();
        patterns.dedup();
        PatternSet { patterns }
    }

    /// True unless the two sets are provably disjoint.
    pub fn overlaps(&self, other: &PatternSet) -> bool {
        self.patterns.iter().any(|a| other.patterns.iter().any(|b| a.may_overlap(b)))
    }
}

impl fmt::Debug for PatternSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.patterns.iter().map(|p| p.as_str())).finish()
    }
}

impl FromIterator<PathPattern> for PatternSet {
    fn from_iter<I: IntoIterator<Item = PathPattern>>(iter: I) -> Self {
        let mut patterns: Vec<_> = iter.into_iter().collect();
        patterns.sort();
        patterns.dedup();
        PatternSet { patterns }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> PatternSet {
        PatternSet::parse(items.iter().copied()).unwrap()
    }

    #[test]
    fn matching_follows_glob_semantics() {
        let s = set(&["src/auth/**", "tests/auth/*.rs"]);
        assert!(s.matches("src/auth/mod.rs"));
        assert!(s.matches("src/auth/deep/nested/file.rs"));
        assert!(s.matches("src/auth"));
        assert!(s.matches("tests/auth/login.rs"));
        assert!(!s.matches("tests/auth/deep/login.rs"));
        assert!(!s.matches("src/parser/mod.rs"));
    }

    #[test]
    fn leading_slash_and_dot_are_normalized() {
        assert!(set(&["/src/**"]).matches("./src/a.rs"));
    }

    #[test]
    fn empty_set_matches_nothing_and_is_subset_of_everything() {
        let e = PatternSet::empty();
        assert!(!e.matches("anything"));
        assert!(e.is_subset_of(&set(&["src/**"])));
        assert!(e.is_subset_of(&PatternSet::empty()));
    }

    #[test]
    fn subset_recognizes_real_containment() {
        assert!(set(&["src/auth/**"]).is_subset_of(&set(&["**"])));
        assert!(set(&["src/auth/**"]).is_subset_of(&set(&["src/**"])));
        assert!(set(&["src/auth/mod.rs"]).is_subset_of(&set(&["src/auth/*.rs"])));
        assert!(set(&["src/auth/mod.rs"]).is_subset_of(&set(&["src/**"])));
        assert!(set(&["**"]).is_subset_of(&set(&["**"])));
        assert!(set(&["src/a.rs", "src/b.rs"]).is_subset_of(&set(&["src/*.rs"])));
    }

    #[test]
    fn subset_never_approves_an_escape() {
        assert!(!set(&["**"]).is_subset_of(&set(&["src/**"])));
        assert!(!set(&["src/**"]).is_subset_of(&set(&["src/auth/**"])));
        assert!(!set(&["src/auth/deep/x.rs"]).is_subset_of(&set(&["src/auth/*.rs"])));
        assert!(!set(&["src/x.rs"]).is_subset_of(&PatternSet::empty()));
        assert!(!set(&["src/*.rs"]).is_subset_of(&set(&["src/a*.rs"])));
        assert!(!set(&["../secrets"]).is_subset_of(&set(&["src/**"])));
    }

    #[test]
    fn overlap_is_conservative_but_finds_disjointness() {
        assert!(set(&["src/auth/**"]).overlaps(&set(&["src/**"])));
        assert!(set(&["src/auth/**"]).overlaps(&set(&["**"])));
        assert!(!set(&["src/auth/**"]).overlaps(&set(&["docs/**"])));
        assert!(!set(&["src/auth/**"]).overlaps(&PatternSet::empty()));
        assert!(set(&["src/a.rs"]).overlaps(&set(&["src/a.rs"])));
    }

    #[test]
    fn union_and_intersect_behave() {
        let u = set(&["src/**"]).union(&set(&["docs/**"]));
        assert!(u.matches("src/a") && u.matches("docs/b"));
        let i = set(&["src/auth/**", "docs/**"]).intersect(&set(&["src/**"]));
        assert!(i.matches("src/auth/x"));
        assert!(!i.matches("docs/b"));
    }

    #[test]
    fn text_matching_lets_star_cross_separators() {
        let s = set(&["*rm -rf*", "cargo*"]);
        assert!(s.matches_text("cargo run -- rm -rf /"));
        assert!(s.matches_text("cargo test"));
        assert!(!s.matches_text("git status"));
        // The path matcher deliberately does not: `*` stays inside one segment there.
        assert!(!s.matches("cargo run -- rm -rf /"));
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        assert!(PathPattern::new("src/[").is_err());
    }
}
