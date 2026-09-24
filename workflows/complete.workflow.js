export const meta = {
  name: 'tm-complete',
  description: 'Drive tm to completion: each round probes every flow, sweeps every crate and client for stubs and jank, lands every open task in batches (no-build editors + one integrator), and asks one judge whether it is tasteful, coherent and drivable end to end; repeats until converged. Resume-safe: TASKS.md and the integrate branch are the state.',
  phases: [
    { title: 'Prep', detail: 'build tm, sync the integration worktree, load TASKS.md' },
    { title: 'Map', detail: 'flow probes, per-crate sweeps, CLI groups: small agents, one job each' },
    { title: 'Reduce', detail: 'dedupe findings into TASKS.md' },
    { title: 'Edit', detail: 'no-build editors, one task each' },
    { title: 'Integrate', detail: 'one integrator per batch: build, verify, commit, land, push' },
    { title: 'Judge', detail: 'is it tasteful, coherent and drivable end to end?' },
  ],
}

const PRIMARY = '/Users/allie/Develop/ticket-master'
const INTEG = `${PRIMARY}/.claude/worktrees/tm-integrate`
const BIN = '/tmp/tm-wide/tm'
const BATCH = 8
const maxRounds = (args && args.maxRounds) || 8
const stamp = (args && args.stamp) || 'unstamped'
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
      log('HALTED: three agents in a row returned nothing (usage limit). The auto-resume cron relaunches this workflow fresh once usage is back; TASKS.md and the integrate branch carry the state.')
    }
  } else nullStreak = 0
  return r
}

const tasks = new Map()
let seq = 0
function addTask(t, prio, note) {
  if (!t || !t.id || tasks.has(t.id)) return
  tasks.set(t.id, { id: t.id, m: t.m || 'sonnet', f: t.f || [], d: t.d || [], prio, seq: seq++, status: 'pending', tries: 0, note: note || '', retried: !!note })
}
const SHAREABLE = /(^|\/)(CLAUDE\.md|SPEC\.md|README\.md|args\.rs|lib\.rs|main\.rs|mod\.rs|mise\.toml|Cargo\.toml|Cargo\.lock|package\.json|pnpm-lock\.yaml|TASKS\.md|backlog\.md)$/
function depState(t) {
  for (const d of t.d || []) {
    const dt = tasks.get(d)
    if (!dt) continue
    if (dt.status === 'failed' || dt.status === 'skipped') return 'dead'
    if (dt.status !== 'merged') return 'wait'
  }
  return 'ok'
}

// ---------- schemas ----------
const S = (props, req) => ({ type: 'object', properties: props, required: req || Object.keys(props) })
const str = { type: 'string' }
const strs = { type: 'array', items: str }
const bool = { type: 'boolean' }
const SEVE = { enum: ['critical', 'high', 'medium', 'low'] }
const META = S({ id: str, severity: SEVE, m: { enum: ['haiku', 'sonnet', 'opus'] }, f: strs, d: strs })
const LOAD = S({
  closed: strs,
  open: { type: 'array', items: META },
  retry: { type: 'array', items: S({ id: str, severity: SEVE, m: { enum: ['haiku', 'sonnet', 'opus'] }, f: strs, d: strs, reason: str }) },
})
const PREP = S({ ok: bool, notes: str, cli: { type: 'array', items: S({ cmd: str, children: strs }) } })
const FINDING = S({
  slug: str, title: str, severity: SEVE, kind: { enum: ['bug', 'copy', 'hierarchy', 'missing', 'stub', 'ux', 'docs'] },
  files: strs, change: str, acceptance: str, test_command: str,
  model: { enum: ['haiku', 'sonnet'] }, builds_rust: bool, evidence: str,
})
const FINDINGS = S({ pass: bool, works: strs, tasks: { type: 'array', items: FINDING } })
const RECORD = S({ tasks: { type: 'array', items: META }, notes: str })
const EDIT = S({ id: str, status: { enum: ['edited', 'already-done', 'blocked'] }, files: strs, shared_edits: str, summary: str, tests: str }, ['id', 'status', 'summary'])
const INTEGRATE = S({ landed: strs, failed: { type: 'array', items: S({ id: str, reason: str }) }, green: bool, head: str, pushed: bool, main_updated: bool, notes: str })
const JUDGE = S({
  done: bool, verdict: str,
  surfaces: { type: 'array', items: S({ surface: str, score: { type: 'integer' }, note: str }) },
  tasks: { type: 'array', items: FINDING },
})

// ---------- prompt fragments ----------
const RULES = `Hard rules (CLAUDE.md has the why):
- \`unset CARGO_TARGET_DIR\` before any cargo/mise build; never raise the -j 2 cap.
- Never run the compiled tm inside ${PRIMARY} or any worktree. Use a scratch dir and a scratch TM_HOME with literal paths in every command (shell variables don't persist between Bash calls); \`TM_TEST_MOCK_PROVIDER=1\` for the mock provider.
- Never read, print, echo, cat, copy or commit .env or any value from it. Never touch .claude/worktrees/odw-* and never run \`mise run clean\` or \`mise run worktree:clean\`.
- Commit messages carry no model names and end with: Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`
const VOICE = `Voice for anything a person reads (CLI output, errors, TUI labels and hints, HTTP errors, web UI):
- Talk to a person, not a log. Say what happened, then what to do next ("Run \`tm ticket retry T-12\` to try again.").
- Plain words. No internal type, enum or struct names, no {:?} debug output, no raw JSON (unless --json), no event-kind strings, no "Successfully", no stacked "Error: failed to X: error: Y".
- Name things the way the product does: "ticket T-12 (Fix the login redirect)", "worker", "project". Use the CLI's own verbs. Short, sentence case, placeholders intact.`
const FINDING_RULES = (max) => `Report at most ${max} tasks, most severe first, each atomic (one worker, one sitting) and self-contained: exact files to change (grep ${PRIMARY} for a message's text to find its source), the change, an observable acceptance check, the exact test command (\`mise run test:crate -- <crate>\` or \`pnpm -C clients/<x> test\`), model (haiku for mechanical edits, sonnet otherwise), builds_rust. Severity: critical = the flow does not work at all; high = works but wrong, misleading or unusable; medium = rough, confusing, missing option or affordance; low = polish. Kinds: bug, copy, hierarchy, missing, stub (looks real but isn't), ux, docs (a doc claims something false). Evidence: the command you ran or file:line, with a ≤3-line excerpt. pass=false if what you were asked to check does not work. works: short lines of what does work. Skip anything already open in ${PRIMARY}/docs/tasks/TASKS.md (\`grep '^- \\[ \\]' ${PRIMARY}/docs/tasks/TASKS.md\`).`
const SEARCH = `Finding code (this is how you save tokens): load the zvec-grep MCP tools with ToolSearch ("select:mcp__zvec_grep__zvec_grep_search,mcp__zvec_grep__zvec_grep_rg") and use zvec_grep_search for concepts and "where does X happen", zvec_grep_rg for exact strings, symbols and regexes, always with root "${PRIMARY}" (the indexed checkout; ${INTEG} has the same tree, so map a path there before reading or editing it in the integration worktree). A search snippet is evidence: read a file only for what the snippet lacks, and only those lines (Read with offset/limit). No broad file reads, no directory-wide cat. Never create, rebuild or drop an index.`
const SCRATCH = `Scratch sandbox. Shell variables and cd do NOT persist between your Bash calls: a later \`$S\` or \`$TM_HOME\` is EMPTY, and tm then runs in ${PRIMARY} against the real project store (this has happened). So: first run \`mktemp -d\` twice and note the two literal paths (call them SCRATCH and HOME). Copy the binary with \`cp ${BIN} SCRATCH/tm\`. From then on, start EVERY command that runs tm with the literal \`cd SCRATCH && TM_HOME=HOME \` prefix (paths pasted in, no variables), and run it as \`SCRATCH/tm\`. git init a tiny realistic project with one commit inside SCRATCH (or clone into SCRATCH/repo and cd there the same way). Never run tm anywhere under ${PRIMARY}, and don't run cargo. Bound long runs: \`perl -e 'alarm shift; exec @ARGV' 300 SCRATCH/tm ...\`.`
const LIVE = `Real provider: load the repo credentials into that one tm process only: \`(set -a; . ${PRIMARY}/.env; set +a; exec SCRATCH/tm <args>)\`. Never print or copy anything from .env. If that is denied, fall back to TM_TEST_MOCK_PROVIDER=1 and say so in works.`

const PROBES = [
  { key: 'lifecycle-accept', m: 'haiku', brief: 'Offline happy path: `tm init`, `tm ticket new "<objective>"`, `tm ticket activate`, `tm run <T>` with the mock (or scripted) provider reaches submitted, then `tm ticket accept` → closed. Check `tm tickets --json` and `tm events` at each step.' },
  { key: 'lifecycle-reject-retry', m: 'haiku', brief: 'Reject → rework, and escalated → `tm ticket retry --guidance`: states, messages and next steps are clear and correct.' },
  { key: 'deps-pm', m: 'haiku', brief: 'Project management: dependencies (B waits for A), milestones, priorities, due dates/timeline, and what `tm sched run` (bounded 120s) does with them. Everything the CLI offers for planning works and is consistent.' },
  { key: 'serve-api', m: 'haiku', brief: '`tm serve` (workers on, free port): full ticket lifecycle over HTTP with curl including SSE /events, /tickets/{id}/events paging, /schema, /health, transitions; /app/ serves the web client (--web-dir ' + PRIMARY + '/clients/web/dist). Errors are JSON with a human message.' },
  { key: 'web-ui', m: 'sonnet', brief: 'The web client: with `tm serve` running (mock provider, --web-dir ' + PRIMARY + '/clients/web/dist), use Playwright from ' + PRIMARY + '/clients/web/node_modules (headless chromium) to load /app/, create a ticket, open it, and act on it (accept/reject/retry when offered). Screenshot each step and judge layout, copy and flow.' },
  { key: 'mcp', m: 'haiku', brief: '`tm mcp` over stdio (newline-delimited JSON-RPC) after `tm init`: initialize, tools/list, then call every tool once (dispatch, list, show, search, symbol tools). The dispatched ticket gets worked; descriptions and errors are clear.' },
  { key: 'genesis-offline', m: 'haiku', brief: 'Genesis offline (mock) on "a Python CLI todo app with tests", bounded 300s: it terminates, creates files, tickets and milestones, and the tickets can be worked by `tm sched run`.' },
  { key: 'genesis-live', m: 'sonnet', brief: 'Genesis with the real provider on "a tiny Python CLI that counts words in a file, with pytest tests", bounded 600s, then `tm sched run` bounded 600s: does it end with a project whose tests pass (`uvx pytest`)?', live: true },
  { key: 'nav', m: 'haiku', brief: 'Code navigation on a never-indexed clone (`git clone --depth 1 file://' + PRIMARY + ' SCRATCH/repo`, `tm init`): every symbol/search/history/outline command returns right, fresh results, including after adding a file and editing another. Report timings.' },
  { key: 'semantic', m: 'sonnet', brief: 'Semantic search quality on a clone as above: 6 conceptual queries ("where are tickets moved between states", "how is the provider chosen for a request", "what decides the token budget of a context pack", "where are events hash-chained", "how does the TUI render the chat transcript", "where are worktrees created for runs"). The right file is in the top 3 for each; the embedder is a real model (potion), not the hash stand-in.' },
  { key: 'prefetch', m: 'sonnet', brief: 'Context prefetch and budget: for a ticket about a specific function in a clone as above, a person can see what context the worker got (outlines, symbols, history, search hits), it is relevant, and tokens/budget are shown clearly; same for a chat turn (`tm -p --json`, mock).' },
  { key: 'live-ticket', m: 'sonnet', brief: 'Real provider end to end: a tiny Python project with a failing pytest test; `tm ticket new "Make the failing test pass without changing the test"`, `tm run <T>` bounded 600s: edits the right file, verification runs the tests, reaches submitted; accept → closed.', live: true },
  { key: 'live-background', m: 'sonnet', brief: 'Real provider, background work: queue three small tickets in a tiny project (one depending on another) and run `tm sched run` bounded 900s: all reach submitted in dependency order, with clear progress output; accept them.', live: true },
  { key: 'live-nav-chat', m: 'sonnet', brief: 'Real provider chat needing navigation, in a clone of ' + PRIMARY + ' after `tm init`: `tm -p --json "Where is the ticket state machine and which transitions exist? Cite files."`: uses navigation/search tools, answers correctly, reasonable tokens.', live: true },
  { key: 'decider', m: 'haiku', brief: 'System-one deciders (D-020): whatever is wired (mock decider, Jev over Vercel AI Gateway, Laya via a local mlx_lm.server) classifies a new ticket in shadow mode and records it; config picks the decider; errors are clear when none is configured. Use the mock decider unless a real one is configured.' },
  { key: 'tui', m: 'sonnet', brief: 'The TUI (mock provider). Load the terminal-mcp skill, createSession for your own session (never the default), destroySession at the end; at most 30 tool calls. In SCRATCH after `tm init` launch SCRATCH/tm: chat welcome, `/` command list, `?` shortcuts, ← ← tickets screen, dispatch, Space peek, Ctrl+B board, the milestones/timeline views if present, Esc back. Screenshot; report broken keys, glitches, confusing states, visible machine-speak.' },
  { key: 'providers', m: 'haiku', brief: 'Provider and model setup from nothing: with no credentials, what `tm`, `tm doctor`, `tm auth`/provider commands and the chat\'s /connect, /provider, /config and /model tell a new user. Then configure a provider (real credentials only through the one-process .env load; otherwise point it at a dummy OpenAI-compatible URL) and check routing, fallback and the errors when it\'s unreachable. Every state is explained, with the next step.' },
  { key: 'install', m: 'haiku', brief: 'Install and first run as a new user: follow README.md and docs/install.md in ' + PRIMARY + ' literally (use `mise run release` or the documented local path, never publish), then the first-run experience: `tm`, `tm --help`, `tm doctor` in an empty project. Everything documented works as written.' },
]

const CRATES = ['tm-acp', 'tm-agent', 'tm-auth', 'tm-browser', 'tm-codeintel', 'tm-computer', 'tm-context', 'tm-core', 'tm-docs', 'tm-e2e', 'tm-events', 'tm-genesis', 'tm-harness', 'tm-mcp', 'tm-mirror', 'tm-notify', 'tm-provider', 'tm-pty', 'tm-scheduler', 'tm-server', 'tm-templates', 'tm-types', 'tm-wiki', 'tm-workflow', 'xtask']
const SWEEPS = [
  ...CRATES.map(c => ({ key: c, where: `crates/${c}` })),
  { key: 'tm-cli-1', where: 'crates/tm-cli/src (the first third of `ls crates/tm-cli/src | sort`)' },
  { key: 'tm-cli-2', where: 'crates/tm-cli/src (the second third of `ls crates/tm-cli/src | sort`)' },
  { key: 'tm-cli-3', where: 'crates/tm-cli/src (the last third of `ls crates/tm-cli/src | sort`) and crates/tm-cli/tests' },
  { key: 'tm-tui-chat', where: 'crates/tm-tui/src/chat' },
  { key: 'tm-tui-screens', where: 'crates/tm-tui/src/screens and the rest of crates/tm-tui/src' },
  { key: 'web', where: 'clients/web/src' },
  { key: 'ts', where: 'clients/ts' },
  { key: 'vscode', where: 'clients/vscode/src' },
  { key: 'macos', where: 'clients/macos' },
  { key: 'docs', where: 'CLAUDE.md, SPEC.md and docs/decisions (claims vs. the code: check each concrete claim you can in a few greps)' },
]

// ---------- steps ----------
async function prep(round) {
  return withLock(primaryLock, () => call(`Round ${round} of run ${stamp}: prepare.
1. Integration worktree ${INTEG} (branch integrate): if missing, \`git -C ${PRIMARY} worktree add ${INTEG} integrate\` (or \`-b integrate ... main\` if the branch doesn't exist). If it has uncommitted changes, they are a batch a halted run left half-done: discard them (\`git -C ${INTEG} checkout -- . && git -C ${INTEG} clean -fd\`), their tasks are still open in TASKS.md. Then \`git -C ${INTEG} merge --no-edit main\`; if main in ${PRIMARY} is behind integrate, \`git -C ${PRIMARY} merge --ff-only integrate\` (skip if refused).
2. Build from the integrate branch: \`cd ${INTEG}; unset CARGO_TARGET_DIR; mise run build\`, \`pnpm -C clients/web install --frozen-lockfile && pnpm -C clients/web build\`, \`mkdir -p /tmp/tm-wide && cp target/debug/tm ${BIN}\`. Also make ${PRIMARY}/clients/web/dist current: \`pnpm -C ${PRIMARY}/clients/web install --frozen-lockfile && pnpm -C ${PRIMARY}/clients/web build\`.
3. cli: in a scratch dir (\`cd $(mktemp -d); export TM_HOME=$(mktemp -d)\`), \`${BIN} --help\` and \`${BIN} <sub> --help\` for each subcommand: one entry per top-level subcommand (not help) with its child names.
Return ok, notes (one line), cli.

${RULES}`, { label: `prep:r${round}`, phase: 'Prep', schema: PREP, model: 'haiku', effort: 'low' }))
}

async function load(round) {
  return call(`Run ${stamp}, round ${round}. Read ${PRIMARY}/.claude/worktrees/tm-integrate/docs/tasks/TASKS.md (read-only; it is the newest copy). Return:
- closed: ids (the bold text after the checkbox) of every "- [x]" task, and of every "- [~]" task whose reason says it needs the owner, is superseded, is already satisfied, or "gave up after a retry".
- open: every "- [ ]" task: id; severity from its "severity:" field, else "high" for tasks in section T and "medium" for batch sections B1..B22; m (its model); f (its file paths from "files:"); d (its deps from "deps:", empty for "none").
- retry: every other "- [~]" task (deferred because it failed or a dependency did not land): same fields plus reason.`,
    { label: `load:r${round}`, phase: 'Prep', schema: LOAD, model: 'haiku', effort: 'low' })
}

function probePrompt(p, round) {
  return `Check ONE thing about tm (a Rust ticket and agent harness; CLAUDE.md in ${PRIMARY} describes its surfaces) by using it, then stop. Round ${round}: earlier rounds fixed what they found; confirm it works now and find what is still wrong.
What to check: ${p.brief}
${SCRATCH}
${p.live ? LIVE : 'Use the mock provider (TM_TEST_MOCK_PROVIDER=1) unless the brief says otherwise.'}
At most about 30 tool calls. Read code only to pin an observed defect to the file and function that must change.
${SEARCH}
${VOICE}
${FINDING_RULES(6)}

${RULES}`
}

function sweepPrompt(s) {
  return `Sweep ONE part of the tm codebase for things that make it feel vibe-coded rather than finished: ${PRIMARY}/${s.where}. Read-only.
Look for: stubs and placeholders that pretend to work (todo!(), unimplemented!(), "not yet", hardcoded fake values such as a cost of 0, functions that return canned data); features that exist but aren't wired to any surface (nothing calls them outside tests); two concepts that do the same thing under different names; dead code; user-facing text that is machine-speak; error handling that swallows failures; doc comments or docs that claim something the code doesn't do. At most about 30 tool calls.
${SEARCH}
 Only report what you confirmed in the code (cite file:line).
${VOICE}
${FINDING_RULES(8)}

${RULES}`
}

function cliPrompt(g) {
  return `Exercise \`tm ${g.cmd}\`${g.children.length ? ` and its subcommands (${g.children.join(', ')})` : ''} as a new user would, mock provider. ${SCRATCH}
For each: --help, one happy path (create what it needs first), one mistake (missing arg, unknown id, wrong state). Judge whether it does what its name says, whether a person understands the output and errors, whether it sits in the right place in the command tree (see the command-surfaces decision doc in ${PRIMARY}/docs/decisions if there is one), and whether any of it only pretends to work. At most about 25 commands.
${SEARCH}
${VOICE}
${FINDING_RULES(6)}

${RULES}`
}

async function record(label, intro, payload, idPrefix) {
  return withLock(primaryLock, () => call(`Record tasks into docs/tasks/TASKS.md in the integration worktree ${INTEG} (branch integrate; it must have no uncommitted changes other than yours).
${intro}
Input (JSON): ${JSON.stringify(payload)}
1. Don't read source files. Read docs/tasks/README.md, section "## T — Found in live trials" of TASKS.md, and the open titles (\`grep '^- \\[ \\]' docs/tasks/TASKS.md\`).
2. Merge input tasks that describe the same defect; drop any already covered by an open task (you may add one clarifying sentence to that task's change line).
3. Unique ids \`${idPrefix}-<slug>\`; deps = ids of new or open tasks it needs first.
4. Append at the end of section T in the existing entry format, model line \`model: <m> · severity: <s> · builds Rust: <yes|no> · area: <area> · deps: <ids or none>\`, then files/change/acceptance/test/evidence lines, each entry self-contained.
5. \`mise run hygiene\` passes (no bare D-NNN for decision docs that don't exist). Commit only TASKS.md: "docs(tasks): ${label}". Then \`git -C ${PRIMARY} merge --ff-only integrate\` (skip if refused) and \`git push origin integrate:main\` (fetch and merge origin/main first if rejected; never force).
Return tasks (metadata of each task you added: id, severity, m, f, d) and notes.

${RULES}`, { label: `record:${label}`, phase: 'Reduce', schema: RECORD, model: 'sonnet', effort: 'low' }))
}

function editPrompt(t, owned, shared, others) {
  return `Make the code change for ONE task, editing files in the shared integration worktree ${INTEG} (other agents edit other files there right now).
Task \`${t.id}\`: its spec is the \`**${t.id}**\` entry in ${INTEG}/docs/tasks/TASKS.md (grep for it; don't read the whole file).${t.note ? `\nEarlier attempt: ${t.note}\nTake a different approach where that one failed.` : ''}
Files you own in this batch: ${owned.join(', ') || '(none declared: create or edit only files nobody else owns)'}.
${shared.length ? `Shared files (other tasks need them too): ${shared.join(', ')}. Do NOT edit them; put exactly what must change in each (file, where, text to add or replace) in shared_edits for the integrator.` : ''}
Files other agents own right now (never touch): ${others.join(', ') || 'none'}.
Head start: branches named salvage/* hold unverified edits from a halted earlier run (\`git -C ${INTEG} diff integrate salvage/br1.1 -- <your files>\`). If one touches your files for this task, reuse what's right instead of starting over.
Don't build, run cargo or tests, commit, or change git state (read-only git diff/log/show are fine). You may run \`rustfmt --edition 2021 --check <file>\` for syntax, and use LSP diagnostics if you have them. ${SEARCH} LSP diagnostics and go-to-definition are fine too. Match the surrounding style and comment density. Add or update the unit tests that prove the acceptance check in files you own (#[cfg(test)] at the bottom of the file). A task that needs a decision doc writes the next free docs/decisions/D-NNN-*.md (\`ls ${INTEG}/docs/decisions\`; D-002's format). If the code already does what the task asks, return already-done with evidence; if only the owner can decide something, return blocked and say what.
${VOICE}
Return id, status, files (every file changed or created), shared_edits, summary (2 sentences), tests (which tests prove it).`
}

async function integrate(n, batch, edits) {
  return withLock(primaryLock, () => call(`Land batch ${n} in the integration worktree ${INTEG} (branch integrate). Editors changed files there without building. Their reports: ${JSON.stringify(edits)}
Tasks: ${batch.map(t => t.id).join(', ')} (specs in ${INTEG}/docs/tasks/TASKS.md).
1. \`cd ${INTEG}; unset CARGO_TARGET_DIR\`. Apply each report's shared_edits.
2. \`mise run fmt\`, then \`env -u CARGO_TARGET_DIR cargo check --workspace --all-targets -j 2\`; fix errors. If client files changed, run that client's pnpm test and build.
3. \`mise run verify\` (run it in the background and wait; it is slow). Fix what fails (use zvec_grep_rg to find tests pinned to a message's text instead of reading test files): compile errors, clippy, tests pinned to old message text (update them when the new text is right), hygiene. A task you can't make work with a reasonable fix: revert only its files (\`git checkout -- <files>\`, delete files it created) and report it failed with a one-line reason. Re-run until green.
4. Commit one commit per task whose files are its own ("<type>(<scope>): <summary>" and a "Task: <id>" line), shared-file changes in a final commit. In docs/tasks/TASKS.md turn each landed task's "- [ ]" or "- [~]" into "- [x]" and append " (landed <short sha>)"; tasks an editor reported already-done get "- [x]" with " (already satisfied)"; blocked ones get "- [~]" with " (needs the owner: <what>)". Commit "docs(tasks): land batch ${n}".
5. \`git merge --no-edit main\` (re-run affected tests if main moved), \`git -C ${PRIMARY} merge --ff-only integrate\` (if refused because of someone's local changes, leave them: main_updated=false), \`git push origin integrate:main\` (if rejected, \`git fetch origin main && git merge --no-edit origin/main\`, re-check, push; retry network errors 4 times at 2s/4s/8s/16s; never force).
Return landed, failed ({id, reason}), green, head (short sha), pushed, main_updated, notes (≤3 lines).

${RULES}`, { label: `integrate:${n}`, phase: 'Integrate', schema: INTEGRATE, model: 'sonnet', effort: 'medium' }))
}

function nextBatch() {
  for (const t of tasks.values()) if (t.status === 'pending' && depState(t) === 'dead') t.status = 'skipped'
  const ready = [...tasks.values()].filter(t => t.status === 'pending' && depState(t) === 'ok').sort((a, b) => a.prio - b.prio || a.seq - b.seq)
  const chosen = []
  const owner = new Map()
  for (const t of ready) {
    if (chosen.length >= BATCH) break
    if (t.f.some(f => !SHAREABLE.test(f) && owner.has(f))) continue
    chosen.push(t)
    for (const f of t.f) { if (!owner.has(f)) owner.set(f, []); owner.get(f).push(t.id) }
  }
  return chosen.map(t => ({
    t,
    owned: t.f.filter(f => owner.get(f).length === 1),
    shared: t.f.filter(f => owner.get(f).length > 1),
    others: [...owner.keys()].filter(f => !t.f.includes(f)),
  }))
}

let batchNo = 0
async function buildLoop(round) {
  while (!halted) {
    const plan = nextBatch()
    if (!plan.length) break
    batchNo++
    const tag = `r${round}b${batchNo}`
    log(`Batch ${tag}: ${plan.map(p => p.t.id).join(', ')}`)
    for (const p of plan) { p.t.status = 'running'; p.t.tries++ }
    const edits = await parallel(plan.map(p => () => {
      const model = p.t.m === 'haiku' && p.t.tries === 1 ? 'haiku' : 'sonnet'
      const effort = p.t.m === 'opus' || p.t.tries > 1 ? 'high' : p.t.m === 'haiku' ? 'low' : 'medium'
      return call(editPrompt(p.t, p.owned, p.shared, p.others), { label: `edit:${p.t.id}`, phase: 'Edit', schema: EDIT, model, effort })
    }))
    if (halted) { for (const p of plan) p.t.status = 'pending'; break }
    const reports = edits.map((e, i) => e || { id: plan[i].t.id, status: 'blocked', summary: 'the editor died without reporting' })
    const res = await integrate(tag, plan.map(p => p.t), reports)
    if (!res) { for (const p of plan) p.t.status = 'pending'; break }
    const landed = new Set(res.landed || [])
    const failed = new Map((res.failed || []).map(f => [f.id, f.reason]))
    for (const [i, p] of plan.entries()) {
      const rep = reports[i]
      if (landed.has(p.t.id) || rep.status === 'already-done') p.t.status = 'merged'
      else if (rep.status === 'blocked' && rep.summary !== 'the editor died without reporting') { p.t.status = 'blocked'; p.t.note = rep.summary }
      else if (p.t.tries >= 2) { p.t.status = 'failed'; p.t.note = failed.get(p.t.id) || rep.summary }
      else { p.t.status = 'pending'; p.t.note = failed.get(p.t.id) || rep.summary }
    }
    log(`Batch ${tag}: ${landed.size}/${plan.length} landed, ${res.green ? 'green' : 'RED'}, pushed=${res.pushed}. ${res.notes}`)
  }
}

async function markDeferred() {
  const dead = [...tasks.values()].filter(t => (t.status === 'failed' || t.status === 'skipped') && !t.marked)
  if (!dead.length || halted) return
  dead.forEach(t => { t.marked = true })
  await withLock(primaryLock, () => call(`In ${INTEG} (branch integrate), docs/tasks/TASKS.md: mark these deferred, turning "- [ ] **<id>**" into "- [~] **<id>**" and appending the reason in parentheses:
${dead.map(t => `- ${t.id}: ${(t.retried ? `gave up after a retry: ${t.note}` : t.status === 'skipped' ? 'a task it depends on did not land' : `failed twice: ${t.note}`).replace(/\n/g, ' ').slice(0, 250)}`).join('\n')}
Commit only that file ("docs(tasks): defer ${dead.length} task(s)"), then \`git -C ${PRIMARY} merge --ff-only integrate\` (skip if refused) and \`git push origin integrate:main\`. Return ok and notes.

${RULES}`, { label: 'mark-deferred', phase: 'Integrate', schema: S({ ok: bool, notes: str }), model: 'haiku', effort: 'low' }))
}

async function judge(round, probeSummary, openCount) {
  return call(`You decide whether ticket-master (tm) is finished. The standard: "implemented till no issues or remaining work, and in-depth analysis finds it tasteful, coherent, and it can actually be driven through its various surfaces to work on projects end to end." Round ${round}.
This round's probes (pass/fail and finding titles): ${JSON.stringify(probeSummary)}
Open tasks remaining in TASKS.md: ${openCount}.
Read ${PRIMARY}/CLAUDE.md, docs/decisions/D-019-claude-code-parity-shell.md and the command-surfaces decision doc if one exists. ${SEARCH}
Then drive the product yourself for about 25 tool calls: ${SCRATCH} Try the chat (\`tm -p\`, mock), the tickets flow, \`tm serve\` briefly, and whatever the probes found weakest. Judge taste (does it feel designed: consistent names, calm clear copy, sensible defaults, no leftover scaffolding), coherence (do tickets, sessions, workers, projects, milestones and deciders fit one model?), and drivability (can a person get real work done end to end on each surface?).
Return done (true only if you would ship it to a demanding user as is), verdict (≤5 lines), surfaces (surface, score 1-10, note), and tasks: the gaps that stand between this and done, most important first.
${VOICE}
${FINDING_RULES(10)}

${RULES}`, { label: `judge:r${round}`, phase: 'Judge', schema: JUDGE, model: 'opus', effort: 'medium' })
}

// ---------- main ----------
const history = []
let converged = false
for (let round = 1; round <= maxRounds && !halted && !converged; round++) {
  phase('Prep')
  const p = await prep(round)
  if (!p || !p.ok) { log(`Round ${round}: prep failed (${p ? p.notes : 'no report'})`); break }
  const st = await load(round)
  if (!st) break
  tasks.clear()
  for (const id of st.closed) tasks.set(id, { id, status: 'merged', f: [], d: [] })
  for (const t of st.open) addTask(t, SEV[t.severity] ?? 2)
  for (const t of st.retry) addTask(t, (SEV[t.severity] ?? 2) + 0.5, `deferred before: ${t.reason}`)
  log(`Round ${round}: ${st.open.length} open, ${st.retry.length} to retry, ${st.closed.length} closed`)

  // Land what is already known before looking for more.
  phase('Edit')
  await buildLoop(round)
  await markDeferred()
  if (halted) break

  phase('Map')
  const sweepThisRound = round === 1 || round % 2 === 0
  const [probeRes, sweepRes, cliRes] = await Promise.all([
    parallel(PROBES.map(pr => () => call(probePrompt(pr, round), { label: `probe:${pr.key}:r${round}`, phase: 'Map', schema: FINDINGS, model: pr.m, effort: 'low' }))),
    sweepThisRound ? parallel(SWEEPS.map(s => () => call(sweepPrompt(s), { label: `sweep:${s.key}:r${round}`, phase: 'Map', schema: FINDINGS, model: 'haiku', effort: 'low' }))) : Promise.resolve([]),
    round === 1 ? parallel(p.cli.map(g => () => call(cliPrompt(g), { label: `cli:${g.cmd}:r${round}`, phase: 'Map', schema: FINDINGS, model: 'haiku', effort: 'low' }))) : Promise.resolve([]),
  ])
  if (halted) break
  const probes = probeRes.map((r, i) => r && { probe: PROBES[i].key, pass: r.pass, works: r.works, tasks: r.tasks }).filter(Boolean)
  const sweeps = sweepRes.map((r, i) => r && { area: SWEEPS[i].key, tasks: r.tasks }).filter(r => r && r.tasks.length)
  const clis = cliRes.map((r, i) => r && { group: p.cli[i].cmd, tasks: r.tasks }).filter(r => r && r.tasks.length)
  const probeSummary = probes.map(x => ({ probe: x.probe, pass: x.pass, findings: x.tasks.map(t => `${t.severity}: ${t.title}`) }))

  phase('Reduce')
  const recs = [await record(`round ${round} probes`, 'Findings from small probes that each exercised one flow for real. Make sure offline end-to-end tests exist (crates/tm-e2e or crates/tm-cli/tests) for every probe that failed, so the fix stays fixed; add them as tasks if missing.', probes, `p${round}`)]
  if (sweeps.length || clis.length) recs.push(await record(`round ${round} sweeps`, 'Findings from read-only sweeps of each crate and client (stubs, unwired features, duplicate concepts, machine-speak, false docs) and from exercising each CLI group.', { sweeps, cli: clis }, `s${round}`))
  let fresh = 0
  for (const r of recs) if (r) for (const t of r.tasks) { addTask(t, SEV[t.severity] ?? 2); fresh++ }
  const serious = probes.flatMap(x => x.tasks).concat(sweeps.flatMap(x => x.tasks)).filter(t => t.severity === 'critical' || t.severity === 'high').length

  phase('Edit')
  await buildLoop(round)
  await markDeferred()
  if (halted) break

  phase('Judge')
  const open = [...tasks.values()].filter(t => t.status === 'pending' || t.status === 'running').length
  const blocked = [...tasks.values()].filter(t => t.status === 'blocked').map(t => ({ id: t.id, note: t.note }))
  const j = await judge(round, probeSummary, open)
  let judgeTasks = 0
  if (j && j.tasks.length) {
    const r = await record(`round ${round} judge`, `Gaps named by the final judge. Verdict: ${j.verdict}`, j.tasks, `j${round}`)
    if (r) for (const t of r.tasks) { addTask(t, SEV[t.severity] ?? 1); judgeTasks++ }
  }
  const allPass = probes.length === PROBES.length && probes.every(x => x.pass)
  history.push({ round, probes: probes.map(x => `${x.probe}: ${x.pass ? 'pass' : 'FAIL'}`), fresh, serious, judge: j && { done: j.done, verdict: j.verdict, surfaces: j.surfaces }, blocked, open })
  log(`Round ${round}: probes ${probes.filter(x => x.pass).length}/${PROBES.length} pass, ${fresh} new tasks (${serious} critical/high), judge ${j ? (j.done ? 'DONE' : 'not done') : 'missing'}, ${judgeTasks} judge tasks`)
  converged = allPass && serious === 0 && !!(j && j.done) && judgeTasks === 0 && open === 0
}

return {
  converged,
  halted,
  history,
  stillOpen: [...tasks.values()].filter(t => ['pending', 'running'].includes(t.status)).map(t => t.id),
  failed: [...tasks.values()].filter(t => t.status === 'failed').map(t => ({ id: t.id, note: t.note })),
  blocked: [...tasks.values()].filter(t => t.status === 'blocked').map(t => ({ id: t.id, note: t.note })),
}
