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

# `tasks-all.workflow.js`: work off docs/tasks/TASKS.md (Claude Code Workflow)

A Claude Code `Workflow` script, not an ODW one. Each round has four steps:

1. It builds `tm`, and six Sonnet testers drive each surface for real: the ticket flow, Genesis,
   the TUI, the server with the web client and MCP, code navigation, and the CLI with settings
   and project management.
2. It records their findings as tasks in section T of `docs/tasks/TASKS.md`.
3. It implements every open task. Each task gets its own Sonnet or Haiku worker in its own
   worktree, with at most two Rust builds at once.
4. Merges happen one at a time in the primary checkout, and each checks its task off. A full
   `mise run verify` and a push run every six merges.

Rounds repeat until a round finds nothing critical or high, three rounds at most. TASKS.md is the
only resume state. After a usage-limit halt, relaunch it fresh with a new `args.stamp` and
`args.skipFirstTrial: true`, rather than `resumeFromRunId`. `args.planTasks` carries the plan
tasks' dependency and file metadata.
