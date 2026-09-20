# D-014 — `PatternSet::is_subset_of` wrongly approved escapes through `**`

**Status:** accepted · **Date:** 2026-09-20 · **Supersedes:** nothing

## Context

`crates/tm-types/src/pattern.rs`'s module doc states the safety contract plainly: `is_subset_of`
backs authority attenuation (`Authority::containment_failures`, `crates/tm-types/src/
authority.rs`), and "may answer `false` for a set that is in fact contained... but it must never
answer `true` for one that is not." A false `true` there means a delegatee's `repository.write`,
`shell.allow`, etc. can be judged "within" its grantor's when it is not — a real escalation path,
not a cosmetic bug.

`crates/tm-types/tests/authority_laws.rs`'s `pattern_subset_is_matching_safe` proptest checks
exactly this law against real glob matching (`PatternSet::matches`) rather than trusting the
structural check's own reasoning. It failed on fresh seeds. Minimal reproduction:
`PatternSet::parse(["docs"]).is_subset_of(&PatternSet::parse(["*/**/**"]))` returned `true`, but
`PatternSet::parse(["*/**/**"]).matches("docs")` is `false` — so the subset claim was false.
Confirmed deterministically (not just via the proptest, which — see "What this cost to verify"
below — turned out to be a weak detector for this specific bug): running the exact reported
triple directly against the pre-fix code reproduces it every time; ~22,000 random proptest cases
against that same pre-fix code found it zero times.

Fixing the reported shape surfaced a **second**, structurally distinct false-positive class,
found by an exhaustive (not random) differential check written to validate the first fix. Fixing
*that* one, in turn, surfaced a **third**: adversarial re-review of the second fix (not any
automated check) found that its own disjunct construction was itself unsound for patterns with a
trailing or doubled `/` next to the final `**`. All three are fixed here; all three live in the
same function for the same underlying reason: the structural subset check reasons about pattern
segments in the abstract, but the real matcher (`PatternSet::matches`) has concrete quirks the
structural check did not originally model, and each fix attempt had to be checked against that
real matcher rather than trusted on its own reasoning.

## Decision

Three changes to `crates/tm-types/src/pattern.rs`:

1. **`segments_imply`**: a trailing, `**`-only remainder of the containing pattern `q` may only
   be treated as "vacuously satisfied" when reached through genuine `**`-driven absorption, never
   as a fallback after `q`'s plain (literal/`*`/`?`) component exhausts the contained pattern
   `p`'s last real segment. Relative to the original buggy code, this change is strictly
   narrowing: it can only turn a previous `true` into `false`, never the reverse (see "Why"
   below).
2. **`PathPattern::implied_by`**: compares every *match disjunct* of the contained pattern against
   every match disjunct of the containing pattern, where a pattern's disjuncts are its own segment
   list plus — only if it ends in a trailing `**` component — that segment list with the final
   `**` stripped once. This mirrors `PatternSet::compiled`'s own special case (below) instead of
   silently ignoring it. Relative to change 1 alone, this one is **not** purely narrowing — it
   correctly adds new `true` results the single-disjunct comparison was *under*-claiming (see the
   `src`-vs-`src/**` example below) alongside removing the ones bug 2 was over-claiming. Its
   correctness is established directly (by exhaustive comparison against `PatternSet::matches`,
   below), not by a narrowing argument.
3. **`PathPattern::match_disjuncts`'s stripped-disjunct gate**: only trusts the "one trailing
   `/**` stripped" disjunct when the stripped prefix contains no leading, trailing, or doubled
   `/` — computed from the pattern's *source string* via the same `strip_suffix("/**")` operation
   `PatternSet::compiled` itself performs, not by slicing the already-`segments()`-filtered list.
   Strictly narrowing relative to change 2 alone: it removes disjuncts change 2 would otherwise
   have credited a messy pattern with, and adds none.

## Why

### Bug 1: a plain component exhausting `p` doesn't "open" a `**` for free

`segments_imply`'s `p.is_empty()` base case (`return q.iter().all(|s| *s == "**")`) could be
reached two different ways, and the old code could not tell them apart:

1. **Legitimate:** `q`'s head is already `**`, and repeated absorption of `p`'s real segments
   (one at a time) eventually drains `p` while `q`'s `**` never advances. A `**` genuinely can
   match zero further segments here — nothing more needs proving.
2. **Unsound:** `q`'s head is a *plain* component. It matches `p`'s one and only remaining
   segment via `segment_implies`, which consumes that segment whole. The recursive call that
   follows, with `p` now `[]`, lands in the *same* base case — but arriving there this way is not
   sound: `q`'s remaining `**` components would need an actual path separator that no path
   matched by `p` can supply. `globset` agrees: `src/**` does not match bare `src`.

The fix inserts an explicit check in the plain-component branch: when matching `q`'s head
consumes `p`'s last segment, require `q`'s remaining components to *also* be empty, rather than
recursing into the lenient base case. `qt.is_empty()` implies the old `qt.iter().all(|s| *s ==
"**")` (vacuously, over an empty list) but not the reverse, and the rest of the recursion is pure
`&&`/`||` over sub-results with no negation — so this change can only ever turn a previous `true`
into `false`, never introduce a new `true`.

### Bug 2: `PatternSet::compiled`'s trailing-`/**` hack isn't symmetric across repeats

`PatternSet::compiled` has a hand-written special case: a pattern ending in `/**` also gets a
second, separately-compiled matcher for that same source string with the trailing `/**` stripped
*once* — "`src/**` should also cover the directory itself when it is named directly," per its own
comment. This is necessary because the raw compiled glob for `src/**` genuinely does not match
bare `src` (confirmed empirically against `globset` directly). Applied once, not recursively:
`src/**/**` reduces only to `src/**`, which itself still does not match bare `src`.

The original `implied_by` compared raw segment lists only, with no knowledge of this hack on
*either* side of the comparison. An exhaustive differential check (`exhaustive_differential` in
`pattern.rs`, added for this fix — see below) enumerated every `(p, q, path)` triple over the
proptest's own alphabet up to length 3 and found 18 violations, all one shape:
`is_subset_of(["X/**"], ["X/**/**"])` (and the `*`-leading equivalent) claimed `true`, but
`["X/**"].matches("X")` is `true` (via the one-level hack) while `["X/**/**"].matches("X")` is
`false` (the hack only strips one level, and one level is not enough here). `is_subset_of` was
crediting `q` with covering a disjunct of `p` that `q`'s real matcher does not actually accept.

`PathPattern::match_disjuncts` makes this explicit instead of implicit: it returns the one or two
segment-lists `PatternSet::compiled` would actually register a matcher for, and `implied_by` now
requires *every* disjunct of the contained pattern to be covered by *some* disjunct of the
containing pattern — exactly mirroring what "a path matches this pattern" means for the real
matcher (`matches` = matches any registered disjunct).

### Bug 3: the first version of `match_disjuncts` used the wrong operation to find the stripped form

The first implementation of `match_disjuncts` computed the stripped disjunct by slicing
[`PathPattern::segments`] (dropping the final `"**"` element) rather than replicating
`compiled`'s actual `self.0.strip_suffix("/**")` on the source string. `segments` filters out
empty `/`-separated components, so it cannot tell a "clean" pattern from a messy one: unlike
`compiled`, it does not distinguish `"src/**"`'s pattern (which is what `compiled` is really
looking for) from `"src/**/"` (does not even textually end with `"/**"`; `compiled` never adds a
bare-prefix matcher for it) or `"src//**"` (does end with `"/**"`, but strips down to `"src/"`,
and `compile_glob("src/")` — confirmed directly against `globset` — does **not** match bare
`"src"`, even though `segments("src/") == segments("src")`). The naive segments-slicing version of
`match_disjuncts` credited both messy patterns with the same bare-`src` disjunct as the clean
`"src/**"`, so `is_subset_of(["src"], ["src/**/"])` and `is_subset_of(["src"], ["src//**"])` both
wrongly returned `true`. Found by adversarial self-review of the bug-2 fix, not by any of this
change's own automated checks (the exhaustive alphabet has no doubled or trailing slashes in it —
see "What this cost to verify" below).

The fix makes `match_disjuncts` compute the stripped disjunct from `self.0.strip_suffix("/**")`
directly — the same string-level operation `compiled` performs — and additionally requires the
stripped prefix to contain no empty `/`-separated component (i.e. no leading, trailing, or
doubled `/`) before trusting it. A pattern that fails that check simply gets no extra disjunct;
conservative rather than clever, per this module's own stated preference.

## What this costs, stated plainly

The structural check now hard-codes knowledge of `PatternSet::compiled`'s one-level trailing-
`/**` strip. These two are no longer independent: if `compiled`'s hack ever changes (stripped
recursively, dropped, extended to `/*` as well, etc.), `match_disjuncts` has to change with it or
`is_subset_of` silently drifts out of sync with what `matches` actually accepts — in either
direction. There is no compiler-enforced link between them; a future change to one without the
other is a real risk this decision creates. (`exhaustive_differential`, described below, is the
best available guard against that drift, but it only runs it for the patterns already in its
fixed alphabet — it does not fail loudly and specifically point at `compiled` if someone changes
the hack there without touching `pattern.rs`'s pattern-comparison code.)

Fixing bug 2 also *improves* precision in a case bug 1's original fix had deliberately sacrificed:
`is_subset_of(["src"], ["src/**"])` is now correctly `true` (it was `false` after the first,
narrower fix, and that was flagged in review as an unnecessarily conservative — but safe —
approximation at the time; the disjunct fix closes that gap honestly instead of leaving it as
accepted slack).

## What this cost to verify: random sampling missed both bugs

`pattern_subset_is_matching_safe`'s random-seed proptest, run against the confirmed-buggy
pre-fix code, found **zero** failures across ~22,000 generated cases (8 runs of 256, then one run
of 20,000). The bug is real and 100% reproducible on the exact reported triple, but is a narrow
enough slice of the generator's `(p, q, path)` space that random sampling alone did not
demonstrate it existed, and would not have demonstrated its absence after the fix either. Two
things did the actual verification work here:

- A **deterministic, targeted repro** (the exact reported triple, checked directly rather than
  hoping a random run would regenerate it) — this is what actually confirmed the bug pre-fix and
  its absence post-fix.
- An **exhaustive differential check** (`exhaustive_differential` module in `pattern.rs`,
  committed at pattern/path length 2 for CI speed; length 3 — 584 patterns × 584 patterns × 258
  paths, ~88M triples — was run by hand in `--release` during development) comparing
  `is_subset_of`'s claim against `PatternSet::matches`'s real answer for every triple in a bounded
  alphabet. This is what found bug 2, which the random proptest never surfaced in any of the runs
  above, before or after the bug-1 fix.

The proptest remains valuable as an always-on, wide-alphabet, regression-shaped check, but this
episode is evidence it is not sufficient on its own for a security-relevant structural property
like this one; exhaustive enumeration over a small, representative alphabet is a strictly stronger
check for exactly this bug class and is now a permanent part of the test suite alongside it.

Bug 3 (above) is evidence that even the exhaustive check is not sufficient on its own: its fixed
alphabet contains no pattern with a trailing or doubled `/`, so it could not have found bug 3 no
matter how large a length bound it ran at. What did find it was a deliberate adversarial
self-review pass over the bug-2 fix's own reasoning (this session's `advisor` tool) rather than
any test — a reminder that "the exhaustive check passes" and "this code is correct" are not the
same claim when the check's own alphabet is the thing in question.

## Bug 4 (follow-up): `[...]` character classes, fixed after this decision was first accepted

The section below originally read, verbatim: "`segment_implies` (the character-level half of the
comparison) treats `[...]` glob character classes in `p` as literal bytes rather than a class
match, e.g. `is_subset_of(["[ab]"], ["????"])` returns `true` while
`PatternSet::parse(["????"]).matches("a")` is `false` (`[ab]` is one character; `????` requires
four, and `segment_implies` matches each `?` against one of `[`, `a`, `b`, `]` positionally rather
than reasoning about the class). This is the same false-positive class as bugs 1 and 2 above,
confirmed by direct check, and is invisible to both the random proptest and
`exhaustive_differential` as committed, because neither's alphabet contains `?` or `[`. Left
unfixed and unscoped here — it needs its own investigation into how `[...]` classes should
structurally compose (their contents are not currently validated as a proper class by this code at
all, only by `globset` at `PathPattern::new` time) rather than a hasty bolt-on next to two
unrelated fixes." That investigation happened as a direct follow-up in the same session and is
recorded here rather than in a new decision doc — see "Why this doc, not a new one" below.

Confirmed directly, matching this doc's own established method (not just via the proptest):
running `set(&["[ab]"]).is_subset_of(&set(&["????"]))` against the pre-follow-up code returns
`true` every time; the real matcher, `PatternSet::parse(["????"]).matches("a")`, is `false`.

### The fix: tokenize each segment instead of comparing raw bytes

`segment_implies` now tokenizes each pattern segment into `SegTok`s (`Star`, `Question`, `Class`,
`Literal`) before comparing (delegating to a new `tokens_imply`), so a `[...]` class is one
comparison unit worth exactly one matched character, not N raw source bytes. A new `CharClass`
type parses `[...]` byte-for-byte, mirroring `globset`'s own class grammar (leading `!`/`^`
negation, `]` literal only as the first member, `-` ranges), and exposes a 256-entry `coverage()`
table (which bytes the class actually matches, negation already applied). Structural comparison
between any two fixed-width constructs (`Literal`, `Question`, or `Class`, in any combination)
reduces to one rule: `p`'s coverage must be a subset of `q`'s, checked by direct enumeration of
both tables.

**Handled precisely, not just conservatively:**
- `Class` vs `Class`: exact coverage-subset check, correct for arbitrary combinations of
  literals, ranges, and negation (e.g. `is_subset_of(["[a]"], ["[ab]"])` is `true`;
  `is_subset_of(["b"], ["[!a]"])` is `true`; `is_subset_of(["a"], ["[!a]"])` is `false`).
- `Class` vs `Literal` (either direction): a literal is a subset of any class containing it; a
  class is a subset of a literal only when it is a non-negated singleton for that exact
  character — both directions fall out of the same coverage-subset check with no special-casing.
- `Class`/`Literal` vs `Question`: `?` is unconstrained (any byte except `/`), so it always
  covers a class or literal's single produced character; a class can never cover `?` unless its
  own coverage happened to be the full non-`/` universe (the coverage check handles this exactly
  too, it just almost never holds for a realistic class).
- `Star` absorbing one more character of `p` (the pre-existing `*`/`**`-absorption recursion,
  structurally unchanged for `Literal`/`Question`/`Star` p-heads) now also absorbs a `Class`
  p-head, but only when that class's own coverage excludes `/` (see bug 4b immediately below for
  why that guard exists).

**Deliberately conservative, because the true answer is always `false`, not because it's unknown:**
`Star`/`Question` in `p` opposite a fixed-width `q` head (`Literal`/`Question`/`Class`) is always
rejected — not a hedge, but exact: a single fixed character genuinely cannot cover a construct
that can also produce zero or two-or-more characters, so `false` is the correct answer, not a
conservative stand-in for "can't tell."

**Conservative because precise reasoning was judged out of scope, per this doc's own stated
preference for "conservative rather than clever" (bug 3's fix, above):** `tokenize` returns `None`
— `segment_implies` then answers `false` for the whole comparison — for a class containing any
non-ASCII byte, or for a class that fails to parse as well-formed at all. Both are real,
reachable cases, not just defensive dead code:
- **Non-ASCII class members.** `CharClass`'s `ranges: Vec<(u8, u8)>` reasons one *byte* at a
  time; `globset` parses a class one *Unicode scalar value* at a time. These disagree for a
  multi-byte UTF-8 character: `[é]` is the one-member class `{'é'}` to `globset`, but would
  decompose into the two-member byte set `{0xC3, 0xA9}` here — a different, and against another
  multi-byte class sharing one of those bytes, unsoundly inflated claim than what actually
  matches. Precise Unicode-scalar-aware class reasoning was judged not worth the added complexity
  for this fix; bailing out to `false` is safe and cheap.
- **A class split apart by this module's own `/`-based segment slicing.** `PathPattern::segments`
  and `match_disjuncts` split the (already fully validated) pattern source on `/`. A class that
  itself contains a literal `/` member is validated as a whole by `compile_glob` at
  `PathPattern::new` time, but the segment split doesn't know that and cuts it in half regardless:
  `"a[/]b"` is a real, compiling pattern whose segment split is `["a[", "]b"]`, handing
  `parse_class` an unclosed class on each side. `parse_class` reports this as `None` rather than
  misreading a fragment as a complete class.

### Bug 4b: a `[...]` class can cross `/`; `*`/`?` provably cannot — found by adversarial review of the first draft

The first draft of this fix modeled `Star`'s p-absorption step as "drop one token of `p`,
unconditionally, whatever kind of token it is" — a direct generalization of the pre-existing
byte-dropping behavior. Adversarial review (this session's `advisor` tool, following the same
practice bugs 2 and 3 above already established as load-bearing) found this unsound by the same
underlying mechanism as bugs 1–3: the structural check reasoned about `q`/`p` in the abstract
without checking a real matcher quirk. `compile_glob`'s `literal_separator(true)` makes `globset`
compile `*` as `[^/]*` and `?` as `[^/]` — both provably never match `/` — but compiles a `[...]`
class with no such restriction (confirmed directly in `globset 0.4.20`'s `tokens_to_regex`: the
`Token::Class` arm emits the class's ranges unmodified, unlike the `Token::Any`/`ZeroOrMore` arms).
A negated class like `[!a]` can therefore match a literal `/`, letting it cross what its pattern's
own source text looks like a single path segment. Confirmed directly against the real matcher:
`PatternSet::parse(["x[!a]y"]).matches("x/y")` is `true` (the class realizes the middle `/`) while
`PatternSet::parse(["x*y"]).matches("x/y")` and `PatternSet::parse(["x?y"]).matches("x/y")` are
both `false`. The first draft's `Star` absorption step would have credited `"x*y"` with covering
`"x[!a]y"`'s `/`-crossing disjunct, which is exactly the shape of false positive this whole
decision exists to close. The fix: `Star`'s p-absorption step now requires a `Class` p-head's own
coverage to exclude `/` before absorbing it (a `Literal`/`Question`/`Star` p-head is unaffected —
their coverage never included `/` in the first place, so the guard is a no-op for them, which is
also how the fix stays a pure extension rather than a behavior change for every pattern that
contains no class).

### Bug 4c: the new conservatism broke reflexivity for one class of pattern — found by a second adversarial review pass

A second adversarial review pass (requested specifically before treating this follow-up as done,
matching this repo's own convention that a finished-feeling change still gets one distrustful pass
before handback) found that `tokenize` returning `None` — needed for the two conservative cases
above (non-ASCII class members; a class fragment split apart by `/`-based segment slicing) — has a
consequence neither of those cases' own regression tests exposed on their own: comparing such a
pattern *against itself* also answers `false`, because `segment_implies` never gets far enough to
even attempt the comparison. `is_subset_of` is supposed to be reflexive for every pattern (`p` is
always trivially a subset of itself) — `authority_laws.rs`'s `reflexivity` proptest law checks
exactly this — but that law's generator alphabet contains no `[` (the same alphabet-luck bugs 1–4
were each found or missed by, noted explicitly so the next person extending that generator has a
pointer instead of a surprising law failure), so this went undetected until asked for directly
rather than surfacing as a CI failure.

The same gap also breaks `intersect_is_idempotent`: `PatternSet::intersect`'s self-intersection
filters each pattern by `.any(|q| p.implied_by(q))` over the *same* set's own patterns, so a
pattern with an unparseable class would have been silently dropped from its own intersection with
itself.

Fixed by adding an identity short-circuit to `PathPattern::implied_by`: equal source strings
(`self.0 == other.0`) return `true` immediately, before the structural (tokenize-dependent)
comparison runs at all. This is sound unconditionally, not merely for the classes this module
happens to be able to tokenize: two `PathPattern`s built from the identical source string compile
to the identical `compile_glob` matcher and the identical `match_disjuncts` output regardless of
whether `segment_implies` can reason about what's inside them, so `p.implied_by(p)` is always
correct to answer `true` by construction, independent of this fix's own tokenization limits. This
restores both laws exactly, confirmed directly (`subset_is_reflexive_even_through_an_unparseable_
non_ascii_class` and the updated `subset_is_conservative_about_a_class_split_by_segment_slicing`,
both in `pattern.rs`) and via `cargo test -p tm-types --test authority_laws`.

### Why this doc, not a new one

This repository amends a decision doc in place, rather than filing a new `D-NNN`, when new
information is a direct continuation of the same decision rather than a new one — confirmed by
precedent, not assumed: `docs/decisions/D-003-project-scope.md` was edited in place twice after
its initial acceptance (`85a13fc`, `3763134`), both times adding newly-discovered detail about the
*same* decision (promotion behavior, an exit-code choice) without a `Supersedes:` bump or a new
file. This fix is a closer fit for that pattern than for a new decision: it isn't a new choice,
it's this decision's own explicitly-deferred follow-up ("Left unfixed and unscoped here" above),
resolved using the exact same method (structural check vs. real `PatternSet::matches`, exhaustive
enumeration, adversarial review) this doc already established. `Status`/`Date`/`Supersedes` above
are left as originally accepted, matching D-003's own precedent of not bumping them for an
in-place addition.

## Call sites checked

`is_subset_of` is called from `Authority::containment_failures` (`repository.read`,
`repository.write`, `shell.allow`) and, transitively through `PatternSet::intersect`, from
`Authority::intersect` (`repository.read`, `repository.write`, `shell.allow` again) and
`crates/tm-cli/src/tickets.rs`'s `ticket_delegate`. All of these are "allow"-shaped sets, where
this fix's occasional extra conservatism (bug 1's narrow fix alone) or corrected precision (after
the disjunct fix) is the safe direction either way. `shell.deny` — the one field where *shrinking*
would be the unsafe direction — is combined with `.union()`, not `.intersect()`, in
`Authority::intersect` and `Authority::with_inherited_denials`; it never goes through
`is_subset_of` or `PatternSet::intersect` at all, so this fix cannot affect it. No default
authority, role, genesis-domain, or template pattern data in the workspace (as opposed to ad hoc
test fixtures) was found using the specific bare-segment-vs-trailing-`/**` shape this fix changes
the answer for.

**Bug 4's own call-site survey** (the same question, asked again for the `[...]`-class follow-up,
per this section's own precedent): a workspace-wide search for `[...]`-class syntax in real
pattern data — every non-test call site feeding `PatternSet::parse`/`RepoAuthority`/
`ShellAuthority` (`crates/tm-genesis/src/attach.rs`'s `investigation_authority` —
`PatternSet::all()`/`PatternSet::empty()`, no classes; `crates/tm-templates/src/verify.rs`'s
shell allowlist — `["true*", "false*", "echo*"]`, no classes) — found zero uses of `[...]` in real
authority/role/genesis/template data anywhere in the workspace; every `[`-containing pattern in
the repository is inside `pattern.rs`'s own tests or `authority_laws.rs`'s test-only alphabet.
This is what makes bug 4b's guard and the non-ASCII/unparseable-class conservatism free in
practice today, not just theoretically safe: there is currently no real pattern this fix makes
*more* conservative than before for any live call site (`segment_implies` previously mishandled
`[...]` in the unsafe direction only — see the false-positive claim above — never the safe one).
That's a fact about today's data, not a guarantee; the identity short-circuit in `implied_by` (see
"The fix" above) means the one law this new conservatism could otherwise have put at risk —
reflexivity, `p.implied_by(p)` — holds unconditionally regardless of what future pattern data
looks like, so this isn't relied on to keep the algebra's laws intact going forward, only cited as
evidence the current conservatism costs nothing today.

## Verification

- `cargo test -p tm-types` (unit tests, including three new regression tests —
  `subset_does_not_vacuously_absorb_a_trailing_double_star`,
  `subset_respects_the_trailing_double_star_bare_prefix_disjunct`, and
  `subset_does_not_trust_a_messy_stripped_prefix` — plus the committed
  `exhaustive_differential::exhaustive_no_false_positive_up_to_length_2`) and
  `cargo test -p tm-types --test authority_laws` (the 12-law proptest suite) pass: 84 unit tests,
  12 proptest laws, 0 failures.
- The exact originally-reported triple was confirmed to fail on the pre-fix code and pass on the
  final post-fix code via a direct, non-random check.
- `pattern_subset_is_matching_safe` was run repeatedly with fresh random seeds against the final
  code (multiple rounds, including runs at 1,000 and 2,000 cases each) with zero failures. Note:
  an earlier, larger random run at 20,000 cases was executed against the **pre-fix** code
  specifically to characterize the bug's (low) random-detection rate, not as post-fix evidence —
  see "What this cost to verify" above for why that distinction matters here.
- The `exhaustive_differential` check was run at pattern/path length 3 by hand in `--release`
  against the final code: 0 violations across all 3,514,734 triples where `implied_by` held (out
  of an ~88M triple space). This alphabet does not exercise bug 3's messy-slash shape (see above);
  that shape is covered only by the hand-written `subset_does_not_trust_a_messy_stripped_prefix`
  regression test.
- `mise run verify` (fmt check, `clippy --workspace --all-targets -D warnings`,
  `cargo test --workspace`, hygiene) passes with 0 failures across 2,983 passed tests
  workspace-wide (4 pre-existing, unrelated ignores).

### Bug 4 (follow-up) verification

- The exact originally-reported triple (`is_subset_of(["[ab]"], ["????"])`) was confirmed to
  return `true` against the pre-follow-up code and `false` against the fixed code, via a direct,
  non-random check (a scratch test run against a temporary copy of the pre-fix file, not just
  read-through reasoning about the old byte-level code).
- Seven new regression tests in `crates/tm-types/src/pattern.rs`
  (`subset_reasons_about_a_character_class_not_its_source_bytes`,
  `subset_recognizes_a_character_class_implied_by_question_mark`,
  `subset_recognizes_real_character_class_containment`,
  `subset_never_approves_an_escape_through_a_character_class`,
  `subset_does_not_let_a_slash_crossing_class_escape_through_star_or_question`,
  `subset_is_conservative_about_a_class_split_by_segment_slicing`, and
  `subset_is_reflexive_even_through_an_unparseable_non_ascii_class`) cover: the original report;
  cases that should — and do — hold through a class (a class implied by `?`, a narrower class or
  literal implied by a wider class, negation) so the fix isn't just conservative to the point of
  uselessness; the bug 4b slash-crossing shape found by the first adversarial review pass; and,
  after bug 4c's fix, that reflexivity holds through both an unparseable-by-slicing class and an
  unparseable non-ASCII class specifically (not just "most patterns," which the identity
  short-circuit could satisfy vacuously if these two exact cases weren't each checked directly).
  `cargo test -p tm-types` (unit tests) now passes 91 (84 + these 7), still 0 failures.
- `exhaustive_differential`'s alphabet gained `?`, `????`, `[ab]`, and a mixed slash-crossing
  shape (`x[!a]y`/`x*y`/`x?y`, alongside single-character path literals `x`/`y`/`a`/`b`) at the
  same length-2 bound as before. `????` specifically, not just `?`, is what makes this alphabet
  addition actually exercise the originally-reported bug: this check only ever flags a false
  *positive* (it skips every pair where `implied_by` is already `false`), and `[ab]` vs. `?` alone
  is a false-*negative* shape (bug 4's "should hold" case, structurally invisible to this check
  regardless of alphabet) — confirmed directly by re-running this exact alphabet against a
  temporarily-restored pre-fix `segment_implies`: it reports 316 violations, including
  `("[ab]", "????", "a")` and `("[ab]", "????", "b")`, the originally-reported shape exactly.
  Separately, the mixed slash-crossing segment matters for bug 4b specifically: a class-only
  alphabet entry that never shares a *segment* with a `*`/`?` entry can never exercise that
  asymmetry (the same lesson bug 3 already drew about this alphabet being the thing in question,
  applied to itself a second time). `exhaustive_no_false_positive_up_to_length_2` still runs in
  about 1.3s in a debug build (up from well under a second) and found 0 violations against the
  fixed code.
- `pattern_subset_is_matching_safe` (the random proptest) was re-run multiple times against the
  fixed code with 0 failures — expected to remain a weak detector for this specific bug class,
  same as bugs 1/2/3: its own generator alphabet (`crates/tm-types/tests/authority_laws.rs`'s
  `SEGMENTS`) still contains no `?` or `[`, and extending it was out of this follow-up's scope
  (the task was to extend the *exhaustive* check; the proptest generator is a separate piece of
  infrastructure the original three fixes also left untouched).
- `mise run verify` equivalent (`cargo fmt --all`, `cargo build --workspace -j 2`,
  `cargo clippy --workspace --all-targets -j 2 -- -D warnings`, `cargo test --workspace -j 2`,
  `cargo run -p xtask -- hygiene`) passes with 0 failures, 0 clippy warnings, and a clean hygiene
  scan: 2,992 tests passed workspace-wide, 0 failed, 4 ignored (the same pre-existing, unrelated
  ignores noted above).
