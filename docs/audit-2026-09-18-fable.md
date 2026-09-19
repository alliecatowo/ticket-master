# Audit — 2026-09-18

> Scope: SPEC.md §0–§32, `docs/backlog.md` (all sections, treated as binding), `docs/thesis.md`,
> D-001, D-002, all 19 crates, all 4 clients. Method: every claim below carries a `file:line`
> anchor into `main` at `756f7b5`. Two in-flight worktree branches (`wf_75d66949-a4a-1`: tm-tui
> into tm-cli + D-002 gaps; `wf_75d66949-a4a-2`: tm-browser `BrowserProvider` / managed /
> remote-cdp / browser.toml, plus §30.1/§30.2 and §16.4 proptests on both) have uncommitted work
> (7 changed files each) and are **not** re-flagged here except where their scope explicitly
> leaves something out.
>
> Every punch-list item has an id (`B-` blocks-core-thesis, `M-` missing-promised-feature,
> `P-` polish, `N-` nice-to-have), and items tagged **[architecture]** should be routed to land
> before or alongside the features that depend on them.

---

## Executive summary

The sharpest finding is not any one missing feature; it is that the codebase has a recurring
**built-but-unreachable** failure mode, and it has a single root cause. `tm-tui` (9,164 lines, 167
tests, seven screens) is not a dependency of `tm-cli`, so the bare-`tm` TTY path is a `stdin`
readline loop (`crates/tm-cli/src/agent.rs:110-112`). `tm-computer` (4,409 lines) and `tm-browser`
(3,818 lines) are not dependencies of `tm-agent` and appear nowhere in the agent's closed
`ToolName` enum (`crates/tm-agent/src/tools.rs:59-183`), so no agent can ever open a page or click
a button — "computer use" and "browser automation" exist only as `tm browser`/`tm computer` CLI
verbs a human runs by hand (`crates/tm-cli/src/drive.rs`). The same pattern repeats at every
cross-crate seam: the scheduler leases tickets to a placeholder holder and never dispatches an
executor (`crates/tm-scheduler/src/driver.rs:143-153`); `tm run <ticket>` compiles a context pack
and prints "Agent loop execution not yet implemented." (`crates/tm-cli/src/sched.rs:318`); `tm
mirror push|pull|status`, `tm harness promote|epochs`, `tm bench list|run` and `tm docs
list|check|reconcile` return fixed values regardless of project state
(`crates/tm-cli/src/ops.rs:38,80,109,403,494-546,668-708`); the agent loop appends no
`usage.recorded`, `command.*`, `session.*` or `approval.*` events at all (no `store.record_usage`
or `EventDraft` anywhere in `crates/tm-agent/src`). The root cause is documented in the code
itself: the workspace was scaffolded for parallel implementation under a "touch only the file you
were assigned" rule (`crates/tm-provider/src/providers/frontier.rs:26`,
`registry.rs:25,81,107`, `crates/tm-genesis/src/compile.rs:487`, `clients/web/README.md:14`,
`clients/vscode/README.md:17`, `crates/tm-tui/src/screens/diff_viewer.rs:34`), so every
requirement that lives *between* two owners — §11's tool catalog, §8.2's command events, §12's
one-transaction commit, §13's persisted mirror links, §14's session events, §21.5's receipts —
was dropped at the boundary and never picked up by an integration pass. Individually the leaf
crates are good (the event log, state machine, authority algebra, scheduler core, provider fabric,
code intel and context compiler are real and well tested); the system they were meant to form
does not yet run end to end. Fixing this is less about writing new crates than about giving
`tm-core` a durable home for what the leaf crates produce (B-05), making capability registration
polymorphic instead of hand-edited (A-01), and closing the scheduler→executor→evidence loop (B-01).

---

## Part 1 — Root cause analysis [architecture]

### A-02 One cause, many symptoms: ownership-scoped scaffolding with no integration pass

Direct evidence that crates were written by parallel agents with per-file ownership and no
authority to touch a sibling:

| Location | Quote |
|---|---|
| `crates/tm-provider/src/providers/frontier.rs:26` | "does not own (see the workspace rule: touch only the file you were assigned)" |
| `crates/tm-provider/src/providers/registry.rs:25,81,107` | "which this agent does not own"; "`fabric.rs` belongs to a different owner" |
| `crates/tm-genesis/src/compile.rs:487` | "this crate does not own a batch-commit API on `Store` (`tm-core` is owned by another agent)" |
| `crates/tm-agent/src/tools.rs:1670-1673` | "`Store` exposes no generic 'append arbitrary event drafts' entry point … so there is nowhere in `tm-core`'s finished public API to commit them" — `command.started`/`command.completed` drafts are dropped on the floor |
| `crates/tm-agent/src/tools.rs:21` | `ticket.delegated` payload exists "but no `Store` method that emits it" |
| `crates/tm-tui/src/lib.rs:17-18` | "every one of the nine parts scaffolded for parallel implementation" |
| `crates/tm-tui/src/screens/diff_viewer.rs:34`, `ticket_graph.rs:15-21` | "out of scope for this crate, per its dependency list"; "a stub should not guess that contract" |
| `clients/web/README.md:5-20`, `clients/vscode/README.md:11-20` | both say `clients/ts/` "does not exist" and hand-roll `fetch` because the task was scoped to one directory — `clients/ts/` does exist (`clients/ts/package.json:2`), nobody went back |
| `crates/tm-scheduler/src/driver.rs:143-146` | "Provider-fabric resolution of a live worker … is out of this crate's scope (`tm-agent` owns spawning)" — and `tm-agent` never did |

Git history confirms there was never a wiring attempt that got lost: `crates/tm-browser` and
`crates/tm-computer` were added in `7151227` and touched only by doc commits since; `tools.rs`
has two commits, both doc-only. `tm-tui` landed in `f3181bb` with `tm-cli` untouched until the
in-flight branch. Nothing was reverted; the seam was simply never anyone's job.

**Per-instance mechanism** (why wiring is non-trivial, not just a missing `Cargo.toml` line):

- **tm-computer / tm-browser → tm-agent.** `ToolRegistry::dispatch` is a *sync* `fn`
  (`tools.rs:1132`) taking a `ToolContext` with fixed borrowed fields (`tools.rs:1726-1752`),
  while `BrowserSession::eval` (`crates/tm-browser/src/session.rs:755`) and every
  `tm_computer::backend::Backend` method (`backend.rs:212-260`) are `async`. `ToolName` is a
  closed enum: adding one tool means editing six places (`ToolName`, `ALL`, `as_str`, `parse`,
  `standard()`, the `dispatch` match). `tm-browser` invented its own `ActionKind` enum
  (`crates/tm-browser/src/authority.rs`) rather than extending `tm_types::Action`, and
  `tm_types::Action` (`crates/tm-types/src/action.rs:13-60`) has no `computer.input` /
  `computer.capture` / `computer.clipboard` / `browser.navigate` variants despite §20.5 requiring
  them. So wiring requires: async dispatch, a session-registry field on `ToolContext`, new
  `Action` variants, and a registration mechanism — i.e. A-01.
- **tm-tui → tm-cli.** `tm-tui` depends only on `tm-types`, so its screens carry no
  `tm-core` data model; the in-flight branch adds `crates/tm-cli/src/tui.rs` as the adapter
  around `Dashboard`. Correct shape; just late.
- **tm-scheduler → executor.** There is no `Executor` trait anywhere (see Corrections). The
  driver's `Lease` arm invents a holder id and stops.
- **Everything → tm-core persistence.** `Store`'s entire public API is: `open open_with
  create_ticket update_ticket add_dependency activate transition submit verify audit close cancel
  reopen record_failure acquire_lease heartbeat release expire_leases record_decision supersede
  create_milestone close_milestone reopen_milestone store_artifact attach_evidence record_usage
  view scheduler_view rebuild check_invariants counters` (`crates/tm-core/src/store.rs`). The
  schema declares `sessions`, `participants`, `docs`, `doc_provenance`, `harness_epochs`,
  `mirror_links`, `provider_usage` tables (`schema.rs:5,253`) but nothing writes them, and
  `materialize.rs` no-ops the corresponding events. This single gap is what forces tm-docs,
  tm-harness epochs, tm-mirror links, sessions and command events to be volatile, and it is why
  six CLI verbs are fixed-output stubs (B-06).

**Recommendation.** Add a workspace-level *integration contract* test in `xtask` that (a) parses
every `crates/*/Cargo.toml` into a graph and asserts the SPEC-mandated consumer edges exist
(`tm-cli→tm-tui`, `tm-agent→tm-browser`, `tm-agent→tm-computer`, `tm-agent→tm-pty`,
scheduler→executor dispatch), and (b) asserts every capability crate's tools appear in the
assembled `ToolRegistry` (A-01). A crate that is "done" but unreachable should fail `just verify`.

### A-01 Named recommendation: `CapabilityProvider` — polymorphic tool registration

**The bug being fixed.** `ToolRegistry::standard()` (`crates/tm-agent/src/tools.rs:571-1102`) is
a ~530-line hand-written literal. Nothing in the build prompts or forces a new capability crate to
be wired in; that is exactly why tm-computer and tm-browser went dark, and it will happen again
for tm-pty (§22), MCP-client tools (§28.1), and any future capability.

**Target shape** (precise enough to implement):

```rust
// crates/tm-types/src/capability.rs  (new; tm-types so every capability crate can implement it
// without depending on tm-agent, and tm-agent depends on nothing but the trait)
pub struct ToolSchema {
    pub name: &'static str,                 // dotted wire name, e.g. "browser.click"
    pub description: &'static str,
    pub input_schema: serde_json::Value,    // JSON Schema
    pub cost: CostClass,                    // move CostClass here from tm-agent
}

pub trait CapabilityProvider: Send + Sync {
    /// Slug, e.g. "builtin", "browser", "computer", "pty", "mcp:<server>".
    fn id(&self) -> &str;
    /// Every tool this capability contributes.
    fn tools(&self) -> Vec<ToolSchema>;
    /// Pure: map a call's input to the Action `Authority::permits` gates it on.
    fn to_action(&self, tool: &str, input: &serde_json::Value) -> Result<Action>;
    /// §30.1: the slice of Authority this capability needs to be admitted at all. A worker
    /// whose granted authority does not `contains()` this slice gets *no schema* for any of
    /// this provider's tools. (Compose with the in-flight admit-by-authority filter — this is
    /// the same mechanism, not a second one.)
    fn requires(&self) -> AuthorityRequirement;   // e.g. network.docs || network.allowlist non-empty
    /// Execute. Async, because browser/computer/pty are. Receives the per-call context.
    async fn invoke(&self, tool: &str, input: serde_json::Value, ctx: &CallContext)
        -> Result<serde_json::Value>;
}
```

`ToolRegistry` becomes `Vec<Arc<dyn CapabilityProvider>>` plus a name→provider index built at
construction; `ToolRegistry::for_authority(&Authority)` yields the admitted subset;
`tool_defs()` is derived from that. `AgentLoop::new` takes the registry the binary assembled:
`tm-cli` registers `BuiltinTools` (today's 39, moved into `impl CapabilityProvider`),
`tm_browser::BrowserCapability`, `tm_computer::ComputerCapability`, `tm_pty::PtyCapability`, and
later `tm_mcp::McpClientCapability` per configured server — assembled from `harness.toml` /
project config, not compile-time only. `CallContext` replaces today's `ToolContext` and adds a
`SessionRegistry` (lease-keyed browser/computer/pty sessions, §19.1b) and an `EffectJournal`
(B-11).

**What it must not become.**
1. Not the current pattern: a hardcoded enum plus a literal `Vec` a human must remember to
   extend. The compiler should refuse a capability crate that does not implement the trait, and
   the integration test in A-02 should refuse a binary that does not register it.
2. Not "make everything an MCP server". SPEC §30's whole critique of the ecosystem is that
   runtime-injected tool schemas are uncontrolled and paid for on every turn. Native
   capabilities (shell, git, edit, browser, computer, pty) stay in-process, typed, and
   authority-checked by the algebra; MCP (§28.1) is an inbound/outbound *protocol adapter* for
   interop, and the MCP client shows up as *one* `CapabilityProvider` whose tools are filtered by
   the same `requires()` predicate — not as the substrate.

**Reuse of existing idioms.** `tm_mirror::Tracker` (`crates/tm-mirror/src/tracker.rs:104-121`)
is the closest existing shape: `name()`, `capabilities()`, async I/O methods, `tm_types::Result`.
`tm_provider::Provider` (`crates/tm-provider/src/fabric.rs:27-38`) has `id()` + async but no
`capabilities()` on the trait (it lives on a separate `ProviderInfo`) and its own `ProviderError`.
The in-flight `BrowserProvider` (`.claude/worktrees/wf_75d66949-a4a-2/crates/tm-browser/src/provider.rs:88-100`)
copies the Tracker shape (`id()`, `capabilities()`, async `acquire`/`release`,
`TmError::Provider`). The recommended convention for all four provider-shaped traits — providers,
trackers, executors (§24.2 already specifies `id()`/`capabilities()`/async), and capability
providers — is therefore the Tracker/BrowserProvider shape: `fn id(&self) -> &str`, `fn
capabilities(&self) -> XCapabilities`, async operations returning `tm_types::Result<T>`, string-id
registration into a registry keyed by slug, selection by a TOML table. `Provider` should be
brought into line (fold `Capabilities` onto the trait, map `ProviderError` into `TmError`
variants) rather than left as the outlier.

### A-03 Interface consistency across the pluggable seams

| Seam | Trait | Sync/async | Error type | `capabilities()` on trait | Registration / selection | Authority hook |
|---|---|---|---|---|---|---|
| Model providers §6 | `tm_provider::Provider` (`fabric.rs:27`) | async | own `ProviderError` (`types.rs:189`), not `TmError` | no — separate `ProviderInfo::capabilities` (`providers/mod.rs`) | string `match` on slug in `Registry::build_provider` (`registry.rs:156-170`), `providers.toml` | none (budget charged by tm-agent, not here) |
| Trackers §13 | `tm_mirror::Tracker` (`tracker.rs:104`) | async | `tm_types::Result` | yes | `AdapterKind` enum (`config.rs:18-29`) + `mirror.toml` | **none** — external writes are ungated |
| Computer backends §20 | `tm_computer::Backend` (`backend.rs:212`) | async | own `ComputerError` → `TmError` (`lib.rs:108`) | `probe()` returns `Capabilities` | env-var `select_backend` (`backend.rs:82`) | session-level `ApprovalRequired` flag (`session.rs:114,174`), not `Authority::permits` |
| Browser (main) §19 | none — concrete `BrowserSession`; `discover()` (`discover.rs:65`) | async | `CdpError`/`SnapshotError` → `TmError` | none | env `TM_BROWSER` / system search | own `NavigationGuard` + own `ActionKind` enum (`authority.rs`); `eval/click/type` only *traced* (`session.rs:788`), never gated |
| Browser (in-flight) §19.1a | `BrowserProvider` | async | `TmError::Provider` | yes | registry by slug + `browser.toml` | (unchanged) |
| Executors §24 | **does not exist** | — | — | — | — | — |
| Embedders §7 | `tm_codeintel::Embedder` (`embed.rs:22`) | sync | `Result` | `dims()` | hardcoded `LocalHashEmbedder` | n/a |
| Commands §8.2 | `CommandExecutor` / `CommandCache` (`command.rs:105,131`) | sync | `Result` | none | injected by tm-cli (`agent.rs:538`) | `command::run` takes `&Authority` |
| Artifacts | `tm_browser::ArtifactSink` (`session.rs:45`) vs `CommandCache::put` vs `Store::store_artifact` | sync | `Result` | — | three different sinks for one concept | — |
| Bench | `tm_harness::SeededProvider` (`bench.rs:91`) | sync | `Result` | — | no production impl; `tm bench run` fabricates a report | — |

Findings: four traits solve one shaped problem four ways; `Provider` is the outlier on both error
type and capability declaration; **`Authority::permits` is called in exactly one place in the
workspace** (`crates/tm-agent/src/tools.rs:1151`) — tm-mirror, tm-server routes, tm-browser
actions and tm-computer input all bypass the algebra (A-04). Twenty crate-local error enums exist;
only five convert into `TmError` (`CompilationError`, `ReconcileError`, `SnapshotError`,
`ComputerError`, `CdpError`); `ProviderError`, `RuntimeError`, `ServerError`, `AuthError`,
`PatchError`, `SelectionError` and friends each stand alone, and most conversions flatten into
`TmError::Provider(String)`/`Storage(String)`, losing structure (P-07).

### A-04 Authority gating is not uniform at effect boundaries

- tm-agent tools: gated (`tools.rs:1151`). Correct.
- tm-browser: navigation/download gated by `NavigationGuard` against `Authority.network`
  (`authority.rs:35,51`); `click/type/select/press/eval` only append a `TraceEvent`
  (`session.rs:629-788`). §20.5's `browser.navigate` Action class does not exist in `tm_types`.
- tm-computer: `ComputerSession::act` checks an approval boolean and the panic stop
  (`session.rs:170-174`), never an `Authority`; `computer.input/capture/clipboard` Action classes
  do not exist.
- tm-mirror: `Tracker::push` writes to GitHub/Linear/Jira/GitLab with no authority check at all
  (`permits` count 0 in `crates/tm-mirror/src`). §13 says mirror writes are effects; §21.5 says
  every effect carries a receipt. Neither holds.
- tm-server: every mutating route takes `actor` from the request body
  (`routes.rs:288,390`, `authority` defaults to `Authority::none()`), with one shared bearer token
  (`auth.rs:72`). Any client can act as any participant; no per-client attenuated authority
  (backlog "Remote control" requires it).
- tm-core: `Store::transition/reopen/cancel` raise ad hoc `TmError::AuthorityDenied`
  (`store.rs:688,723,757,1208,1246`) by inspecting flags directly rather than through
  `Authority::permits(&Action)`.

Recommendation: one `Gate` in `tm-types` (`fn gate(auth, action, oversight) -> Decision`) that
every effect boundary calls, plus an `Action` enum extended with the §19.4/§20.5/§22.4 classes, and
a hygiene test that greps for external-effect call sites (`reqwest::Client`, `Runtime.evaluate`,
`CGEventPost`, `XTest`, `std::process::Command`) outside an allow-listed gated wrapper.

### A-05 Duplicate ad hoc mechanisms where one abstraction should exist

- **Idempotency (three ways).** GitHub: in-memory `Mutex<BTreeMap<TicketId,u64>>`
  (`github.rs:62,112`) — lost on restart. Linear: `tm-id:<ticket>` label search (`linear.rs:14-18`)
  — durable but adapter-private. SyncEngine: content-hash on `MirrorLink` (`sync.rs:194-212`) —
  needs a persisted link that nothing persists. Commands: blake3 cache key by content
  (`fingerprint.rs`) — keyed by *what* ran, not *(ticket, attempt, effect)*. See B-11.
- **Harness capacity cap (two ways).** `tm_harness::efficacy::EfficacyBudget`
  (`efficacy.rs:23-120`) and `SchedulingPolicy::harness_capacity_fraction`
  (`crates/tm-scheduler/src/policy.rs:115`); tm-scheduler does not depend on tm-harness, so the
  scheduler's is the live one and tm-harness's is orphaned.
- **Artifact sinks (three ways).** `ArtifactSink`, `CommandCache::put`, `Store::store_artifact`.
- **Command execution.** `tm_context::CommandExecutor` + `ProcessCommandExecutor` in tm-cli
  (`agent.rs:530-556`) is the gated path; tm-mirror's `reqwest` calls and tm-browser's process
  launch are separate ungated paths.
- **Speculative generality with one (or zero) implementations:** `SeededProvider` (no production
  impl), `GitInspector`, `DecisionStore`, `LeaseView`. Not wrong, but they should not be counted as
  "pluggable" in status reports.

### A-06 The closed `EventKind` catalogue has no extension process

§3.2 makes the catalogue closed on purpose, but there is no recorded process for the version bump
it requires, and every gap below (goal events, tool-denial events, effect receipts, session events
from the CLI) needs new kinds. Recommend a `D-003` that defines: how a kind is added, the
`materialize.rs` arm that must accompany it, and the replay fixture that must be regenerated.

---

## Part 2 — Prioritized punch list

### Tier 1 — blocks-core-thesis

**B-01 The scheduler never dispatches work; the loop does not close.** [architecture]
Evidence: `crates/tm-scheduler/src/driver.rs:143-153` leases to `agent:<role>/<ticket>` and
returns; `crates/tm-cli/src/sched.rs:105-149` (`tm sched run`) only ticks;
`crates/tm-cli/src/sched.rs:288-320` (`tm run`) compiles a pack then prints "not yet
implemented"; the only entry to `AgentLoop::run` is the interactive readline
(`crates/tm-cli/src/agent.rs:213-268`) with `Authority::root()` and `Budget::unlimited()`
(`agent.rs:226-227`) — i.e. the one path that runs an agent ignores the ticket's authority.
Done looks like: `Executor` trait + `ExecutorTask/Outcome/Capabilities` in `tm-core` (§24.2);
`tm_agent::BuiltinExecutor` implementing it; an `ExecutorDispatcher` the scheduler driver calls on
`SchedulerAction::Lease` (holder = real participant id, TTL heartbeat task, outcome →
`Store::submit` with evidence, failure → `record_failure`); `tm run` and `tm sched run` both go
through it; the ticket's attenuated authority and budget are what the loop receives.

**B-02 Computer use and browser automation are unreachable by any agent.** [architecture]
Evidence: `crates/tm-agent/Cargo.toml` (no `tm-browser`/`tm-computer`); `ToolName` closed enum
`tools.rs:59-183` has no `browser.*`/`computer.*`; dispatch is sync (`tools.rs:1132`) vs async
crates; `tm_types::Action` lacks §20.5 classes (`action.rs:13-60`). `tm browser`/`tm computer`
CLI verbs exist (`drive.rs`) but only for a human. Done looks like: A-01 implemented; `tm-browser`
and `tm-computer` each ship a `CapabilityProvider` exposing the §19.3 / §20.2 tool lists;
`Action::{BrowserNavigate, ComputerInput, ComputerCapture, ComputerClipboard}` added and default
oversight puts `ComputerInput/Clipboard` behind approval; sessions keyed by `(project, ticket,
lease)` in a `SessionRegistry` torn down on lease expiry (§19.1b) with a test that expiring a lease
kills the browser process. Downgrade §20's status to PARTIAL until then.

**B-03 tm-tui unreachable from `tm`.** In flight on `wf_75d66949-a4a-1` (adds
`crates/tm-cli/src/tui.rs`, `tm-cli/Cargo.toml` dep, `crates/tm-cli/tests/`). Not re-flagged;
verify on merge that bare `tm` on a TTY enters `Runtime::run` and `--plain`/`NO_COLOR`/`TERM=dumb`
fall back to the readline loop.

**B-04 There is no `Executor` trait (correction — the prior survey said "trait + builtin only").**
Evidence: `rg "trait Executor"` finds nothing; only `ExecutorRequirements`
(`crates/tm-core/src/ticket.rs:189`) and the scheduler's `ExecutorAvailability`
(`select.rs:48`, a role-availability probe). Done looks like: as B-01, plus
`ExecutorCapabilities { streaming, tool_use, patch_output, interactive, accepts_context_pack,
sandboxed, max_context_tokens, cost_class }` matched by `select.rs` against
`ExecutorRequirements` (refuse mismatch), a `human` executor whose `execute` opens an approval and
waits, and sandbox derivation `fn sandbox_for(&Authority) -> Sandbox { fs_scope, net_policy,
cmd_allow }` with return-scope validation of the diff against `authority.repository.write`.

**B-05 `tm-core::Store` has no durable home for half the system.** [architecture]
Evidence: `Store` public API (list in A-02); tables `sessions/participants/docs/doc_provenance/
harness_epochs/mirror_links/provider_usage` declared (`schema.rs:5,253`) but unwritten;
`materialize.rs:13,471` no-ops those events; `tools.rs:1670-1673` drops `command.*` drafts;
`compile.rs:483-495` cannot commit genesis in one transaction. Done looks like:
`Store::append(drafts: Vec<EventDraft>) -> Result<Vec<Event>>` that runs the materializer in the
same transaction (typed drafts only, so the closed catalogue still holds); typed helpers
`start_session/end_session`, `register_doc/invalidate_doc/reconcile_doc`, `promote_epoch`,
`link_mirror/update_mirror_link`, `record_command`; `Store::transaction(|tx| ..)` for genesis;
materializer arms for each; the replay fixture extended so `rebuild()` covers them.

**B-06 Six CLI verbs return fixed output regardless of state.**
Evidence: `tm mirror push` `{"pushed":0}` (`ops.rs:668-676`), `pull` (`:684-694`), `status`
`{"mirrors":[]}` (`:700-708`); `tm harness promote` `{"status":"pending","next_epoch":1}`
(`:403`), `epochs` placeholder (`:415-430`); `tm bench list` "example" task (`:494-510`), `run`
fabricated empty `BenchmarkReport` (`:530-546`); `tm docs list/check/reconcile` build
`DocRegistry::new()` empty every call (`:38,80,109`) so `tm docs check` — the CI gate — always
passes. `bench/tasks/` is empty. Done looks like: each verb loads from `Store` (after B-05) and
drives the real engine: `SyncEngine::push/pull` over trackers from `mirror.toml`
(`sync.rs:196,236`), `EpochRegistry::promote` with `PromotionGate::evaluate`, `BenchRunner::run_all`
over a `SeededProvider` adapter around `MockProvider` with tasks discovered from `bench/tasks/*.toml`
(add at least one real task), `Assessor::assess` over a registry loaded from `docs` tables with
`docs/.tmdocs.toml` / front matter discovery. Golden `--json` snapshots for each.

**B-07 The agent loop writes nothing durable: no usage, no commands, no sessions, no approvals.**
Evidence: `crates/tm-agent/src` contains no `record_usage`, no `EventDraft`; `FabricRecord`
("for the caller to turn into a workspace event", `fabric.rs:40`) has no consumer outside
tm-provider; CLI approvals are a `y/n` on stdin (`agent.rs:316-338`) with no
`approval.requested/decided` event; `tm-server`'s `create_session` appends no `session.started`;
budget is tracked in a local `effective_budget` (`agent_loop.rs:217-218`) never debited in the
store. Consequence: invariants 2, 7, 11, 12 hold vacuously, and the "zero-setup cost accounting"
win in the backlog is impossible. Done looks like: `AgentLoop` gets a `&Store` event path (B-05)
and after every `Fabric::execute` calls `Store::record_usage` (which already debits ancestors,
`store.rs:1377-1449`); `FabricRecord` → `provider.selected/degraded/exhausted`; tool denials and
calls → new kinds `tool.invoked`/`tool.denied` (A-06); `approval.requested/decided` via
`tm-server`'s approvals module reused in-process; `session.started/ended` at loop start/end.

**B-08 §30.3/§30.4 context pruning does not exist (the "dynamic context trimming" ask).**
Evidence: `rebuild_messages` (`agent_loop.rs:478-527`) replays every `StepRecord` verbatim each
turn; no supersession, no addressable results, no compaction; the in-flight branch covers only
§30.1 (authority-derived tool surface) and §30.2 (per-section rent). Done looks like: (1) each
`ToolCallRecord` gets a deterministic `result_key` (`fs.read` → path; `search.*` → normalized
query; `shell.run` → command key; `symbol.*` → symbol+path); (2) a pure
`fn working_set(steps: &[StepRecord]) -> Vec<StepRef>` that keeps only the latest record per key,
drops results whose file was later edited by `edit.*`, and keeps every assistant-text step;
(3) `rebuild_messages` renders the working set, replacing pruned results with a one-line
`[superseded by step N]` stub so tool-use/result pairing stays valid for the provider;
(4) goal state (B-09) is re-injected verbatim at the top of every rebuild; (5) property test:
same step log → identical messages on replay; (6) `AgentOutcome` reports bytes pruned per turn so
§30.2's ledger can show it.

**B-09 §29 goal loop does not exist.**
Evidence: `EventKind` (`crates/tm-events/src/kind.rs`) has no `goal.*`/`step.*`; nothing in
tm-agent tracks a durable objective; no TUI surface. Done looks like: kinds `goal.set`,
`goal.step_added`, `goal.step_completed`, `goal.reoriented`, `goal.claimed_complete` with payloads
in `payload.rs` and a `goals` materialized table; `AgentLoop` sets the goal from the ticket
objective on step 0, re-reads it from the store at every step (re-orientation), bounds
auto-continue by `CycleBudget` + a project-wide `max_events_per_ticket` backstop, and ends with
`goal.claimed_complete` → `Store::submit` (never `verify`); the TUI dashboard renders the goal
from the materialized table.

**B-10 §31 budget-aware execution is the opposite of the spec today.**
Evidence: `Store::record_usage` transitions to `Recovery` with `FailureClass::BudgetExhausted`
(`store.rs:1377-1449`), consuming an attempt; `AgentOutcome::BudgetExhausted`
(`agent_loop.rs:240`) is terminal; `ContextPack` sections (`sections.rs:104-402`) have no
remaining-budget section. Done looks like: new `Trigger::BudgetHandoff` → `Ready` without
`attempts += 1` and without `Recovery` (state-machine change + exhaustive-table update +
regression test); a `SectionKind::Budget` rendering remaining tokens/dollars/wall, burn rate, and
the tier menu from `RoleTable` prices; loop checks `can_afford(next_effect)` before starting an
effect and hands off cleanly (`goal.*` persisted, lease released, `ticket.budget_handoff` event);
scheduler refuses dispatch when estimated cost > remaining and reserves verification budget.

**B-11 §21.5 idempotent effects: no generic mechanism; the ad hoc one is volatile.** [architecture]
Evidence: A-05 (three mechanisms); `github.rs:62,112` in-memory; `sync.rs:194-212` needs a
persisted `MirrorLink` that B-05 shows nothing persists; browser form submits and `git push` have
no receipt at all. Done looks like: `effects` table `(key TEXT PK, ticket, attempt, kind, status
journaled|completed|failed, receipt_artifact, started, completed)`; `EffectKey =
blake3(ticket ‖ attempt ‖ kind ‖ canonical_args)`; `Store::begin_effect(key) -> Result<EffectGuard>`
(journal-first, refuses if a `completed` row exists, returns the prior receipt);
`EffectGuard::complete(receipt_artifact)`; wrappers for `command::run` (non-cacheable commands),
`Tracker::push`, `git.commit/push`, `browser.click/type/press` on forms, `computer.input`; the
"effect ran, receipt lost" case handled per kind by an `confirm(&self) -> Option<Receipt>` probe
(GitHub: search `tm-id` label; git push: `ls-remote`; command: none — re-run) named in each
wrapper; `crash_recovery.rs` extended with "effect ran, process died before receipt".

**B-12 §28 adapter layers: no ACP, no MCP, no `tm-auth`.**
Evidence: `rg -i "\bacp\b|\bmcp\b|keychain|oauth|pkce"` over `crates/` hits only doc comments;
credentials are `std::env::var` in 24 sites across tm-provider (`anthropic.rs:55`,
`providers/openai.rs:54-63`, `openrouter.rs:114-118,189-192`, `cloud.rs:65-71,210-215,352-357`,
`gemini.rs:64-68`, `frontier.rs:69-70,131-132,191`), tm-mirror (`github.rs:75`, `linear.rs:60`,
`jira.rs:147-149`, `gitlab.rs:46`) and tm-server (`auth.rs:72`). Done looks like:
(1) `crates/tm-auth`: `trait AuthAdapter { fn id(); fn kind() -> ApiKey|SubscriptionOAuth|CloudIam|
PlatformEphemeral|Delegated; async fn credential(&self) -> Result<Credential>; fn entitlement(&self)
-> Entitlement { quota_class, rpm, tpm, metered: bool, daily_ceiling } }`; `EnvApiKey`,
`KeychainApiKey` (`security-framework` on macOS / `secret-service` on Linux — needs a dependency
decision), `DeviceCodeOAuth` with refresh; `tm auth <provider>` verb; `Credential` implements
`Debug`/`Display` as `<redacted>` and `Serialize` is not derived; a redaction test that scans every
`--json` snapshot, error message and event payload fixture for a canary key. (2) `crates/tm-acp`:
JSON-RPC 2.0 over stdio client (`initialize`, `session/new`, `session/prompt`, permission
callbacks answered from `Authority::permits`) as the `acp` executor (B-04), and an ACP *agent*
server exposing a project so Zed can drive it. (3) `crates/tm-mcp`: client (stdio + SSE
transports, `tools/list` → one `CapabilityProvider` per server, filtered by `requires()`), and
server exposing `ticket.*`, `search.*`, `symbol.*` read tools over stdio. Fabric routes on
`Entitlement` (subscription vs metered) per §28.2.

**B-13 §25 workflow definitions: missing.**
Evidence: no `workflow` module or TOML parser anywhere. Minimum real implementation:
`crates/tm-workflow` with `WorkflowDef` (TOML: `params`, `[[node]] { id, role, objective
(template), for_each: Static(Vec)|FromOutput(node.field), depends, join: all|any{timeout,
merge}, budget, verification, loop: Option<CycleBudget> }`); `fn expand(def, params, view) ->
Result<GraphProposal>` reusing `tm_genesis::compile::validate_graph` and committing via
`Store::transaction` (B-05); `workflows` table storing versioned defs by content hash with running
instances pinned to a version; `tm workflow list|show|run <name> --param k=v`; `tm doctor` warning
for 1×1 workflows; starter library `review-change` and `harness-benchmark` as fixtures with
snapshot tests of the expanded graph.

**B-14 §26 project wiki: missing.**
Evidence: no `/wiki` route (`routes.rs:55-85`), no wiki module. Minimum real implementation:
`crates/tm-wiki` (or module in tm-docs) that assembles pages from `Store::view()`, `CodeIntel`
outlines and git history: `architecture/<crate>` (module tree + public symbols), `decisions/`
(with supersession chains), `history/<path>` (`history.why`), `tickets/`, `glossary`; each page is
a `DocRecord` with `mode = generated` and `derived_from` so `Assessor` marks it stale and the
banner names the invalidating ticket/decision; written to `docs/wiki/*.md`; served at
`GET /wiki/*` (markdown → HTML) with links to `/tickets/:id`; a `SectionKind::Wiki` retrieval
source in `tm-context` ranked with code. Human pages under `docs/wiki/` with `mode = human` are
never rewritten (reuse the §9 test).

**B-15 §27 project templates: missing.**
Evidence: no template registry, `manifest.toml`, or `skill.md` handling. Minimum real
implementation: `crates/tm-templates` with `TemplateManifest` (id, version, params, tags,
checksum), `apply(template, params, dest)` with `{{param}}` substitution, `verify.toml` executed
through the gated `command::run` path, `skill.md` loaded as a context section (data, not
instructions); registry = `templates.toml` listing path/git/registry sources pinned by version +
blake3; genesis `compile.rs` selects a template by capability tag when the spec names a stack;
starter set of three (Ratatui, Axum, Rust crate) as in-repo fixtures with a CI job that scaffolds
each and runs its `verify.toml`; remaining starters tracked as follow-ups.

**B-16 §22 `tm-pty`: missing, but the seed exists.**
Evidence: no `crates/tm-pty`; `crates/tm-tui/src/testing.rs:187-345` already has `PtyHarness`
(`portable-pty` + `vt100`: `spawn`, `screen`, `write`, `signal`, `wait_for`) gated `cfg(test)`.
Done looks like: extract it into `crates/tm-pty` as `PtySession { spawn, screen, diff, send, key,
expect, resize, wait_exit, record }` with an asciicast writer to an artifact; a `PtyCapability`
(A-01) exposing `pty.*` tools; `Action::PtySend` gated like `RunCommand` plus a distinct approval
class; sessions killed by process group on lease expiry; output bounded into artifacts; tm-tui's
tests depend on `tm-pty` as a dev-dependency instead of carrying their own.

### Tier 2 — missing-promised-feature

**M-01 Browser provider matrix (§19.1a) — remaining after the in-flight branch.**
Evidence on `main`: `discover.rs:65-70` searches `TM_BROWSER` then the system (violates §19.1;
`lib.rs:4-5` still documents it). In flight: `BrowserProvider`, `managed`, `remote-cdp`,
`browser.toml`, downloader. Explicitly deferred and still owed: `docker` (pinned image, connect
over CDP), `browserbase`, `steel`, `browserless`, `hyperbrowser` (each: `acquire` → vendor API →
CDP ws URL; `capabilities()` truthfully declaring `pinned_version: false` unless the vendor pins,
`stealth`, `proxy`, `video_recording`). Also not in either branch's scope: the `(project, ticket,
lease)` session registry, `tm browser list`, `tm browser install`, and tearing the session down
on lease expiry (§19.1b) — these belong with B-02. Adapters should be unit-tested against
recorded vendor JSON with no network, per §13.1's convention.

**M-02 Computer use (§20) is PARTIAL, not DONE.**
Evidence: real backends (`macos.rs`, `linux.rs` incl. Xvfb `linux.rs:1023`), `tm doctor` TCC
probe (`project.rs:787-799`), approval flag + panic stop (`session.rs:114,170-174`) exist; but no
lease coupling, no `Authority` gating, no `Action` classes, no agent reachability. Done: B-02.

**M-03 GitHub app and Linear Agent (backlog "First-party integrations"): not started.**
Evidence: `rg -i "webhook|installation|AgentSession|agentActivity|elicitation|check_run"` over
`crates/tm-mirror/src` and `crates/tm-server/src` returns nothing; `github.rs` is a PAT-based
issues sync (`GITHUB_TOKEN`, `github.rs:31`); `linear.rs` is an API-key GraphQL sync. Done looks
like: (1) `POST /webhooks/github` and `/webhooks/linear` in tm-server with HMAC verification,
that append one event (`mirror.pulled` or a new `webhook.received`) and return 200 in <5s;
(2) a Linear Agent adapter: OAuth2 `actor=app` install flow via `tm-auth`, `AgentSessionEvent`
`created`/`prompted` → create/attach a `Draft` ticket, emit an `agentActivityCreate{thought}`
within 10s from the webhook handler itself, then map `elicitation` ← `approval.requested`,
`action` ← effect boundary (B-11), `response` ← `ticket.verified`, `error` ← `ticket.verification_failed`
via an event subscriber; (3) GitHub App: installation-token minting per lease (`tm-auth`
`PlatformEphemeral`), Checks API run per verification ticket, PR review comments carrying evidence
artifact links, issue→ticket adoption. Fixtures for every webhook payload; a test asserting the
10s/5s SLAs against a `FixedClock`.

**M-04 Ecosystem table stakes (backlog) — none present.**
Evidence: `rg -i "AGENTS\.md|SKILL\.md|PreToolUse|PostToolUse|opentelemetry|checkpoint|redact|notif"`
over `crates/` finds nothing relevant (the only `checkpoint` hit is genesis prose,
`stages.rs:431`); `oversight.toml` is never loaded (`Oversight` type exists in `tm-types` but is
referenced only by tests, `crates/tm-e2e/tests/authority_e2e.rs:181`); `git.worktree` is a raw
tool (`tools.rs:1483`), not an isolation flag. Done looks like, per item: `AGENTS.md` read as a
`SectionKind::Conventions` source walking up from the ticket's claimed paths; `SKILL.md`
discovery under `.tm/skills/**` with metadata-only in the pack and body loaded by a `skill.load`
tool; hooks as a `hooks.toml` with `PreToolUse/PostToolUse/UserPromptSubmit/Stop/SessionStart`
handlers (shell or in-process) that can return `deny|rewrite(input)|allow`, invoked inside
`ToolRegistry::dispatch` before `permits`; `oversight.toml` loaded into `Oversight` and passed to
the gate (the sandbox/approval split *stated* in those terms in docs); checkpoints = `tm ticket
fork <T> --at <seq>` replaying a ticket's events into a new ticket plus a per-turn
`git stash`-style snapshot artifact; `--worktree` on `tm run` and per-delegated-worker isolation
via `git worktree add` under `.tm/worktrees/`; `tracing-opentelemetry` behind a feature flag with
`TM_OTEL_ENDPOINT`; desktop notifications (`notify-rust`/`terminal-notifier`, OSC-9 fallback) on
`approval.requested`/`ticket.escalated`; secret redaction at `Fabric::execute` boundary with local
restore.

**M-05 Remote control / teleport (backlog): not started, and `attach` is overloaded.**
Evidence: `tm attach [path]` is repository assimilation (`args.rs:68`, §12), not `tm attach <url>`;
no push/pull verbs; `tm-server` has one shared `TM_SERVER_TOKEN` (`auth.rs:72`), no per-client
authority, client-asserted `actor` (`routes.rs:288,390`); no SSH/relay transport. Done looks like:
rename the assimilation verb to `tm adopt` (or `tm attach --repo`) before adding `tm attach <url>`;
`tm serve --expose` (push) mints a per-client token bound to an attenuated `Authority` and a lease
(`session.joined` event); `tm attach <url>` (pull) resumes from `seq` using the existing
`ResumableStream` (`sse.rs:36-45`); server derives `actor` from the token, never the body;
transports as a provider trait (`local`, `ssh` via `ssh -L`, `relay` with E2E encryption — the
relay needs a dependency decision); resolve the single-writer boundary with a documented
"one scheduler per log, attached clients are viewers plus command submitters" rule.

**M-06 Documentation retrieval (Context7 or equivalent): not started.**
Done looks like: `trait DocsSource { fn resolve(&self, crate_or_pkg, version) -> Option<LibId>;
async fn fetch(&self, id, query) -> Result<Vec<DocChunk>> }` with `Context7Http`, `LlmsTxt`,
`LocalIndex` impls; versions taken from `Cargo.lock`/`pnpm-lock.yaml`; cached under
`.tm/docs-cache/<pkg>@<ver>/` with fetched-at provenance; a `SectionKind::LibraryDocs` in
`tm-context` ranked by the same fusion; offline-safe (cache hit or nothing).

**M-07 §16 test-strategy gaps.**
Evidence: proptest exists only in `crates/tm-types/tests/authority_laws.rs` (11 laws incl.
pattern-subset safety and budget non-negativity — a correction, see below); missing: scheduler
determinism as a property (`plan()` on generated views), replay-equivalence as a property, CLI
golden tests (`insta` used 0 times in `crates/tm-cli`), server contract tests over real HTTP (only
in-process handler tests, `routes.rs:1351+`), attach e2e (§16.11; `genesis_e2e.rs` has no attach
case), `tm docs check` on this repo is vacuous (B-06; also no `docs/architecture.md` and no
`docs/.tmdocs.toml` exist). Exhaustive `TicketState × Trigger` (`machine.rs:173-176`), 200-event
replay (`replay.rs:31`), tamper detection (`replay.rs:118`, `log.rs:599`), 64-reader stress
(`concurrency.rs:187`), crash recovery, genesis e2e, hygiene (`xtask/src/hygiene.rs`) and
invariants 1–12 (`invariants.rs`) are present. The in-flight branches add proptests for authority,
subset safety, scheduler determinism and budgets — verify on merge they cover `plan()` and not just
`select()`. Done: add the missing five, and make `cargo xtask verify` run `pnpm test`/`swift test`
when toolchains exist (§18.5).

**M-08 §12 genesis does not commit the graph in one transaction.**
Evidence: `compile.rs:483-495` — tickets, then milestones, then edges, each its own transaction,
with best-effort cancel on failure. Done: `Store::transaction` (B-05) and a test that a failing
edge leaves zero tickets behind.

**M-09 §7/§32 retrieval rungs: `ApiEmbedder` and LSP absent.**
Evidence: only `LocalHashEmbedder` (`embed.rs:43,148`); `rg -i lsp` over `crates/` is empty;
callers/callees are tree-sitter heuristics (`symbols.rs`). Done: `ApiEmbedder` via
`Fabric::embed` on role `embedder`, cached by content hash; an `lsp` module in tm-codeintel that
spawns `rust-analyzer`/`typescript-language-server`/`pyright` when present and serves
`definition/references/callHierarchy` with the heuristic as fallback, plus diagnostics streamed
into `edit.*` results (rung 5).

**M-10 §13 tracker adapters: two correctness gaps.**
Evidence: Jira `resolve_transition` returns the target name without querying allowed transitions
(`jira.rs:220-227`, "For now"); `GitLabTracker` derives `Debug` with a `token: String` field
(`gitlab.rs:36-41`). Done: fetch `/issue/{key}/transitions` and pick by `to.name`; drop `Debug`
or implement it manually with the token redacted (add the redaction test from B-12).

**M-11 §6 provider count is overstated.**
Evidence: registry lists 21 (`registry.rs:31-52`); `BedrockProvider` returns
`bedrock_unavailable()` for every call (`cloud.rs:192,288-296`, SigV4 unimplemented);
`VertexProvider` requires a pre-minted token (`cloud.rs:352,388`). Done: SigV4 signer (needs a
dependency decision, `aws-sigv4` or hand-rolled) behind `tm-auth`'s `CloudIam`; Vertex ADC/service
account minting likewise; `tm provider list` should show `unavailable` for these rather than
`configured`.

**M-12 §18 clients: real scaffolding, but the shared-client rule is broken and the serve story is.**
Evidence: `clients/ts` exists (`@ticketmaster/client`, 1,870 lines, 19 tests, SSE reconnect) but
neither `clients/web/package.json:14-18` nor `clients/vscode/package.json` depends on it; both
hand-roll `fetch` (`clients/web/src/api/client.ts`, `clients/vscode/src/ticketmaster/client.ts`)
and their READMEs still claim `clients/ts/` does not exist; `GET /schema` is a hand-written
partial (`routes.rs:539-570`) and `generated.ts` is 30 lines; `serve.rs:80-84` looks for
`web/dist` and `apps/web/dist`, not `clients/web/dist`, so `tm serve` never serves the built
canvas; `ServeArgs` has no `--open`. Web: 5 of 8 §18.2 views are `NotBuilt`
(`clients/web/README.md` "explicitly NOT built"). VS Code: no SSE, never run against a server, no
host tests. macOS: no menu-bar extra, no notifications, no reconnect, Liquid Glass used correctly
(`RootView.swift:41-79`). Done: web and vscode import `@ticketmaster/client` and delete their
copies; `/schema` generated from Rust types (`schemars`, needs a dependency decision) with the
drift check in CI; `find_web_client_dir` adds `clients/web/dist`; `--open`; the remaining views
tracked in each README (already done) plus `T-E2E-WEB`/`T-VIS-REG`.

**M-13 §21.4 admission control is only worker ceilings.**
Evidence: `SchedulingPolicy` fields (`policy.rs:34-132`) have `max_in_flight_*` and
`harness_capacity_fraction` but no `max_unverified_tickets`, `max_review_queue_age_hours`,
`resume_below`. Done: add them; `AdmissionGate::check` refuses `Lease` when
submitted+verifying ≥ ceiling or oldest awaiting-audit age > limit, and resumes only below
`resume_below`; `tm sched plan` output shows the refusal reason; retire
`tm_harness::efficacy` in favour of the scheduler's field (A-05).

**M-14 §9 docs: staleness never triggers; nothing is registered.**
Evidence: `Assessor::assess` is called only by `tm docs check` (`ops.rs:82`) over an empty
registry; no `index.updated`/commit hook calls it; `apply_regeneration` discards `content_hash`
(`reconcile.rs:179,190`); this repo has no `docs/architecture.md` or `.tmdocs.toml`. Done: B-05
persistence; `CodeIntel::index` emits `index.updated` and the CLI runs `assess` after it; register
this repo's own docs with `derived_from` so §16.15 means something.

**M-15 §14 sessions are not events.**
Evidence: `POST /sessions` (`routes.rs:79`) keeps sessions in `AppState` memory; no
`session.started/joined/left/ended` is appended by the server or the CLI (`rg SessionStarted` hits
only a test fixture, `sse.rs:198-215`). Done: B-05 helpers, called from both.

**M-16 §4.4 `oversight.toml` is never loaded.** Evidence: `Oversight` type in `tm-types`,
`rg Oversight` outside tm-types hits only tests. Done: parse `.tm/oversight.toml` in
`project::open`, pass to the gate (A-04), default steady-state puts `computer.input/clipboard`
and `Spend > spend_over` behind approval.

**M-17 §19.4/§20.5/§22.4 `Action` classes missing.** Evidence: `action.rs:13-60`. Done: as B-02
and B-16; update the authority proptests' generators to cover the new variants.

### Tier 3 — polish

**P-01 Security sanity.**
- Credentials: no provider logs a key (checked all `from_env` sites and `Debug` derives; only
  `GitLabTracker` (M-10) and `ServerConfig` (`state.rs:42-50`, `token: Option<String>` with
  `derive(Debug)`) can leak through `{:?}`). Fix `ServerConfig` the same way.
- `shell.run` executes `argv` directly (no `sh -c`), `env_clear()` with an empty allowlist
  (`agent.rs:538-556`, `tools.rs:1655-1663`) — good. **But `cwd` is not clamped:**
  `resolve_cwd` is `root.join(rel)` (`tools.rs:531-534`), so a model-supplied `cwd` of `"/"` or
  `"../.."` escapes the repository (`Path::join` replaces the base on an absolute path), and
  `Action::RunCommand` carries only `argv` (`tools.rs:302-306`, `action.rs:25-28`), so
  `Authority::permits` never sees the directory. Fix: canonicalize and require
  `starts_with(root)`, and add `cwd` to `Action::RunCommand` so `shell.allow/deny` and
  `repository.write` can gate it. Regression test: `cwd: "/tmp"` must be `Denied`.
- `browser.eval` passes model-supplied JS straight to `Runtime.evaluate` (`session.rs:755-761`)
  ungated; acceptable only once gated by an `Action` and `network.allowlist` (A-04).
- `tm-server`: actor impersonation (A-04, M-05); single static bearer token.
- No redaction test exists (backlog "Adapter layers" asks for one that fails the build).

**P-02 Unused dependency.** `tm-agent` lists `tm-events` (`crates/tm-agent/Cargo.toml`) and
uses it only in a comment (`tools.rs:21`). Either use it (B-07) or drop it.

**P-03 Stale documentation inside the code.** `clients/web/README.md:5-20` and
`clients/vscode/README.md:11-20` (clients/ts "does not exist"); `crates/tm-browser/src/lib.rs:4-5`
(system Chromium — being fixed in flight); `registry.rs:70-84` documents a slug-collision gap that
`Fabric::register_provider` now keys by `ModelId` (`fabric.rs:108-120`) — verify and delete the
note; `crates/tm-tui` screens still say "stub" in doc comments after `756f7b5`.

**P-04 Server polls the log instead of subscribing.** `AppState::spawn_broadcast_poller` every
100 ms (`serve.rs:29,39`) while `EventLog::subscribe` exists (`stream.rs`). Fine for now; wire
`subscribe` when the scheduler and server share a process (B-01).

**P-05 Layout drift from §1.** `tests/` is empty (e2e lives in `crates/tm-e2e/tests`, which is
better — update SPEC); `docs/contracts/` has only `invariants.md`, not per-crate contracts;
`docs/architecture.md` does not exist.

**P-06 `tm provider status` reports `no_live_fabric` unconditionally** (`ops.rs:272`), and
`ProviderInfo::is_configured` is env-presence only (`providers/mod.rs:109-114`). After `tm-auth`,
report entitlement.

**P-07 Error-type drift.** Twenty crate-local error enums, five `From` impls into `TmError`,
most flatten to strings (A-03). Decide: either every crate's error converts losslessly into a
`TmError` variant with a `source`, or `TmError` grows `Browser/Computer/Server` variants. Add a
hygiene check that every `pub enum *Error` has a `From` into `TmError`.

**P-08 TUI screen adapters.** In flight; on merge, check `diff_viewer`/`ticket_graph` receive real
data providers rather than the "caller-supplied concern" placeholders (`diff_viewer.rs:28-36`).

**P-09 Scheduler holder ids.** Even after B-01, keep the deterministic-holder behaviour for
tests but make it an explicit `ExecutorId` type, not a formatted string (`driver.rs:147`).

### Tier 4 — nice-to-have

**N-01 §23 iOS simulator.** Stretch by the spec's own words; build it as a `CapabilityProvider`
(A-01) when the time comes so it does not become another isolated crate.

**N-02 Backlog research gaps not yet scheduled.** Time travel / fork at seq (`tm ticket fork`),
distilled memory layer, tight autofix loop distinct from Recovery, model tier as explicit cost
lever (`RoleTable` already implies it), living spec reconciliation, org-scoped authority ceiling,
global event-rate backstop (B-09 adds a per-ticket one).

**N-03 Registry / hub for templates, skills, MCP servers** (Continue Hub shape) — after B-15.

---

## Part 3 — Corrections to the "already known" baseline

- **§24 "trait + builtin adapter only" is wrong.** There is no `Executor` trait, no
  `ExecutorTask/Outcome/Capabilities`, and no builtin adapter — `tm-agent` is invoked directly by
  the CLI. (B-04.)
- **§2 "no proptest despite mandate" is partly wrong.** `crates/tm-types/tests/authority_laws.rs`
  has 11 `proptest!` laws (reflexivity, transitivity, attenuate never widens, intersection lower
  bound, root/none bounds, pattern-subset matching safety, delegation chains, budget try_spend
  safety; 256 cases). What is missing is property coverage *outside* tm-types (scheduler
  determinism, replay equivalence, state-machine totality as a property). (M-07.)
- **§20 "done, tested in isolation" should read PARTIAL.** Real backends, but no agent
  reachability, no lease coupling, no authority classes. (B-02, M-02.)
- **§13 "done, 6 trackers" overstates.** The adapters and `SyncEngine` are real and tested, but
  `tm mirror push|pull|status` are hard-coded stubs and `mirror_links` are never persisted, so
  nothing mirrors end to end from the binary. (B-06, B-05.)
- **§10 "done" overstates.** `tm harness promote|epochs` and `tm bench list|run` are stubs and
  `bench/tasks/` is empty; `HarnessEpoch`/`BenchRunner` exist as library code only. (B-06.)
- **§9 "done" overstates.** No doc is ever registered; `tm docs check` is vacuous. (B-06, M-14.)
- **§14 "done" overstates.** Sessions are memory-only; no `session.*` events. (M-15.)
- **§11 "done" overstates.** Tool denials, commands and usage are never recorded as events
  (§11 says "recorded as an event"; §8.2 says `command.started/completed`). (B-07.)
- **§12 "done" overstates.** The graph is not committed in one transaction. (M-08.)
- **§21.5 "ad hoc handling in the GitHub mirror" is generous.** The GitHub handling is an
  in-memory map that does not survive a restart. (B-11.)
- **§6 "18 backends, exceeds spec".** 21 registered, one non-functional (Bedrock), one requiring a
  hand-minted token (Vertex). (M-11.)
- **§18 "4 real clients".** Accurate as scaffolding; but the shared client is unused by two of
  them and `tm serve` cannot find the web build. (M-12.)
- **§21.4 admission**: the prior survey did not flag it; the specified ceilings are absent. (M-13.)
- **`crates/tm-e2e` invariant tests**: all twelve `invariant_<n>_*` tests exist, including 9
  (`invariants.rs:438`) — the contract in `docs/contracts/invariants.md` is honoured.

## Part 4 — In-flight branches, for the record

- `wf_75d66949-a4a-1`: modifies `tm-cli/{Cargo.toml,args.rs,lib.rs,main.rs}`, adds
  `tm-cli/src/tui.rs` and `tm-cli/tests/`, touches `tm-tui/runtime.rs` and `screens/dashboard.rs`.
  Closes B-03. Does not touch B-01/B-02.
- `wf_75d66949-a4a-2`: deletes `tm-browser/src/discover.rs`, adds `config.rs`, `downloader.rs`,
  `launch.rs`, `managed.rs`, `provider.rs`, `remote_cdp.rs`. Closes the §19.1 violation and adds
  `managed`/`remote-cdp`. Leaves M-01's cloud/docker providers, the lease-keyed session registry,
  and agent reachability (B-02) open.
- Both: §30.1/§30.2 in tm-agent/tm-context and §16.4 proptests. Neither touches §30.3/§30.4 (B-08).

## Tally

| Tier | Count | Ids |
|---|---|---|
| blocks-core-thesis | 16 | B-01 … B-16 |
| missing-promised-feature | 17 | M-01 … M-17 |
| polish | 9 | P-01 … P-09 |
| nice-to-have | 3 | N-01 … N-03 |
| architecture (cross-cutting, tagged) | 6 | A-01 … A-06 (A-01 is the named `CapabilityProvider` recommendation) |
