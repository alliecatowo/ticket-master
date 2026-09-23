# `docs/tasks/`: the working task list

This directory holds the consolidated, executable task list for ticket-master. It is written by a
whole-codebase audit and worked off by orchestration workflows, which check tasks off here as they
land.

- `TASKS.md` is the master list. It holds every open task from the audits
  (`docs/audit-*.md`), `docs/backlog.md`, and the proposed decision docs, deduplicated into
  **atomic** tasks. Each task names the files it edits, the change, an acceptance check, the exact
  test command, a model tier (haiku, sonnet, or opus), and its dependencies. Tasks are grouped into
  batches that touch disjoint files, with at most two Rust-building tasks per batch (this is an 8GB
  machine; see `CLAUDE.md`). Every batch ends in a gate: `mise run verify`, plus the web suite for
  `clients/web`.
- Tasks are marked `[x]` with the commit that landed them, or `[~]` with a reason when they're
  deferred. Don't delete finished tasks; the list is also the record of what was done.
- Items the owner has deferred or not yet approved (e.g. D-020, jev) stay in `docs/backlog.md`
  under "Ask the owner later", not here.
