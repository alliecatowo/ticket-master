#!/bin/sh
# Real-task trials (docs/audits/2026-09-25-bench-plan.md, Track B), run by OpenCode + Luna:
# each trial takes a real closed GitHub issue, runs it through tm and through OpenCode in fresh
# clones at the fix's base commit, checks each result against the fix's own tests, and grades
# the experience. Results go to docs/trials/ (committed through the integration branch); the
# improvements it finds go to the task inbox that scripts/luna-loop.sh files into TASKS.md.
# Loops forever: a new pass starts once 5+ commits have landed since the last one.
set -u
PRIMARY=$(cd "$(dirname "$0")/.." && pwd)
INTEG="$PRIMARY/.claude/worktrees/tm-integrate"
MODEL=${LUNA_MODEL:-openai/gpt-6-luna}
LOG="$HOME/Library/Logs/tm-luna-trials.log"
ROOT=/tmp/tm-trials
INBOX=/tmp/tm-luna/inbox
mkdir -p "$ROOT" "$INBOX"
log() { echo "$(date '+%Y-%m-%d %H:%M:%S') $*" | tee -a "$LOG"; }
limit() { secs=$1; shift; perl -e '$s=shift; $pid=fork; if(!$pid){setpgrp(0,0); exec @ARGV} $SIG{ALRM}=sub{kill "TERM",-$pid; sleep 5; kill "KILL",-$pid; exit 124}; alarm $s; waitpid($pid,0); exit($?>>8)' "$secs" "$@"; }

TRIALS='psf/requests#7432 python
pallets/click#3822 python
sindresorhus/ky#878 typescript
spf13/cobra#2257 go
BurntSushi/ripgrep#3376 rust
gohugoio/hugo#15360 go'

last=""
while :; do
  head=$(git -C "$INTEG" rev-parse integrate)
  if [ -n "$last" ] && [ "$(git -C "$INTEG" rev-list --count "$last..$head")" -lt 5 ]; then sleep 1800; continue; fi
  pass=$(date +%Y%m%d-%H%M)
  dir="$ROOT/$pass"; mkdir -p "$dir/bin"
  cp "$INTEG/target/debug/tm" "$dir/bin/tm" || { sleep 600; continue; }
  log "pass $pass on $(git -C "$INTEG" rev-parse --short "$head")"
  # Trials run up to 2 at a time (each already isolated in its own $dir/$slug); a
  # separate mutex additionally caps rust/go trials (real compiler builds) to 1 at a
  # time, so at most one heavy build and one light (python/typescript) trial overlap.
  SEMDIR="$dir/.sem"; HEAVYLOCK="$dir/.heavylock"; mkdir -p "$SEMDIR"
  acquire_slot() { while :; do n=$(ls "$SEMDIR" 2>/dev/null | wc -l | tr -d ' '); [ "$n" -lt 2 ] && { : >"$SEMDIR/$1"; return; }; sleep 5; done; }
  release_slot() { rm -f "$SEMDIR/$1"; }
  acquire_heavy() { while ! mkdir "$HEAVYLOCK" 2>/dev/null; do sleep 5; done; }
  release_heavy() { rmdir "$HEAVYLOCK" 2>/dev/null; }
  printf '%s\n' "$TRIALS" | while read -r issue lang; do
    slug=$(echo "$issue" | tr '/#' '--')
    ( acquire_slot "$slug"
      case "$lang" in rust|go) acquire_heavy ;; esac
    log "$slug: start"
    limit 5400 opencode run -m "$MODEL" --auto --dir "$dir" "You run one benchmark trial and grade it. Work only under $dir/$slug. Issue: https://github.com/$issue ($lang). Protocol:
1. With gh, find the PR that fixed the issue; note its base SHA and merge SHA and the test files it changed. If no merged fix with tests exists, write $dir/$slug.json with {\"skipped\": \"<why>\"} and stop.
2. Two fresh clones at the base SHA: $dir/$slug/tm and $dir/$slug/opencode. Run the project's setup (venv + pip install -e .[test], pnpm install, go mod download; nothing for cargo) in both, before any agent starts.
3. Task text for both tools: the issue's title and body, verbatim (gh issue view).
4. tm arm, in $dir/$slug/tm, with TM_HOME=$dir/$slug/tm-home TM_NOTIFY=0 and credentials loaded only inside the same command (\`set -a; . $PRIMARY/.env; set +a; ...\`; NEVER print or read .env otherwise): $dir/bin/tm init, then $dir/bin/tm ticket new \"<task text>\", then $dir/bin/tm run <ticket id> bounded to 1200s. Capture all output and \`tm events\` to $dir/$slug/tm.log. Record wall time and tokens.
5. OpenCode arm, in $dir/$slug/opencode: \`opencode run --auto --format json -m $MODEL \"<task text>\" < /dev/null\`, bounded to 1200s, output to $dir/$slug/opencode.log.
6. For each arm, overwrite the fix PR's test files from the merge SHA (\`git checkout <merge-sha> -- <test files>\`) and run the relevant tests. Record pass/fail mechanically.
7. Grade each arm 1-5 on navigation quality, context efficiency, plan quality, verification honesty, UX/copy clarity, recoverability, and cost/time, using the anchors in $INTEG/docs/audits/2026-09-25-bench-plan.md (Rubric). Add a paragraph on what would make it better. For tm, be concrete about tm's own UX: its messages, commands, and what a user would have been confused by.
8. Write $dir/$slug.json: {issue, base_sha, merge_sha, arms: [{tool, passed, wall_seconds, tokens, scores:{...}, better}]}.
9. For every concrete tm problem you saw (a bug, a confusing message, wasted context, a missing capability that OpenCode had), append a task to $INBOX/trial-$pass-$slug.md in exactly this format:
- [ ] **t$pass-$slug-<kebab-id>** — <title>
  model: sonnet · severity: critical|high|medium|low · builds Rust: yes · area: <area> · deps: none
  files: \`<path in the tm repo, $INTEG>\`
  change: <precise change>
  acceptance: <observable proof>
  test: <command>
  evidence: <log file + line/quote>
Read the tm source in $INTEG to point at real files; never edit it." </dev/null >"$dir/$slug.runner.txt" 2>&1
      log "$slug: done ($(test -f "$dir/$slug.json" && echo graded || echo 'no result'))"
      rm -rf "$dir/$slug/tm/target" "$dir/$slug/opencode/target" "$dir/$slug/tm/node_modules" "$dir/$slug/opencode/node_modules"
      case "$lang" in rust|go) release_heavy ;; esac
      release_slot "$slug"
    ) &
  done
  wait
  rm -rf "$SEMDIR" "$HEAVYLOCK"
  limit 1800 opencode run -m "$MODEL" --auto --dir "$dir" "Read every $dir/*.json trial result and write $dir/SCOREBOARD.md: a table per tool (pass rate, mean score per rubric dimension, median wall time and tokens), tm vs OpenCode per trial, and the 5 biggest things tm should improve, citing trials. Plain English." </dev/null >"$dir/scoreboard.runner.txt" 2>&1
  mkdir -p "$INTEG/docs/trials/$pass" && cp "$dir"/*.json "$dir/SCOREBOARD.md" "$INTEG/docs/trials/$pass/" 2>/dev/null
  git -C "$INTEG" add "docs/trials/$pass" && git -C "$INTEG" commit -qm "docs(trials): real-task trial pass $pass" -- "docs/trials/$pass" && log "committed docs/trials/$pass"
  last=$head
done
