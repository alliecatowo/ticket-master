# Ticketmaster — Implementation Specification v1

> Status: authoritative. Derived from the Ticketmaster product brief (`docs/vision.md`).
> This document pins the contracts that the implementation must satisfy. It is durable, but
> per the brief, *the spec is not scripture* — changes are recorded as decisions (`D-*`).

---

## 0. Thesis restated as an engineering constraint

Deterministic machinery beneath nondeterministic intelligence.

Concretely, this means the following are **pure software** with no model in the loop:

- computing which tickets are READY
- dependency satisfaction and cycle budgets
- lease acquisition, heartbeat, expiry, authority reversion
- authority attenuation and enforcement
- resource-conflict detection
- retry / backoff / escalation policy
- provider routing under quota and health constraints
- event ordering, materialization, replay
- documentation staleness propagation
- command artifact reuse

And the following are the **only** places inference is invoked:

- Genesis vision / spec / graph compilation
- planning + decomposition of ambiguous tickets
- implementation (the coding agent)
- semantic audit
- maturity-gate evaluation
- recovery diagnosis after repeated failure

Everything in the first list must be unit-testable with **no network and no model**.

---

## 1. Language, layout, toolchain

- **Rust**, edition 2021, stable toolchain (pinned via `rust-toolchain.toml`).
- Cargo workspace, one crate per architectural layer.
- Single user-facing binary: `tm`.
- Storage: SQLite (`rusqlite`, bundled) at `<project>/.tm/project.db`. No external services required.
- Everything works **offline** except provider calls and mirror sync.

```
ticket-master/
  Cargo.toml                  # workspace
  rust-toolchain.toml
  SPEC.md                     # this file
  docs/
    vision.md                 # preserved product brief (the seed)
    architecture.md           # generated/maintained, provenance-tracked
    contracts/                # per-crate implementation contracts
  crates/
    tm-types/                 # shared ids, errors, clock, time, ids, globs, authority
    tm-events/                # event log: schema, append, read, subscribe, replay
    tm-core/                  # project state: tickets, graph, decisions, milestones,
                              #   artifacts, leases, budgets, state machine, store
    tm-scheduler/             # readiness, leasing, conflicts, retry, recovery, cycles
    tm-provider/              # provider fabric: roles, quotas, health, routing, ledger
    tm-codeintel/             # semantic + lexical + symbol + git-history retrieval
    tm-context/               # context compilation, command artifact cache
    tm-docs/                  # documentation provenance + staleness
    tm-harness/               # harness config, epochs, metrics, benchmarks, promotion
    tm-agent/                 # the coding agent: tool loop, tool gating by authority
    tm-genesis/               # intent -> vision -> spec -> graph -> ignition -> maturity
    tm-mirror/                # projection into external trackers (GitHub Issues)
    tm-server/                # HTTP + SSE API, sessions, presence, approvals
    tm-cli/                   # `tm` binary
  tests/                      # workspace-level end-to-end tests
  bench/                      # repository-local harness benchmark tasks
```

### Dependency policy

Allowed: `rusqlite`(bundled), `serde`/`serde_json`, `toml`, `thiserror`, `anyhow`, `clap`(derive),
`tokio`, `axum`, `tower-http`, `reqwest`(rustls, json, stream), `tracing`/`tracing-subscriber`,
`regex`, `globset`, `ignore`, `git2`, `tree-sitter` + grammars (rust, go, typescript, tsx,
javascript, python), `blake3`, `uuid`, `time`, `rand`, `crossbeam-channel`, `parking_lot`,
`similar`, `unicode-segmentation`, `dirs`, `tempfile`(dev), `proptest`(dev), `insta`(dev),
`assert_cmd`(dev), `predicates`(dev), `serial_test`(dev).

Anything else requires a recorded decision.

---

## 2. Cross-cutting primitives (`tm-types`)

### 2.1 Identifiers

Human-legible, sortable, stable. Rendered exactly as written.

| Kind | Format | Example |
|---|---|---|
| Ticket | `T-<n>` | `T-184` |
| Verification node | `V-<n>` | `V-51` |
| Audit node | `A-<n>` | `A-19` |
| Milestone | `M-<n>` | `M-12` |
| Decision | `D-<n>` | `D-019` |
| Artifact | `ART-<hex12>` | `ART-9f2a1c0b77de` |
| Session | `S-<n>` | `S-41` |
| Lease | `L-<hex12>` | |
| Participant | `agent:<provider>/<id>` or `human:<handle>` | `agent:claude/a81` |

`Id` is a newtype over `String` with a checked constructor per kind; parsing is total and
round-trips through serde as the rendered string. Numeric suffixes are allocated by a
monotonic per-kind counter held in project state (never reused, never renumbered).

### 2.2 Determinism substrate

```rust
pub trait Clock: Send + Sync { fn now(&self) -> Timestamp; }
pub trait IdSource: Send + Sync { fn next(&self, kind: IdKind) -> Id; fn random_hex(&self, n: usize) -> String; }
```

- `SystemClock`, `FixedClock(Cell<Timestamp>)` (test: advance explicitly).
- `CounterIds` seeded from persisted counters; `TestIds` deterministic from a seed.
- **No** direct use of `SystemTime::now`, `Instant::now`, `uuid::new_v4`, or `rand::thread_rng`
  outside `tm-types` impls. Enforced by a workspace test that greps the source tree.

### 2.3 Errors

`thiserror` per crate; every crate exposes `Error` + `Result<T>`. `tm-types::TmError` is the
shared root with variants: `NotFound`, `Conflict`, `InvalidTransition`, `AuthorityDenied`,
`BudgetExhausted`, `LeaseExpired`, `Storage`, `Provider`, `Io`, `Parse`, `Invariant`.
`Invariant` means a bug: it must never be produced by user input.

### 2.4 Path patterns

`PathPattern` wraps a glob. `PatternSet` supports:
- `matches(path) -> bool`
- `is_subset_of(other) -> bool` — conservative containment used for authority attenuation.
  Implemented by pattern-implication: `p ⊑ q` iff `q` matches every literal prefix expansion of
  `p` under the rule set {`**` absorbs any segments, `*` absorbs one segment's characters,
  literal must equal literal}. Conservative = may answer `false` for an actually-safe subset;
  must **never** answer `true` for an unsafe one. Property-tested.

---

## 3. Event log (`tm-events`)

### 3.1 Schema

```sql
CREATE TABLE events (
  seq            INTEGER PRIMARY KEY AUTOINCREMENT,  -- total order, gapless per project
  ts             TEXT    NOT NULL,                   -- RFC3339 UTC
  kind           TEXT    NOT NULL,                   -- dotted event name
  subject        TEXT    NOT NULL,                   -- primary object id ("" if none)
  actor          TEXT    NOT NULL,                   -- participant id
  session        TEXT,                               -- S-<n> if produced inside a session
  causation      INTEGER,                            -- seq of the event that caused this
  correlation    TEXT,                                -- groups a logical operation
  payload        TEXT    NOT NULL,                   -- JSON, schema per kind
  hash           TEXT    NOT NULL                    -- blake3(prev_hash || canonical_body)
);
```

- Append-only. There is **no** `UPDATE` or `DELETE` on this table anywhere in the codebase
  (enforced by a source-grep test and by a SQLite trigger that raises on update/delete).
- `hash` chains the log; `tm doctor` verifies the chain.
- Writers serialize through a single write connection; readers use additional read-only
  connections (WAL mode).

### 3.2 Event catalogue

The full set of kinds is closed and exhaustive (`enum EventKind`, serde-renamed to dotted form).
Unknown kinds fail to parse — forward compatibility is handled by version bumps, not leniency.

```
project.created            project.attached
ticket.created             ticket.updated             ticket.state_changed
ticket.dependency_added    ticket.dependency_removed  ticket.child_added
ticket.leased              ticket.heartbeat           ticket.lease_expired
ticket.lease_released      ticket.delegated           ticket.submitted
ticket.verified            ticket.verification_failed ticket.audited
ticket.audit_rejected      ticket.closed              ticket.cancelled
ticket.reopened            ticket.failed              ticket.retry_scheduled
ticket.escalated           ticket.budget_exhausted
decision.created           decision.superseded
authority.granted          authority.delegated        authority.revoked       authority.reverted
resource.claimed           resource.released          resource.conflict_detected
artifact.created
command.started            command.completed
milestone.created          milestone.closed           milestone.reopened
session.started            session.joined             session.left            session.ended
presence.updated           comment.created            approval.requested      approval.decided
provider.selected          provider.exhausted         provider.degraded       provider.recovered
executor.failed            usage.recorded
doc.registered             doc.generated              doc.invalidated         doc.reconciled
index.updated
harness.changed            harness.benchmarked        harness.promoted
genesis.started            genesis.stage_entered      genesis.stage_completed
genesis.assumption_recorded genesis.maturity_evaluated genesis.completed
mirror.linked              mirror.pushed              mirror.pulled
```

Each kind has a typed payload struct; `Event::payload_typed()` returns the enum.

### 3.3 API

```rust
pub struct EventLog { /* owns write conn + pool */ }
impl EventLog {
    pub fn open(path: &Path) -> Result<Self>;
    pub fn append(&self, draft: EventDraft) -> Result<Event>;          // assigns seq/ts/hash
    pub fn append_all(&self, drafts: Vec<EventDraft>) -> Result<Vec<Event>>; // one transaction
    pub fn read_from(&self, seq: u64, limit: usize) -> Result<Vec<Event>>;
    pub fn read_subject(&self, subject: &Id) -> Result<Vec<Event>>;
    pub fn head(&self) -> Result<u64>;
    pub fn verify_chain(&self) -> Result<ChainReport>;
    pub fn subscribe(&self) -> EventStream;    // broadcast to in-process listeners
}
```

Atomicity rule: **a state transition and its events commit in the same SQLite transaction.**

---

## 4. Project state (`tm-core`)

### 4.1 Materialized views

Tables: `tickets`, `ticket_deps`, `ticket_children`, `leases`, `resource_claims`, `decisions`,
`milestones`, `artifacts`, `evidence`, `budgets`, `participants`, `sessions`, `counters`,
`docs`, `doc_provenance`, `provider_usage`, `harness_epochs`, `mirror_links`, `meta`.

Every row in every one of these tables is derivable by replaying `events` from seq 0.
`Store::rebuild()` drops and replays. A test asserts, for a rich fixture project, that
`rebuild()` produces byte-identical materialized state.

### 4.2 The ticket

```rust
pub struct Ticket {
    pub id: TicketId,
    pub kind: TicketKind,            // Work | Verification | Audit | Investigation | Recovery | Harness
    pub objective: String,           // natural language, may be fuzzy
    pub state: TicketState,
    pub parent: Option<TicketId>,
    pub children: Vec<TicketId>,
    pub dependencies: Vec<TicketId>,
    pub milestone: Option<MilestoneId>,
    pub authority: Authority,        // the authority the ticket may lease out
    pub resources: Vec<ResourceClaim>,
    pub executor: ExecutorRequirements,  // role, min capability, human_required
    pub context_refs: Vec<ContextRef>,
    pub success: Vec<Predicate>,     // machine-checkable where possible
    pub verification: VerificationPolicy,
    pub budget: Budget,
    pub retry: RetryPolicy,
    pub cycle: Option<CycleBudget>,
    pub attempts: u32,
    pub failures: Vec<FailureRecord>,
    pub priority: i32,
    pub created: Timestamp,
    pub updated: Timestamp,
}
```

### 4.3 State machine

```
              ┌──────────────────────────────────────────────┐
              ▼                                              │
   DRAFT ─► BLOCKED ─► READY ─► LEASED ─► RUNNING ─► SUBMITTED ─► VERIFYING ─► AUDITING ─► CLOSED
              ▲  ▲        │        │         │  │                     │            │
              │  └────────┘        │         │  └──► RECOVERY ◄───────┘            │
              │  (dep reopened)    │         │           │                         │
              │                    └─────────┘           ├──► READY (retry)        │
              │              (lease expired/released)    ├──► REWORK ──► READY     │
              │                                          └──► ESCALATED ─► BLOCKED │
              │                                                                    │
              └──────────────── REPLAN ◄────────────── AUDITING (rejected) ◄───────┘
   any non-CLOSED ─► CANCELLED
   CLOSED ─► REOPENED(=BLOCKED) requires milestone-scoped authority
```

States: `Draft, Blocked, Ready, Leased, Running, Submitted, Verifying, Auditing, Rework,
Replan, Recovery, Escalated, Closed, Cancelled`.

Transition legality is a **pure function**:

```rust
pub fn transition(from: TicketState, trigger: Trigger) -> Result<TicketState, InvalidTransition>;
```

Rules (exhaustive table in `tm-core::machine`, mirrored by an exhaustive unit test that
enumerates `TicketState × Trigger`):

1. `Draft -> Blocked` on `Activate`; if no unsatisfied dependencies it continues to `Ready`.
2. `Blocked -> Ready` only via `DependenciesSatisfied`, which the scheduler computes; a ticket
   with any dependency not in `Closed` **cannot** be Ready. Cancelled dependencies do not
   satisfy; they block until removed or the ticket is replanned.
3. `Ready -> Leased` on `LeaseAcquired`. Only the scheduler emits this.
4. `Leased -> Running` on `WorkStarted`; `Leased|Running -> Ready` on `LeaseExpired`/`LeaseReleased`
   (attempt count already incremented at lease time, so a crashed worker cannot loop for free).
5. `Running -> Submitted` on `Submit(evidence)`. A submission **must** carry evidence; a worker
   cannot mark itself verified.
6. `Submitted -> Verifying` automatically; `Verifying -> Auditing` on `VerificationPassed`;
   `Verifying -> Recovery` on `VerificationFailed`.
7. `Auditing -> Closed` on `AuditPassed`; `-> Rework` on `AuditRejectedMinor`;
   `-> Replan` on `AuditRejectedStructural`.
8. `Recovery` applies `RetryPolicy`: `-> Ready` while `attempts < max_attempts` and budget
   remains; `-> Escalated` otherwise, or immediately for non-retryable failure classes.
9. `Rework -> Ready`, `Replan -> Blocked` (children regenerated by a planner ticket).
10. `Escalated -> Blocked` once a human or higher authority responds; `-> Cancelled` on abandon.
11. `Closed -> Blocked` only via `Reopen` carrying `authority.project.reopen_milestone` when the
    ticket's milestone is closed.
12. `-> Cancelled` from any non-terminal state with `tickets.cancel` authority.

Invariant checks run after every transition (`Store::check_invariants`, also exposed as
`tm doctor`):
- no ticket is `Ready|Leased|Running` with an unsatisfied dependency
- no ticket holds two live leases
- no two live leases hold conflicting exclusive resource claims
- child authority ⊆ parent authority for every parent/child edge
- every `Closed` ticket has at least one verification evidence record unless its
  `VerificationPolicy` is `None` (only allowed for `Investigation`/`Harness` kinds)
- the dependency graph's **non-cycle region** is acyclic: cycles are legal only when every
  edge in the cycle is marked `DependencyKind::Loop` and the cycle has a `CycleBudget`

### 4.4 Authority

```rust
pub struct Authority {
    pub repository: RepoAuthority { read: PatternSet, write: PatternSet },
    pub git: GitAuthority { commit: bool, branch: bool, merge: bool, push: bool, force: bool },
    pub tickets: TicketAuthority { create_children, delegate_children, modify_siblings,
                                   close, cancel, reopen },
    pub project: ProjectAuthority { modify_spec, modify_vision, modify_milestones,
                                    close_milestone, reopen_milestone, modify_harness },
    pub network: NetworkAuthority { docs: bool, arbitrary: bool, allowlist: Vec<String> },
    pub shell: ShellAuthority { enabled: bool, allow: PatternSet, deny: PatternSet },
    pub resources: ResourceAuthority { max_workers: u32, max_concurrent_commands: u32 },
    pub budget: Budget { tokens: u64, dollars_micros: u64, wall_seconds: u64 },
}
```

Core operations, all pure:

```rust
impl Authority {
    pub fn attenuate(&self, requested: &Authority) -> Result<Authority, AuthorityDenied>;
    pub fn contains(&self, other: &Authority) -> bool;   // other ⊆ self, field-wise
    pub fn intersect(&self, other: &Authority) -> Authority;
    pub fn permits(&self, action: &Action) -> Decision;  // Allow | Deny(reason) | NeedsApproval(scope)
    pub const ROOT: fn() -> Authority;   // full
    pub const NONE: fn() -> Authority;   // empty
}
```

Laws (property-tested with `proptest`):
- `a.contains(&a.intersect(b))` and `b.contains(&a.intersect(b))`
- `attenuate` never returns an authority not contained by `self`
- `contains` is reflexive, transitive, antisymmetric up to normalization
- delegation chains attenuate monotonically: `a ⊇ b ⊇ c` for any delegation path
- boolean flags: child may only have `true` where parent has `true`
- budgets: child budget ≤ parent remaining budget, and child spend debits parent

`Action` covers every gated operation: `ReadPath`, `WritePath`, `RunCommand`, `GitOp`,
`NetFetch`, `TicketOp`, `ProjectOp`, `Spend`.

Oversight config (`oversight.toml`, project state) maps actions to `Autonomous` /
`ApprovalRequired`, plus a `spend_over` threshold. `permits` returns `NeedsApproval` accordingly;
the caller must open an approval and block. Approvals are events.

### 4.5 Leases

```rust
pub struct Lease { id, ticket, holder: ParticipantId, authority: Authority,
                   resources: Vec<ResourceClaim>, acquired: Timestamp,
                   heartbeat: Timestamp, ttl_seconds: u32, epoch: u64 }
```

- `acquire(ticket, holder, ttl)` → fails if ticket not `Ready`, if resource claims conflict with
  a live lease, or if requested authority ⊄ ticket authority.
- `heartbeat(lease)` → fails on an expired lease (`LeaseExpired`); the worker must stop.
- `expire_due(now)` → pure sweep; emits `ticket.lease_expired` + `authority.reverted`, releases
  claims, returns ticket to `Ready`, records a `FailureRecord{class: ExecutorCrash}`.
- Resource conflict: two claims conflict iff their path pattern sets can overlap and at least one
  is `Exclusive`. Overlap test is conservative in the *safe* direction (may report a conflict that
  could not occur; must never miss a real one).

### 4.6 Decisions, milestones, artifacts, evidence

- `Decision { id, subject, decision, reason, evidence: Vec<ArtifactId>, affected_tickets,
  affected_docs, author, ts, supersedes: Option<DecisionId>, superseded_by: Option<DecisionId> }`.
  Superseding is an event; the old decision is never mutated in the log.
- `Milestone { id, title, tickets, state: Open|Closed, closed_by, assumptions: Vec<DecisionId> }`.
  Closing requires all member tickets `Closed|Cancelled`. Reopening requires authority and emits
  `milestone.reopened` plus `ticket.reopened` for affected descendants.
- `Artifact { id, kind: CommandOutput|Patch|File|Report|Index|Benchmark|Transcript,
  media_type, bytes_len, hash, storage: Inline(Vec<u8>)|OnDisk(PathBuf), meta: Json }`.
  Artifacts ≤ 64 KiB inline in SQLite; larger spill to `.tm/artifacts/<hash>`.
- `Evidence { ticket, kind: TestRun|Diff|CommandOutput|Review|HumanAttestation,
  artifact, produced_by, ts, summary }`. Verifiers read evidence; workers produce it.

### 4.7 Budgets

Hierarchical. Project → milestone → ticket → lease. Spend recorded via `usage.recorded` debits
every ancestor. `Budget::try_spend` is atomic in the same transaction as the usage event and
returns `BudgetExhausted` rather than going negative. Exhaustion transitions the ticket to
`Recovery` with class `BudgetExhausted`, which is non-retryable → `Escalated`.

---

## 5. Scheduler (`tm-scheduler`)

Pure core + thin driver. The pure core:

```rust
pub fn plan(state: &SchedulerView, now: Timestamp, policy: &SchedulingPolicy) -> Vec<SchedulerAction>;
```

`SchedulerAction` ∈ { `MarkReady(t)`, `MarkBlocked(t)`, `Lease{ticket, executor, ttl}`,
`ExpireLease(l)`, `Retry{ticket, after}`, `Escalate{ticket, reason}`, `OpenRecovery(t)`,
`CloseMilestone(m)`, `Noop }`.

Given identical `SchedulerView` + `now` + `policy`, `plan` returns an identical action list.
This is the property that makes the whole system testable: **the scheduler is a pure function
of project state**.

Ordering / selection policy (deterministic, documented, tie-broken by ticket id):
1. priority desc
2. critical-path length desc (longest chain of blocked descendants)
3. milestone deadline proximity
4. ticket id asc

Executor selection: match `ExecutorRequirements.role` against provider fabric availability;
skip roles with no healthy provider and record `provider.exhausted` once per window;
respect `resources.max_workers` at project and parent-ticket scope.

Retry: `RetryPolicy { max_attempts, backoff: Exponential{base_ms, factor, max_ms}, jitter: false }`.
Jitter is off by default *because determinism matters more than thundering-herd avoidance at
this scale*; when enabled it draws from the injected `IdSource`'s seeded RNG.

Cycles: `CycleBudget { max_iterations, max_tokens, max_dollars_micros, exit: Predicate,
escalate_after: u32 }`. Each traversal of a `Loop` edge increments the cycle counter; exceeding
any bound escalates. A cycle without a budget is an invariant violation at creation time.

The driver (`SchedulerLoop`) ticks on an interval and on event notification, applies actions
through `tm-core`, and is itself deterministic given an injected clock — tests drive it by
advancing a `FixedClock` and asserting the emitted event sequence.

---

## 6. Provider fabric (`tm-provider`)

### 6.1 Roles, not models

```
vision.frontier  planner.frontier  architect.frontier
coder.deep  coder.fast  explorer.cheap
reviewer.semantic  auditor.semantic  synthesizer.long_context
summarizer.cheap  embedder  computer_use
```

`providers.toml` (project state, versioned) maps role → ordered candidate list:

```toml
[[role.coder_fast.candidates]]
provider = "anthropic"
model    = "claude-sonnet-5"
max_concurrency = 4

[[role.coder_fast.candidates]]
provider = "anthropic"
model    = "claude-haiku-4-5-20251001"
max_concurrency = 8
degraded_ok = true
```

### 6.2 Fabric responsibilities

Track per (provider, model): rate limits (rpm/tpm), daily/monthly caps, live concurrency,
health (circuit breaker: Closed → Open on N failures in window → HalfOpen after cooldown),
observed latency (EWMA), price per token, and cumulative accounting.

```rust
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    async fn complete(&self, req: CompletionRequest) -> Result<Completion, ProviderError>;
    async fn embed(&self, req: EmbedRequest) -> Result<Embeddings, ProviderError>;
}
pub struct Fabric { /* registry + state + ledger */ }
impl Fabric {
    pub fn route(&self, role: Role, need: &Need, now: Timestamp) -> RouteDecision; // pure given state
    pub async fn execute(&self, role: Role, req: CompletionRequest) -> Result<Completion>;
}
```

`RouteDecision` ∈ { `Use(candidate)`, `Degrade(candidate, reason)`, `Wait(until)`, `Exhausted }`.
`route` is **pure** over fabric state — unit-tested exhaustively without any HTTP.

Fallback semantics per `Need.tolerance`: `Strict` (wait rather than degrade — architecture,
audit), `Preferred` (degrade one tier), `Any` (degrade freely — formatting, summarization).

Implementations shipped:
- `AnthropicProvider` — Messages API over `reqwest`, streaming, tool-use blocks, usage
  accounting, prompt-cache aware, retries on 429/5xx with `Retry-After` honoring.
- `MockProvider` — deterministic scripted responses keyed by a hash of the request; used by
  every test that needs inference. Supports injected failures, latency, and quota exhaustion.

No test in the workspace performs a real network call. A workspace test asserts that.

---

## 7. Code intelligence (`tm-codeintel`)

Index lives in `.tm/index.db` (separate file so it can be rebuilt/blown away independently).

### 7.1 Ingestion

Walk with `ignore` (respects `.gitignore`, `.tmignore`). Per file store `path`, `blake3`,
`size`, `lang`, `mtime`. Incremental: on change, only re-chunk/re-embed that file and
invalidate its symbols and edges. `index.updated` events carry counts.

### 7.2 Modes

1. **Exact** — `search.exact(literal)`, `search.regex(pattern)`; streaming, respects ignore rules,
   returns `Hit{path, line, col, line_text, byte_range}`.
2. **Semantic** — chunking: syntax-aware via tree-sitter where available (function/class/impl
   granularity, ≤ 400 tokens, 15% overlap at boundaries), fallback to sliding window.
   `Embedder` trait:
   ```rust
   pub trait Embedder: Send + Sync { fn dims(&self) -> usize; fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>; }
   ```
   - `LocalHashEmbedder` (default, **no network**): 512-dim; char 3-5-gram + token unigram/bigram
     hashing into buckets, sublinear TF, IDF from corpus stats, L2-normalized. Deterministic.
   - `ApiEmbedder` — via `Fabric::embed` on role `embedder`, cached by content hash.
   Storage: `f32` blobs; search = cosine over candidate set, pre-filtered by an inverted
   token index to avoid scanning everything. Concurrency: many readers, single writer,
   `BEGIN IMMEDIATE` for mutations, WAL. A stress test runs 64 concurrent readers against a
   live writer and asserts zero corruption and no reader errors.
3. **Symbols** — tree-sitter queries per language produce `Symbol{name, kind, path, range,
   container, signature, doc}` and `Reference{symbol_hint, path, range}`. Resolution is
   heuristic but explicit: exact-name + same-file-scope preferred, then same-package, then
   workspace-wide with an ambiguity flag. Exposed: `definition`, `references`, `implementations`,
   `callers`, `callees`, `type_of`, `outline`, `rename_preview` (returns a patch, never writes).
4. **History** — `git2` walk indexes commit messages, changed paths, and hunk texts into both
   lexical and semantic indexes. Queries: `history.why(path, line_range)` (blame → commits →
   messages + PR-ish trailers), `history.search(query)` semantic over messages+diffs,
   `history.deleted(query)` finds implementations that no longer exist.

### 7.3 Hybrid retrieval

```rust
pub fn hybrid(q: &Query, ctx: &RetrievalContext) -> Vec<RankedHit>;
```

Signals fused with weighted reciprocal-rank fusion (weights live in `harness.toml`, so
harness engineering can tune them): semantic rank, lexical rank, symbol-graph proximity to
seed symbols, path affinity to the ticket's resource claims, recency of edit, co-change
frequency from git history, and ticket-context linkage. Every returned hit carries its
`explain: Vec<SignalContribution>` — retrieval must be debuggable, and benchmarks score it.

---

## 8. Context compilation & command artifacts (`tm-context`)

### 8.1 Context packs

```rust
pub struct ContextPack { pub sections: Vec<Section>, pub tokens: usize, pub provenance: Vec<ContextRef> }
pub fn compile(ticket: &Ticket, view: &ProjectView, ci: &CodeIntel, budget: TokenBudget) -> ContextPack;
```

Sections in priority order, each with a budget share: ticket objective & success predicates;
active decisions affecting the ticket's paths; parent/dependency outputs and evidence; hybrid
retrieval results for the objective; symbol outlines for claimed paths; relevant git history;
prior failures for this ticket; harness/project conventions. Deterministic given inputs;
snapshot-tested. Overflow is resolved by dropping from the lowest-priority section first and
recording what was dropped in `provenance`.

### 8.2 Command artifacts

```rust
pub fn run(cmd: &CommandSpec, cache: &CommandCache, auth: &Authority) -> Result<CommandResult>;
```

Key = blake3 of (argv, cwd, env allowlist, repo dirty-state fingerprint, declared inputs).
On hit, return the stored artifact without executing. `CommandResult` exposes
`stdout_artifact`, `stderr_artifact`, `exit_code`, `duration`, plus `query(Head(n) | Tail(n) |
Grep(re) | Range(a,b) | Json(pointer))` so an agent never re-runs a command to see more of its
output. `command.started`/`command.completed` events; output always stored in full.
Non-deterministic commands opt out via `CommandSpec.cacheable = false`.

---

## 9. Documentation (`tm-docs`)

```toml
# docs/architecture.md front matter (or docs/.tmdocs.toml entry)
[doc]
id = "architecture"
mode = "generated"          # generated | maintained | human
derived_from = ["crates/tm-core/src/**", "D-019", "D-027", "providers.toml"]
```

- `doc.registered` on discovery; provenance stored in `doc_provenance`.
- After every commit/index update, `tm-docs::assess(changes)` computes affected docs and emits
  `doc.invalidated` → doc state `Stale`. States: `Fresh | Stale | Reconciling | Unverified`.
- `tm docs check` exits 1 if any doc is `Stale` (CI gate). `tm docs reconcile [id]` opens
  reconciliation tickets (mode `generated` → regeneration ticket; `maintained`/`human` → review
  ticket that a human can close with an attestation, which is itself evidence).
- Human-written docs are never overwritten by the system. Ever. Enforced by test.

---

## 10. Harness (`tm-harness`)

`harness.toml` is versioned project state and covers: tool preferences, search routing weights,
context compilation policy, role→capability mapping, verification policy defaults, prompt
fragments, command handling, editing behavior, retrieval rules.

- `HarnessEpoch { number, config_hash, config, promoted_at, promoted_by, benchmark: BenchmarkReport }`.
- A session pins `harness_epoch` at start and **never** changes it mid-session (test: mutate
  `harness.toml` mid-session, assert the running session's resolved config is unchanged and a
  new session picks up the new epoch).
- Metrics recorded per ticket/session: wall time, tokens in/out, dollars, tool calls, searches
  before first relevant hit, verification failures, retries, context bytes, commands re-run,
  human interventions.
- Benchmarks: `bench/tasks/*.toml`, each a repo-local task with a deterministic fixture, a
  scoring script, and expected outcomes. `tm bench run --epoch N` replays them with a seeded
  `MockProvider` (deterministic) or a real provider (`--live`). `tm bench compare A B` produces a
  report. Promotion requires `require_benchmark_gain` to be satisfied when configured.
- Efficacy budget: `efficacy.max_compute_fraction` caps the share of scheduler capacity that
  `TicketKind::Harness` tickets may consume; the scheduler enforces it as a hard admission rule.

---

## 11. Agent (`tm-agent`)

A tool-using loop, provider-agnostic, authority-gated.

```rust
pub struct AgentLoop { fabric, tools: ToolRegistry, authority: Authority, budget: Budget, cache, ci }
pub async fn run(&mut self, task: AgentTask) -> Result<AgentOutcome>;
```

Tools (each declares a JSON schema, an `Action` for authority gating, and a cost class):

```
search.semantic  search.exact  search.regex  search.hybrid
symbol.definition  symbol.references  symbol.callers  symbol.callees  symbol.outline  symbol.rename_preview
history.why  history.search  history.deleted
fs.read  fs.read_range  fs.list  fs.stat
edit.apply_patch  edit.write_file  edit.create_file  edit.delete_file
shell.run  shell.query_output
git.status  git.diff  git.log  git.commit  git.branch  git.worktree
test.run  build.run
ticket.create_child  ticket.delegate  ticket.submit  ticket.comment
decision.record  artifact.store  evidence.attach
ask.human
```

Every tool call is gated by `Authority::permits`; a denial is returned to the model as a tool
result (not an exception) so it can adapt, and is recorded as an event. Approval-required
actions suspend the loop, emit `approval.requested`, and resume on `approval.decided`.

Edits are applied through a patch engine with conflict detection (`similar`), never blind
overwrites; every edit produces a `Patch` artifact. The loop enforces the budget before each
provider call and stops cleanly with `AgentOutcome::BudgetExhausted` rather than mid-edit.

**Verification separation** (non-negotiable): the agent that produced a change may not run its
own audit. `submit` attaches evidence; a distinct verification ticket (`V-*`) runs mechanical
checks; a distinct audit ticket (`A-*`) with a different lease and a `reviewer.semantic` role
judges intent satisfaction. Enforced in `tm-core` (`AuditorMustDiffer` invariant).

---

## 12. Genesis (`tm-genesis`)

Stage machine, each stage a typed artifact persisted as project state:

```
Seed -> Vision -> Spec -> GraphCompilation -> Ignition -> V0 -> Evaluation -> V1
     -> Stabilization -> MaturityGate -> (pass) AuthorityReconvergence -> SteadyState
```

- **Seed** preserves `raw_prompt` verbatim forever, plus `explicit_constraints`,
  `inferred_constraints`, `assumptions`, `unresolved_questions`. Bias to action: a question is
  only surfaced to the human if `blocking == true`, meaning no reasonable assumption exists;
  everything else becomes a recorded assumption (`genesis.assumption_recorded`) that can be
  cheaply superseded later.
- **Vision** (`vision.frontier`): product thesis, UX, taste, constraints, identity, non-goals,
  architectural character, "what would make this spiritually wrong".
- **Spec** (`architect.frontier`): PRD, architecture, interfaces, data model, tech choices,
  quality bar, security model, testing strategy, v0/v1 definitions, milestones.
- **GraphCompilation** (`planner.frontier`): emits tickets, deps, milestones, authority domains,
  verification policies, resource declarations, executor roles, budgets — all as events, in one
  transaction, validated against `tm-core` invariants before commit. Invalid graphs are rejected
  and regenerated with the validation errors fed back (bounded attempts, then escalation).
- **Ignition**: a project mode flag, not a separate code path. It sets `IgnitionPolicy`:
  broader default authority, planning/implementation interleaved, ticket mutation allowed
  without milestone ceremony, exploratory-code tolerance, aggressive fan-out, and a V0 objective
  pinned as the top-priority milestone. Every relaxation is an explicit policy field —
  there is no "genesis magic" scattered through the code.
- **Maturity gate**: deterministic predicate evaluation over project state (working end-to-end
  artifact exists, V1 milestone closed, verification pass rate ≥ threshold over the last N
  tickets, spec churn rate below threshold, no open structural audits) — plus one frontier
  judgment call recorded as a decision. Both must agree. Failing the gate keeps the project in
  Stabilization and records why.
- **Authority reconvergence**: on gate pass, all Genesis-scoped leases are revoked
  (`authority.reverted`), the ignition policy is replaced by the steady-state policy, and
  `genesis.completed` is emitted.

`tm attach` (existing repository) runs the assimilation path instead: index, symbol graph,
git-history ingest, doc discovery, build/test-system detection, external-tracker detection,
convention inference — then creates `T-001 Understand current architecture sufficiently to
safely accept autonomous work` and asks only materially blocking questions.

---

## 13. Mirroring (`tm-mirror`)

**Ticketmaster's graph is authoritative. External trackers are projections.**

```rust
pub trait Tracker: Send + Sync {
    async fn push(&self, projection: &Projection) -> Result<ExternalRef>;
    async fn pull(&self, since: Timestamp) -> Result<Vec<ExternalChange>>;
}
```

- `ProjectionPolicy` decides which tickets surface (default: tickets with `mirror = true` or
  whose kind is `Work` and whose parent is a milestone; never `Verification`, `Audit`,
  `Recovery`, or machine-only nodes) and how children roll up into one external issue.
- Inbound changes are **translated into events**, never applied as direct state writes, and only
  for a semantically meaningful allowlist: status hints, assignment, comments, priority, and
  human-created issues (which become new tickets in `Draft`).
- Conflict rule: on divergence, Ticketmaster wins for orchestration fields; the external system
  wins for human-presentation fields (title prose, labels, assignee display).
### 13.1 Shipped adapters

Task providers are plural on purpose: which tracker a team uses is an integration detail, and
Ticketmaster's graph is authoritative regardless of which one is attached. All of them implement
the same `Tracker` trait, are selected by `mirror.toml`, and may be attached simultaneously (a
project can mirror to Linear for product and GitHub Issues for open-source contributors).

| Adapter | Transport | Auth | Notes |
|---|---|---|---|
| `github` | REST v3 | `GITHUB_TOKEN` / `gh auth token` | Issues, comments, labels, assignees, milestones |
| `linear` | GraphQL | `LINEAR_API_KEY` | Issues, states, cycles, projects; native parent/child |
| `jira` | REST v3 | `JIRA_EMAIL` + `JIRA_API_TOKEN` | Issues, transitions, epics; workflow-constrained states |
| `gitlab` | REST v4 | `GITLAB_TOKEN` | Issues, notes, labels, milestones |
| `null` | — | — | Accepts and discards; the default when nothing is configured |
| `recording` | — | — | Test double capturing every call for assertions |

Adapters differ in what they can represent, so capability is declared, not assumed:

```rust
pub struct TrackerCapabilities {
    pub parent_child: bool,        // Linear yes; GitHub Issues only via task lists
    pub arbitrary_states: bool,    // Jira workflows constrain transitions
    pub milestones: bool,
    pub labels: bool,
    pub comments: bool,
    pub max_body_bytes: usize,
}
```

`ProjectionPolicy` consults capabilities and degrades deliberately: where `parent_child` is false,
descendants roll up into a checklist in the body rather than being dropped; where
`arbitrary_states` is false, internal states map onto the nearest configured external state and the
exact internal state stays in Ticketmaster. A degradation is recorded in the mirror link, never
silently applied, so it is visible why an external issue looks coarser than the real graph.

Every adapter is unit-tested against recorded request/response fixtures with **no network**, and
each has a round-trip test asserting that pushing then pulling a projection is idempotent.

---

## 14. Server, sessions, multiplayer (`tm-server`)

`axum`. Endpoints (JSON):

```
GET  /health
GET  /events?from=<seq>            # SSE stream, resumable by seq
GET  /state                        # materialized snapshot + head seq
GET  /tickets  /tickets/:id        POST /tickets  PATCH /tickets/:id
POST /tickets/:id/transition       POST /tickets/:id/lease  /heartbeat  /release
GET  /graph                        # nodes + edges for the canvas
GET/POST /decisions  /milestones  /artifacts  /docs  /approvals
POST /sessions  DELETE /sessions/:id   POST /sessions/:id/presence
GET  /presence                     GET /providers   GET /harness   GET /metrics
```

- Auth: local socket / loopback by default; bearer token when bound to a non-loopback address.
- Sessions are **views**: a session holds a transcript and a pinned harness epoch, but anything
  that matters is promoted to durable objects (`decision.created`, `artifact.created`,
  `evidence.attach`, `ticket.*`). A test asserts a fresh worker can complete a handed-off ticket
  using only durable state, with the prior transcript deleted.
- Presence: participant → current ticket/file/action, TTL'd, broadcast over SSE. Path leases are
  surfaced so humans and agents collide on the same substrate.
- Comments and approvals are first-class events; an approval blocks the requesting agent.

---

## 15. CLI (`tm-cli`)

`tm` with no arguments opens the **coding agent** in the current project (the standalone-quality
client). Subcommands:

```
tm init | attach [path] | genesis [--prompt <text>|-]
tm status                          # the "since you left" report
tm ticket list|show|new|edit|close|cancel|reopen|tree|delegate|submit
tm dep add|rm|graph
tm sched plan|tick|run|pause|resume
tm lease list|acquire|release|expire
tm run <ticket>                    # execute one ticket to completion in the foreground
tm search <query> [--exact|--regex|--semantic|--hybrid]
tm symbol def|refs|callers|callees|outline
tm history why <path>[:line] | search <query> | deleted <query>
tm docs list|check|reconcile
tm decision list|show|new|supersede
tm milestone list|close|reopen
tm provider list|status|test
tm harness show|set|epochs|promote
tm bench list|run|compare
tm mirror link|push|pull|status
tm serve [--addr]
tm events tail|show|replay|verify
tm doctor                          # invariants + hash chain + index health
```

Output: human-readable by default, `--json` everywhere (stable schemas, snapshot-tested),
`--quiet`, `--no-color`. Exit codes: 0 ok, 1 domain failure, 2 usage error, 3 authority denied,
4 budget exhausted, 5 invariant violation.

---

## 16. Testing strategy (the guarantee)

A change is not done until **all** of the following pass via `just verify` / `cargo xtask verify`:

1. `cargo fmt --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace` (unit + integration + doc tests)
4. Property tests: authority algebra, pattern subset safety, state machine totality,
   scheduler determinism, budget non-negativity, replay equivalence.
5. Exhaustive transition test over `TicketState × Trigger`.
6. Replay test: for a fixture project with ≥ 200 events, `rebuild()` == incremental state.
7. Hash-chain integrity test, including a tamper-detection case.
8. Concurrency test: 64 readers + 1 writer on the index; N workers contending for leases and
   conflicting resource claims; assert no double-lease, no lost update, no corruption.
9. Crash-recovery test: kill a worker mid-run (simulated), assert lease expiry returns the
   ticket to `Ready` with authority reverted and attempt counted.
10. End-to-end genesis test: seed prompt → `MockProvider` → tickets → scheduled execution →
    verification → audit → V0 artifact → evaluation → maturity gate, all offline and deterministic.
11. End-to-end attach test against a synthetic existing repo with history.
12. CLI golden tests (`assert_cmd` + `insta`) for every subcommand's `--json` output.
13. Server contract tests, including SSE resume-from-seq.
14. Hygiene tests: no `SystemTime::now`/`thread_rng` outside `tm-types`; no `UPDATE`/`DELETE`
    on `events`; no network in tests; no `unwrap()` in non-test code outside of documented
    invariant sites (allow-listed).
15. Docs: `tm docs check` clean on this repository itself.

CI (`.github/workflows/ci.yml`) runs the same `verify` target on Linux and macOS.

---

## 17. Invariants (the product promises, as assertions)

1. The project — not any agent or session — is the persistent entity.
2. All durable truth is derivable from the event log by replay.
3. No model decides a transition that software can decide.
4. Authority is explicit, scoped, leased, attenuating, revocable, auditable; a child can never
   exceed its parent.
5. A dead worker cannot block the project: leases expire and authority reverts.
6. Workers submit evidence; a different executor verifies; a different one audits.
7. Expensive commands run once; their full output is durable and queryable.
8. Documentation knows when the facts beneath it changed; human prose is never overwritten.
9. External trackers are mirrors, never the orchestration database.
10. Provider exhaustion is routable state, not an exception.
11. Harness changes are benchmarked, promoted as epochs, and never mutate a live session.
12. Sessions are views; a fresh worker can always continue from durable state alone.

Each invariant maps to at least one named test (`invariant_<n>_*`), listed in
`docs/contracts/invariants.md`.

---

## 18. Surfaces

Everything in this section is a **client of `tm-server`** (§14). No surface talks to SQLite, and no
surface owns truth: they render the materialized state and stream changes over SSE. That is what
makes "multiple clients, one world" (§59 of the brief) true by construction rather than by
discipline.

**Depth is deliberately uneven.** The CLI is the surface that has to prove the system actually
works, so it is built to full depth: every subcommand, the interactive coding agent, stable
`--json` everywhere. The web app, the macOS app and the editor extension are built as real
scaffolding — correct structure, generated types, a working vertical slice (connect, stream the
event log, render live tickets, perform one mutation), tests and a green build — rather than to
feature parity with the CLI. That is an honest V0 for those surfaces: they demonstrably work
against the real API and are laid out so filling in the remaining views is ordinary work, not a
rewrite. Anything left unbuilt in a surface is listed in that surface's README as open work, not
implied to exist.

```
              tm-core (SQLite: project.db, index.db)
                        │
                   tm-server  REST + SSE
                        │
   ┌────────────┬───────┴────────┬──────────────────┐
   │            │                │                  │
  tm  (CLI)   web app      Ticketmaster.app     VS Code ext
  Rust        Vite/React   SwiftUI (macOS 26)   TypeScript
                   └──────── @ticketmaster/client ────┘
```

### 18.1 `clients/ts/` — the shared TypeScript client

One package, consumed by both the web app and the VS Code extension. Nothing else may hand-roll
`fetch` against the API.

- Typed bindings for every endpoint and every event payload. The types are **generated** from the
  Rust types (`tm-server` exposes `GET /schema` returning JSON Schema; a `pnpm gen` step writes
  `src/generated.ts`). A CI check regenerates and fails on drift, so the surfaces cannot silently
  disagree with Core.
- `TicketmasterClient` — REST methods, plus `subscribe(fromSeq)` returning an async iterable over
  SSE that reconnects with `Last-Event-ID` and never drops or duplicates across a reconnect.
- `ProjectStore` — an in-memory materialized view fed by the event stream, so a surface renders
  from the same event-sourced model Core uses. Pure and unit-testable with a scripted event array.
- Tests: `vitest`, including a reconnect test that asserts exactly-once delivery across a
  simulated drop.

### 18.2 `clients/web/` — the project canvas

Vite + React + TypeScript. Served by `tm serve` as static assets, so `tm serve --open` is the whole
setup story. No SSR, no framework lock-in, no build step the Rust binary cannot run.

Views, drawn from the brief's §43 native project canvas and §40 multiplayer:

| View | Contents |
|---|---|
| Status | The "since you left" report: closed, repaired, reconciled, benchmarked, needs-you |
| Graph | Dependency and parent/child graph, milestones as cuts, loop edges marked |
| Backlog | Ready / blocked / active, filterable by milestone, kind, executor role |
| Ticket | Objective, state, authority, evidence, failures, artifacts, event history |
| Rooms | Per-milestone activity: participants, comments, live presence |
| Review | Diffs and evidence awaiting audit, with approve/reject |
| Decisions | Decision log with supersession chains |
| Providers | Role→model routing, quota headroom, breaker state, spend |

Rules: presence and path leases are shown wherever work is shown, because collision prevention is
the same substrate for humans and agents. Approvals are first-class and blocking. Every mutation
goes through the client package; optimistic updates are reconciled against the event stream.

### 18.3 `clients/macos/` — `Ticketmaster.app`

SwiftUI, macOS 26 (Tahoe), Swift 6.3, built with Xcode 26. A Swift package (`TicketmasterKit`) for
models, the API client and view models — so the logic is testable with `swift test` without
launching a UI — plus a thin app target.

**Liquid Glass is the real thing, not a blur imitation.** Use the system APIs:

- `.glassEffect(_:in:)` for floating controls and panels; `.glassEffect(.regular.tint(_).interactive())`
  where the control responds to pointer or press.
- `GlassEffectContainer` to group nearby glass elements so they blend and morph as one, instead of
  stacking independent glass layers (stacked glass is the single most common way to get this wrong).
- `@Namespace` + `.glassEffectID(_:in:)` for morphing transitions between related glass elements.
- `.buttonStyle(.glass)` / `.glassProminent` for buttons; `ToolbarSpacer` to group toolbar items.
- `.backgroundExtensionEffect()` where content should bleed under a sidebar or inspector.
- Toolbars, sidebars, sheets and inspectors adopt Liquid Glass automatically on macOS 26 — do not
  hand-roll what the system already gives you, and do **not** substitute `.ultraThinMaterial`.
- Respect Reduce Transparency and Increase Contrast: the app must stay legible and correct with
  those on, which is an accessibility requirement, not a nicety.

Structure: `NavigationSplitView` — sidebar (milestones, saved views, participants), content (the
ticket list or graph), detail (ticket inspector). Menu bar extra showing active work and
needs-you count. Native notifications for `approval.requested` and `ticket.escalated`.

### 18.4 `clients/vscode/` — the editor surface

TypeScript extension over `@ticketmaster/client`:

- Tree view of milestones → tickets → children, with state badges.
- Presence and path leases surfaced as editor decorations, so you can see that `src/auth/**` is
  leased by `T-184` before you start editing it.
- Decisions affecting the open file shown as a CodeLens.
- Commands: open ticket, claim ticket, submit with evidence, record a decision, run `tm` on the
  current selection.
- Tests: `vitest` for the pure logic; the VS Code host integration is exercised through the
  extension test runner.

### 18.5 Verification ladder

Staged deliberately: a surface is "done" at level 1, and levels 2 and 3 are tracked as real work
rather than aspiration.

1. **Now — build, unit and contract.** `cargo test` for the CLI; `vitest` for the TS packages
   including `ProjectStore` reducer tests and SSE reconnect tests; `swift test` for
   `TicketmasterKit`; `xcodebuild -scheme Ticketmaster build` for the app; API-contract tests for
   web and extension run against a real `tm serve` on an ephemeral port with a seeded project.
   Schema-drift check between Rust types and generated TS types.
2. **Next — end-to-end.** Playwright against a live `tm serve`: create a ticket, watch it appear
   over SSE without a refresh, approve a blocked action, see the agent resume. Tracked as
   `T-E2E-WEB`.
3. **Next — visual regression.** Snapshot the canvas views and the Mac app's glass surfaces in
   light and dark, with Reduce Transparency both on and off, and diff against baselines. Tracked
   as `T-VIS-REG`. Liquid Glass is exactly the kind of thing that regresses invisibly, so this
   matters more here than in a flat UI.

Surfaces never gate Core: a broken surface build must not fail `cargo test --workspace`. The
`verify` target runs the Rust gate always, and the surface gates when their toolchains are present.

---

## 19. Browser automation (`tm-browser`)

An agent that cannot open a page cannot check whether the thing it built works. Browser control is
therefore a first-class executor capability, not an optional plugin — but it must be *cheap*, or
agents will avoid it and go back to guessing.

### 19.1 No-friction principle

- **No Node, no Playwright, no driver binaries.** `tm-browser` speaks the Chrome DevTools Protocol
  directly over a WebSocket. The only external dependency is a Chromium-family browser.
- **Bring-your-own browser, discovered not downloaded.** Probe, in order: `$TM_BROWSER`, Chrome,
  Chromium, Brave, Edge in the platform's usual locations. If none is found, say so with the exact
  install command for the platform rather than silently downloading 150 MB.
- Launch headless by default (`--headless=new`), with `--remote-debugging-port=0` and a
  throwaway profile directory, so sessions never collide and never touch the user's real profile.
- `tm browser open <url>` must work on a clean machine with a browser installed and no other setup.

### 19.2 The agent-facing surface is the accessibility tree, not pixels

The default observation is a **structured snapshot** of the page derived from the accessibility
tree: role, name, value, state and a stable `ref` per node. This is the single most important
design decision in this crate.

```
- button "Approve T-184" [ref=e17]
- textbox "Objective" [ref=e21] value="fix auth refresh race"
- list "Ready tickets" [ref=e30]
  - link "T-191 Windows packaging" [ref=e31]
```

An agent then acts by reference — `browser.click(ref: "e17")` — instead of guessing CSS selectors
or reading coordinates off a screenshot. That is cheaper (a snapshot is a few hundred tokens where
a screenshot is thousands), more reliable (no pixel hunting), and diffable between steps so the
agent can see what its action changed. Screenshots remain available for visual checks and for the
visual-regression work in §18.5, but they are not the default perception channel.

### 19.3 Capabilities

```
browser.open(url) / close / list_tabs / switch_tab
browser.snapshot()            -> accessibility tree with refs (the default observation)
browser.screenshot(full_page) -> PNG artifact
browser.click(ref) / hover(ref) / type(ref, text) / select(ref, value) / press(key)
browser.eval(js)              -> JSON value
browser.wait_for(selector | text | network_idle, timeout)
browser.console()             -> collected console messages since last call
browser.network()             -> request/response log, with bodies stored as artifacts
browser.cookies() / set_cookie / storage_state
browser.pdf()                 -> PDF artifact
```

Every command output that exceeds a threshold becomes an **artifact** (§8.2) rather than being
inlined, so a huge DOM or network log is stored once and queried, never pasted into a context
window twice.

### 19.4 Authority and safety

Browser actions are gated like everything else. `Authority.network` governs which origins may be
navigated to; `arbitrary: false` with an allowlist means an agent can drive `localhost:7777` for a
smoke test but cannot wander onto the open internet. Downloads land in an artifact directory, never
in the repository. Every session records a trace (navigations, actions, console errors) as evidence
attachable to a ticket — so "I verified the page works" is a claim backed by a replayable log.

---

## 20. Computer use (`tm-computer`)

Beyond the browser: drive a real desktop. macOS and Linux only — Windows is explicitly out of
scope, and the crate should say so rather than pretending to be portable.

### 20.1 Backends

| Backend | Input | Capture | Element tree | Headless |
|---|---|---|---|---|
| `macos` | `CGEvent` (Quartz) | `ScreenCaptureKit`, `CGDisplayCreateImage` fallback | `AXUIElement` accessibility API | **No** — see §20.3 |
| `x11` | `XTest` | `XGetImage` / SHM | AT-SPI where available | **Yes** — `Xvfb` |
| `wayland` | `libei` / portal `RemoteDesktop` | portal `ScreenCast` (PipeWire) | AT-SPI | Partial — compositor dependent |

Backend selection is automatic (`WAYLAND_DISPLAY` → wayland, `DISPLAY` → x11, macOS → macos) and
overridable with `TM_COMPUTER_BACKEND`. An unavailable backend fails with a precise, actionable
message — which permission, which package, which environment variable — never a generic error.

### 20.2 Surface

Mirroring §19.2: the preferred observation is the **accessibility element tree** (`AXUIElement` on
macOS, AT-SPI on Linux) with stable refs, falling back to a screenshot when the tree is unavailable
or the target is a canvas-style app that exposes nothing useful.

```
computer.snapshot()              -> element tree with refs, or a screenshot when unavailable
computer.screenshot(display | window)
computer.click(ref | x,y) / double_click / right_click / drag(from, to)
computer.type(text) / key(chord)      e.g. key("cmd+shift+4"), key("ctrl+alt+t")
computer.scroll(ref | x,y, dx, dy)
computer.windows() / focus(window) / move(window) / resize(window)
computer.clipboard_get() / clipboard_set(text)
computer.launch(app) / quit(app)
```

### 20.3 Headless desktops, honestly

On **Linux this genuinely works**: `tm computer --headless` starts an `Xvfb` display, optionally
with a window manager, runs the session against it, and tears it down. Nothing appears on a real
screen, nothing steals the user's focus, and many sessions can run concurrently on separate display
numbers. This is the supported path for autonomous GUI work and for CI.

On **macOS it does not**, and the crate must not pretend otherwise. There is no supported virtual
display that a headless daemon can drive: Quartz event injection targets the active session, and
screen capture requires a real (or VNC-attached) session. `tm computer` on macOS therefore runs
**attended** — it drives the logged-in desktop, which means it visibly moves the user's cursor and
requires explicit consent. For unattended macOS work the documented answer is a dedicated machine
or VM with an auto-logged-in session, not a flag that quietly does something weaker.

Because attended automation takes over a human's input devices, macOS sessions additionally require
an explicit approval (§4.4 oversight) and support a **panic stop**: moving the physical mouse during
an agent-driven session, or pressing the configured abort chord, revokes the lease immediately.

### 20.4 Permissions

macOS requires TCC grants for Accessibility and Screen Recording. `tm doctor` checks both, reports
which is missing, and prints the exact `System Settings` path — a permissions failure must never
surface as a mysterious empty screenshot. Linux X11 needs `XTEST`; Wayland needs a portal-capable
session, and `tm doctor` names the missing piece.

### 20.5 Authority

New action classes, gated exactly like paths and commands: `computer.input`, `computer.capture`,
`computer.clipboard`, `browser.navigate`. Default steady-state oversight puts `computer.input` and
`computer.clipboard` behind approval — an agent that can synthesize keystrokes into whatever window
has focus is strictly more dangerous than one that can write files inside a scoped path, and the
authority model should reflect that rather than treating it as another tool.

---

## 21. Loop and graph contracts

Ticketmaster is, structurally, a loop-and-graph engine, and it is worth stating the contracts it
implements so they can be reviewed as contracts rather than inferred from code.

### 21.1 The ticket is a loop contract

Every dimension a loop contract must answer has a home in the ticket (§4.2):

| Loop contract dimension | Where it lives |
|---|---|
| Goal and input | `objective`, `context_refs` |
| State | project state in SQLite, not a context window |
| Actions | `authority` (what may be done), `resources` (where) |
| Observation | `evidence`, command artifacts, code intelligence |
| Verification | `success` predicates, `verification` policy, the `V-`/`A-` nodes |
| Stop and escalation | `retry`, `cycle` budget, `budget`, `Escalated` state |
| Evidence | `artifacts` + `evidence`, immutable and replayable |

The anti-pattern this forecloses is "keep trying until done": a loop with no termination, cost or
escalation boundary. Every cycle in the graph carries a `CycleBudget`, and a cycle without one is
rejected at creation as an invariant violation (§4.3).

### 21.2 Static control plane, dynamic work graph

The work graph is deliberately plastic — tickets are created, split, merged and abandoned as
reality lands. The **control plane is not**. These are static, versioned project state, changed
only through an explicit authorized transition:

- which ticket kinds and states exist, and which transitions are legal (§4.3)
- the authority lattice and the oversight policy (§4.4)
- resource ceilings, budgets and the efficacy fraction (§5, §10)
- who may close a milestone, reopen one, or promote a harness epoch

Dynamically created tickets are therefore constrained rather than free: a child's authority is
attenuated from its parent, its budget is debited from its parent's remaining, its provenance
(which ticket and which executor created it) is recorded, and the whole proposed subgraph is
validated against invariants before it commits. Topology may drift; policy may not.

### 21.3 Three loops, three owners

| Loop | Owner | Cadence |
|---|---|---|
| Execution — plan, act, observe, verify, retry within budget | the scheduler and executors | continuous |
| Governance — goals, authority, budgets, acceptance, release | the human, through oversight policy | per decision |
| Improvement — telemetry to candidate change to benchmark to epoch | harness engineering (§10) | per epoch |

Removing a human from repetitive execution is not the same as removing human accountability: the
governance loop is where a named person stays responsible for irreversible effects, and the
oversight policy is the machine-readable form of that.

### 21.4 Admission control

A finite retry budget bounds one ticket; it does not bound the system. Work can be *authored*
faster than it can be *verified*, and an unbounded authoring rate quietly converts into an
unbounded review queue — the backlog grows, verification lags, and the graph's trustworthiness
decays while every individual ticket looks well-behaved.

The scheduler therefore reserves verification capacity before dispatching new work:

```toml
[admission]
max_unverified_tickets      = 24   # submitted-or-verifying, not yet audited
max_review_queue_age_hours  = 12   # oldest item awaiting human judgment
resume_below                = 16   # hysteresis: do not restart at the pause threshold
```

When a ceiling is reached, the scheduler stops *starting* new work while allowing in-flight
verification and rework to drain, and resumes only below a distinct lower threshold so the system
does not oscillate at the boundary. This is an explicit admission rule in `plan()`, visible in
`tm sched plan` output, not an emergent property.

### 21.5 Idempotent effects across resume

Crash recovery restores execution; it does not undo effects. Every external effect an executor can
perform — a command, a push, a mirror write, a browser form submission — carries an idempotency key
derived from `(ticket, attempt, effect)` and records a receipt as an artifact. On resume, an effect
whose receipt already exists is not repeated. Resuming an interrupted ticket establishes continuity
of execution, and nothing more: whether the delivered result was *sufficient* is the verifier's
question, and whether its effects can be reversed is the oversight policy's.
