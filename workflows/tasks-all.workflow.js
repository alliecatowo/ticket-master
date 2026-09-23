export const meta = {
  name: 'tasks-all',
  description: 'Trial every tm surface for real, record findings in docs/tasks/TASKS.md, implement every open task with Sonnet/Haiku workers (2 Rust builds max), merge serially, gate with verify, re-trial until clean',
  phases: [
    { title: 'Load', detail: 'read TASKS.md state (resume-safe)' },
    { title: 'Trial', detail: 'drive each surface for real, report defects' },
    { title: 'Record', detail: 'dedupe findings into TASKS.md' },
    { title: 'Implement', detail: 'one worktree worker per task, 2 Rust builds max' },
    { title: 'Integrate', detail: 'serial merges, TASKS.md check-off, verify gates, push' },
  ],
}

const PRIMARY = '/Users/allie/Develop/ticket-master'
const RUST_SLOTS = 2
const OTHER_SLOTS = 3
const GATE_EVERY = 6
const planTasks = (args && args.planTasks) || []
const maxRounds = (args && args.maxRounds) || 3
const skipFirstTrial = !!(args && args.skipFirstTrial)
const SEV = { critical: 0, high: 1, medium: 2, low: 4 }

// ---------- plumbing ----------
function makeSem(n) {
  let free = n
  const q = []
  return {
    async acquire() { if (free > 0) { free--; return } await new Promise(r => q.push(r)) },
    release() { const r = q.shift(); if (r) r(); else free++ },
  }
}
async function withLock(sem, fn) { await sem.acquire(); try { return await fn() } finally { sem.release() } }
const primaryLock = makeSem(1)

let nullStreak = 0
let halted = false
async function call(prompt, opts) {
  if (halted) return null
  const r = await agent(prompt, opts)
  if (r == null) {
    nullStreak++
    if (nullStreak >= 3 && !halted) {
      halted = true
      log('Three agents in a row returned nothing (probably a usage limit). No new work starts. Once usage resets, relaunch this script FRESH (not resumeFromRunId) with a new args.stamp and args.skipFirstTrial: true; TASKS.md carries the state.')
    }
  } else nullStreak = 0
  return r
}

const tasks = new Map()
let seq = 0
function addTask(t, prio) {
  if (!t || !t.id || tasks.has(t.id)) return
  tasks.set(t.id, { id: t.id, m: t.m || 'sonnet', r: t.r !== false, f: t.f || [], d: t.d || [], prio, seq: seq++, status: 'pending' })
}
const IGNORE_OVERLAP = /(^|\/)(CLAUDE\.md|SPEC\.md|README\.md|args\.rs|lib\.rs|main\.rs|mod\.rs|mise\.toml|Cargo\.toml|Cargo\.lock|package\.json|pnpm-lock\.yaml)$|^docs\//
const realFiles = t => (t.f || []).filter(f => !IGNORE_OVERLAP.test(f))
const overlap = (a, b) => { const B = realFiles(b); return realFiles(a).some(x => B.includes(x)) }
function depState(t) {
  for (const d of t.d || []) {
    const dt = tasks.get(d)
    if (!dt) continue
    if (dt.status === 'failed' || dt.status === 'skipped') return 'dead'
    if (dt.status !== 'merged') return 'wait'
  }
  return 'ok'
}

const toMark = []      // {id, box: 'x'|'~', note, worktrees: []}
const followups = []   // strings from implementers
const gates = []
const trialLog = []
let mergesSinceGate = 0

// ---------- schemas ----------
const S = (props, req) => ({ type: 'object', properties: props, required: req || Object.keys(props) })
const str = { type: 'string' }
const strs = { type: 'array', items: str }
const LOAD = S({
  closed: strs,
  open_t: { type: 'array', items: S({ id: str, severity: { enum: ['critical', 'high', 'medium', 'low'] }, m: { enum: ['haiku', 'sonnet', 'opus'] }, r: { type: 'boolean' }, f: strs, d: strs }) },
})
const FINDING = S({
  slug: str, title: str, severity: { enum: ['critical', 'high', 'medium', 'low'] },
  files: strs, change: str, acceptance: str, test_command: str,
  model: { enum: ['haiku', 'sonnet'] }, builds_rust: { type: 'boolean' }, evidence: str,
})
const TRIAL = S({ works: strs, tasks: { type: 'array', items: FINDING } })
const RECORD = S({
  tasks: { type: 'array', items: S({ id: str, severity: { enum: ['critical', 'high', 'medium', 'low'] }, m: { enum: ['haiku', 'sonnet'] }, r: { type: 'boolean' }, f: strs, d: strs }) },
  merged_into_existing: { type: 'integer' },
})
const PREP = S({ ok: { type: 'boolean' }, bin: str, notes: str })
const IMPL = S({
  status: { enum: ['done', 'already-done', 'blocked', 'failed'] },
  branch: str, worktree: str, commit: str, crates: strs, summary: str, followups: strs,
}, ['status', 'summary'])
const MERGE = S({ merged: { type: 'boolean' }, sha: str, reason: str }, ['merged', 'reason'])
const GATE = S({ green: { type: 'boolean' }, pushed: { type: 'boolean' }, head: str, notes: str })
const MARK = S({ ok: { type: 'boolean' }, notes: str })

// ---------- shared prompt fragments ----------
const RULES = `Hard rules for this repo (see CLAUDE.md for the why):
- \`unset CARGO_TARGET_DIR\` in every shell before building. Builds only through mise tasks or \`env -u CARGO_TARGET_DIR cargo ... -j 2\`; never raise the -j 2 cap.
- Never run the compiled \`tm\` inside ${PRIMARY} or any worktree root. Manual checks: \`S=$(mktemp -d)\`, \`export TM_HOME=$(mktemp -d)\`, work in $S, \`TM_TEST_MOCK_PROVIDER=1\` for the mock provider.
- Never read, print, echo, cat, copy or commit \`.env\` or any value from it.
- Never touch \`.claude/worktrees/odw-*\` (the owner's parallel provider-overhaul workflow) and never run \`mise run clean\` or \`mise run worktree:clean\`.
- No model identifiers in commits. Every commit message ends with the line: Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`

const TRIAL_BASE = (round, bin, max) => `You are a hands-on tester of ticket-master (\`tm\`), a Rust ticket and agent harness. CLAUDE.md in ${PRIMARY} describes every surface. Your job is to USE one surface for real, the way its owner would, and report concrete defects as atomic fix tasks. This is round ${round}${round > 1 ? ' (earlier rounds already fixed what they found; confirm those flows now work end to end and find what is still wrong)' : ''}.

Do not edit anything in ${PRIMARY}. Read code only to pin a defect you OBSERVED to the file and function that must change. Don't audit code for its own sake.

Binary: \`S=$(mktemp -d); cp ${bin} "$S/tm"\`, and use "$S/tm". \`export TM_HOME=$(mktemp -d)\`. Work only in $S: \`git init\` a small realistic project there. Never run tm inside ${PRIMARY} or any worktree.
Mock provider: \`TM_TEST_MOCK_PROVIDER=1\`. Real provider (only where your brief asks): load the repo's credentials into that one process, e.g. \`(set -a; . ${PRIMARY}/.env; set +a; exec "$S/tm" <args>)\`. Never cat, print, echo or copy .env or any value from it, and never write one into a file or your output.
If running with .env loaded is denied, don't look for another way to load it: fall back to the mock provider and say so in \`works\`.
Bound long runs: \`perl -e 'alarm shift; exec @ARGV' 600 "$S/tm" ...\` (macOS has no \`timeout\`). Don't run cargo; the binary is already built.

Report at most ${max} tasks, most severe first. Each task must be atomic (one Sonnet worker, one sitting): the files to change, the change, an observable acceptance check, and the exact test command (\`mise run test:crate -- <crate>\` or \`pnpm -C clients/<x> test\`). Severity: critical = the flow does not work at all; high = works but wrong, misleading or unusable; medium = rough, confusing, a missing option or affordance; low = polish. Count as defects: copy problems (machine-speak, jargon, raw ids/enums/debug dumps shown to people, dead ends, errors without a next step), fake or stubbed behavior that looks real, and concepts that don't hang together. Put the command and a ≤3-line output excerpt in \`evidence\`. In \`works\`, list short lines of what you verified works.
Skip provider/model/config setup UX (crates/tm-provider and the chat's /connect, /provider, /config belong to another workstream); mention it in \`works\` only if it blocked you. Before reporting, \`grep '^- \\[' ${PRIMARY}/docs/tasks/TASKS.md\` and don't re-report an open task already listed there.`

const FACETS = [
  { key: 'flow', brief: `THE CORE PROMISE: a request becomes a ticket, a worker implements it, the work is verified, and the ticket finishes. In $S build a small Python or Rust project with one failing test and one missing feature. Drive: \`tm init\`, \`tm ticket new\`, \`tm ticket activate\`, \`tm run <T>\`, and a background \`tm sched run\` working several tickets, including one that depends on another. Follow each ticket through every state: does verification actually run the project's tests/checks, and what do verifying/auditing mean here? Then submitted → \`tm ticket accept\` → closed; \`reject --reason\` → rework; out of attempts → \`retry --guidance\`. Mock provider first, then ONE real-provider run of the missing-feature ticket to see a real model implement and verify it. Check \`tm events\`, \`tm tickets --json\`, the diff in the repo and what a person sees at each step. Also check the chat → ticket path: \`tm -p\` asking it to queue work.` },
  { key: 'genesis', brief: `GENESIS: turning an idea into a working project. Read \`tm genesis --help\`, then run it offline (mock) and with the real provider on a small idea ("a Python CLI todo app with tests"). Does it terminate? What does it create: files, tickets, milestones, docs? Can \`tm sched run\` then work the created tickets into a project whose tests pass? Interrupt it and resume. Report everything that stops Genesis from producing a real, working project.` },
  { key: 'tui', brief: `THE TUI. Load the terminal-mcp skill, call createSession for your own PTY session (never use the default session) and destroySession when done. Launch "$S/tm" in the scratch repo with the mock provider. Walk the chat (welcome, typing a request, \`/\` commands, \`?\` shortcuts, \`!\` shell, \`@\` files, /status /cost /model /resume /init /compact), then the tickets screen (← twice or /tickets): dispatch a ticket from its input, Space to peek, the numbered accept/reject/retry/queue choices, Ctrl+B board, attaching a ticket's chat. Screenshot to judge layout. Report broken keys, rendering glitches, confusing states, every piece of jank or machine-speak copy, and missing affordances. Also report which project-management views are missing (milestones, dependencies, timeline/calendar), each as a concrete atomic task saying where it lives in the TUI and which store data backs it.` },
  { key: 'serve', brief: `THE SERVER, WEB CLIENT AND MCP. Start "$S/tm" serve on a free port (with workers, mock provider) in the scratch repo and drive a whole ticket lifecycle over HTTP with curl: create, activate, watch /events (SSE), transition accept/reject/retry, page /tickets/{id}/events, /schema, /health, /state. Point \`--web-dir\` at ${PRIMARY}/clients/web/dist (already built), load /app/ and its assets with curl; if Playwright is already installed under ${PRIMARY}/clients/web/node_modules, take one headless screenshot of the home and a ticket page. Then \`tm mcp\` over stdio (newline-delimited JSON-RPC): initialize, tools/list, tools/call ticket_dispatch, ticket_show, search and symbol tools, and confirm the dispatched ticket actually gets worked.` },
  { key: 'codenav', brief: `CODE NAVIGATION, CONTEXT AND BUDGET, as an agent experiences them. \`git clone --depth 1 file://${PRIMARY} "$S/repo"\` (sources only) and \`tm init\` there. On the never-indexed project, run every navigation and search command \`tm --help\` lists (symbol, search, history, outline and so on), then edit a file and add a new one and check freshness. Then ONE real-provider \`tm -p --json\` turn asking "where is the ticket state machine and which transitions exist?" and ONE real-provider ticket needing code navigation. Check what context was prefetched, whether the agent used the navigation tools, and how tokens and budget are reported. Report wrong or empty results, a stale index, slow paths and missing tools.` },
  { key: 'cli', brief: `EVERY CLI VERB, SETTINGS, AND PROJECT MANAGEMENT. Walk \`tm --help\` and every subcommand's \`--help\`; try common mistakes (typos, missing args, unknown ticket ids, wrong state) and judge each message. Settings: what can be configured (\`tm config\`, config files, env vars), are defaults sane, can a person discover them? Leave provider/model setup out. Project management: milestones, dependencies, timelines, calendar, priorities, labels, epics. What exists in the CLI and the store, and what is stubbed or missing? Propose concrete atomic tasks for the gaps (data model, CLI verbs, TUI view), ordered so a worker can build them one after another (use depends_on-style wording in \`change\`).` },
]

// ---------- agents ----------
async function load() {
  return call(`Run ${(args && args.stamp) || 'unstamped'}. Read ${PRIMARY}/docs/tasks/TASKS.md (read-only; change nothing). Return:
- closed: the id (the bold text right after the checkbox) of every task line starting with "- [x]" or "- [~]".
- open_t: every task under the "## T — Found in live trials" heading that starts with "- [ ]": its id; severity (from the "severity:" field on its model line if present, else "high"); m = its model (haiku, sonnet or opus); r = true if "builds Rust: yes"; f = its file paths from the "files:" line; d = its dependency ids from "deps:" (empty if "none").`,
    { label: 'load', phase: 'Load', schema: LOAD, model: 'haiku', effort: 'low' })
}

async function prep(round) {
  return withLock(primaryLock, () => call(`In ${PRIMARY} (main branch; do not change tracked files): \`unset CARGO_TARGET_DIR\`, then \`mise run build\`, then \`pnpm -C clients/web install --frozen-lockfile && pnpm -C clients/web build\` (dist is gitignored). Copy the built tm binary (target/debug/tm) to /tmp/tm-trials/tm-r${round} (mkdir -p) and run \`/tmp/tm-trials/tm-r${round} --version\` from /tmp. Return ok, bin (that path), and notes (one line; the web build result).`,
    { label: `prep:r${round}`, phase: 'Trial', schema: PREP, model: 'haiku', effort: 'low' }))
}

async function runTrials(round, bin) {
  const max = round === 1 ? 12 : 8
  const results = await parallel(FACETS.map(f => () =>
    call(`${TRIAL_BASE(round, bin, max)}\n\nYour surface:\n${f.brief}`,
      { label: `trial:${f.key}:r${round}`, phase: 'Trial', schema: TRIAL, model: 'sonnet', effort: 'medium' })))
  const found = results.map((r, i) => r && { facet: FACETS[i].key, works: r.works, tasks: r.tasks }).filter(Boolean)
  trialLog.push({ round, facets: found.map(x => `${x.facet}: ${x.tasks.length} found, ${x.works.length} works`), works: found.flatMap(x => x.works.map(w => `${x.facet}: ${w}`)) })
  if (!found.length) return 0
  const fu = followups.splice(0)
  const rec = await withLock(primaryLock, () => call(`You record test findings into the task list. Work in ${PRIMARY} on branch main (you are the only agent writing there right now; the owner's own session might commit too, so never reset or discard commits you didn't make).

Findings from round ${round} of live trials, as JSON (facet, works, tasks):
${JSON.stringify(found)}
${fu.length ? `\nFollow-ups noticed by implementers (turn the real ones into tasks the same way; drop vague ones):\n${JSON.stringify(fu)}\n` : ''}
Steps:
1. Read docs/tasks/TASKS.md and docs/tasks/README.md (format and conventions).
2. Dedupe: merge findings that describe the same defect (across facets), and drop any finding already covered by an OPEN task in TASKS.md. If it adds real detail to that open task, append one sentence to that task's change: line instead, and count it in merged_into_existing.
3. Give each remaining task a unique kebab id \`r${round}-<facet>-<slug>\`. Where one needs another first, set deps (ids of other new or existing open tasks).
4. Write each under the "## T — Found in live trials" heading (after the existing entries there), exactly in the existing entry format, but with the model line as: \`model: <m> · severity: <severity> · builds Rust: <yes|no> · area: <facet> · deps: <ids or none>\`, followed by files/change/acceptance/test lines, and one \`evidence:\` line. Keep the change text specific and self-contained, since a worker reads only this entry.
5. Under that heading, also add a short "Verified working (round ${round})" list from the works lines (at most 15 bullets; merge similar ones).
6. \`mise run hygiene\` must still pass (don't write bare D-NNN numbers for decisions that don't exist yet). Commit only docs/tasks/TASKS.md: "docs(tasks): record round ${round} trial findings".
Return the new tasks' metadata: id, severity, m (haiku|sonnet), r (builds Rust), f (files), d (deps).

${RULES}`, { label: `record:r${round}`, phase: 'Record', schema: RECORD, model: 'sonnet', effort: 'low' }))
  if (!rec) return 0
  for (const t of rec.tasks) addTask(t, SEV[t.severity] ?? 2)
  log(`Round ${round}: recorded ${rec.tasks.length} new tasks (${rec.merged_into_existing} merged into existing ones)`)
  return rec.tasks.filter(t => t.severity === 'critical' || t.severity === 'high').length
}

function implPrompt(t, note, attempt) {
  return `Implement ONE task in the ticket-master repo, in the isolated git worktree you were started in.

Task id: \`${t.id}\`. Its full spec (files, change, acceptance, test) is the \`**${t.id}**\` entry in docs/tasks/TASKS.md. Read that entry first (grep for it; don't read the whole file). If your worktree's copy lacks it, read it from ${PRIMARY}/docs/tasks/TASKS.md.${note ? `\n\nAttempt ${attempt}. ${note}` : ''}

Steps:
1. \`unset CARGO_TARGET_DIR\`; \`git rev-parse --show-toplevel\` must NOT be ${PRIMARY} itself (you must be in a separate worktree). If it is, stop and return status "failed" with that as the summary. Then bring your branch up to date with local main: \`git merge --ff-only main || git merge --no-edit main\`.
2. Read only the code you need (prefer the zvec-grep search/rg tools, or LSP, over wide reads). If the spec is obsolete because main already does this, return "already-done" with the evidence. If it needs a decision only the owner can make, return "blocked" and say what.
3. Implement it, scoped to the task, matching the surrounding code's style, naming and comment density. User-facing text must be plain, friendly and specific: no machine-speak, no raw enum or debug output, and errors say what to do next.
4. Tests: add or adjust tests that prove the acceptance check (put #[cfg(test)] code at the bottom of the file). Run the task's test command, \`env -u CARGO_TARGET_DIR cargo clippy -p <crate> --all-targets -j 2 -- -D warnings\` for each Rust crate you touched, \`mise run fmt\` and \`mise run hygiene\`. For clients, run that client's pnpm test/build.
5. Docs in the same change: a CLAUDE.md line for a new CLI verb or mise task, SPEC.md if it now disagrees. If the spec says \`[new decision: X]\`, write the next free docs/decisions/D-NNN-*.md (check \`ls docs/decisions\` on main at ${PRIMARY} too, and follow D-002's format). Don't edit docs/tasks/TASKS.md; the integrator ticks it off.
6. Review your own diff skeptically before committing: does it really meet the acceptance check, is anything left stubbed, is there any unwrap/expect in non-test code? Fix what you find.
7. Commit on your branch (conventional message).
8. Whatever the outcome (done, failed, blocked, already-done), finish with \`rm -rf target\` in your worktree: the merge builds in the primary checkout, and this machine's disk is tight.
Return status, branch (\`git branch --show-current\`), worktree (the toplevel path), commit (full sha), crates (Rust crates or clients touched), summary (2-3 sentences), and followups (real defects you noticed but that were out of scope, one line each; empty if none).

${RULES}`
}

function mergePrompt(t, impl) {
  return `Integrate one finished task branch into main, in the primary checkout ${PRIMARY}. You are the only workflow agent writing there right now; the owner's own session may commit there too, so never discard, reset or rewrite commits you didn't make, and never force-push.

Task \`${t.id}\`: branch \`${impl.branch}\`, worktree ${impl.worktree}, commit ${impl.commit}. Touched: ${(impl.crates || []).join(', ') || 'see the diff'}. Worker's summary: ${impl.summary}

1. \`cd ${PRIMARY}; unset CARGO_TARGET_DIR\`. Confirm \`git branch --show-current\` is main. If tracked files have uncommitted changes (untracked HELLO*.md files are expected; leave them), someone else is mid-change: wait with a bounded loop (\`for i in $(seq 20); do git diff --quiet && git diff --cached --quiet && break; sleep 30; done\`) and then continue, or return merged=false, reason "primary-dirty".
2. \`PRE=$(git rev-parse HEAD)\`, then \`git merge --no-ff --no-edit ${impl.branch}\`. Resolve conflicts so that both sides' intent survives (for docs and registration files, keep both additions).
3. Check the merge: \`mise run test:crate -- <crate>\` for each touched Rust crate, \`env -u CARGO_TARGET_DIR cargo clippy -p <crate> --all-targets -j 2 -- -D warnings\`, \`cargo fmt --all -- --check\` (on failure run \`mise run fmt\` and commit), and \`pnpm -C clients/<x> test\` for touched clients. If something fails because of the merge and the fix is small, fix it and commit. If not, and \`git log --oneline $PRE..HEAD\` shows only your merge and fix commits, \`git reset --hard $PRE\` and return merged=false with a 2-line reason quoting the failure.
4. In docs/tasks/TASKS.md turn \`- [ ] **${t.id}**\` into \`- [x] **${t.id}**\` and append \` (landed <short merge sha>)\` to that line. Commit: "docs(tasks): check off ${t.id}".
5. \`git worktree remove --force ${impl.worktree}\`, then \`git branch -D ${impl.branch}\` if merged. If you didn't merge, still remove the worktree but keep the branch.
Return merged, sha (the merge commit), and reason (one line: what happened).

${RULES}`
}

async function gate(kind) {
  const g = await withLock(primaryLock, () => call(`Run the full gate on main in the primary checkout ${PRIMARY} and push it. The owner's own session may also commit there, so never discard, reset or rewrite commits you didn't make, and never force-push.

1. \`cd ${PRIMARY}; unset CARGO_TARGET_DIR\`; confirm the branch is main.
2. \`mise run verify\`. If it fails, find the cause (often two merged tasks that each passed alone), fix it minimally, commit ("fix: ..."), and re-run. At most two fix rounds; if it's still red, say exactly what fails.
3. If green: \`git push -u origin main\`. If it's rejected because origin moved, \`git pull --no-rebase --no-edit origin main\`, re-run \`mise run verify\`, and push. On network errors retry up to 4 times (2s, 4s, 8s, 16s).
Return green, pushed, head (short sha) and notes (at most 3 lines).

${RULES}`, { label: `gate:${kind}`, phase: 'Integrate', schema: GATE, model: 'sonnet', effort: 'medium' }))
  if (g) { gates.push({ kind, ...g }); log(`Gate ${kind}: ${g.green ? 'green' : 'RED'}${g.pushed ? ', pushed' : ''} at ${g.head}`) }
  return g
}

async function flushMarks() {
  if (!toMark.length || halted) return
  const batch = toMark.splice(0)
  await withLock(primaryLock, () => call(`In ${PRIMARY} on main (the owner's session may commit there too; never discard others' commits), update docs/tasks/TASKS.md for these tasks and clean up their worktrees:
${batch.map(x => `- \`${x.id}\`: change its checkbox to \`[${x.box}]\` and append " (${x.note.replace(/\n/g, ' ').slice(0, 300)})" to that line.${x.worktrees.length ? ` Then \`git worktree remove --force\` ${x.worktrees.join(', ')} (keep the branches).` : ''}`).join('\n')}
Commit only docs/tasks/TASKS.md: "docs(tasks): mark ${batch.length} task(s) deferred or already done". Return ok and notes.

${RULES}`, { label: 'mark', phase: 'Integrate', schema: MARK, model: 'haiku', effort: 'low' }))
}

// ---------- one task, start to finish ----------
async function lifecycle(t) {
  let note = ''
  const worktrees = []
  for (let attempt = 1; attempt <= 2; attempt++) {
    if (halted) { t.status = 'pending'; return }
    const model = t.m === 'haiku' ? 'haiku' : 'sonnet'
    const effort = t.m === 'haiku' ? 'low' : t.m === 'opus' ? 'high' : 'medium'
    const impl = await call(implPrompt(t, note, attempt), {
      label: `impl:${t.id}${attempt > 1 ? ':2' : ''}`, phase: 'Implement', schema: IMPL,
      model, effort, isolation: 'worktree',
    })
    if (!impl) { if (halted) { t.status = 'pending'; return } note = 'The previous attempt died without reporting; start fresh.'; continue }
    if (impl.worktree) worktrees.push(impl.worktree)
    if (impl.status === 'already-done') {
      t.status = 'merged'
      toMark.push({ id: t.id, box: 'x', note: `already satisfied on main: ${impl.summary}`, worktrees: impl.worktree ? [impl.worktree] : [] })
      return
    }
    if (impl.status === 'done' && impl.commit && impl.branch) {
      let mr = await withLock(primaryLock, () => call(mergePrompt(t, impl), { label: `merge:${t.id}`, phase: 'Integrate', schema: MERGE, model: 'sonnet', effort: 'low' }))
      if (mr && !mr.merged && /primary-dirty/.test(mr.reason || '')) {
        mr = await withLock(primaryLock, () => call(mergePrompt(t, impl), { label: `merge:${t.id}:2`, phase: 'Integrate', schema: MERGE, model: 'sonnet', effort: 'low' }))
      }
      if (mr && mr.merged) {
        t.status = 'merged'
        for (const f of impl.followups || []) followups.push(`${t.id}: ${f}`)
        mergesSinceGate++
        if (mergesSinceGate >= GATE_EVERY) { mergesSinceGate = 0; await gate(`after-${t.id}`) }
        return
      }
      if (!mr && halted) { t.status = 'pending'; return }
      note = `The previous attempt (branch ${impl.branch}) did not integrate: ${mr ? mr.reason : 'the merge agent died'}. Start from current main; you may cherry-pick from that branch.`
      worktrees.pop() // the merge agent removes the worktree either way
    } else {
      note = `The previous attempt reported ${impl.status}: ${impl.summary}`
      if (impl.status === 'blocked') break
    }
  }
  if (halted) { t.status = 'pending'; return }
  t.status = 'failed'
  t.reason = note.slice(0, 300)
  toMark.push({ id: t.id, box: '~', note: `deferred by the tasks-all workflow: ${t.reason}`, worktrees })
}

// ---------- scheduler ----------
async function drain(feeder) {
  const running = new Map()
  let feederDone = !feeder
  const feederP = feeder ? feeder.then(() => { feederDone = true }, () => { feederDone = true }) : null
  while (!halted) {
    for (const t of tasks.values()) {
      if (t.status === 'pending' && depState(t) === 'dead') {
        t.status = 'skipped'
        toMark.push({ id: t.id, box: '~', note: 'deferred: a task it depends on did not land', worktrees: [] })
      }
    }
    const run = [...tasks.values()].filter(t => t.status === 'running')
    let rustN = run.filter(t => t.r).length
    let otherN = run.length - rustN
    const ready = [...tasks.values()].filter(t => t.status === 'pending' && depState(t) === 'ok').sort((a, b) => a.prio - b.prio || a.seq - b.seq)
    for (const t of ready) {
      if (t.r ? rustN >= RUST_SLOTS : otherN >= OTHER_SLOTS) continue
      if (run.some(o => overlap(o, t))) continue
      t.status = 'running'
      run.push(t)
      if (t.r) rustN++; else otherN++
      running.set(t.id, lifecycle(t).catch(e => { t.status = 'failed'; t.reason = String(e).slice(0, 200) }).then(() => { running.delete(t.id) }))
    }
    if (running.size === 0) {
      if (feederDone) break
      await feederP
      continue
    }
    await Promise.race([...running.values(), ...(feederDone ? [] : [feederP])])
  }
  await Promise.all([...running.values()])
  if (feederP) await feederP
}

// ---------- main ----------
phase('Load')
const state = await load()
if (!state) return { halted: true, error: 'could not read TASKS.md' }
for (const id of state.closed) tasks.set(id, { id, status: 'merged', f: [], d: [], r: false })
for (const t of state.open_t) addTask(t, SEV[t.severity] ?? 1)
for (const t of planTasks) addTask({ id: t.id, m: t.m, r: t.r, f: t.f, d: t.d }, t.b === 0 ? 1 : t.b <= 8 ? 1.5 : 2.5)
log(`Loaded ${state.closed.length} closed and ${[...tasks.values()].filter(t => t.status === 'pending').length} open tasks`)

let roundsRun = 0
for (let round = 1; round <= maxRounds && !halted; round++) {
  roundsRun = round
  let feeder = null
  let highFound = null
  if (!(round === 1 && skipFirstTrial)) {
    phase('Trial')
    const p = await prep(round)
    if (p && p.ok) feeder = runTrials(round, p.bin).then(n => { highFound = n })
    else log(`Round ${round}: the trial build failed (${p ? p.notes : 'no report'}); implementing without trials`)
  }
  phase('Implement')
  await drain(feeder)
  await flushMarks()
  if (halted) break
  await gate(`round-${round}`)
  mergesSinceGate = 0
  if (highFound === 0 || feeder === null && round > 1) break
}

const all = [...tasks.values()].filter(t => t.prio !== undefined)
const by = s => all.filter(t => t.status === s).map(t => t.id)
return {
  halted,
  roundsRun,
  merged: by('merged').length,
  failed: all.filter(t => t.status === 'failed').map(t => ({ id: t.id, reason: t.reason })),
  skipped: by('skipped'),
  pending: by('pending'),
  gates,
  trials: trialLog.map(t => ({ round: t.round, facets: t.facets, works: t.works.slice(0, 40) })),
  followupsLeft: followups,
}
