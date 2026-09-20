# D-003 — Project scope: workspace vs. state, and where bare `tm` writes

**Status:** accepted · **Date:** 2026-09-19 · **Supersedes:** nothing

## Context

Before this decision, a Ticketmaster project had exactly one location: `<root>/.tm`, where `root`
was wherever a human happened to run `tm` from (or the nearest ancestor with a `.tm` directory
already in it). Bare `tm` — the zero-setup entry point meant to compete with `claude`/`codex`'s
"just run it" bar — auto-bootstrapped that directory the moment it found none: `tm init`'s
equivalent for an empty directory, or a full `tm attach` assimilation (history indexing, doc
discovery, a `T-001` investigation ticket) for an existing git repository. That is the single most
user-visible problem this decision fixes: running bare `tm` in `~/scratch` to ask it one question
wrote a `.tm/` directory (`project.db`, possibly `index.db`, possibly a git-history index) into
`~/scratch`, un-asked. Worse, in a real repository with committed history, it silently ran a full
assimilation — real disk writes, real indexing work, a new ticket — before the human had said
anything at all beyond "run `tm`".

The fix requires a place for that state to go that is not the workspace. This decision defines
that place and the resolution order every command uses to find it.

`docs/audit-2026-09-18-fable.md`'s Phase 1-A (commit `0d044b2`) already did the kernel-level
groundwork: `tm-core::Store`, `tm-codeintel::CodeIntel`, and `tm-server`'s `AppState` all gained an
explicit state directory distinct from the workspace root they used to conflate, with `_at`-suffixed
constructors taking it directly and the old constructors surviving as one-line `.tm`-joining shims.
This decision (Phase 1-B) is what actually uses that seam: the CLI-level scope split, resolution,
and visibility.

## Decision

A project now has three location facts instead of one:

- **`root: PathBuf`** — the *workspace*: the git toplevel (`git2::Repository::discover(cwd)`'s
  `workdir`) when the current directory is inside a repository, else the canonical current
  directory. Unchanged in meaning from before this decision for everything that was always
  workspace-relative and stays that way: `tm-codeintel` indexing targets, `templates.toml`,
  `bench/tasks/`, `browser.toml`, the web client build directory, and wiki docs output.
- **`state_dir: PathBuf`** — where a project's durable state actually lives: `project.db`,
  `index.db`, `artifacts/`, `harness.toml`, `mirror.toml`, `workflows/`, `bench/` (the report
  output side, not the task definitions), `sched.paused`, `browser-session.json` — everything that
  used to be assumed to live under `<root>/.tm`. In **repo scope** it still does:
  `state_dir = <root>/.tm`. In **global scope** it is `$TM_HOME/projects/<key>/`, entirely outside
  the workspace.
- **`scope: Scope`** — `Repo` or `Global`, naming which of the above applies.

**`$TM_HOME` layout.** `$TM_HOME` defaults to `$HOME/.tm` (the `TM_HOME` env var overrides it) —
the same convention `tm-browser`'s `managed.rs` already used for `~/.tm/browsers`. A global
project's directory is `$TM_HOME/projects/<key>/`, where `key = sanitize(root) + '-' +
<first 8 hex chars of blake3(canonical root path bytes)>`; `sanitize` strips a leading `/`, maps
`/` and every character outside `[A-Za-z0-9._-]` to `-`, collapses runs of `-`, and truncates to
100 characters — e.g. `/Users/allie/Develop/foo` → `Users-allie-Develop-foo-3f9a1c2b`. The
directory holds `project.db`, `index.db`, `artifacts/`, and a `workspace.json` marker
(`{"workspace": "<canonical root>", "created_at": <ts>, "schema": 1}`) recording which real
workspace it belongs to, since the sanitized name alone is lossy.

**Resolution.** One function, `resolve_scope(explicit, cwd)`, used by every opener:

1. `--project P` → `Repo { root: P, state_dir: P/.tm }`, always `exists: true` (unchanged:
   still creates-if-absent, exactly as `--project` always has).
2. Walking up from `cwd` finds a `.tm/` directory (`locate`, unchanged) → `Repo` there,
   `exists: true` — **except** when that `.tm/` is the real `$HOME/.tm` itself (the default global
   `TM_HOME`), which `locate`'s generic walk-to-`/` would otherwise reach and misresolve as an
   ordinary repo rooted at the user's home directory. This exclusion compares against
   `dirs::home_dir()` directly, not `tm_home()` (which reads the overridable `TM_HOME` env var) —
   a test that overrides `TM_HOME` to a hermetic tempdir doesn't change what `locate` actually
   walks on disk, so the guard has to be anchored to the real home directory to still catch it.
3. `$TM_HOME/projects/<key>/project.db` exists for this workspace → `Global`, `exists: true`.
4. Otherwise → `Global`, `exists: false`.

Two openers consume that resolution differently:

- **`open_for_command`** — every explicit subcommand (`tm status`, `tm ticket list`, ...). Errors
  `NotFound` when `exists` is `false`. Subcommands never create state in either scope; a user who
  typed a specific subcommand already knows enough to run `tm init` first.
- **`open_bare`** — bare `tm` only. When resolution lands on `Global` with `exists: false`, it
  silently creates the global directory (opens the store there, writes `workspace.json`) and
  prints nothing — no announcement, no side-effect note the way the old bootstrap printed one.
  Otherwise it just opens whatever `resolve_scope` found. It never assimilates a git repository,
  never creates a `T-001` investigation ticket, and never writes into the workspace.

**Promotion (Phase 1-C).** `tm init` (and `tm attach`/`tm genesis`'s creation path) now checks for
an existing global session keyed by `workspace_root_for(dir)` before falling back to a fresh
`create_project_dir`. When one exists, `promote_global` moves it into `<dir>/.tm` in three steps:
verify the source's hash chain first (hard stop before anything destructive), copy `project.db` via
SQLite's online backup API (`rusqlite::backup::Backup`, WAL-safe — never a raw `fs::copy` of a
possibly-open database) plus an explicit allowlist of state extras (`artifacts/`, `harness.toml`,
`mirror.toml`, `sched.paused`, `workflows/`, `bench/`), then independently re-verify the
destination (chain, invariants, event/ticket-count equality against the source) before treating the
promotion as real. On any failure past the copy step, the half-built destination is removed and the
source is never touched. On success, the source's `project.db`/`index.db` and copied extras are
deleted (leaving it `exists: false` for any future `resolve_scope` call against that workspace),
but `workspace.json` survives alongside a new `promoted.json` marker so `tm project list` still
shows the workspace with a `promoted: {to, at, events}` pointer instead of the row silently
vanishing. `--fresh` on `tm init` skips promotion entirely and leaves the global session untouched,
for the rare case of deliberately wanting a second, separate repo-scoped project.

One `tm-core` gap this surfaced, not fixed by this track: `Store::rebuild()`'s doc-commented claim
of byte-identical event-log replay does not hold for artifacts — `ArtifactCreated`'s `hash`/
`bytes_len` are written via a side-channel raw-SQL update in the same transaction as the event
append, and pure replay has no way to redo that. `verify_promotion` works around it by running
`rebuild()`+`check_invariants()` against a throwaway scratch copy of the destination (proving the
event log itself replays cleanly) and `check_invariants()` alone (non-mutating) against the real
destination, rather than rebuilding the real store in place. Fixing `rebuild()` itself is separate,
pre-existing `tm-core` work, independent of scope/promotion.

**`tm attach` and `tm genesis` stay repo-level, unchanged.** Both are explicit, deliberate acts of
creating a *repo-scoped* project — `attach` assimilates a specific repository, `genesis` seeds a
brand new one from a prompt — not the "I might just be asking a question" bare-`tm` path this
decision is about. Routing either through global-scope resolution would be solving a problem
neither command has: nobody who typed `tm attach` is confused about whether they meant to create
project state.

## Why

The old auto-bootstrap conflated two different things a human might mean by running bare `tm` in a
directory: "treat this as a project, starting now" and "let me just ask something, I don't
necessarily want a project here." It optimized for the first at the expense of the second, and
silently — no confirmation, no dry-run, no undo. A tool that writes to disk and runs a real
assimilation before the user has typed a single word is not a good default, however convenient it
is once you've opted in.

Global scope resolves that without giving up the "just run it" bar: bare `tm` still works
immediately in a directory with nothing in it, but the state it creates lives somewhere the human
would have to go looking for (`$TM_HOME`), not somewhere they'll trip over (`git status`, `ls`,
their own repository). `tm project show`/`tm project list` make that state discoverable on demand
rather than either hidden entirely or forced into view.

Deriving the workspace root via `git2::Repository::discover` (falling back to the canonical cwd)
rather than always using the literal invocation directory means the same repository always hashes
to the same global project key, regardless of which subdirectory a human happens to be standing in
when they first run bare `tm` — the same property `.git`-discovery already gives every other git
tool.

## What this costs, stated plainly

**A relocated project's recorded artifact paths go stale, and there is no rewrite.** The event log
is the one thing in this system that is never mutated after the fact (`SPEC.md`'s hash-chain
invariant) — an `artifact.created` event's `OnDisk` storage path is whatever was true at write
time, forever. Promotion (Phase 1-C) moves a project's `state_dir` from `$TM_HOME/projects/<key>/`
into `<root>/.tm`, which makes every previously recorded absolute artifact path wrong the instant
the move happens; there is no event to append that fixes history, and there should not be
(rewriting the log to "correct" it would break the hash chain it exists to protect). Phase 1-A's
kernel seam anticipated this: `Store::view()` carries a read-time fallback — an `OnDisk` path that
no longer exists on disk resolves instead to `<state_dir>/artifacts/<hash>`, recomputed from the
artifact's content hash against the *current* `state_dir` rather than the one recorded at write
time. That is a deliberate trade: a small, permanent bit of indirection on every artifact read, in
exchange for never rewriting the durable log to paper over a directory move. Phase 1-C's own tests
exercise this path for real (a spilled artifact surviving a promotion and reading back correctly
through the fallback).

**Two things to remember, not one.** Every place in this codebase (and every place in a human's
head) that used to say "the project directory" now has to say which of `root` or `state_dir` it
means. Getting that wrong in either direction is a real bug class: using `root` where `state_dir`
was meant writes into the workspace again (the exact thing this decision exists to stop); using
`state_dir` where `root` was meant breaks code indexing, template resolution, or bench task
discovery, none of which have any business following a project out of the workspace.

**A newly-invisible failure mode.** Before this decision, "no project" was a loud, immediate error
on every command. Now bare `tm` can quietly succeed in global scope in a directory the human never
intended to treat as a project at all — the `tm project list`/`tm project show` surface and the
scope line printed at the top of every interactive session (plain loop and TUI alike) are the
mitigation, not a complete fix: a human who never looks at either can still accumulate global
projects under `$TM_HOME` for directories they only meant to poke at once.
