#!/bin/sh
# Keep work going without a human: forever, (1) fold new tasks from the trials inbox into
# docs/tasks/TASKS.md, (2) work every open task with scripts/luna-drive.sh, (3) when nothing is
# open, have Luna audit tm hands-on and file new tasks into the inbox. Stop it with
# `pkill -f luna-loop.sh`.
#
# Inbox: /tmp/tm-luna/inbox/*.md, each holding tasks in TASKS.md's format; written by
# scripts/luna-trials.sh and by the audit step below.
set -u
PRIMARY=$(cd "$(dirname "$0")/.." && pwd)
INTEG="$PRIMARY/.claude/worktrees/tm-integrate"
MODEL=${LUNA_MODEL:-openai/gpt-6-luna}
LOG="$HOME/Library/Logs/tm-luna-drive.log"
INBOX=/tmp/tm-luna/inbox
mkdir -p "$INBOX/done"
export INTEGDOCS="$INTEG/docs/decisions"
log() { echo "$(date '+%Y-%m-%d %H:%M:%S') loop: $*" | tee -a "$LOG"; }
limit() { secs=$1; shift; perl -e '$s=shift; $pid=fork; if(!$pid){setpgrp(0,0); exec @ARGV} $SIG{ALRM}=sub{kill "TERM",-$pid; sleep 5; kill "KILL",-$pid; exit 124}; alarm $s; waitpid($pid,0); exit($?>>8)' "$secs" "$@"; }
open_count() { grep -c '^- \[ \] \*\*' "$INTEG/docs/tasks/TASKS.md"; }

fold_inbox() {
  set -- "$INBOX"/*.md
  [ -e "$1" ] || return 0
  F="$INTEG/docs/tasks/TASKS.md"
  grep -q '^## V — Found by trials and audits' "$F" || printf '\n## V — Found by trials and audits\n\n' >>"$F"
  for f in "$@"; do
    # keep only well-formed task blocks, and skip ids TASKS.md already has
    awk -v tasks="$F" '
      BEGIN { while ((getline l < tasks) > 0) if (match(l, /^- \[.\] \*\*[^*]+\*\*/)) { id=substr(l, 9, RLENGTH-10); seen[id]=1 } }
      /^- \[ \] \*\*/ { match($0, /\*\*[^*]+\*\*/); id=substr($0, RSTART+2, RLENGTH-4); keep = !(id in seen); seen[id]=1 }
      /^- \[/ && !/^- \[ \] \*\*/ { keep=0 }
      keep { print }' "$f" >>"$F"
    mv "$f" "$INBOX/done/"
  done
  # dangling D-NNN refs would fail hygiene; neutralise them
  perl -pi -e 's/\bD-(\d{3})\b/(glob("$ENV{INTEGDOCS}\/D-$1-*"))[0] ? "D-$1" : "[new decision]"/ge' "$F" 2>/dev/null
  if [ -n "$(git -C "$INTEG" status --porcelain docs/tasks/TASKS.md)" ]; then
    git -C "$INTEG" commit -qm "docs(tasks): file tasks found by trials and audits" docs/tasks/TASKS.md
    git -C "$PRIMARY" merge -q --ff-only integrate 2>/dev/null; git -C "$INTEG" push -q origin integrate:main 2>/dev/null
    log "folded inbox; $(open_count) open"
  fi
}

audit() {
  stamp=$(date +%m%d%H%M)
  bin=/tmp/tm-luna/audit-$stamp; mkdir -p "$bin"
  cp "$INTEG/target/debug/tm" "$bin/tm" 2>/dev/null
  log "audit $stamp"
  limit 3600 opencode run -m "$MODEL" --auto --dir "$bin" "You are auditing ticket-master (tm), a ticket-driven coding-agent harness; source at $INTEG (read it, never edit it), binary at $bin/tm. The owner's bar: tasteful, coherent, and drivable end to end through every surface (CLI, TUI, tm serve web/API, tm mcp), able to work real projects better than Claude Code or OpenCode. Read docs/audits/2026-09-25-opus-audit.md and the last 40 lines of $LOG for what was already found and fixed.
Hands-on: in fresh \`mktemp -d\` dirs with TM_HOME set to another mktemp dir, drive real flows: tm init; tm ticket new + tm run on a small real bug in a tiny Python project; tm genesis --prompt for a small app; tm search (exact/semantic) on a clone of $INTEG; tm doctor; tm serve + curl; tm mcp tools/list; tm --help and the subcommand helps. For live provider runs load credentials only within one command: \`set -a; . $PRIMARY/.env; set +a; <cmd>\` and NEVER print or read the file otherwise. Bound every run with a time limit. Never run tm inside $PRIMARY or $INTEG.
Then write 5-15 NEW tasks (not already in $INTEG/docs/tasks/TASKS.md) to $INBOX/audit-$stamp.md, each exactly in this format:
- [ ] **v$stamp-<kebab-id>** — <title>
  model: sonnet · severity: critical|high|medium|low · builds Rust: yes|no · area: <area> · deps: none
  files: \`<path>\`
  change: <precise change with grep anchors>
  acceptance: <observable proof>
  test: <command>
  evidence: <command + what you observed>
Only real, reproduced problems or clearly valuable improvements; each doable by one worker in under an hour." </dev/null >"$bin/audit.txt" 2>&1
}

while pgrep -f 'scripts/luna-drive.sh' >/dev/null; do sleep 60; done
idle=0
while :; do
  fold_inbox
  if [ "$(open_count)" -gt 0 ]; then
    idle=0; "$PRIMARY/scripts/luna-drive.sh"
  else
    audit; fold_inbox
    if [ "$(open_count)" -eq 0 ]; then idle=$((idle + 1)); log "nothing to do (idle $idle)"; sleep $((1800 * idle)); fi
  fi
done
