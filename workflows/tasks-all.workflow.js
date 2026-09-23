export const meta = {
  name: 'tasks-wide',
  description: 'Wide map-reduce over tm: many small probes/sweeps find defects, one design pass fixes command hierarchy, recorders write TASKS.md, then batches of no-build editors + one integrator that builds, verifies and lands each batch',
  phases: [
    { title: 'Inventory', detail: 'build tm, list commands and copy-bearing files, set up the integration worktree' },
    { title: 'Map', detail: 'small agents: one CLI group, one file of copy, or one flow probe each' },
    { title: 'Reduce', detail: 'command-architecture design, dedupe into TASKS.md' },
    { title: 'Edit', detail: 'no-build editors, one task each, disjoint files' },
    { title: 'Integrate', detail: 'one agent per batch: build, fix, verify, commit, land on main, push' },
  ],
}

const PRIMARY = '/Users/allie/Develop/ticket-master'
const INTEG = `${PRIMARY}/.claude/worktrees/tm-integrate`
const BIN = '/tmp/tm-wide/tm'
const BATCH = 8
const planTasks = (args && args.planTasks) || []
const skipDiscovery = !!(args && args.skipDiscovery)
const maxProbeRounds = (args && args.maxProbeRounds) || 2
const CRITIC_JOURNAL = (args && args.criticJournal) || ''
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
      log('Three agents in a row returned nothing (probably a usage limit). Stopping. Relaunch FRESH with a new args.stamp and args.skipDiscovery: true; TASKS.md and the tm-integrate worktree carry the state.')
    }
  } else nullStreak = 0
  return r
}

const tasks = new Map()
let seq = 0
function addTask(t, prio) {
  if (!t || !t.id || tasks.has(t.id)) return
  tasks.set(t.id, { id: t.id, m: t.m || 'sonnet', r: t.r !== false, f: t.f || [], d: t.d || [], prio, seq: seq++, status: 'pending', tries: 0, note: '' })
}
// Files several tasks in one batch may share: the integrator applies their edits.
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
const META = S({ id: str, severity: SEVE, m: { enum: ['haiku', 'sonnet', 'opus'] }, r: bool, f: strs, d: strs })
const LOAD = S({ closed: strs, open: { type: 'array', items: META } })
const INV = S({
  ok: bool, notes: str,
  cli: { type: 'array', items: S({ cmd: str, children: strs }) },
  slash: strs,
  copyFiles: strs,
})
const FINDING = S({
  slug: str, title: str, severity: SEVE, kind: { enum: ['bug', 'copy', 'hierarchy', 'missing', 'stub', 'ux'] },
  files: strs, change: str, acceptance: str, test_command: str,
  model: { enum: ['haiku', 'sonnet'] }, builds_rust: bool, evidence: str,
})
const FINDINGS = S({ pass: bool, works: strs, tasks: { type: 'array', items: FINDING } })
const COPY = S({ changed: { type: 'integer' }, changes: { type: 'array', items: S({ old: str, new: str }) } })
const DESIGN = S({ design: str, tasks: { type: 'array', items: FINDING } })
const JEV = S({ summary: str, laya: str, jev: str, tasks: { type: 'array', items: FINDING } })
const RECORD = S({ tasks: { type: 'array', items: META }, notes: str })
const EDIT = S({
  id: str, status: { enum: ['edited', 'already-done', 'blocked'] },
  files: strs, shared_edits: str, summary: str, tests: str,
}, ['id', 'status', 'summary'])
const INTEGRATE = S({
  landed: strs, failed: { type: 'array', items: S({ id: str, reason: str }) },
  green: bool, head: str, pushed: bool, main_updated: bool, notes: str,
})

// ---------- shared prompt text ----------
const RULES = `Hard rules (CLAUDE.md has the why):
- \`unset CARGO_TARGET_DIR\` before any cargo/mise build; never raise the -j 2 cap.
- Never run the compiled tm inside ${PRIMARY} or any worktree. Use \`S=$(mktemp -d)\`, \`export TM_HOME=$(mktemp -d)\`, work in $S; \`TM_TEST_MOCK_PROVIDER=1\` for the mock provider.
- Never read, print, echo, cat, copy or commit .env or any value from it. Never touch .claude/worktrees/odw-* and never run \`mise run clean\` or \`mise run worktree:clean\`.
- Commit messages carry no model names and end with: Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`

const VOICE = `Voice for anything a person reads (CLI output, errors, TUI labels and hints, HTTP errors, web UI):
- Talk to a person, not a log. Say what happened, then what to do next ("Run \`tm ticket retry T-12\` to try again.").
- Plain words. No internal type, enum or struct names, no {:?} debug output, no raw JSON (unless the command is explicitly --json), no event-kind strings, no "Successfully", no stacked "Error: failed to X: error: Y".
- Name things the way the product does: "ticket T-12 (Fix the login redirect)", "worker", "project". Use the CLI's own verbs.
- Short: one line when possible. Sentence case. Keep every format placeholder and its argument order intact.`

const FINDING_RULES = (max) => `Report at most ${max} tasks, most severe first, each atomic (one worker, one sitting) and self-contained: the files to change (exact paths; find the source of a message by grepping ${PRIMARY}/crates or clients for its text), the change, an observable acceptance check, the exact test command (\`mise run test:crate -- <crate>\` or \`pnpm -C clients/<x> test\`), model (haiku for mechanical edits, sonnet otherwise), builds_rust. Severity: critical = the flow does not work at all; high = works but wrong, misleading or unusable; medium = rough, confusing, missing option or affordance; low = polish. Kinds: bug, copy, hierarchy (wrong place, duplicate, inconsistent name), missing, stub (looks real but isn't), ux. Put the command you ran and a ≤3-line output excerpt in evidence. Set pass=false if the thing you were asked to check does not work. List what works in works (short lines). Don't report provider/model setup UX (crates/tm-provider, the chat's /connect /provider /config): another workstream owns it. Skip anything already listed as open in ${PRIMARY}/docs/tasks/TASKS.md (\`grep '^- \\[ \\]' ${PRIMARY}/docs/tasks/TASKS.md\`).`

const SCRATCH = `Binary: \`S=$(mktemp -d); cp ${BIN} "$S/tm"; export TM_HOME=$(mktemp -d)\`; work only inside $S (git init a tiny realistic project there, with one commit). Never run tm inside ${PRIMARY} or any worktree, and don't run cargo. Bound long runs: \`perl -e 'alarm shift; exec @ARGV' 300 "$S/tm" ...\`.`
const LIVE = `Real provider: load the repo credentials into that one tm process only: \`(set -a; . ${PRIMARY}/.env; set +a; exec "$S/tm" <args>)\`. Never print or copy anything from .env. If that is denied, fall back to TM_TEST_MOCK_PROVIDER=1 and say so in works.`

// Small, single-purpose flow probes. Each checks one thing and stops.
const PROBES = [
  { key: 'lifecycle-accept', m: 'haiku', brief: 'Ticket lifecycle, happy path, mock provider: `tm init`, `tm ticket new "<objective>"`, `tm ticket activate`, `tm run <T>`. Does it reach submitted (not escalated)? Then `tm ticket accept` → closed. Check `tm tickets --json` and `tm events` at each step. If the mock cannot finish any ticket, that alone is a critical finding: the offline happy path needs a scripted provider that makes a small edit and submits.' },
  { key: 'lifecycle-reject-retry', m: 'haiku', brief: 'Reject and retry, mock provider: get a ticket to submitted or escalated. `tm ticket reject <T> --reason "..."` and see what happens next (does it rework?); for an escalated ticket, `tm ticket retry <T> --guidance "..."`. Are the states, messages and next steps clear?' },
  { key: 'deps-sched', m: 'haiku', brief: 'Dependencies and the background scheduler, mock provider: create ticket B depending on ticket A (find how: `tm ticket new --help`, `tm ticket --help`), run `tm sched run` bounded to 120s. Is B held until A closes? What does the scheduler print? Are milestones, priorities or due dates available at all?' },
  { key: 'serve-api', m: 'haiku', brief: 'HTTP API, mock provider: `tm serve` on a free port (with workers). With curl: POST /tickets, activate, GET /tickets/{id}, GET /tickets/{id}/events, /schema, /health, /state, the transition endpoint (accept/reject/retry), and 5s of GET /events (SSE). Also GET /app/ with --web-dir ' + PRIMARY + '/clients/web/dist. Are errors JSON with a human message?' },
  { key: 'mcp', m: 'haiku', brief: '`tm mcp` over stdio, mock provider, after `tm init`: send newline-delimited JSON-RPC: initialize, tools/list, tools/call for ticket_dispatch, ticket_list, ticket_show, search_exact, search_hybrid, symbol_def, symbol_outline. Is the dispatched ticket worked? Are tool descriptions and errors clear?' },
  { key: 'genesis-offline', m: 'haiku', brief: 'Genesis offline: `tm genesis --help`, then run it with TM_TEST_MOCK_PROVIDER=1 on "a Python CLI todo app with tests", bounded to 300s. Does it terminate? What does it create (files, tickets, milestones, docs)? Can the resulting tickets be worked with `tm sched run`?' },
  { key: 'genesis-live', m: 'sonnet', brief: 'Genesis with the real provider on "a tiny Python CLI that counts words in a file, with pytest tests", bounded to 600s. Does it terminate, and does it produce a project whose tests pass (run pytest via `uvx pytest` if needed)? What stops it from making a real project?', live: true },
  { key: 'nav-fresh', m: 'haiku', brief: 'Code navigation freshness: `git clone --depth 1 file://' + PRIMARY + ' "$S/repo"`, `tm init` there. On the never-indexed project run every symbol/search/history/outline command `tm --help` lists (e.g. symbol def/refs/callers for `transition` in tm-core). Then add a new file with a new function and edit another; query again. Report empty, wrong, stale or slow results with timings.' },
  { key: 'nav-semantic', m: 'sonnet', brief: 'Semantic search quality: in a clone as above, run the hybrid/semantic search command for 6 conceptual queries ("where are tickets moved between states", "how is the provider chosen for a request", "what decides the token budget of a context pack", "where are events hash-chained", "how does the TUI render the chat transcript", "where are worktrees created for runs"). For each, is the right file in the top 5? Find out which embedder is used and whether it is a real model or a hash stand-in, and say what it would take to use a real local embedding model.' },
  { key: 'prefetch', m: 'sonnet', brief: 'Context prefetch and budget: in a clone as above, create a ticket about a specific function and find how to see the context pack a worker gets for it (a command, `tm run` output, the events log or the store). What was prefetched: outlines, symbols, history, search hits? Is it relevant, and how are tokens and budget shown to a person? Also check what a chat turn prefetches (`tm -p --json` with the mock provider).' },
  { key: 'live-ticket', m: 'sonnet', brief: 'One real end-to-end ticket with the real provider: in a tiny Python project with a failing pytest test, `tm ticket new "Make the failing test pass without changing the test"`, then `tm run <T>` bounded to 600s. Did it edit the right file, run the tests (verification), and reach submitted? Accept it. Report every place the flow broke or was confusing, and how long it took.', live: true },
  { key: 'live-nav-chat', m: 'sonnet', brief: 'One real chat turn needing navigation, real provider: in a clone of ' + PRIMARY + ' (as above, after `tm init`), `tm -p --json "Where is the ticket state machine and which transitions exist? Cite files."`. Did the agent use navigation/search tools (check the steps), was the answer right, and how many tokens did it take?', live: true },
  { key: 'tui', m: 'sonnet', brief: 'The TUI, mock provider. Load the terminal-mcp skill, createSession for your own session (never the default) and destroySession at the end; at most 25 tool calls. In $S (after `tm init`) launch "$S/tm". Screenshot: the chat welcome; `/` command list; `?` shortcuts; ← twice to the tickets screen; type an objective and Enter to dispatch; Space to peek; Ctrl+B board; Esc back. Report broken keys, glitches, confusing states and machine-speak you can SEE.' },
]

// ---------- steps ----------
async function load(stamp) {
  return call(`Run ${stamp}. Read ${PRIMARY}/docs/tasks/TASKS.md (read-only). Return:
- closed: the id (the bold text after the checkbox) of every task line starting "- [x]" or "- [~]".
- open: every task starting "- [ ]" that is NOT in a section whose heading starts with a batch name like "## B1".."## B22" (i.e. tasks in section T and any D-020 or other added sections): id; severity from its "severity:" field if present, else "high"; m (its model); r (true if "builds Rust: yes"); f (its file paths); d (its deps, empty for "none").`,
    { label: 'load', phase: 'Inventory', schema: LOAD, model: 'haiku', effort: 'low' })
}

async function inventory() {
  return call(`Prepare a tm test run. Steps:
1. In ${PRIMARY} (branch main; don't change tracked files): \`unset CARGO_TARGET_DIR; mise run build\`, then \`pnpm -C clients/web install --frozen-lockfile && pnpm -C clients/web build\`. \`mkdir -p /tmp/tm-wide && cp target/debug/tm ${BIN}\`.
2. Integration worktree: if ${INTEG} does not exist, \`git -C ${PRIMARY} worktree add -b integrate ${INTEG} main\` (if branch integrate already exists, add it without -b). If it exists: it must have no uncommitted changes (if it has some, report them in notes and \`git -C ${INTEG} checkout -- . && git -C ${INTEG} clean -fd\`), then \`git -C ${INTEG} merge --no-edit main\`.
3. CLI inventory: in a scratch dir (\`cd $(mktemp -d); export TM_HOME=$(mktemp -d)\`), run \`${BIN} --help\`, and \`${BIN} <sub> --help\` for each subcommand. Return cli: one entry per top-level subcommand (except help) with its child subcommand names (empty if none).
4. slash: the slash command names listed in ${PRIMARY}/crates/tm-tui/src/chat/commands.rs.
5. copyFiles: source files with the most user-facing text. Run \`rg -c '(println!|eprintln!|writeln!|bail!|anyhow!|format!\\(\\s*"|Line::from|Span::|\\.title\\(|TmError::)' crates/tm-cli/src crates/tm-tui/src crates/tm-server/src crates/tm-mcp/src crates/tm-genesis/src crates/tm-scheduler/src crates/tm-core/src\` in ${PRIMARY}; keep files with a count of 6 or more, excluding tests/ directories. Add each .tsx under clients/web/src/views and clients/web/src/components that is not a test. Return at most 45 paths relative to the repo root, largest counts first.
Return ok, notes (one line), cli, slash, copyFiles.

${RULES}`, { label: 'inventory', phase: 'Inventory', schema: INV, model: 'haiku', effort: 'low' })
}

function cliPrompt(g) {
  return `Exercise \`tm ${g.cmd}\`${g.children.length ? ` and its subcommands (${g.children.join(', ')})` : ''} the way a new user would, with the mock provider (TM_TEST_MOCK_PROVIDER=1). ${SCRATCH}
For each: read --help, run it once on a sensible happy path (create what it needs first, e.g. \`tm init\`, a ticket), and try one mistake (missing argument, unknown id, wrong state). Judge: does it do what its name says; would a person understand the output and errors; is it in the right place in the command tree (duplicated elsewhere, inconsistently named, should be a flag instead); is any of it a stub that only pretends. Stay within about 25 commands. Don't read code except to find where a bad message comes from.
${VOICE}
${FINDING_RULES(8)}

${RULES}`
}

function copyPrompt(path) {
  return `Rewrite the user-facing text in ONE file: ${INTEG}/${path}. Edit it in place in that worktree. Only string contents change: no logic, no signatures, no other files. Don't build or commit.
User-facing = what a person reads: CLI output, errors, prompts, TUI labels, hints and status lines, HTTP error messages, web UI text. Not user-facing: tracing/log lines, test code, doc comments, JSON keys, event kinds, identifiers, clap arg names.
${VOICE}
Leave text that is already clear alone; don't churn. Return changed (count) and changes (old → new for each changed string, trimmed to 120 chars each) so the integrator can update tests that match exact text.`
}

function probePrompt(p, round) {
  return `Check ONE thing about tm (\`tm\`, a Rust ticket and agent harness; CLAUDE.md in ${PRIMARY} describes its surfaces) by using it, then stop. Round ${round}${round > 1 ? ': earlier rounds fixed what they found, so confirm it now works and find what is still wrong' : ''}.
What to check: ${p.brief}
${SCRATCH}
${p.live ? LIVE : 'Use the mock provider (TM_TEST_MOCK_PROVIDER=1).'}
Stay within about 30 tool calls. Read code only to pin an observed defect to the file and function that must change.
${VOICE}
${FINDING_RULES(6)}

${RULES}`
}

async function jevResearch() {
  return call(`Make tm's system-one decision providers real (docs/decisions/D-020-system-one-decision-providers.md and docs/vision/system-one-decisions.md in ${PRIMARY}: small fast classifier/decider models, Jev and Laya, used for ticket triage, routing and intent classification). The owner has now asked for this. Research and prove the path, in at most about 40 tool calls:
1. Laya on Kaggle: a Kaggle API token is at ~/.kaggle/access_token; use it only as \`KAGGLE_API_TOKEN=$(cat ~/.kaggle/access_token) uvx kaggle ...\` (never print it). Find the Laya model (kaggle models list -s laya, or search by owner if the vision doc names one); note size, format and license. If it is MLX or convertible and 4GB or smaller, download it to /tmp/laya and run one classification locally with \`uvx --from mlx-lm mlx_lm.generate\` (or mlx_lm.server, which serves an OpenAI-compatible API on localhost). Check free memory first (vm_stat); skip the local run if it would push the 8GB machine into swap, and say so.
2. Jev on Vercel AI Gateway: find its model id, the request shape (OpenAI-compatible chat completions at the gateway), and how auth works for this user (AI_GATEWAY_API_KEY or a Vercel OIDC token; the Vercel CLI is logged in). Search the web and Vercel docs; don't create keys or projects.
3. Read the D-020 doc and ${PRIMARY}/crates/tm-provider (roles, Fabric) to see where a decider plugs in.
Return: summary (what works now), laya (source, size, how to run locally, measured latency or why not), jev (model id, endpoint, auth), and tasks: atomic implementation tasks that get tm to (a) a DecisionProvider trait with a deterministic mock, (b) an OpenAI-compatible HTTP decider usable for both Jev on the gateway and Laya served locally by mlx_lm.server, (c) config to pick one, (d) shadow-mode triage of new tickets (classify, record an event, don't act), (e) a small offline eval. Follow the task format below. Keep secrets out of files and output.
${FINDING_RULES(10)}

${RULES}`, { label: 'research:jev-laya', phase: 'Map', schema: JEV, model: 'sonnet', effort: 'medium' })
}

async function design(inv, cliFindings, probeFindings) {
  return call(`You design tm's command surfaces so they feel like one coherent, tasteful product, not a pile of verbs: the CLI tree, the chat's slash commands, and the TUI's screen hierarchy (including project-management views: milestones, dependencies, timeline/calendar, the Kanban board). Read CLAUDE.md's surface description and docs/decisions/D-019-claude-code-parity-shell.md in ${PRIMARY}; Claude Code (the \`claude\` CLI, \`claude agents\`, its slash commands such as /context /memory /agents /todos /export /doctor /permissions /review /add-dir) is the reference for feel.
Current CLI: ${JSON.stringify(inv.cli)}
Current slash commands: ${JSON.stringify(inv.slash)}
Findings from people exercising the CLI: ${JSON.stringify(cliFindings)}
Flow probe results (pass/fail and titles): ${JSON.stringify(probeFindings)}
Produce: design, a short markdown description of the target structure (at most 60 lines: what stays, what merges, renames, moves under another verb or becomes a flag, what's hidden as plumbing, which slash commands to add and what each does, where PM views live); and tasks, at most 20 atomic tasks that get there, ordered so each builds on the last (mention prerequisites in change). The first task writes the design as the next free docs/decisions/D-NNN-command-surfaces.md. Keep old verbs working as hidden aliases where scripts may rely on them. Don't touch provider/model setup (/connect /provider /config).
${FINDING_RULES(20)}

${RULES}`, { label: 'design:surfaces', phase: 'Reduce', schema: DESIGN, model: 'opus', effort: 'medium' })
}

async function record(label, intro, payload, idPrefix) {
  return withLock(primaryLock, () => call(`Record tasks into ${PRIMARY}/docs/tasks/TASKS.md on branch main. The owner's own session may also commit there: never reset or discard commits you didn't make.
${intro}
Input (JSON): ${JSON.stringify(payload)}
Steps:
1. Read docs/tasks/README.md and the section "## T — Found in live trials" of TASKS.md, and grep the open task titles (\`grep '^- \\[ \\]' docs/tasks/TASKS.md\`).
2. Merge input tasks that describe the same defect, and drop any already covered by an open task (you may append one clarifying sentence to that task's change line instead).
3. Give each a unique id \`${idPrefix}-<slug>\` and deps (ids of new or existing open tasks it needs first).
4. Append them at the end of section T, in the existing entry format, with the model line \`model: <m> · severity: <s> · builds Rust: <yes|no> · area: <area> · deps: <ids or none>\` then files, change, acceptance, test and evidence lines. Write each so a worker can act on it with no other context.
5. \`mise run hygiene\` must pass (no bare D-NNN for decision docs that don't exist yet). Commit only TASKS.md: "docs(tasks): ${label}".
6. If ${INTEG} exists and \`git -C ${INTEG} status --porcelain\` is empty, \`git -C ${INTEG} merge --no-edit main\` so the integration worktree has these entries.
Return tasks (metadata for each task you added: id, severity, m, r, f, d) and notes (one line).

${RULES}`, { label: `record:${label}`, phase: 'Reduce', schema: RECORD, model: 'sonnet', effort: 'low' }))
}

async function criticPass() {
  if (!CRITIC_JOURNAL) return null
  return withLock(primaryLock, () => call(`The benchmark-readiness audit's completeness critic has finished. Its result is the line with "label":"critic" (type "result") in ${CRITIC_JOURNAL}; extract it with node or jq without printing the whole journal. Apply what is right to ${PRIMARY}/docs/tasks/TASKS.md on main: fix wrong file paths, split tasks that aren't atomic, add missing tasks at the end of section T (id prefix \`critic-\`, same entry format with a severity field), and mark obsolete ones "[~] (why)". Keep it tight: only changes the critic justifies with evidence. \`mise run hygiene\` must pass. Commit only TASKS.md: "docs(tasks): apply the audit critic". Return tasks (metadata of tasks you ADDED) and notes.

${RULES}`, { label: 'record:critic', phase: 'Reduce', schema: RECORD, model: 'sonnet', effort: 'low' }))
}

function editPrompt(t, owned, shared, others) {
  return `Make the code change for ONE task, editing files in the shared integration worktree ${INTEG} (other agents are editing other files there right now).
Task \`${t.id}\`: its spec is the \`**${t.id}**\` entry in ${PRIMARY}/docs/tasks/TASKS.md (grep for it; don't read the whole file; if it isn't there, try ${INTEG}/docs/tasks/TASKS.md).${t.note ? `\nA previous attempt failed: ${t.note}` : ''}
Files you own in this batch: ${owned.join(', ') || '(none declared: create or edit only files no one else owns)'}.
${shared.length ? `Shared files (other tasks in this batch need them too): ${shared.join(', ')}. Do NOT edit these; put exactly what must change in each (file, where, the text to add or replace) in shared_edits, and the integrator applies it.` : ''}
Files other agents own right now (never touch): ${others.join(', ') || 'none'}.
Rules: don't build, don't run cargo or tests, don't commit, don't touch git. You may run \`rustfmt --edition 2021 --check <file>\` to catch syntax errors, and LSP diagnostics if you have them. Read only the code you need (zvec-grep search, rg, LSP). Match the surrounding style and comment density. Add or update the unit tests that prove the acceptance check, in the files you own (#[cfg(test)] at the bottom of the file). If the task needs a new decision doc, write the next free docs/decisions/D-NNN-*.md (check \`ls ${INTEG}/docs/decisions ${PRIMARY}/docs/decisions\`; D-002's format). If main already does what the task asks, return already-done with the evidence; if only the owner can decide something, return blocked.
${VOICE}
Return id, status, files (every file you changed or created), shared_edits (empty if none), summary (2 sentences), tests (which tests to run to prove it).`
}

async function integrate(n, batch, edits) {
  return withLock(primaryLock, () => call(`You land batch ${n} in the integration worktree ${INTEG} (branch integrate). Editors changed files there without building. Their reports: ${JSON.stringify(edits)}
Tasks in this batch: ${batch.map(t => t.id).join(', ')} (specs in ${INTEG}/docs/tasks/TASKS.md).
1. \`cd ${INTEG}; unset CARGO_TARGET_DIR\`. Apply each report's shared_edits to the shared files.
2. \`mise run fmt\`, then \`env -u CARGO_TARGET_DIR cargo check --workspace --all-targets -j 2\` and fix errors. If client files changed, run that client's pnpm test/build too.
3. \`mise run verify\` (run it in the background and wait for it; it takes a while). Fix what fails: compile errors, clippy, tests asserting old message text (update them to the new text when the new text is right), hygiene. For a task whose change you can't make work with a reasonable fix, revert only its files (\`git checkout -- <files>\`, remove files it created) and report it failed with a one-line reason. Re-run until green.
4. Commit: one commit per task where its files are its own ("<type>(<scope>): <summary>" plus "Task: <id>"), shared-file changes in a final commit. Then in docs/tasks/TASKS.md turn each landed \`- [ ] **<id>**\` into \`- [x] **<id>**\` with " (landed <short sha>)" appended; do the same for tasks an editor reported already-done, with " (already satisfied)". Leave failed ones unchecked. Commit "docs(tasks): land batch ${n}".
5. Land it: \`git merge --no-edit main\` (if main moved, re-run the affected crates' tests), then in ${PRIMARY}: \`git merge --ff-only integrate\` (if the primary checkout refuses because of someone's local changes, don't touch them: set main_updated=false and continue). Push: \`git -C ${INTEG} push origin integrate:main\` (if rejected, \`git fetch origin main && git merge --no-edit origin/main\`, re-check, push again; retry network errors 4 times, 2s/4s/8s/16s). Never force-push.
Return landed (ids), failed ({id, reason}), green, head (short sha), pushed, main_updated, notes (≤3 lines).

${RULES}`, { label: `integrate:b${n}`, phase: 'Integrate', schema: INTEGRATE, model: 'sonnet', effort: 'medium' }))
}

// ---------- batch builder ----------
function nextBatch() {
  for (const t of tasks.values()) {
    if (t.status === 'pending' && depState(t) === 'dead') t.status = 'skipped'
  }
  const ready = [...tasks.values()].filter(t => t.status === 'pending' && depState(t) === 'ok').sort((a, b) => a.prio - b.prio || a.seq - b.seq)
  const chosen = []
  const owner = new Map()
  for (const t of ready) {
    if (chosen.length >= BATCH) break
    const clash = t.f.some(f => !SHAREABLE.test(f) && owner.has(f))
    if (clash) continue
    chosen.push(t)
    for (const f of t.f) { if (!owner.has(f)) owner.set(f, []); owner.get(f).push(t.id) }
  }
  return chosen.map(t => {
    const owned = t.f.filter(f => owner.get(f).length === 1)
    const shared = t.f.filter(f => owner.get(f).length > 1)
    const others = [...owner.keys()].filter(f => !t.f.includes(f))
    return { t, owned, shared, others }
  })
}

async function buildLoop(label) {
  let n = 0
  while (!halted) {
    const plan = nextBatch()
    if (!plan.length) break
    n++
    const tag = `${label}.${n}`
    log(`Batch ${tag}: ${plan.map(p => p.t.id).join(', ')}`)
    for (const p of plan) { p.t.status = 'running'; p.t.tries++ }
    const edits = await parallel(plan.map(p => () => {
      const model = p.t.m === 'haiku' ? 'haiku' : 'sonnet'
      const effort = p.t.m === 'haiku' ? 'low' : p.t.m === 'opus' ? 'high' : 'medium'
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
      else if (rep.status === 'blocked' || p.t.tries >= 2) { p.t.status = 'failed'; p.t.note = failed.get(p.t.id) || rep.summary }
      else { p.t.status = 'pending'; p.t.note = failed.get(p.t.id) || rep.summary }
    }
    log(`Batch ${tag}: ${landed.size} landed, ${res.green ? 'green' : 'RED'}, pushed=${res.pushed}, main=${res.main_updated}. ${res.notes}`)
  }
}

async function markDeferred() {
  const dead = [...tasks.values()].filter(t => (t.status === 'failed' || t.status === 'skipped') && !t.marked)
  if (!dead.length || halted) return
  dead.forEach(t => { t.marked = true })
  await withLock(primaryLock, () => call(`In ${INTEG} (branch integrate), in docs/tasks/TASKS.md mark these tasks deferred: change "- [ ] **<id>**" to "- [~] **<id>**" and append the reason in parentheses:
${dead.map(t => `- ${t.id}: ${(t.status === 'skipped' ? 'a task it depends on did not land' : t.note || 'failed twice').replace(/\n/g, ' ').slice(0, 250)}`).join('\n')}
Commit only that file ("docs(tasks): defer ${dead.length} task(s)"), \`git merge --no-edit main\`, then \`git -C ${PRIMARY} merge --ff-only integrate\` (skip if refused) and \`git push origin integrate:main\`. Return ok and notes.

${RULES}`, { label: 'mark-deferred', phase: 'Integrate', schema: S({ ok: bool, notes: str }), model: 'haiku', effort: 'low' }))
}

// ---------- main ----------
phase('Inventory')
const stamp = (args && args.stamp) || 'unstamped'
const inv = await inventory()
if (!inv || !inv.ok) return { halted: true, error: 'inventory failed', notes: inv && inv.notes }
const st = await load(stamp)
if (!st) return { halted: true, error: 'could not read TASKS.md' }
for (const id of st.closed) tasks.set(id, { id, status: 'merged', f: [], d: [], r: false })
for (const t of st.open) addTask(t, SEV[t.severity] ?? 1)
for (const t of planTasks) addTask({ id: t.id, m: t.m, r: t.r, f: t.f, d: t.d }, t.b === 0 ? 1 : t.b <= 8 ? 1.5 : 2.5)
log(`Inventory: ${inv.cli.length} CLI groups, ${inv.slash.length} slash commands, ${inv.copyFiles.length} copy files. Tasks: ${st.closed.length} closed, ${[...tasks.values()].filter(t => t.status === 'pending').length} open.`)

const summary = { rounds: [] }
if (!skipDiscovery) {
  phase('Map')
  // Everything in the map runs at once: CLI groups, copy sweep (edits INTEG directly), probes, jev research.
  const [cliRes, copyRes, probeRes, jev] = await Promise.all([
    parallel(inv.cli.map(g => () => call(cliPrompt(g), { label: `cli:${g.cmd}`, phase: 'Map', schema: FINDINGS, model: 'haiku', effort: 'low' }))),
    parallel(inv.copyFiles.map(p => () => call(copyPrompt(p), { label: `copy:${p.split('/').pop()}`, phase: 'Map', schema: COPY, model: 'sonnet', effort: 'low' }))),
    parallel(PROBES.map(p => () => call(probePrompt(p, 1), { label: `probe:${p.key}`, phase: 'Map', schema: FINDINGS, model: p.m, effort: 'low' }))),
    jevResearch(),
  ])
  const cliFindings = cliRes.map((r, i) => r && { group: inv.cli[i].cmd, tasks: r.tasks }).filter(Boolean)
  const probeFindings = probeRes.map((r, i) => r && { probe: PROBES[i].key, pass: r.pass, works: r.works, tasks: r.tasks }).filter(Boolean)
  const copyChanges = copyRes.map((r, i) => r && r.changed ? { file: inv.copyFiles[i], changes: r.changes } : null).filter(Boolean)
  summary.rounds.push({ round: 1, probes: probeFindings.map(p => `${p.probe}: ${p.pass ? 'pass' : 'FAIL'} (${p.tasks.length})`), copyFiles: copyChanges.length, jev: jev && jev.summary })

  phase('Reduce')
  // The copy sweep lands first, as its own batch, so later batches start from clean text.
  if (copyChanges.length && !halted) {
    const res = await integrate('copy', [], copyChanges.map(c => ({ id: `copy:${c.file}`, status: 'edited', files: [c.file], shared_edits: '', summary: `rewrote ${c.changes.length} user-facing strings`, tests: `changed strings: ${JSON.stringify(c.changes).slice(0, 1500)}` })))
    log(`Copy sweep: ${res ? `${res.green ? 'green' : 'RED'}, pushed=${res.pushed}` : 'integrator died'}`)
  }
  const dz = await design(inv, cliFindings.map(c => ({ group: c.group, tasks: c.tasks.map(t => ({ title: t.title, kind: t.kind, severity: t.severity })) })), probeFindings.map(p => ({ probe: p.probe, pass: p.pass, titles: p.tasks.map(t => t.title) })))
  const recs = []
  if (dz) recs.push(await record('command surfaces design', `These come from a design pass over the CLI tree, slash commands and TUI hierarchy. Design summary:\n${dz.design}\nThe CLI findings below were inputs to it; keep only CLI findings the design tasks don't already cover.`, { design_tasks: dz.tasks, cli_findings: cliFindings }, 's1'))
  recs.push(await record('flow probe findings', 'These come from small probes that each exercised one flow for real. Make sure the list also contains offline end-to-end tests (crates/tm-e2e or crates/tm-cli/tests) for: the full ticket lifecycle to closed with a scripted provider, reject/retry, dependencies, the HTTP API, tm mcp dispatch, Genesis offline, and navigation freshness plus search plus prefetch. Those tests are what makes "it works" stay true; add them as tasks if the probes did not.', probeFindings, 'p1'))
  if (jev) recs.push(await record('jev and laya', `System-one decision providers (D-020) are back on: the owner asked for Jev/Laya support. Research summary: ${jev.summary}\nLaya: ${jev.laya}\nJev: ${jev.jev}\nAlso move D-020 out of docs/backlog.md's "Ask the owner later" (the owner approved it on 2026-09-23) and set the doc's status to accepted in a task, not by hand.`, jev.tasks, 'd20'))
  recs.push(await criticPass())
  for (const r of recs) if (r) for (const t of r.tasks) addTask(t, SEV[t.severity] ?? 2)
  log(`Recorded ${recs.filter(Boolean).reduce((a, r) => a + r.tasks.length, 0)} new tasks`)
}

phase('Edit')
await buildLoop('r1')
await markDeferred()

for (let round = 2; round <= maxProbeRounds && !halted; round++) {
  phase('Map')
  const probeRes = await parallel(PROBES.map(p => () => call(probePrompt(p, round), { label: `probe:${p.key}:r${round}`, phase: 'Map', schema: FINDINGS, model: p.m, effort: 'low' })))
  const pf = probeRes.map((r, i) => r && { probe: PROBES[i].key, pass: r.pass, works: r.works, tasks: r.tasks }).filter(Boolean)
  summary.rounds.push({ round, probes: pf.map(p => `${p.probe}: ${p.pass ? 'pass' : 'FAIL'} (${p.tasks.length})`) })
  const serious = pf.flatMap(p => p.tasks).filter(t => t.severity === 'critical' || t.severity === 'high')
  const rec = await record(`round ${round} probe findings`, 'Findings from re-running the flow probes after the fixes landed.', pf, `p${round}`)
  if (rec) for (const t of rec.tasks) addTask(t, SEV[t.severity] ?? 2)
  phase('Edit')
  await buildLoop(`r${round}`)
  await markDeferred()
  if (!serious.length) break
}

const all = [...tasks.values()].filter(t => t.prio !== undefined)
return {
  halted,
  summary,
  merged: all.filter(t => t.status === 'merged').length,
  failed: all.filter(t => t.status === 'failed').map(t => ({ id: t.id, note: t.note })),
  skipped: all.filter(t => t.status === 'skipped').map(t => t.id),
  pending: all.filter(t => t.status === 'pending' || t.status === 'running').map(t => t.id),
}
