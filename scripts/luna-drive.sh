#!/bin/sh
# Work every open task in docs/tasks/TASKS.md with OpenCode + Luna, one at a time, in the
# integration worktree: implement -> `mise run verify` -> commit, tick, fast-forward main, push.
# A task that still fails verify after one fix attempt is reverted and marked "- [~]".
#
#   scripts/luna-drive.sh [--only <task-id>] [--max <n>]
#
# Logs to ~/Library/Logs/tm-luna-drive.log; per-task transcripts under /tmp/tm-luna/.
set -u

PRIMARY=$(cd "$(dirname "$0")/.." && pwd)
INTEG="$PRIMARY/.claude/worktrees/tm-integrate"
MODEL=${LUNA_MODEL:-openai/gpt-6-luna}
LOG="$HOME/Library/Logs/tm-luna-drive.log"
WORK=/tmp/tm-luna
IMPL_SECS=${LUNA_IMPL_SECS:-2700}
ONLY=""
MAX=1000
while [ $# -gt 0 ]; do
  case "$1" in
    --only) ONLY=$2; shift 2 ;;
    --max) MAX=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
mkdir -p "$WORK"
unset CARGO_TARGET_DIR

log() { echo "$(date '+%Y-%m-%d %H:%M:%S') $*" | tee -a "$LOG"; }
# Run a command with a wall-clock limit (macOS has no timeout(1)).
limit() { secs=$1; shift; perl -e '$s=shift; $pid=fork; if(!$pid){setpgrp(0,0); exec @ARGV} $SIG{ALRM}=sub{kill "TERM",-$pid; sleep 5; kill "KILL",-$pid; exit 124}; alarm $s; waitpid($pid,0); exit($?>>8)' "$secs" "$@"; }

task_block() { # the task's lines, from its checkbox line to the line before the next task/heading
  awk -v id="$1" '
    $0 ~ "^- \\[.\\] \\*\\*" id "\\*\\*" {on=1; print; next}
    on && (/^- \[/ || /^## /) {exit}
    on {print}' "$INTEG/docs/tasks/TASKS.md"
}
open_ids() {
  sed -n 's/^- \[ \] \*\*\([A-Za-z0-9._-]*\)\*\*.*/\1/p' "$INTEG/docs/tasks/TASKS.md"
}
reset_tree() { git -C "$INTEG" checkout -q -- . && git -C "$INTEG" clean -fdq -e target -e node_modules; }
verify() { # $1 = log file; returns cargo's status
  (cd "$INTEG" && mise run fmt >/dev/null 2>&1; mise run verify) >"$1" 2>&1
}
luna() { # $1 = prompt, $2 = transcript
  limit "$IMPL_SECS" opencode run -m "$MODEL" --auto --dir "$INTEG" "$1" </dev/null >"$2" 2>&1
}
tick() { # $1 id, $2 "x" or "~", $3 note
  /usr/bin/perl -0pi -e 's/^- \[[ ~]\] (\*\*\Q'"$1"'\E\*\*[^\n]*)/- ['"$2"'] $1 ('"$3"')/m' "$INTEG/docs/tasks/TASKS.md"
}
land() { # fast-forward main and push; a refused ff or push is logged, not fatal
  git -C "$PRIMARY" merge -q --ff-only integrate 2>>"$LOG" || log "main did not fast-forward"
  git -C "$INTEG" push -q origin integrate:main 2>>"$LOG" || log "push failed"
}

RULES='Rules: work only inside this directory (the integration worktree). Never run the compiled tm binary here; test it only in a fresh `mktemp -d` directory with TM_HOME set to another `mktemp -d`. Never print, cat or read .env. Do not commit, push, or edit docs/tasks/TASKS.md: the driver does that. Keep user-facing text plain English (no SPEC section numbers, no internal jargon). Put #[cfg(test)] code at the bottom of a file. When you change behaviour CLAUDE.md documents, update CLAUDE.md in the same change. Before finishing, run `mise run test:crate -- <crate>` for each crate you touched and make it pass; `mise run verify` runs after you (fmt check, clippy -D warnings, all tests, hygiene).'

if [ -n "$(git -C "$INTEG" status --porcelain)" ]; then
  b="salvage/luna-$(date +%Y%m%d%H%M%S)"
  git -C "$INTEG" checkout -q -b "$b" && git -C "$INTEG" add -A && git -C "$INTEG" commit -qm "salvage: unverified edits left in the integration worktree" && git -C "$INTEG" checkout -q integrate
  log "saved leftover edits to $b"
fi
git -C "$INTEG" merge -q --no-edit main 2>>"$LOG"

n=0
for id in ${ONLY:-$(open_ids)}; do
  [ "$n" -ge "$MAX" ] && break
  n=$((n + 1))
  block=$(task_block "$id")
  [ -z "$block" ] && { log "$id: not found"; continue; }
  # `open_ids()` above is a one-time snapshot taken when this whole loop started, not re-checked
  # per id -- on a long run, another actor (a landed subagent, a manual push) can land or claim
  # this exact id in the meantime. Re-check the live marker right before dispatching, so a
  # since-landed/-claimed task is skipped instead of wastefully re-implemented (or, worse, handed
  # to the model with its own "(landed <sha>)" text still in the prompt, confusing it into either
  # redoing finished work or doing nothing productive with a wasted turn either way).
  case "$block" in
    "- [ ]"*) ;;
    *)
      log "$id: skipped, no longer open (landed or claimed elsewhere since this pass started)"
      continue
      ;;
  esac
  title=$(printf '%s\n' "$block" | head -1 | sed 's/^- \[.\] \*\*[^*]*\*\* — //')
  log "$id: start ($title)"
  prompt="Implement this task in the ticket-master Rust workspace (see CLAUDE.md and SPEC.md section 0).

$block

$RULES"
  luna "$prompt" "$WORK/$id.impl.txt"; rc=$?
  [ $rc -eq 124 ] && log "$id: implementation hit the ${IMPL_SECS}s limit"
  if [ -z "$(git -C "$INTEG" status --porcelain)" ]; then
    log "$id: no changes"; tick "$id" "~" "needs another pass: the worker made no changes"
    git -C "$INTEG" commit -qam "docs(tasks): defer $id" && land; continue
  fi
  if ! verify "$WORK/$id.verify1.txt"; then
    log "$id: verify failed, one fix attempt"
    fail=$(grep -E '^(error|warning)|FAILED|panicked|violation|Diff in' "$WORK/$id.verify1.txt" | head -60)
    luna "Your change for task $id fails \`mise run verify\`. Fix it without dropping the task's intent. Failures:
$fail

$block

$RULES" "$WORK/$id.fix.txt"
    if ! verify "$WORK/$id.verify2.txt"; then
      log "$id: verify failed again, reverted"; reset_tree
      tick "$id" "~" "needs another pass: verify failed, see /tmp/tm-luna/$id.verify2.txt"
      git -C "$INTEG" commit -qam "docs(tasks): defer $id" && land; continue
    fi
  fi
  git -C "$INTEG" add -A
  git -C "$INTEG" commit -qm "$title

Task: $id

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" || { log "$id: commit failed"; reset_tree; continue; }
  sha=$(git -C "$INTEG" rev-parse --short HEAD)
  tick "$id" "x" "landed $sha"
  git -C "$INTEG" commit -qam "docs(tasks): land $id"
  land
  log "$id: landed $sha"
done
log "done: $n task(s) attempted"
