# D-015 — `docs/decisions/*.md` is the wiki's decision source, not `tm_core::Decision`

**Status:** accepted · **Date:** 2026-09-20 · **Supersedes:** nothing

## Context

`SPEC.md` §26.2/§26.3 specified the wiki's "decision history" page family as derived from
`tm_core::Decision`/`DecisionId` — a `Store`-backed entity created via `tm decision new` and
chained via `tm decision supersede`, with `crates/tm-wiki/src/decisions.rs` reading
`Store::view().decisions` and rendering `decisions/<id>.md` pages plus a `decisions.md` index. In
practice, this repo has never used that path for an architecture decision record: `CLAUDE.md`'s
"Keep documentation honest as you change things" section has said, since before this repo had a
wiki generator at all, "A new/changed decision gets a `docs/decisions/D-NNN-*.md`" — hand-authored
markdown, numbered by hand, with no `Store` entity behind it. By the time this was resolved, 14 such
files existed (`D-001` through `D-014`), and `docs/wiki/decisions.md` rendered "No decisions
recorded yet" despite that, because it was reading an empty map: nothing in this repo calls
`tm decision new` for an architecture decision, and running the compiled `tm` binary against this
repo's own checkout to populate one is exactly the contamination incident
`docs/decisions/D-003-project-scope.md` and `.claude/skills/dispatch-background-agent/SKILL.md` both
treat as a failure mode to avoid, not a normal workflow step.

`docs/backlog.md`'s "Open decision" entry for this (now resolved, see that file) named the concrete
cost of leaving it unresolved: this same working session hit five separate `D-NNN` numbering
collisions (`D-004`, `D-008` ×2, `D-010` ×2, `D-012`) from parallel tracks each independently
choosing "the next free number" from a directory listing with no shared source of truth, plus five
stale `D-NNN` cross-references left behind by a rename — the exact class of problem a real,
queryable `DecisionId` registry exists to prevent. That cost is real, but it is a cost of *not
having a registry*, not evidence that the registry has to be `tm_core::Decision` specifically; the
xtask hygiene check added for the cross-reference half of that problem
(`crates/xtask/src/hygiene.rs`'s dangling-`D-NNN` check) already mitigates it without touching
`Store` at all.

## Decision

`tm-wiki`'s decision-history page family reads `docs/decisions/D-NNN-*.md` directly
(`crates/tm-wiki/src/decisions.rs`): it parses each file's title line (`# D-NNN — <title>`) and its
`**Status:**`/`**Date:**`/`**Supersedes:**` metadata line (accepting both real, in-use shapes — a
single inline line separated by `·`, and D-001's three separate bullet lines), builds the
supersession chain from each file's own `Supersedes:` field, and renders one
`decisions/<file-stem>.md` page per decision (linking back to the real
`docs/decisions/D-NNN-*.md` file rather than duplicating its prose) plus a `decisions.md` index.
`decisions::pages` takes a project root path, not a `&ProjectView`/`&Store` — it is purely
filesystem-driven, matching the "walk real files, extract structure" pattern
`architecture::pages`/`history::pages` already use in the same crate, not the `Store::view()`
pattern `tickets::page`/`glossary::page` use. `SPEC.md` §26.2/§26.3 are corrected to describe this
mechanism instead of the `Decision`/`DecisionId` one.

`tm_core::Decision`/`DecisionId` and `tm decision new`/`tm decision supersede` are **not** removed
or changed. They remain a real, working mechanism — just no longer the wiki's source for this page
family. Whether they still have a genuine use (ticket-level decision tracking distinct from an
architecture decision record) or are now effectively dead code is a separate, bigger call this
decision deliberately does not make; see "What this costs" below.

## Why

Two candidate resolutions were weighed:

1. **Make the wiki read `docs/decisions/*.md` directly** (chosen), correcting `SPEC.md` to match
   the convention this repo actually uses.
2. **Migrate to real `tm_core::Decision` entities**, correcting `CLAUDE.md`'s convention to say
   "run `tm decision new`" instead of "write a markdown file," and importing the 14 existing docs
   as `Decision`s.

Option 2 was rejected because it does not actually fix the problem it looks like it fixes. The
numbering-collision incidents above happened *because* agents write decision docs by hand during
in-progress, parallel work on tracks that have no `.tm/` project of their own to record a `Decision`
into — migrating would mean either running `tm decision new` against this repo's own checkout (the
contamination D-003 exists to prevent) or inventing a second, out-of-band way to create a `Decision`
without a project, which is a bigger, uglier change than it sounds for a problem that markdown files
plus one hygiene check already solve well enough. Markdown decision docs are also strictly more
useful as this repo's own documentation artifact: they are plain files, readable on GitHub and in
any editor with no server or `.tm/` state running, exactly the property `SPEC.md` §26.4 wants for
the wiki's in-repo access mode generally. Option 1 keeps that property instead of trading it away.

## What this costs, stated plainly

**No structural enforcement of the metadata format.** A `Store`-backed `Decision` cannot have a
malformed `supersedes` field — the type system and `Store::supersede` enforce it. A hand-written
`**Supersedes:** D-003` is just a string. `crates/tm-wiki/src/decisions.rs`'s parser is
deliberately permissive about it (see `parse_decision_doc`'s doc comment): a file that doesn't
parse — missing title, missing `**Status:**`/`**Date:**` — is silently skipped from the wiki rather
than failing the whole generation run, matching this crate's existing convention
(`history::pages`'s "no git history for this path, skip it" precedent) but meaning a typo'd
metadata line drops that one decision from `docs/wiki/decisions.md` with no warning at all. A
`Supersedes:` field naming a `D-NNN` that does not resolve to a real file is silently
un-connected — no error, no dangling-link check. `xtask::hygiene`'s dangling-`D-NNN` check does not
cover this either: it checks that a `D-NNN` *token* resolves to a real file anywhere it appears, but
does not specifically validate that a `Supersedes:` field's value is one. Extending that check (or
adding a sibling one) to validate `Supersedes:` fields specifically would close this gap; it was not
done here, and is a reasonable, scoped follow-up for whoever next touches
`crates/xtask/src/hygiene.rs`.

**Two decision docs sharing a number degrades rather than fails.** This is the exact collision
class `docs/backlog.md`'s resolved entry names happening five times in one session. Nothing in
`decisions::pages` detects it: `load_decisions` builds `by_id: HashMap<String, &DecisionDoc>` by
inserting in sorted-by-id order, so the second file with the same `D-NNN` prefix silently loses —
`decisions.md`'s index still lists a row for each file (the index loop iterates the plain
`Vec<DecisionDoc>`, not `by_id`), but the loser's link points at a per-decision page that was never
rendered (`render_page` looked it up by id and got the winner instead), a dead link with no error
anywhere. `xtask::hygiene`'s dangling-`D-NNN` check does not catch this either — both files' names
resolve to a real number, just the same one. This is not a regression from the `Store`-backed
design (a real `DecisionId` would have made the *second* `tm decision new` call fail outright,
which is a real advantage that design had and this one gives up), and is worth a scoped follow-up:
either `load_decisions` erroring loudly on a duplicate id, or a dedicated hygiene check for it.

**Two coexisting metadata formats, permanently.** D-001 (this repo's first decision doc, written
before the convention settled) uses three bullet lines; every decision since uses one inline line.
The parser has to accept both indefinitely, since D-001 is not going to be reformatted just to
simplify a parser — one more small, permanent bit of format tolerance this file has to carry.

**Two more places in this crate still assume the abandoned `Store`-backed path, unfixed by this
decision.** `crates/tm-wiki/src/glossary.rs::page` still reads `view.decisions` for its own
"## Decisions" section (cross-linking to a `decisions/<id>.md` path shape that no longer matches
what `decisions::pages` actually generates) and still tells an empty project to
`` `tm decision new` `` in its empty-state message; `crates/tm-wiki/src/generate.rs::default_history_paths`
still seeds its `docs/decisions/`-derived history-page default from `view.decisions`'s
`affected_paths` rather than the real files. Neither gets any worse from this decision — both were
already reading a map that is empty in every real use of this repo — but neither is fixed by it
either. Fixing them would mean duplicating `decisions.rs`'s file-loading logic into two more call
sites for comparatively little gain (an empty-state message and a rarely-hit history-page default);
left as a known, explicitly-flagged gap rather than folded into this change.

**Whether `tm_core::Decision`/`tm decision new`/`tm decision supersede` are still worth keeping is
now a live, unanswered question.** This decision does not delete or change them, deliberately — that
is a separate, bigger call about whether ticket-level decision tracking is a real, wanted feature on
its own merits, not something to decide as a side effect of fixing the wiki.
