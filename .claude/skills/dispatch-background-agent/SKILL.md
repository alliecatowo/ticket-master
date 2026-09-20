---
name: dispatch-background-agent
description: Use before dispatching a background or parallel implementation subagent against this repo (ticket-master) -- when deciding isolation mode, when a subagent needs to manually/ad-hoc test the compiled `tm` binary, when a subagent reports it is no longer sure which checkout it is in, or right before accepting a subagent's "done" report. Also load this when placing a #[cfg(test)] helper and unsure why `cargo run -p xtask -- hygiene` misclassified it.
---

# Dispatching a background implementation subagent in this repo

This codebase had three real, costly incidents in one working session from getting these things
wrong: a stray `.tm/` directory (once carrying a real 37MB semantic index) written into the
primary checkout by a subagent's own ad hoc testing, breaking the test suite for whoever ran it
next; two subagents that silently resumed work in the primary checkout instead of their assigned
worktree after a mid-task interruption; and confusing hygiene-check failures caused by
`#[cfg(test)]` placement. This skill is the checklist that prevents a repeat.

## 1. `isolation: 'worktree'` is mandatory for any subagent that writes code

Every `Agent()` call in this repo that will edit files must pass `isolation: 'worktree'`. There is
no "small enough change" exception -- the cost of a worktree is a few seconds of setup; the cost
of two agents editing the same primary checkout concurrently is silent data races between
unrelated work.

## 2. Never let a subagent run the real `tm` binary against the primary checkout

`tm` (this repo's own CLI, `crates/tm-cli`) used to auto-create a `.tm/` state directory the
moment it was run in a directory with no project yet -- see
`docs/decisions/D-003-project-scope.md` for the fix (global scope by default; bare `tm` no longer
writes into the workspace un-asked). That fix landed, but the *process* failure that triggered it
is still live: a subagent doing its own manual verification of a `tm`-touching change ran the
compiled binary directly against `/Users/allie/Develop/ticket-master` -- the orchestrator's own
primary checkout -- instead of an isolated tempdir. It happened three times in one session, once
leaving a 37MB real index behind, and broke the test suite for whoever ran `mise run verify` next
without knowing why.

**When briefing a subagent that will exercise `tm` (or any future compiled binary this repo
ships) beyond its own crate's `cargo test`:**

- Tell it explicitly: run the binary against a freshly created tempdir (`mktemp -d`, or Rust's
  `tempfile` crate, matching how this repo's own integration tests already do it -- see
  `crates/tm-e2e`), never against its worktree root and never against any path resolving to the
  primary checkout.
- If the subagent needs to test against something that looks like a real repo (git history,
  committed files), it should `git clone` this repo (or a fixture) into that tempdir, not `cd` into
  the worktree or primary checkout and run `tm` there.
- `.tm/` is gitignored, so a stray one won't show up in `git status` -- it has to be caught by not
  creating it in the first place, or by noticing it before it's inherited by the next session
  (see this repo's `.claude/settings.json` `PostToolUse` hook on `Bash`, which warns when `.tm/`
  exists in the primary checkout; it is a detector, not a preventer -- do not rely on it instead
  of the rule above).

## 3. If a subagent reports it's no longer sure it's in its own worktree: stop

Worktree isolation can silently fail to hold across a resume. Two subagents in one real session
were interrupted mid-task by a session-wide rate limit, resumed via a direct message to the same
agent, and resumed editing directly in the primary checkout -- their own final reports flagged it
only after the fact ("my original worktree was torn down mid-task and the environment repointed me
at the primary checkout"). Both times it resolved without data loss by luck, not by design.

If a subagent's report expresses *any* uncertainty about which checkout it's writing into:

1. **Stop it before any further write.** Do not tell it to "just continue."
2. Have it run `git rev-parse --show-toplevel` and `git branch --show-current` and report both
   verbatim, before touching another file.
3. Compare that toplevel path to the worktree path it was originally assigned
   (`.claude/worktrees/<name>`). If they don't match, it is in the wrong checkout -- treat every
   edit it made since the interruption as suspect and diff them against what the other
   concurrently active agents/orchestrator were doing in that same path before deciding what to
   keep.
4. Only resume once the toplevel path is confirmed correct.

This is cheap insurance: the check is two commands and a string comparison. Skipping it is what
turned "an agent got interrupted" into "two agents might have raced on the same files."

## 4. Do the work, then adversarially review it before reporting done

This is the one thing that worked well and is worth repeating as an instruction, not just a
finding: subagents that called `advisor()` (or otherwise did a deliberate self-adversarial pass)
*before* declaring their task complete caught real bugs that would otherwise have shipped --
an RFC 6749 refresh-token-dropped bug in an OAuth implementation, a leaked browser process from a
failed CDP handshake, a mismatch between listing-time and call-time authority gates. None of these
were caught by the original implementation pass; all were caught by a second, skeptical pass over
the same work.

Concretely: after implementing and before reporting done, ask "if I were reviewing this, not
writing it, what would I distrust?" -- error paths, resource cleanup on the failure branch, and
places where two pieces of code (a check at list time and a check at use time; a token issued and
a token refreshed) need to agree but aren't tested against each other are the highest-yield places
to look. If this session has an `advisor` tool available, use it for exactly this pass before the
final handback.

## 5. The hygiene checker's `#[cfg(test)]` gotcha

`crates/xtask/src/hygiene.rs`'s checks (no `SystemTime::now`/`rand` outside the clock substrate, no
mutation of the `events` table, no network in tests, no bare `unwrap`/`expect` outside tests) all
decide "is this line test code or production code" with a `TestRegionTracker`: a one-way latch
that flips permanently true at the **first** line in the file matching `#[cfg(test)]`, `#[test]`,
or `#![cfg(test)]`, and never flips back. This is a deliberate simplification (see the tracker's
own doc comment: brace-counting is unreliable because a brace inside a string or char literal ends
a region early), not a bug -- but it means:

- A `#[cfg(test)]`-gated helper placed **above** real production code later in the same file will
  cause the hygiene checks to treat everything after it as test code -- a genuine `unwrap()` or a
  real network call past that point will NOT be flagged, silently.
- Conversely, test code that appears **before** the file's actual `#[cfg(test)] mod tests` block
  (a `#[cfg(test)]`-only helper defined early, or -- more commonly -- code that merely *mentions*
  the string `#[cfg(test)]` in a comment before the real block) will make hygiene checks apply
  test-only exemptions too early.

**What to do:** put `#[cfg(test)]`-gated code at the bottom of the file, after all production
code, the way this file's own `mod tests` blocks already do. If a test-only helper genuinely needs
to live earlier in the file for readability, leave a comment at its `#[cfg(test)]` attribute
noting that everything below it in the file is now treated as test code by the hygiene scan, so
the next person doesn't have to rediscover this by debugging a confusing hygiene failure.

## 6. A verify-only agent in the primary checkout still races against your own git operations there

`isolation: 'worktree'` is mandatory for anything that *edits* code (rule 1) -- but a verify-only
agent (build/test/commit a change already sitting uncommitted in the primary checkout) has to run
there by definition, and that creates a different, narrower race: the orchestrator doing a `git
merge`/`git commit` in that same primary checkout while the agent's own `cargo test`/`verify` run
is still in flight. This happened for real: a Phase-1D merge landed on `main` mid-run of a
verify-only agent checking an unrelated one-line `xtask` fix. Nothing broke, but only because that
agent's own `git status --short` caught the transient `UU`/merge-in-progress state and it chose,
unprompted, to re-run the full verify gate against the new HEAD before committing rather than
trust the run that started under the old one.

**When dispatching a verify-only agent against the primary checkout:** either avoid doing your own
git writes there until it reports back, or explicitly tell it in the prompt that a concurrent
merge may land mid-run and it should re-run verify against the final HEAD before committing if it
notices `git status` in a mid-merge or otherwise-unexpected state rather than assuming its
in-flight result is still valid. Don't rely on it noticing this unprompted a second time.

## 7. Use `mise run` tasks, not hand-rolled cargo

`mise run verify`, `mise run hygiene`, `mise run clippy`, etc. already encode the right `-j 2` job
cap and `-p`/`--workspace` scoping for this machine (8GB, tight on disk). See the repo root
`CLAUDE.md` for the full list. A subagent that hand-rolls `cargo build --workspace` without `-j 2`
risks the same disk-space incidents that made the cap necessary in the first place.
