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

## Known separate finding, not fixed here

`segment_implies` (the character-level half of the comparison) treats `[...]` glob character
classes in `p` as literal bytes rather than a class match, e.g.
`is_subset_of(["[ab]"], ["????"])` returns `true` while `PatternSet::parse(["????"]).matches("a")`
is `false` (`[ab]` is one character; `????` requires four, and `segment_implies` matches each `?`
against one of `[`, `a`, `b`, `]` positionally rather than reasoning about the class). This is the
same false-positive class as bugs 1 and 2 above, confirmed by direct check, and is invisible to
both the random proptest and `exhaustive_differential` as committed, because neither's alphabet
contains `?` or `[`. Left unfixed and unscoped here — it needs its own investigation into how
`[...]` classes should structurally compose (their contents are not currently validated as a
proper class by this code at all, only by `globset` at `PathPattern::new` time) rather than a
hasty bolt-on next to two unrelated fixes.

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
