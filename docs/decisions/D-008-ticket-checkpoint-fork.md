# D-008 — Ticket checkpointing: `tm ticket fork <T> --at <seq>` and per-turn workspace snapshots

**Status:** accepted · **Date:** 2026-09-20 · **Supersedes:** nothing

## Context

`docs/audit-2026-09-18-fable.md`'s M-04 ("Ecosystem table stakes") flags checkpointing as a real,
high-value gap: 5 of 12 competitor tools ship it, and the competitive survey calls it "consistently
... the top trust and control feature" — the ability to rewind or branch a session without losing
history. `docs/backlog.md` independently lists it twice ("Checkpoints, distinct from git" and
"Checkpoints and forking from the event log"), the second entry specifically framing it as a place
this project can win outright: "Everyone else bolts a snapshot mechanism onto a transcript. Ours
falls out of replay." A grep for `checkpoint` across `crates/` before this change found exactly one
hit, unrelated genesis prose (`stages.rs:431`) — nothing like this existed.

The audit's "done looks like" line names two distinct things: `tm ticket fork <T> --at <seq>`
replaying a ticket's events into a new ticket, and a per-turn `git stash`-style snapshot artifact.
This is explicitly **not** about git's own commit history — it is about rewinding/branching a
ticket's own durable event-log state, the thing this project already has that a transcript-based
competitor does not.

Two real design questions had to be settled before writing any code, both because the underlying
log is append-only and single-chain (`tm-events::log::EventLog`, `SPEC.md` §3.1):

1. **What does "fork" mean when you cannot literally replay a ticket's own event stream under a
   new id?** Every event in the log chains its hash onto the one before it, in one total order,
   for the whole project — there is no way to "detach" a subsequence of events, retarget their
   `subject`, and splice them onto a different point in the chain without breaking the hash chain
   the log exists to protect (see `docs/decisions/D-003-project-scope.md`'s own "what this costs"
   section for the same discipline applied to a different problem: artifact paths going stale
   after promotion — the durable answer there was "never rewrite the log, add a read-time
   indirection instead," which is the same instinct this decision follows). Naively re-appending
   copies of `T`'s own events under a new id would also double-count anything with a side effect
   (a lease acquired, a budget debited, provider usage recorded) — those events describe things
   that already happened to `T`, not things that should happen again to a new ticket that has done
   nothing yet.
2. **Is there already a way to compute a project's state as of a past `seq`?** No.
   `Store::rebuild()` replays the *whole* log from `seq` 0 to reconstruct current state (used after
   e.g. a schema change); `Store::view()` reads current materialized state. Neither is bounded.
   `crates/tm-cli/src/ops.rs`'s `tm events replay` command's own doc comment already claims to
   "replay them through `tm_core::materialize::replay` against a scratch in-memory view" — but the
   actual implementation is a stub that only counts events and never calls into `tm-core` at all.
   So the "bounded replay to a past point" primitive this decision needs did not exist yet, despite
   a comment implying it did.

## Decision

### `Store::fork_ticket(source, seq, actor) -> (TicketId, Vec<Event>)`

**What "fork" means:** compute `source`'s materialized [`Ticket`] and (if it has one)
[`GoalState`] exactly as they stood after replaying the project's log through `seq` inclusive, then
append a **brand-new** ticket lineage carrying that computed state as its *starting* definition —
never replaying `source`'s own event rows a second time.

**The bounded-replay primitive** (`Store::ticket_and_goal_as_of`, private): reads events `[1,
seq]` via `EventLog::read_range`, then replays them into a throwaway, file-backed scratch SQLite
schema (the same `tempfile::NamedTempFile` + `EventLog::open_with_clock` + `schema::create_views`
technique `crate::materialize`'s own tests already use to exercise `apply` against a real schema
without a live project) via `crate::materialize::replay` — the identical function both the live
append path and `Store::rebuild`'s full-log replay go through. "State as of `seq`" is therefore
derived by the *same mechanism* as "current state," just bounded, which is what makes it
trustworthy: there is no second, parallel implementation of what an event means that could drift
from the real one. The scratch file is discarded when the call returns; the live project's own
`project.db` and materialized tables are never touched. Out-of-range `seq` (`0`, or greater than
the log's current head) is a hard `TmError::invariant`, not silently clamped to whatever the log
actually contains — a caller asking to fork from a point that cannot exist should not silently get
a fork from HEAD instead.

**The event shape**, chosen to reuse as much of the existing catalogue as possible rather than
inventing new machinery:

1. `ticket.created` + `ticket.updated` for the new ticket id — **the exact same two-event
   convention `Store::create_ticket` itself already uses** (per `store.rs`'s own module note: the
   payload catalogue has no room for a full ticket in one event). The `ticket.updated` fields
   object is populated from `source`'s computed snapshot (`kind`, `milestone`, `authority`,
   `resources`, `executor`, `context_refs`, `success`, `verification`, `budget`, `retry`,
   `priority`) instead of caller-supplied values. This means the existing, already-tested
   `apply_ticket_updated` materializer arm does all the real work — zero new materializer code for
   the ticket-shape part of a fork.
2. A new, purpose-built `ticket.forked` event: `{ ticket: <new>, source: <T>, source_seq: <seq> }`.
   Pure provenance, nothing else — carrying no ticket-shape fields itself, since those already
   landed via (1). This is what makes "this ticket was forked from `T` at `seq`" itself part of the
   durable, hash-chained history rather than a side artifact: it is a real event, at a real `seq`,
   chained like everything else, recoverable via `tm events show`/`EventLog::read_subject` for as
   long as the log exists.
3. If `source` had a goal (`crate::goal::GoalState`) as of `seq`: fresh `goal.set` +
   `goal.step_added` (one per step, in order) + `goal.step_completed` (for steps already marked
   done) events for the new ticket, computed from the snapshot — not copied event rows. This is
   the concrete answer to "not literally replaying `T`'s own event stream a second time": these are
   brand-new events, attributed to the new ticket, encoding only the *current computed values* from
   the snapshot, never a second copy of `source`'s own `goal.reoriented`/intermediate
   `goal.step_added` history.

All of the above is appended in one `Store::transaction` call, so a fork either lands completely —
new ticket, provenance, goal reconstruction — or not at all.

**What the new ticket deliberately does *not* inherit**, and why: `state` (always starts
`TicketState::Draft`, like every other ticket — a fork earns its own state transitions through the
real machine rather than being teleported into `source`'s historical position, which would let a
fork skip lease/authority checks a normal ticket can't); `parent`/`children`/`dependencies` (a
fresh lineage starts with none — copying `source`'s edges would make the fork a structural
duplicate entangled with tickets that have no idea it exists); `attempts`/`failures`/`cycle` (these
describe `source`'s own execution history, not a definition a new lineage inherits); the goal's
`last_reoriented_step` and `claimed_complete` flag (both are loop-internal bookkeeping about a
specific *run*, not part of the goal's definition — inheriting `claimed_complete: true` in
particular would let a fork that has done zero real work of its own present as already-finished,
which is actively misleading).

### Per-turn workspace snapshot (`tm-scheduler::snapshot::capture_workspace_snapshot`)

**What it captures:** `git stash create` in the repository root — a real porcelain command that
builds a commit object holding the current index/working-tree state relative to `HEAD`
**without** touching either (unlike `git stash push`, which resets both). The resulting commit is
otherwise unreachable from any ref the instant the process exits, so this also runs `git
update-ref refs/tm/snapshots/<sha> <sha>` to pin it — without that, the very next `git gc` could
reap the only copy. The sha (plus the pinning ref and best-effort `git rev-parse HEAD`) is recorded
as a new `ArtifactKind::WorkspaceSnapshot` artifact via the existing `Store::store_artifact`, tied
to the ticket, the same durable-artifact machinery `ArtifactKind::Patch`/`Report`/etc. already use.

**Why shelling out to `git`, not the `git2` bindings already in this workspace** (used read-only by
`tm-codeintel`/`tm-context`/`tm-genesis`/`tm-wiki`): libgit2's `git_stash_save` binding is
equivalent to `git stash push`, not `git stash create` — it has no separate "build the commit
object without touching the working tree" mode. Reproducing `git stash create`'s exact porcelain
behavior (merge-commit parents, the third "untracked" tree when relevant, index vs. working-tree
diff construction) via `git2`'s lower-level plumbing (`TreeBuilder`, `Repository::index()`,
`write_tree`, `commit_create`) would mean hand-rolling a second implementation of logic git's own
C source already gets right — not a good trade for a project whose SPEC §0 philosophy is to keep
new machinery boring. `tm-agent::tools::run_fixed_git` already establishes shelling out to real
`git` subprocesses as this codebase's convention for exactly this class of operation
(`git status`/`diff`/`log`/`commit`/`checkout -B`/`worktree add`); this follows it.

**Deliberately infallible from the caller's perspective**
(`capture_workspace_snapshot(&Path) -> Option<WorkspaceSnapshot>`, never `Result`): a snapshot is
auxiliary to the turn that produced it, never load-bearing for the ticket's own submission. No
`git` on `$PATH`, `repo_root` not being a git repository, or any other failure is logged via
`tracing::warn!` and treated as "nothing to capture" — it must never turn a successful turn into a
recorded failure.

**Where it is wired:** `ExecutorDispatcher` (`crates/tm-scheduler/src/dispatch.rs`) grows a new
`repo_root: Option<PathBuf>` field, threaded through `spawn_run` → `run_and_report` →
`report_outcome`. `report_outcome` is the scheduler's existing turn-completion path — it already
stores an `ArtifactKind::Patch` for a produced diff right before calling `Store::submit`; the
snapshot capture happens right alongside that, only when `repo_root` is `Some` and a real patch was
produced. `tm-cli`'s `build_dispatcher` (the one real construction site outside tests) passes
`Some(project.root.clone())`; the one test construction site
(`tm-scheduler/src/driver.rs`) passes `None`, so every existing test that builds a dispatcher
compiles and behaves unchanged — `None` is a deliberate, zero-regression default, not an oversight.

## Why

- Reusing `ticket.created`/`ticket.updated` for the fork's ticket-shape state means the existing,
  already-tested materializer arm does the real work, and the payload catalogue needed exactly one
  new addition (`ticket.forked`, pure provenance) rather than a second parallel way to describe a
  ticket.
- Computing "state as of `seq`" through the *same* `materialize::apply`/`replay` functions the live
  path and `Store::rebuild` already use — rather than writing a second, bespoke reconstruction —
  is what makes "at `--at seq`" trustworthy: there is only ever one definition of what replaying an
  event means.
- A fork starting in `Draft` rather than being teleported into `source`'s historical state keeps
  the ticket state machine's own legality guarantees intact for every ticket, forked or not — the
  alternative (materialize it directly into e.g. `Running`) would let a fork skip lease acquisition
  and authority checks a hand-created ticket in that state could never skip.
- Shelling out to `git stash create` rather than reimplementing its tree-construction logic against
  `git2`'s plumbing keeps the new code boring, in the spirit of `SPEC.md` §0 — this workspace
  already has one convention for "real git operations," and inventing a second, parallel one for
  this feature alone would be the kind of harness drift `CLAUDE.md`'s "keep improving itself"
  section calls out.

## What this costs, stated plainly

- **Fork provenance is durable but not (yet) a queryable column.** `ticket.forked` is a real,
  hash-chained event, but `tickets` (the materialized table) has no `forked_from`/
  `forked_from_seq` columns, so `materialize::apply`'s arm for it is a documented no-op — the same
  tradeoff `harness.changed`/`harness.benchmarked` already make in that file. "Was this ticket
  forked, and from what?" is answerable via `tm events`/`EventLog::read_subject`, not via `tm
  ticket show`. Adding those columns would mean a real migration in `tm-core::schema`'s forward-only
  migration list; deferred as a follow-up rather than folded into this change, since the durability
  guarantee the audit actually asked for (provenance survives in the hash chain) does not require
  it.
- **`Store::ticket_and_goal_as_of` pays a full bounded replay on every fork call**, proportional to
  `seq` — one scratch SQLite file created and torn down, one pass through every event from `1` to
  `seq`. This is the same cost model `Store::rebuild()` already accepts for "replay the whole log";
  bounding it at `seq` instead of the log's head does not change the shape of the cost, only how
  much of it you pay. Acceptable for an operation a human explicitly requests; would need a
  materialized point-in-time index if forking became a hot path on a very deep log.
- **A fork's goal reconstruction is a curated subset, not a full replay of the goal's own history.**
  It carries the goal's `text`, its step decomposition, and each step's `done` flag — deliberately
  *not* `last_reoriented_step` or `claimed_complete` (see Decision above for why). A future need
  to inherit re-orientation history specifically would be a real, separate design question, not an
  oversight to silently fix.
- **The workspace snapshot has real gaps, inherited directly from what `git stash create` itself
  does not cover:**
  - **Untracked files are never captured**, exactly like plain `git stash push` without
    `--include-untracked`. A turn that only creates new files with no other changes produces no
    snapshot at all. Reproduced as a real, asserted test
    (`snapshot::tests::untracked_file_alone_is_not_captured`) so a future change to that default is
    caught by a failing test, not discovered by surprise.
  - **The pinning ref is best-effort.** If `git update-ref` itself fails (e.g. a read-only `.git`),
    the function logs and still returns the captured sha — meaning a snapshot artifact can exist
    whose sha is not actually protected from `git gc` after all. This is a real, accepted gap, not
    a silent one: `WorkspaceSnapshot::git_ref` is always populated with the *intended* ref regardless
    of whether pinning actually succeeded, so this cannot be distinguished from a caller reading the
    artifact alone.
  - **Binary and submodule edge cases are exactly whatever real `git stash create` does with them**
    — this decision deliberately defers to git's own porcelain behavior rather than special-casing
    anything, per the "why shell out, not `git2`" reasoning above; any gap in `git stash create`
    itself is a gap here too.
- **Automatic per-turn capture is wired into exactly one path**: `tm-scheduler`'s
  `ExecutorDispatcher`/`report_outcome`, the scheduler-driven dispatch loop `tm sched run`/`tm run`
  use. `tm-agent::BuiltinExecutor`'s own mid-run `ticket.submit` tool (used when an executor submits
  from inside its own loop rather than returning a patch for the scheduler to store) does not
  independently trigger a snapshot — only the scheduler's own patch-storage branch does. A ticket
  submitted entirely through the in-loop tool path with no scheduler-observed `outcome.patch` gets
  no automatic snapshot. Manually invoking `capture_workspace_snapshot` from that path too is a
  reasonable follow-up, not done here to keep this change's blast radius to the one call site this
  decision's own investigation confirmed was safe to thread a new parameter through (two real
  construction sites for `ExecutorDispatcher`, both updated; zero others existed).
