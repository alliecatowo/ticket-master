#!/bin/sh
# dogfood-smoke.sh -- run one real ticket against a scratch clone of this repo and report
# whether tm can dogfood itself: how long the ticket took, how many tokens/tool calls it spent,
# what final state it reached, and what it actually changed.
#
# Nothing in this repo measures "can tm work on itself" (docs/tasks/TASKS.md's
# u1-dogfood-e2e-smoke); a manual run of this shape previously took about 6 calls to set up by
# hand and exposed 4 critical bugs. This script is that setup, made repeatable.
#
# What it does, in order:
#   1. Clones this repo (the checkout scripts/dogfood-smoke.sh itself lives in) into a fresh
#      `mktemp -d`, so the run can never touch this checkout's own git state or leave a `.tm/`
#      behind here (CLAUDE.md's "Never let a subagent run the real tm binary against the primary
#      checkout" -- the same rule applies to this script running itself).
#   2. Builds tm from that clone's own code (not whatever happens to be on PATH), `tm init`s it,
#      and `mise trust`s it.
#   3. Creates one fixed, small ticket (a real, tiny doc-wording fix in this repo) and runs it to
#      completion in the foreground under a wall-clock bound, recording a cassette.
#   4. Prints the final ticket state, `tm stats`'s tokens/tool-call rollup for that ticket, and
#      `git diff --stat` for what it changed, then a one-line verdict.
#
# Not part of `mise run verify` (`mise run dogfood` runs it) -- this makes a real provider call
# (DevPass, sourced from .env) and can take several minutes; it's a manual dogfood check, not a
# CI gate.
#
# Usage: scripts/dogfood-smoke.sh
# Env:   TM_DOGFOOD_WALL_BOUND_SECS  wall-clock bound for the ticket run itself (default 900).

set -u
# Deliberately not `set -e`: this script's job is to report a clear verdict for every failure
# shape (clone, build, init, ticket creation, the run itself, or a run that finishes short of
# Submitted), not to abort silently partway with no verdict printed at all.

PATH="/usr/bin:/bin:/usr/sbin:/sbin:$PATH"
export PATH

WALL_BOUND_SECS="${TM_DOGFOOD_WALL_BOUND_SECS:-900}"

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)
if [ -z "$REPO_ROOT" ]; then
    echo "FAILED: scripts/dogfood-smoke.sh must live inside a git checkout" >&2
    exit 1
fi

TMP=""
cleanup() {
    if [ -n "$TMP" ] && [ -d "$TMP" ]; then
        rm -rf "$TMP"
    fi
}
trap cleanup EXIT INT TERM

TMP=$(mktemp -d "${TMPDIR:-/tmp}/tm-dogfood.XXXXXX")
CLONE="$TMP/repo"

echo "==> Cloning $REPO_ROOT into a scratch dir"
if ! git clone --quiet "file://$REPO_ROOT" "$CLONE"; then
    echo "FAILED: could not clone $REPO_ROOT" >&2
    exit 1
fi

cd "$CLONE" || { echo "FAILED: could not enter the clone" >&2; exit 1; }

# A worktree/clone never gets .env for free (CLAUDE.md's "A worktree does not get .env"); source
# it here, in this subshell only, without ever echoing it, so `tm run` has a real provider
# credential (DevPass) to call.
if [ -f "$REPO_ROOT/.env" ]; then
    set -a
    # shellcheck disable=SC1090
    . "$REPO_ROOT/.env"
    set +a
fi

echo "==> Building tm from the clone's own code (cargo build -p tm-cli -j 2)"
if ! cargo build -p tm-cli -j 2 --quiet; then
    echo "FAILED: build" >&2
    exit 1
fi
TM_BIN="$CLONE/target/debug/tm"

command -v mise >/dev/null 2>&1 && mise trust --yes "$CLONE" >/dev/null 2>&1

echo "==> tm init"
if ! "$TM_BIN" init >/dev/null 2>&1; then
    echo "FAILED: tm init" >&2
    exit 1
fi

# A fixed, small, real objective, so every run of this script exercises the same shape of work
# instead of a random one: crates/tm-cli/src/project.rs's provider_doctor_detail formats each
# unreachable local provider as bare "<id>: not running", with no next step for a person reading
# `tm doctor` to act on -- exactly the plain-words-plus-next-step voice CLAUDE.md asks for.
OBJECTIVE="In crates/tm-cli/src/project.rs's provider_doctor_detail (around line 2071), change the unreachable-local-provider message from \"{id}: not running\" to \"{id}: not running (start it locally to use this provider)\", so tm doctor's provider warning says what to do next. Keep the ReachableNoModels and Ready arms as they are."

echo "==> Creating the fixed smoke ticket"
NEW_OUTPUT=$("$TM_BIN" ticket new "$OBJECTIVE" 2>&1)
TICKET=$(printf '%s\n' "$NEW_OUTPUT" | grep -o 'T-[0-9]\+' | head -n1)
if [ -z "$TICKET" ]; then
    echo "FAILED: could not create a ticket" >&2
    printf '%s\n' "$NEW_OUTPUT" >&2
    exit 1
fi
echo "    $TICKET"

CASSETTE="$TMP/cassette.json"
RUN_START=$(date +%s)

# `tm run` activates a draft ticket itself, so no separate `ticket activate` call is needed.
if command -v timeout >/dev/null 2>&1; then
    timeout "$WALL_BOUND_SECS" "$TM_BIN" --plain run "$TICKET" --record "$CASSETTE"
    RUN_STATUS=$?
else
    # macOS base has no `timeout`; fall back to a background run with a poll loop.
    "$TM_BIN" --plain run "$TICKET" --record "$CASSETTE" &
    RUN_PID=$!
    ELAPSED=0
    while kill -0 "$RUN_PID" 2>/dev/null && [ "$ELAPSED" -lt "$WALL_BOUND_SECS" ]; do
        sleep 5
        ELAPSED=$((ELAPSED + 5))
    done
    if kill -0 "$RUN_PID" 2>/dev/null; then
        kill "$RUN_PID" 2>/dev/null
        wait "$RUN_PID" 2>/dev/null
        RUN_STATUS=124
    else
        wait "$RUN_PID"
        RUN_STATUS=$?
    fi
fi

RUN_END=$(date +%s)
WALL_SECS=$((RUN_END - RUN_START))

STATE=$("$TM_BIN" --json ticket show "$TICKET" 2>/dev/null \
    | grep -o '"state":"[^"]*"' | head -n1 | cut -d'"' -f4)
[ -n "$STATE" ] || STATE="unknown"

STATS_JSON=$("$TM_BIN" --json stats --by ticket --ticket "$TICKET" 2>/dev/null)
TOKENS_IN=$(printf '%s\n' "$STATS_JSON" | grep -o '"tokens_in":[0-9]*' | head -n1 | cut -d: -f2)
TOKENS_OUT=$(printf '%s\n' "$STATS_JSON" | grep -o '"tokens_out":[0-9]*' | head -n1 | cut -d: -f2)
TOOL_CALLS=$(printf '%s\n' "$STATS_JSON" | grep -o '"tool_calls":[0-9]*' | head -n1 | cut -d: -f2)
TOKENS_IN=${TOKENS_IN:-0}
TOKENS_OUT=${TOKENS_OUT:-0}
TOOL_CALLS=${TOOL_CALLS:-0}
TOTAL_TOKENS=$((TOKENS_IN + TOKENS_OUT))

DIFF_STAT=$(git diff --stat 2>/dev/null)
FILES_CHANGED=$(printf '%s\n' "$DIFF_STAT" | tail -n1 | grep -o '^ *[0-9]\+ file[s]\? changed' | grep -o '[0-9]\+' | head -n1)
FILES_CHANGED=${FILES_CHANGED:-0}

echo "==> git diff --stat"
printf '%s\n' "$DIFF_STAT"

if [ "$RUN_STATUS" -eq 124 ]; then
    echo "TIMED OUT after ${WALL_SECS}s waiting on $TICKET (bound ${WALL_BOUND_SECS}s)"
    exit 1
fi

if [ "$STATE" = "Submitted" ] || [ "$STATE" = "Verifying" ] || [ "$STATE" = "Auditing" ] \
    || [ "$STATE" = "Closed" ]; then
    echo "SUBMITTED in ${WALL_SECS}s, ${TOTAL_TOKENS} tokens, ${TOOL_CALLS} tool calls, ${FILES_CHANGED} files changed"
    exit 0
fi

echo "FAILED: $TICKET ended in state \"$STATE\" (exit $RUN_STATUS) after ${WALL_SECS}s, ${TOTAL_TOKENS} tokens, ${TOOL_CALLS} tool calls"
exit 1
