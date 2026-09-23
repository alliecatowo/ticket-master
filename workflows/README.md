# Provider overhaul: isolated Luna workflow

`provider-overhaul.workflow.js` translates the prioritized deep audit into four
independent implementation tracks, then serial integration, a skeptical repair pass,
a first-time-user trial and a final repair/verification pass. The Open Dynamic Workflow
runner does **not** create worktrees, merge branches, or enforce memory limits for Rust.
Prepare the five worktrees below before running. Every editing agent checks its own
checkout path. The workflow has **at most two concurrent Luna calls**, and explicitly
forbids Rust builds until the sole integration worker runs `mise run verify` (`-j 2`).

The exact model is `openai/gpt-6-luna` through the `opencode` adapter. ODW 0.6.1's
OpenCode adapter does not understand OpenCode's `part.text` JSONL field, so
`scripts/odw-opencode-jsonl.mjs` streams the real CLI events and returns only the final
assistant answer. A read-only Luna adapter smoke call with the same schema/CLI was
validated before the workflow was committed. No credentials appear in the workflow.

## Run on this machine

From the primary checkout, with a clean tracked worktree:

```sh
# Worktrees branch from LOCAL HEAD, not origin/main. Existing user worktrees stay alone.
git worktree add -b workflow/odw-provider .claude/worktrees/odw-provider HEAD
git worktree add -b workflow/odw-config .claude/worktrees/odw-config HEAD
git worktree add -b workflow/odw-chat .claude/worktrees/odw-chat HEAD
git worktree add -b workflow/odw-dispatch .claude/worktrees/odw-dispatch HEAD
git worktree add -b workflow/odw-integrate .claude/worktrees/odw-integrate HEAD

npx -y -p @travisliu/open-dynamic-workflow@0.6.1 open-dynamic-workflow \
  validate workflows/provider-overhaul.workflow.js
npx -y -p @travisliu/open-dynamic-workflow@0.6.1 open-dynamic-workflow \
  run workflows/provider-overhaul.workflow.js --dry-run --concurrency 2
npx -y -p @travisliu/open-dynamic-workflow@0.6.1 open-dynamic-workflow \
  run workflows/provider-overhaul.workflow.js --concurrency 2 --max-agent-calls 8 \
  --timeout-ms 7200000 --no-retry
```

Artifacts go under the locally configured `/var/folders/.../opencode/odw-runs/`, **outside
the git repo** (prompts/outputs may contain private source). The worktree root strings
inside the workflow are absolute for this machine; update `root` and `outDir` for any
other checkout. Do not symlink `.env` into implementation worktrees. For real-provider
manual testing, if genuinely needed, deliberately link `.env` to ONE disposable fixture
root outside the repo; never put credentials in prompts or a worktree commit.

## Integrate and release

Inspect the run report (`finalCommit`, `remainingRisks`), `git status`/`git diff` in the
integration worktree, its log and the entire diff from the base. Only after its final
`mise run verify` is green and no release blocker remains:

```sh
git merge --ff-only workflow/odw-integrate
mise run verify
git status --short
git push origin main
# After inspecting the tag diff and remote CI, cut the next version deliberately:
# git tag v0.1.1 && git push origin v0.1.1
# gh run watch <release-run-id> && gh release view v0.1.1
```

`mise run verify` is rerun on the final primary HEAD, not trusted solely from the
integration worktree. A failed release job is not a published working binary — inspect
each asset and smoke-test it before updating README install claims. Do not run
`mise run worktree:clean` while the five active worktrees still contain changes.

# `tasks-all.workflow.js`: wide map-reduce over docs/tasks/TASKS.md (Claude Code Workflow)

This is a Claude Code `Workflow` script, not an ODW one. It uses many small agents rather than a
few large ones.

1. **Inventory.** Build `tm`, list the CLI groups, slash commands and files that print text, and
   set up the `.claude/worktrees/tm-integrate` worktree (branch `integrate`).
2. **Map.** Small agents, each with one job, all at once:
   - one Haiku agent per CLI group;
   - one agent per file for the user-facing copy sweep (it edits `tm-integrate` directly, no
     build);
   - one probe per flow (ticket lifecycle, dependencies, API, MCP, Genesis, navigation,
     semantic search, prefetch, TUI, plus three real-provider runs);
   - one research agent for Jev/Laya (D-020).
3. **Reduce.** The copy sweep lands as its own batch. One design pass on the command surfaces
   (the CLI tree, slash commands and TUI hierarchy) turns into tasks. Recorders dedupe the
   findings into section T of TASKS.md. The audit critic's corrections are applied.
4. **Batches.** Up to eight tasks per batch:
   - The tasks' files are disjoint. Registration and doc files may be shared; the integrator
     applies the editors' `shared_edits` to them.
   - One editor per task, which never builds.
   - One integrator, which applies the shared edits, builds, runs `mise run verify`, fixes or
     reverts, commits, ticks TASKS.md, fast-forwards `main` and pushes `integrate:main`.
5. **Repeat.** The probes run again after the fixes, and new findings go through another round
   of batches.

TASKS.md and the `integrate` branch are the resume state. After a usage-limit halt, relaunch
fresh with a new `args.stamp` and `args.skipDiscovery: true`.

# `complete.workflow.js`: drive tm to completion (Claude Code Workflow)

This is the convergence loop that runs after `tasks-all`. Each round goes through these steps:

1. **Prep.** Build `tm` from `integrate`, discard any half-done batch a halted run left behind,
   and load every open or retryable task from TASKS.md.
2. **Land.** Work the tasks that are already known through the batch loop: no-build editors, then
   one integrator.
3. **Probe and sweep.** Run 17 single-purpose flow probes, four of them on the real provider.
   On round 1 and every even round, also run read-only sweeps of every crate and client for
   stubs, unwired features, duplicate concepts, machine-speak and false docs. On round 1, also
   exercise each CLI group.
4. **Record and land.** Dedupe the findings into TASKS.md and land them.
5. **Judge.** One judge decides whether tm is tasteful, coherent and drivable end to end, and
   its gaps become tasks.

The loop stops when every probe passes, nothing critical or high was found, the judge says done
and nothing is left open, or when it reaches `args.maxRounds` (default 8). A deferred task gets
one retry in a later run; after that it's marked "gave up after a retry". The state lives in
TASKS.md and on the `integrate` branch, so after a usage-limit halt a fresh relaunch continues
where the last run stopped. The orchestrating session relaunches it on a cron.
