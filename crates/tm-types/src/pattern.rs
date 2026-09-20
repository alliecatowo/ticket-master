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

    /// The distinct segment-lists this pattern's *compiled matcher* actually accepts a path
    /// against.
    ///
    /// This has to mirror [`PatternSet::compiled`] exactly, not just [`PathPattern::segments`]:
    /// a pattern ending in a trailing `**` component also matches its own bare prefix directly
    /// (`src/**` covers a path literally named `src`, not just paths nested under `src/`) —
    /// `compiled` implements that with a second, separately-registered glob for the pattern's
    /// *source string* with one trailing `/**` stripped, applied once, not recursively (so
    /// `src/**/**` gets the stripped form `src/**`, which itself does NOT further strip to
    /// `src`). Any structural containment check has to reason about every disjunct a pattern's
    /// real matcher accepts, or it can credit a pattern with covering more, or less, than it
    /// really does.
    ///
    /// The gate below matches `compiled`'s own string-level `strip_suffix("/**")` exactly,
    /// rather than slicing [`PathPattern::segments`] (which would be a *different*, unsound
    /// operation): `segments` filters out empty components, so `segments("src//**")` and
    /// `segments("src/**")` collapse to the same value even though `compiled` only registers a
    /// bare-prefix matcher for the latter (`"src//**".strip_suffix("/**")` is `Some("src/")`,
    /// and `compile_glob("src/")` does not match bare `"src"` — confirmed directly against
    /// `globset` — while `segments("src/") == segments("src")` would wrongly suggest it does).
    /// A pattern whose stripped prefix contains a leading, trailing, or doubled `/` is treated
    /// conservatively as having no extra disjunct at all, rather than guessing at one.
    fn match_disjuncts(&self) -> Vec<Vec<&str>> {
        let full = self.segments();
        match self.0.strip_suffix("/**") {
            Some(stripped)
                if !stripped.is_empty() && stripped.split('/').all(|s| !s.is_empty()) =>
            {
                vec![full, stripped.split('/').collect()]
            }
            _ => vec![full],
        }
    }

    /// True when every path this pattern matches is also matched by `other`.
    ///
    /// Conservative: a `false` result means "not provably contained". Checks every disjunct of
    /// `self` (see [`PathPattern::match_disjuncts`]) against every disjunct of `other`, since a
    /// path can reach `self` through either of its own disjuncts and has to be covered by
    /// `other` regardless of which one.
    ///
    /// Identical source strings short-circuit to `true` before that structural comparison runs.
    /// This isn't an optimization; it's what keeps `is_subset_of` reflexive (`p.implied_by(p)`)
    /// for every pattern, including one containing a `[...]` class [`segment_implies`] can't
    /// tokenize (a non-ASCII member, or one split into an unclosed fragment by
    /// [`PathPattern::segments`]'s `/`-splitting — see [`parse_class`]'s doc comment) and
    /// therefore always answers conservatively `false` about, itself included. Equal source
    /// strings compile to the identical `compile_glob` matcher and the identical
    /// [`PathPattern::match_disjuncts`] output regardless of whether this module can reason
    /// about their internal structure, so this is sound unconditionally, not just a carve-out.
    pub fn implied_by(&self, other: &PathPattern) -> bool {
        if self.0 == other.0 {
            return true;
        }
        self.match_disjuncts().iter().all(|dp| {
            other
                .match_disjuncts()
                .iter()
                .any(|dq| segments_imply(dq, dp))
        })
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
///
/// The `p.is_empty()` base case below (`q`'s remaining components are all `**`) is only sound
/// when it is reached through `**`-driven absorption: a `**` genuinely can match zero further
/// segments, so once `q` is nothing but `**`s and `p` has run out, there is nothing left to
/// prove. It is NOT sound as a fallback for the plain (non-`**`) branch further down: matching a
/// literal/`*`/`?` component of `q` against `p`'s *last* segment does not "open" a separator for
/// any further `q` components — real, `?`/`*`, or `**` — to consume, because there is no more
/// path content for them to correspond to. `globset` agrees: a compiled glob for `lit/**` does
/// not match bare `lit`, which is exactly why [`PatternSet::compiled`] has to special-case
/// stripping a trailing `/**` to also match the bare prefix for the *concrete-path* matcher. The
/// old code let the plain branch recurse straight into this base case, so e.g. `q = ["*", "**",
/// "**"]` (`*/**/**`) was structurally judged to imply `p = ["docs"]` — even though
/// `PatternSet::matches` (real glob matching) confirms `*/**/**` does not match `"docs"`. The
/// `pt.is_empty()` arm in the plain branch below closes that gap by requiring `q` to *also* be
/// fully consumed at that point, rather than allowing a trailing `**`-only remainder through.
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
    // Invariant: the empty-`p` case returned above, so `p` still has a first segment here.
    let (ph, pt) = p.split_first().expect("p is non-empty");
    if *ph == "**" {
        // A bounded pattern cannot cover an unbounded one.
        return false;
    }
    if !segment_implies(qh.as_bytes(), ph.as_bytes()) {
        return false;
    }
    if pt.is_empty() {
        // `p` is exhausted right after this segment. Do NOT delegate to `segments_imply(qt,
        // pt)`: that would hit the lenient `p.is_empty()` base case above and accept any
        // leftover `**`-only `qt` as vacuously satisfied, which is exactly the unsound shortcut
        // documented above. Require `q` to be exhausted too.
        return qt.is_empty();
    }
    segments_imply(qt, pt)
}

/// A `[...]` character class: matches exactly one character, drawn from (or, if `negated`,
/// excluded from) `ranges`.
///
/// Mirrors `globset`'s own class grammar (`Parser::parse_class` in the vendored `globset`
/// crate's `glob.rs`) at the byte level: an optional leading `!`/`^` negates the whole class;
/// `]` is a literal member only when it is the very first character after `[`/`[!`; `-` is a
/// literal member when it is first, last, or immediately follows another `-`, and forms an
/// inclusive range otherwise.
///
/// Unlike `*`/`?` (compiled as `[^/]*`/`[^/]` under `compile_glob`'s `literal_separator(true)`),
/// `globset`'s own class regex generation applies no such restriction — a class *can* match a
/// literal `/`, e.g. `[!a]` (anything but `a`) or an explicit `[/]`. `coverage` reflects that
/// faithfully; callers that need the `*`/`?` exclusion (`tokens_imply`'s `Star` branch) check for
/// it explicitly rather than this type hiding it.
#[derive(Clone)]
struct CharClass {
    negated: bool,
    ranges: Vec<(u8, u8)>,
}

impl CharClass {
    /// The 256-entry membership table this class matches, expanded once so every comparison in
    /// [`tokens_imply`] is a plain array scan rather than re-deriving membership per byte.
    fn coverage(&self) -> [bool; 256] {
        let mut covered = [false; 256];
        for &(lo, hi) in &self.ranges {
            for b in lo..=hi {
                covered[b as usize] = true;
            }
        }
        if self.negated {
            for c in &mut covered {
                *c = !*c;
            }
        }
        covered
    }
}

/// Parse a `[...]` class starting at `bytes[0] == b'['`, returning the parsed class and the
/// remaining bytes after its closing `]`. See [`CharClass`] for the grammar this mirrors.
///
/// Returns `None` — the caller's cue to answer conservatively rather than guess — for two cases
/// that should not arise from an already-validated [`PathPattern`] but are defensively rejected
/// rather than trusted:
///
/// * A genuinely unclosed class. `PathPattern::new` validates the *whole* pattern string through
///   `compile_glob` before storing it, which already rejects an unclosed `[` — but
///   [`PathPattern::segments`] and [`PathPattern::match_disjuncts`] slice that validated string
///   on `/`, and a class that (unusually) contains a literal `/` member can be split apart by
///   that slicing into two fragments, each individually missing its other half: `"a[/]b"` is a
///   real, valid, compiling pattern whose segment split (`"a["`, `"]b"`) hands this function an
///   unclosed class on each side.
/// * Any class member or range endpoint that is not a single ASCII byte. `ranges: Vec<(u8, u8)>`
///   reasons about class membership one *byte* at a time, but `globset` parses a class one
///   *Unicode scalar value* at a time; for a multi-byte UTF-8 character these disagree — `[é]`
///   is the one-member class `{'é'}` to `globset`, but would decompose into the two-member byte
///   set `{0xC3, 0xA9}` here, a different (and, compared against another multi-byte class
///   sharing one of those bytes, unsoundly inflated) claim than what actually matches. Bailing
///   out entirely for any non-ASCII byte avoids reasoning about a class this representation
///   cannot express faithfully.
fn parse_class(bytes: &[u8]) -> Option<(CharClass, &[u8])> {
    let mut i = 1; // bytes[0] is the leading '['.
    let negated = matches!(bytes.get(i), Some(b'!') | Some(b'^'));
    if negated {
        i += 1;
    }
    let mut ranges: Vec<(u8, u8)> = Vec::new();
    let mut first = true;
    let mut in_range = false;
    loop {
        let b = *bytes.get(i)?;
        if b >= 0x80 {
            return None; // non-ASCII: see doc comment above.
        }
        i += 1;
        match b {
            b']' if !first => break,
            b']' => ranges.push((b']', b']')),
            b'-' => {
                if first {
                    ranges.push((b'-', b'-'));
                } else if in_range {
                    let r = ranges.last_mut()?;
                    if b'-' < r.0 {
                        return None;
                    }
                    r.1 = b'-';
                    in_range = false;
                } else if ranges.is_empty() {
                    return None;
                } else {
                    in_range = true;
                }
            }
            c => {
                if in_range {
                    let r = ranges.last_mut()?;
                    if c < r.0 {
                        return None;
                    }
                    r.1 = c;
                    in_range = false;
                } else {
                    ranges.push((c, c));
                }
            }
        }
        first = false;
    }
    if in_range {
        ranges.push((b'-', b'-'));
    }
    Some((CharClass { negated, ranges }, &bytes[i..]))
}

/// One matcher-level "step" within a single path segment — the granularity `globset`'s own
/// matcher actually consumes, so structural comparison can walk `q` and `p` in lock step even
/// across a multi-byte `[...]` class. [`tokenize`] splits a segment's raw bytes into these before
/// [`tokens_imply`] compares them; this is what fixes `segment_implies` treating `[ab]`'s four
/// source bytes as four independent literal characters instead of the one actual character the
/// class matches (`is_subset_of(["[ab]"], ["????"])` wrongly returned `true` — see
/// `docs/decisions/D-014-pattern-subset-double-star-fix.md`'s "Known separate finding, not fixed
/// here" section for the original report).
enum SegTok {
    /// `*`: zero or more characters (never `/`; see [`CharClass`]'s doc comment).
    Star,
    /// `?`: exactly one character, any byte except `/`.
    Question,
    /// `[...]`: exactly one character, constrained to (or, if negated, excluding) the class.
    Class(CharClass),
    /// Any other byte, matched literally.
    Literal(u8),
}

/// Split one path segment's raw pattern bytes into [`SegTok`]s. `None` means this segment
/// contains a `[...]` class this module cannot faithfully reason about (see [`parse_class`]'s
/// doc comment) — the caller's cue to treat the whole comparison as "not provably contained"
/// rather than guess.
fn tokenize(bytes: &[u8]) -> Option<Vec<SegTok>> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while let Some((&b, tail)) = rest.split_first() {
        match b {
            b'*' => {
                out.push(SegTok::Star);
                rest = tail;
            }
            b'?' => {
                out.push(SegTok::Question);
                rest = tail;
            }
            b'[' => {
                let (class, remainder) = parse_class(rest)?;
                out.push(SegTok::Class(class));
                rest = remainder;
            }
            _ => {
                out.push(SegTok::Literal(b));
                rest = tail;
            }
        }
    }
    Some(out)
}

/// The path separator — the one byte `*`/`?` always exclude (`globset` compiles them as `[^/]*`
/// / `[^/]` under `literal_separator(true)`, `compile_glob`'s own setting) but a `[...]` class
/// does not automatically exclude. This asymmetry is exactly what [`tokens_imply`]'s `Star`
/// branch has to check before crediting `*`/`?` with covering a class that (unusually, but
/// validly) includes `/`.
const SEGMENT_SEPARATOR: u8 = b'/';

/// The 256-entry membership table for a token that always matches exactly one character —
/// `Literal`, `Question`, or `Class` — or `None` for `Star`, which doesn't.
fn fixed_coverage(tok: &SegTok) -> Option<[bool; 256]> {
    match tok {
        SegTok::Literal(b) => {
            let mut c = [false; 256];
            c[*b as usize] = true;
            Some(c)
        }
        SegTok::Question => {
            let mut c = [true; 256];
            c[SEGMENT_SEPARATOR as usize] = false;
            Some(c)
        }
        SegTok::Class(class) => Some(class.coverage()),
        SegTok::Star => None,
    }
}

/// Token-level implication inside one path segment (see [`SegTok`]/[`tokenize`]): the same
/// recursive shape `segment_implies` always used (`*` absorbs zero-or-more, everything else is a
/// single fixed position), generalized from raw bytes to tokens so a `[...]` class compares as
/// the one character it actually matches rather than as its raw source bytes.
///
/// Two cases are handled exactly, not just conservatively:
/// * A fixed-width head on both sides (`Literal`/`Question`/`Class` in any combination) implies
///   iff `p`'s full character coverage is a subset of `q`'s, checked by direct enumeration of
///   both 256-entry coverage tables — exact for arbitrary combinations of literals, ranges, and
///   negation, not just the originally-reported `[ab]`-vs-`????` shape.
/// * `Star` absorbing one more character of `p` (the existing `*`/`**`-absorption recursion) is
///   sound for a `Literal`/`Question` p-head unconditionally, exactly as before this fix — their
///   coverage never includes `/`, so dropping one was always safe — and for a `Class` p-head only
///   when that class's own coverage excludes `/`. Otherwise the class could realize as a `/` that
///   `*`/`?` provably cannot match, and absorbing it would silently credit `*`/`?` with covering
///   a path-separator crossing it cannot actually cross (confirmed directly: `PatternSet::parse(
///   ["x[!a]y"]).matches("x/y")` is `true` — the class realizes the middle `/` — while
///   `PatternSet::parse(["x*y"]).matches("x/y")` is `false`).
///
/// One case is deliberately conservative in the sense of matching reality exactly rather than
/// approximating it: `Star`/`Question` in `p` opposite a fixed-width `q` head is always `false`
/// — not because it might sometimes be true and this function can't tell, but because it never
/// is: a single fixed character can't cover a construct that can also produce zero or
/// two-or-more characters.
fn tokens_imply(q: &[SegTok], p: &[SegTok]) -> bool {
    match q.split_first() {
        None => p.is_empty(),
        Some((SegTok::Star, qt)) => {
            let absorb_one = match p.split_first() {
                None => false,
                Some((SegTok::Star, _)) => true,
                Some((ph, _)) => {
                    matches!(fixed_coverage(ph), Some(cov) if !cov[SEGMENT_SEPARATOR as usize])
                }
            };
            tokens_imply(qt, p) || (absorb_one && tokens_imply(q, &p[1..]))
        }
        Some((qh, qt)) => match (fixed_coverage(qh), p.split_first()) {
            (Some(q_cov), Some((ph, pt))) => match fixed_coverage(ph) {
                Some(p_cov) => {
                    p_cov.iter().zip(q_cov.iter()).all(|(&pc, &qc)| !pc || qc)
                        && tokens_imply(qt, pt)
                }
                None => false,
            },
            _ => false,
        },
    }
}

/// Character-level implication inside one path segment. Tokenizes both sides (see [`tokenize`])
/// and delegates to [`tokens_imply`]; `false` (never provably contained) if either side contains
/// a `[...]` class this module cannot faithfully tokenize (see [`parse_class`]'s doc comment).
fn segment_implies(q: &[u8], p: &[u8]) -> bool {
    match (tokenize(q), tokenize(p)) {
        (Some(qt), Some(pt)) => tokens_imply(&qt, &pt),
        _ => false,
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
        PatternSet {
            patterns: vec![PathPattern("**".to_string())],
        }
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
            Glob::new(p.as_str())
                .map(|g| g.compile_matcher().is_match(text))
                .unwrap_or(false)
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
        self.patterns
            .iter()
            .all(|p| other.patterns.iter().any(|q| p.implied_by(q)))
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
        self.patterns
            .iter()
            .any(|a| other.patterns.iter().any(|b| a.may_overlap(b)))
    }
}

impl fmt::Debug for PatternSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.patterns.iter().map(|p| p.as_str()))
            .finish()
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

    /// Regression for the bug caught by `tm-types`' `pattern_subset_is_matching_safe` proptest
    /// (`crates/tm-types/tests/authority_laws.rs`): `is_subset_of` used to say `true` for cases
    /// where a trailing, `**`-only remainder of `q` was left with nothing in `p` to correspond
    /// to. `["docs"].is_subset_of(["*/**/**"])` was the minimal reproduction (seed
    /// `cc96bc2cd4db31202b9de2cfe2c4d4bafc588bd3a9755afc655ac62a7c5b7500e2`): `"docs"` matches
    /// `p`, and `"*/**/**"` — which requires an actual path separator that a bare one-segment
    /// path does not have — does not match `"docs"`, so the subset claim was false.
    #[test]
    fn subset_does_not_vacuously_absorb_a_trailing_double_star() {
        // The exact reported false positive.
        assert!(!set(&["docs"]).is_subset_of(&set(&["*/**/**"])));
        // Same shape with a literal (not `*`) leading component, and with only one trailing
        // `**` instead of two remaining after the strippable one (see
        // `subset_respects_the_trailing_double_star_bare_prefix_disjunct` below for why
        // `["src"]).is_subset_of(["src/**"])` is legitimately `true`, not this shape).
        assert!(!set(&["src"]).is_subset_of(&set(&["src/**/**"])));
        // A `**` sandwiched between two real components that both have matching content in `p`
        // still legitimately absorbs zero segments -- this must keep working.
        assert!(set(&["a/b"]).is_subset_of(&set(&["a/**/b"])));
        // A pattern that is *entirely* `**` still implies any path, short or long.
        assert!(set(&["docs"]).is_subset_of(&set(&["**/**"])));
    }

    /// Regression for a second false-positive shape found by `exhaustive_differential` (an
    /// exhaustive, not random, differential check against `PatternSet::matches`, below) while
    /// validating the fix above: `PatternSet::compiled` special-
    /// cases a pattern ending in `/**` to *also* match its own bare prefix named directly (so
    /// `src/**` matches a path literally called `src`, per `matching_follows_glob_semantics`
    /// above) -- but that special case is applied to the pattern's own source string exactly
    /// once, not recursively, so `src/**/**` only reduces to `src/**` (which still needs a real
    /// `/` in the path) and does NOT also match bare `src`. `implied_by`/`is_subset_of` has to
    /// account for this real matcher asymmetry on *both* sides being compared
    /// ([`PathPattern::match_disjuncts`]), not just structurally compare raw segment lists, or
    /// it silently over- or under-claims containment whenever a trailing-`/**` pattern's bare-
    /// prefix disjunct is involved.
    #[test]
    fn subset_respects_the_trailing_double_star_bare_prefix_disjunct() {
        // `src/**` really does match bare `src` (the one-level-stripped hack), so it really is
        // implied by an *identical* one-level-stripped bare pattern -- this must be `true`.
        assert!(set(&["src"]).is_subset_of(&set(&["src/**"])));
        assert!(set(&["docs"]).is_subset_of(&set(&["*/**"])));
        // `src/**/**` only strips down to `src/**`, which does NOT also cover bare `src` -- the
        // exhaustive check's originally-found false positive.
        assert!(!set(&["src/**"]).is_subset_of(&set(&["src/**/**"])));
        assert!(!set(&["src/**"]).is_subset_of(&set(&["*/**/**"])));
        assert!(!set(&["*/**"]).is_subset_of(&set(&["*/**/**"])));
    }

    /// Regression for a third false-positive shape, caught by adversarial review of the
    /// disjunct fix above rather than by any automated check: [`PathPattern::segments`] filters
    /// out empty components, so it cannot distinguish a "clean" pattern from one with a
    /// trailing or doubled `/` next to its final `**` -- but `PatternSet::compiled`'s own
    /// `strip_suffix("/**")` gate very much can, and does not register a bare-prefix matcher
    /// for either `"src/**/"` (does not end with the literal `"/**"`) or `"src//**"` (does, but
    /// strips down to `"src/"`, and `compile_glob("src/")` -- confirmed directly -- does not
    /// match bare `"src"` even though `segments("src/") == segments("src")`). A naive
    /// disjunct built by slicing `segments()` would wrongly agree that `["src"]` is implied by
    /// either pattern. `match_disjuncts` closes this by gating on the same string-level
    /// `strip_suffix("/**")` check `compiled` uses, and additionally requiring the stripped
    /// prefix to contain no empty `/`-separated component before trusting it as a disjunct.
    #[test]
    fn subset_does_not_trust_a_messy_stripped_prefix() {
        assert!(!set(&["src"]).is_subset_of(&set(&["src/**/"])));
        assert!(!set(&["src"]).is_subset_of(&set(&["src//**"])));
        // The clean case must still work.
        assert!(set(&["src"]).is_subset_of(&set(&["src/**"])));
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

    /// Regression for the false-positive class D-014 found but explicitly left unfixed (its
    /// "Known separate finding, not fixed here" section): `segment_implies` treated a `[...]`
    /// character class as its raw source bytes (`[`, `a`, `b`, `]` — four literal characters)
    /// instead of the one actual character it matches. `[ab]` matches a single character ('a'
    /// or 'b'); `????` requires exactly four. The old byte-level code happened to line up `[ab]`'s
    /// four *source* bytes against `????`'s four `?`s and wrongly called that a subset.
    #[test]
    fn subset_reasons_about_a_character_class_not_its_source_bytes() {
        // The exact originally-reported false positive.
        assert!(!set(&["[ab]"]).is_subset_of(&set(&["????"])));
        // Same shape, fewer/more `?`s: a one-character class is never implied by a fixed width
        // other than exactly one.
        assert!(!set(&["[ab]"]).is_subset_of(&set(&["??"])));
        assert!(!set(&["[ab]"]).is_subset_of(&set(&[""])));
    }

    /// A `[...]` class is exactly one character, unconstrained-`?`-wide — so it legitimately
    /// *is* implied by a bare `?`, and this must keep working now that `?` reasons about the
    /// class's real width instead of consuming one raw source byte at a time.
    #[test]
    fn subset_recognizes_a_character_class_implied_by_question_mark() {
        assert!(set(&["[ab]"]).is_subset_of(&set(&["?"])));
        assert!(set(&["[a-c]"]).is_subset_of(&set(&["?"])));
    }

    /// Class-vs-class and class-vs-literal containment, computed by direct 256-entry coverage
    /// comparison rather than case analysis over negation/ranges — exercises that the fix is
    /// precise, not just conservatively `false` for everything touching `[...]`.
    #[test]
    fn subset_recognizes_real_character_class_containment() {
        // A narrower class is a subset of a wider one covering all its members.
        assert!(set(&["[a]"]).is_subset_of(&set(&["[ab]"])));
        assert!(set(&["[a-b]"]).is_subset_of(&set(&["[a-c]"])));
        // A literal is a subset of any class that contains it.
        assert!(set(&["x"]).is_subset_of(&set(&["[wxy]"])));
        // A class is a subset of a literal only when it is a non-negated singleton for exactly
        // that character.
        assert!(set(&["[x]"]).is_subset_of(&set(&["x"])));
        assert!(!set(&["[wx]"]).is_subset_of(&set(&["x"])));
        // Negation is accounted for: anything but 'a' contains 'b' and 'c', not 'a'.
        assert!(set(&["b"]).is_subset_of(&set(&["[!a]"])));
        assert!(set(&["c"]).is_subset_of(&set(&["[!a]"])));
        assert!(!set(&["a"]).is_subset_of(&set(&["[!a]"])));
    }

    #[test]
    fn subset_never_approves_an_escape_through_a_character_class() {
        // The wider class is not a subset of the narrower one.
        assert!(!set(&["[ab]"]).is_subset_of(&set(&["[a]"])));
        // Reversed containment must fail too.
        assert!(!set(&["[wxy]"]).is_subset_of(&set(&["x"])));
    }

    /// Regression for a false-positive shape found by adversarial review of this fix's first
    /// draft (not by the exhaustive check, whose alphabet — see `exhaustive_differential` below
    /// — never puts a class in the same segment as `*`/`?`): `globset` compiles `*`/`?` with
    /// `literal_separator(true)` (`[^/]*` / `[^/]`, confirmed never matching `/`), but compiles a
    /// `[...]` class with no such restriction, so a negated class like `[!a]` really can match a
    /// literal `/` and cross what looks, in the pattern's own source text, like a single path
    /// segment. `"x[!a]y"` really does match the two-segment real path `"x/y"` (confirmed
    /// directly against `PatternSet::matches`); `"x*y"`/`"x?y"` do not. The structural check must
    /// not credit `*`/`?` with covering a class whose coverage includes `/`.
    #[test]
    fn subset_does_not_let_a_slash_crossing_class_escape_through_star_or_question() {
        // Confirm the real matcher asymmetry this test guards against.
        assert!(set(&["x[!a]y"]).matches("x/y"));
        assert!(!set(&["x*y"]).matches("x/y"));
        assert!(!set(&["x?y"]).matches("x/y"));
        // The structural check must agree: neither `*` nor `?` provably covers this class.
        assert!(!set(&["x[!a]y"]).is_subset_of(&set(&["x*y"])));
        assert!(!set(&["x[!a]y"]).is_subset_of(&set(&["x?y"])));
        // A class that does NOT include `/` in its coverage is still safely absorbed by `*`.
        assert!(set(&["x[bc]y"]).is_subset_of(&set(&["x*y"])));
        // Identity still holds for a slash-crossing class compared against itself.
        assert!(set(&["x[!a]y"]).is_subset_of(&set(&["x[!a]y"])));
    }

    /// A malformed-looking class produced only by this module's own `/`-splitting of an
    /// otherwise-valid pattern (see `parse_class`'s doc comment) is rejected conservatively
    /// rather than misparsed.
    #[test]
    fn subset_is_conservative_about_a_class_split_by_segment_slicing() {
        // A valid, compiling pattern whose class contains a literal `/` member.
        let p = PathPattern::new("a[/]b").unwrap();
        assert_eq!(p.segments(), vec!["a[", "]b"]);
        // Neither segment tokenizes as a well-formed class on its own, so the structural
        // comparison itself can't prove containment even against an identical copy of the same
        // pattern — but `implied_by`'s identity short-circuit (see its doc comment) proves the
        // reflexive case a different way, without needing the structural check to understand
        // this pattern's `[...]` class at all.
        assert!(set(&["a[/]b"]).is_subset_of(&set(&["a[/]b"])));
        // A *non-identical* pair, each individually unparseable this same way, is not provably
        // contained -- the short-circuit only fires for equal source strings, not merely
        // equivalent-looking ones.
        assert!(!set(&["a[/]b"]).is_subset_of(&set(&["a[/]c"])));
    }

    /// `implied_by`'s identity short-circuit is what keeps `is_subset_of` reflexive for a
    /// pattern containing a class this module can't tokenize at all -- a non-ASCII class member
    /// (see [`parse_class`]'s doc comment) is a realistic case (unlike the previous test's
    /// contrived `/`-splitting one), not just a defensive corner.
    #[test]
    fn subset_is_reflexive_even_through_an_unparseable_non_ascii_class() {
        assert!(set(&["[é]"]).is_subset_of(&set(&["[é]"])));
        // Two different non-ASCII-class patterns are still conservatively not provably
        // contained in each other.
        assert!(!set(&["[é]"]).is_subset_of(&set(&["[è]"])));
    }
}

/// An exhaustive (not random) differential check of [`PathPattern::implied_by`]/
/// [`PatternSet::is_subset_of`] against [`PatternSet::matches`] — the actual safety law
/// `pattern_subset_is_matching_safe` in `crates/tm-types/tests/authority_laws.rs` checks with
/// random sampling. This module exists because random sampling missed real bugs in practice:
/// during development of the fix this module accompanies, ~22,000 random cases against the
/// then-broken code found zero failures (the vulnerable pattern shapes are a narrow slice of
/// the generator's space), while a deterministic, targeted repro found the bug immediately, and
/// a full exhaustive sweep over this same bounded alphabet found a second, distinct false-
/// positive shape random sampling had also missed. Exhaustive enumeration over a small alphabet
/// is a strictly stronger check than random sampling over the same alphabet: it does not depend
/// on luck to hit a narrow bug window.
///
/// Bounded to pattern/path length 2 here so it stays CI-fast. Length 3 was run by hand during
/// development of the original three fixes (584 patterns × 584 patterns × 258 paths, ~88M
/// triples, a few seconds in `--release`) with zero violations post-fix and is not re-run
/// automatically.
///
/// `[ab]`, `????`, `?`, and a mixed slash-crossing shape (`x[!a]y`/`x*y`/`x?y`, alongside
/// single-char path literals `x`/`y`/`a`/`b`) were added to both alphabets for the
/// `[...]`-class fix this module accompanies — see
/// `docs/decisions/D-014-pattern-subset-double-star-fix.md`. Two things this addition has to get
/// right, each missed by an earlier draft of this same alphabet extension:
///
/// * This check only ever flags a *false positive* (`if !p.implied_by(q) { continue; }` skips
///   every pair it doesn't). `[ab]` and a bare `?` alone reproduce a false-*negative* fix (`[ab]`
///   legitimately implied by `?`, wrongly `false` pre-fix) that this check structurally cannot
///   see regardless of alphabet. `????` (four question marks, one segment) is what's needed to
///   reproduce the originally-reported false positive itself
///   (`is_subset_of(["[ab]"], ["????"])`) — confirmed directly: this alphabet addition does
///   flag it against the pre-fix code and finds zero violations against the fixed code.
/// * A class-alphabet entry that never shares a *segment* with a `*`/`?` entry can never
///   exercise the class-can-match-`/`-but-`*`/`?`-can't asymmetry (`x[!a]y`/`x*y`/`x?y`) that bit
///   the fix's first draft — caught by adversarial review, not this check, precisely because the
///   check's alphabet didn't yet contain a mixed segment. The same lesson D-014's own bug 3
///   already drew about this alphabet being the thing in question, drawn a second time about
///   this same alphabet.
#[cfg(test)]
mod exhaustive_differential {
    use super::*;

    const ALPHABET: &[&str] = &[
        "src", "tests", "docs", "auth", "parser", "mod.rs", "*", "**", "?", "????", "[ab]",
        "x[!a]y", "x*y", "x?y",
    ];
    const PATH_LITERALS: &[&str] = &[
        "src", "tests", "docs", "auth", "parser", "mod.rs", "x", "y", "a", "b",
    ];

    fn strings_up_to(alphabet: &[&str], max_len: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut frontier: Vec<String> = vec![String::new()];
        for _ in 0..max_len {
            let mut next = Vec::new();
            for prefix in &frontier {
                for seg in alphabet {
                    let s = if prefix.is_empty() {
                        (*seg).to_string()
                    } else {
                        format!("{prefix}/{seg}")
                    };
                    out.push(s.clone());
                    next.push(s);
                }
            }
            frontier = next;
        }
        out
    }

    #[test]
    fn exhaustive_no_false_positive_up_to_length_2() {
        let patterns = strings_up_to(ALPHABET, 2);
        let paths = strings_up_to(PATH_LITERALS, 2);

        let path_patterns: Vec<PathPattern> = patterns
            .iter()
            .map(|s| PathPattern::new(s).unwrap())
            .collect();

        // Precompute p.matches(path) for every (pattern, path) pair once.
        let match_matrix: Vec<Vec<bool>> = patterns
            .iter()
            .map(|p| {
                let set = PatternSet::parse([p.clone()]).unwrap();
                paths.iter().map(|path| set.matches(path)).collect()
            })
            .collect();

        let mut violations: Vec<(String, String, String)> = Vec::new();
        for (qi, q) in path_patterns.iter().enumerate() {
            for (pi, p) in path_patterns.iter().enumerate() {
                if !p.implied_by(q) {
                    continue;
                }
                for (path_i, path) in paths.iter().enumerate() {
                    if match_matrix[pi][path_i] && !match_matrix[qi][path_i] {
                        violations.push((patterns[pi].clone(), patterns[qi].clone(), path.clone()));
                    }
                }
            }
        }
        assert!(
            violations.is_empty(),
            "is_subset_of lied for {} (pattern, pattern, path) triples: {violations:?}",
            violations.len()
        );
    }
}
