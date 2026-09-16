---
doc:
  id: vision
  mode: human
  derived_from: []
---

# Ticketmaster — Vision

> This is the seed. Every downstream artifact — `SPEC.md`, the ticket graph, the code — traces
> back to it. It is human-authored and the system never overwrites it (`SPEC.md` §9).

## Thesis

Ticketmaster is a persistent software-engineering runtime built around a deterministic
state-machine kernel. It turns intent into software, software into durable project state, and
project state into a continuously operating collaboration between humans, agents, tools, and
deterministic machinery.

It is not primarily a chatbot. It is not primarily a multi-agent framework. It is not an
autonomous developer with an increasingly elaborate personality. It is not a workflow YAML system
with LLM nodes.

Large models are excellent at understanding ambiguous intent, synthesizing incomplete
information, forming plans, making architectural judgments, exploring unfamiliar systems,
generating implementations, reviewing semantic correctness, and recovering when reality
invalidates a plan.

They are a ridiculous thing to use for remembering which jobs are ready, deciding that A can
start after B and C finish, tracking retries, managing leases, enforcing resource conflicts,
remembering which command already ran, determining whether a dependency is closed, applying known
routing policy, storing project decisions, tracking budgets, or preserving project state for six
weeks.

Those are software problems. So: **the model should not be the orchestration system.** Inference
resolves uncertainty; software acts on the resolution.

## The project is the persistent entity

Most agent products revolve around an agent. Ticketmaster revolves around a project.

Agents are transient. Sessions are transient. Providers, models, clients and machines are
replaceable. The project survives all of them, holding objectives, tickets, dependencies,
decisions, authority, milestones, artifacts, evidence, documentation, sessions, participants,
leases, execution history, failures, budgets, provider state, repository state, code intelligence
state, harness configuration, and event history.

The thing running 24/7 is not the agent. The project is running 24/7.

## Layers

```
EXPERIENCES     terminal · IDE · web · mobile · rooms · review · API
COLLABORATION   sessions · presence · comments · pair work · intervention · approvals
CORE            state graph · tickets · authority · scheduler · leases · decisions · events
                milestones · verification · artifacts · budgets · routing · recovery
  GENESIS ENGINE            CODE INTELLIGENCE
  intent → vision → spec    semantic · lexical · symbol graph · history
  spec → project graph
  ignition → v0/v1
  PROVIDER FABRIC           PROJECT KNOWLEDGE
  roles · quotas · fallback docs · decisions · constraints · evidence
EXECUTION       coding agents · frontier models · cheap workers · humans · CI · shells · browsers
```

The coding client is not Core. Genesis is not Core. No model is Core. Core is the persistent
machine underneath all of them, and it should be boring. That is a compliment.

## What the system is built out of

**Events.** Project history is an append-only stream. Current state is a materialized view of it,
which buys replayability, auditability, crash recovery, historical debugging, multiple clients,
synchronization, observability and reproducibility. Nothing important exists solely because some
model remembers it.

**Tickets.** A ticket is not an issue. It is a packet of work, context, authority, evidence and
lifecycle. The natural-language objective can stay fuzzy; the lifecycle around it may not.

**Authority.** Ticketmaster does not fundamentally delegate prompts — it delegates authority.
Authority is explicit, inspectable, delegatable, attenuating, revocable, lease-bound and
auditable. If A delegates to B then `authority(B) ⊆ authority(A)`: a child cannot manufacture
powers the parent never possessed.

**Leases.** Authority is normally temporary. If a worker disappears, the project does not get
stuck behind a dead process: the lease expires and authority reverts to its governing scope. At
larger boundaries — the end of a phase, milestone or epoch — distributed authority reconverges
into a higher scope.

**Verification over self-report.** Workers return evidence; they do not certify themselves. The
worker says what it changed, a verifier says what mechanically happened, an auditor says whether
it actually satisfies intent. This separation is foundational.

**Commands as artifacts.** Expensive commands run once and their full output is stored and
queryable, so no agent ever pays twice for `command | tail -3`, then `| tail -10`, then `| tail -50`.

## Genesis

Ticketmaster must support software that does not exist yet, starting from input as small as
"semantic git history search, like git blame but git why, small local CLI, make it slap".

Genesis prefers action over interrogation: unless a missing decision truly blocks progress, it
makes a reasonable assumption and records it. Stages run Seed → Vision → Spec → Graph compilation
→ Ignition → V0 → Evaluation → V1 → Stabilization → Maturity gate → Authority reconvergence →
Steady state.

During **Ignition** the rules are deliberately looser: broad leased authority, planning and
implementation interleaved, tickets freely created, merged, split or deleted, exploratory code
tolerated, aggressive fan-out. The target is to get to something real as fast as reasonably
possible. Not a perfect backlog. A program.

A V0 is not "60% of the backend exists". It is something a person can touch: the command runs,
the core behavior exists, results appear. Then a human uses it, and that feedback may modify the
vision, supersede decisions, alter architecture, invalidate tickets, or redefine v1.

Eventually the graph becomes trustworthy, exceptional bootstrap authority expires, and steady
state can be stricter without crippling exploration. The project has grown bones.

Existing repositories are first-class. `tm attach .` performs assimilation — index, symbol graph,
history, docs, build system, conventions, external trackers — and the first ticket is often just
*understand the current architecture well enough to safely accept autonomous work*. Ticketmaster
adapts to the repository, not the other way around.

## Code intelligence is infrastructure, not a plugin

Retrieval is one of the highest-frequency operations in agentic engineering, so the default must
not be grep, read the whole file, grep again. Semantic search, exact search, symbol navigation and
git history complement one another and are fused. Current code tells you what exists; history
often tells you why. Context is compiled for a ticket from all of it, so the system stops
repeatedly rediscovering its own codebase.

## Documentation knows when the world changed

The standard failure mode is: implementation changes, README stays confidently wrong for eleven
months. Documentation validity is therefore project state: a doc records what it is derived from,
and a change to that basis marks it stale until it is regenerated, reviewed, or explicitly
dismissed. Human-written prose stays first-class and is never overwritten by the system. The goal
is not "AI writes all docs" — it is "docs should know when the world underneath them changed".

## The harness is software

Ticketmaster's own coding harness — tool preferences, search routing, context compilation, model
roles, verification policy, prompt fragments — lives in the repository as versioned project
state. If it performs poorly on this project, improving it is legitimate engineering work, filed
as ordinary tickets, benchmarked against repository-local tasks, reviewed, and promoted as an
explicit epoch. A live session keeps the epoch it started on, so nothing mutates underneath a
running agent.

This is deliberately not mystical recursive self-improvement. Telemetry reveals inefficiency, a
ticket is created, the change is benchmarked and reviewed, a new epoch is promoted. Ticketmaster
can improve itself precisely because it refuses to mythologize self-improvement.

## Humans and agents share one world

Multiple humans and agents operate inside the same graph. Humans observe, comment, pair, take
over, delegate, review, approve and redirect. Manual work never leaves orchestration: if you go
investigate something yourself, those changes still attach to tickets, decisions, evidence and
verification. Humans are executors.

Social topology comes from the work: if two tickets share an interface, their participants have a
reason to coordinate. The graph determines collaboration, not invented personalities.

External task managers — Linear, GitHub Issues, Jira — are mirrors, never the orchestration
database. Ticketmaster's graph contains machine-only tickets, ephemeral exploration nodes, loops,
authority metadata, leases and verification nodes; it would be absurd to force those into Linear.
Selected state is projected outward; meaningful changes flow back as events.

## Oversight is authority-shaped

Human control corresponds to consequence boundaries, not endless confirmation dialogs. Reading
the repository, running tests, creating child tickets and committing can be autonomous while
schema changes, breaking public APIs, production deploys, closing milestones and large spends ask
first.

## The deepest inversion

Most systems begin with a smart model and bolt on memory, subagents, tools, planning, retries and
orchestration. Ticketmaster begins with a persistent computational engineering system — project
state, authority, work graph, code intelligence, project knowledge, scheduler, collaboration,
evidence, provider fabric, documentation, harness, execution — and leases cognition when it is
useful.

Intelligence is not the thing holding the system together. Intelligence is a resource the system
allocates. The same is true of humans, tools and models. The durable entity is the project itself.

One prompt can become a project. An existing repo can become a project. A project becomes a
persistent machine. Its graph lives beneath every interface. Its code is searchable by meaning,
symbol, text and history. Its docs remain tethered to reality. Its harness can be engineered like
everything else. Its task trackers are mirrors, not masters. Humans and agents enter and leave.
Authority moves through it. Knowledge accumulates beneath it. Intelligence appears where judgment
matters. Software handles everything else.

The agent isn't alive 24/7. The software project is.
