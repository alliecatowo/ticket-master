ster

A persistent shared runtime for software engineering.

Ticketmaster turns intent into software, software into durable project state, and project state into a continuously operating collaboration between humans, agents, tools, and deterministic machinery.

It is not primarily a chatbot.

It is not primarily a multi-agent framework.

It is not an autonomous developer with an increasingly elaborate personality.

It is not a workflow YAML system with LLM nodes.

Ticketmaster is a persistent software-engineering runtime built around a deterministic state-machine kernel.

Humans work inside it.

Agents work inside it.

Humans and agents work together inside it.

Sometimes the system is intensely autonomous.

Sometimes you are manually driving.

Sometimes twenty agents are working overnight.

Sometimes you are just using an excellent coding agent to rename a function.

These are not different modes bolted together.

They are different ways of interacting with the same continuously running project.

---

1. Core thesis

Ticketmaster starts from several observations.

Large models are excellent at:

- understanding ambiguous intent
- synthesizing incomplete information
- forming plans
- making architectural judgments
- exploring unfamiliar systems
- integrating contradictory evidence
- generating implementations
- reviewing semantic correctness
- recovering when reality invalidates a plan

They are a ridiculous thing to use for:

- remembering which jobs are ready
- deciding that A can start after B and C finish
- tracking retries
- managing leases
- enforcing resource conflicts
- remembering which command already ran
- determining whether a dependency is closed
- applying known routing policy
- storing project decisions
- tracking budgets
- preserving project state for six weeks

Those are software problems.

So:

«The model should not be the orchestration system.»

Ticketmaster places deterministic machinery beneath nondeterministic intelligence.

Inference resolves uncertainty.

Software acts on the resolution.

---

2. The project is the persistent entity

Most agent products revolve around an agent.

Ticketmaster revolves around a project.

Agents are transient.

Sessions are transient.

Providers are replaceable.

Models are replaceable.

Clients are replaceable.

Machines are replaceable.

The project survives all of them.

A project contains durable:

- objectives
- tickets
- dependencies
- decisions
- authority
- milestones
- artifacts
- evidence
- documentation
- sessions
- participants
- resource leases
- execution history
- failures
- budgets
- provider state
- repository state
- code intelligence state
- harness configuration
- event history

That project may remain active continuously for days, months, or years.

The thing running 24/7 is not Claude.

The project is running 24/7.

---

3. The architecture

Ticketmaster has several distinct layers.

┌─────────────────────────────────────────────────────────┐
│                       EXPERIENCES                       │
│                                                         │
│ terminal · IDE · web · mobile · rooms · review · API   │
└────────────────────────────┬────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────┐
│                     COLLABORATION                       │
│                                                         │
│ sessions · presence · comments · rooms · multiplayer   │
│ pair work · observation · intervention · approvals     │
└────────────────────────────┬────────────────────────────┘
                             │
┌────────────────────────────▼────────────────────────────┐
│                    TICKETMASTER CORE                    │
│                                                         │
│ state graph · tickets · authority · scheduler · leases │
│ decisions · events · milestones · verification         │
│ artifacts · budgets · routing · recovery               │
└──────────────┬──────────────────────┬───────────────────┘
               │                      │
┌──────────────▼──────────────┐ ┌─────▼──────────────────┐
│       GENESIS ENGINE       │ │ CODE INTELLIGENCE      │
│                             │ │                        │
│ intent → vision → spec     │ │ semantic search        │
│ spec → project graph       │ │ lexical search         │
│ ignition → v0/v1           │ │ LSP / symbol graph     │
│ evaluation → stabilization │ │ history / structure    │
└──────────────┬──────────────┘ └─────┬──────────────────┘
               │                      │
┌──────────────▼──────────────┐ ┌─────▼──────────────────┐
│      PROVIDER FABRIC       │ │ PROJECT KNOWLEDGE      │
│                             │ │                        │
│ roles · quotas · fallback  │ │ docs · decisions       │
│ models · health · pricing  │ │ constraints · evidence │
│ routing · accounting       │ │ generated references   │
└──────────────┬──────────────┘ └─────┬──────────────────┘
               │                      │
┌──────────────▼──────────────────────▼───────────────────┐
│                     EXECUTION                          │
│                                                        │
│ coding agents · frontier models · cheap workers       │
│ humans · CI · shells · browsers · tools · computers   │
└────────────────────────────────────────────────────────┘

The coding client is not Core.

Genesis is not Core.

Claude is not Core.

Core is the persistent machine underneath all of them.

---

4. Ticketmaster Core

Core should be boring.

That is a compliment.

Its responsibility is maintaining authoritative project state and performing deterministic transitions.

Core owns:

tickets
dependency graph
authority graph
decisions
milestones
participants
leases
resources
artifacts
evidence
budgets
provider allocations
execution state
retry policies
verification state
session references
event history

A simplified transition might be:

BLOCKED
   │ dependencies satisfied
   ▼
READY
   │ executor available
   ▼
LEASED
   │
   ▼
RUNNING
   │
   ├── failure ───────► RECOVERY
   │
   ▼
SUBMITTED
   │
   ▼
VERIFYING
   │
   ├── failure ───────► RECOVERY
   │
   ▼
AUDITING
   │
   ├── rejected ──────► REWORK / REPLAN
   │
   ▼
CLOSED

The LLM does not repeatedly decide these transitions.

Software does.

---

5. Event-sourced project truth

Ticketmaster should treat project history as an append-only stream of meaningful events.

For example:

ticket.created
ticket.leased
ticket.delegated
ticket.submitted
ticket.verified
ticket.closed

decision.created
decision.superseded

authority.granted
authority.delegated
authority.revoked
authority.reverted

artifact.created

command.started
command.completed

milestone.closed
milestone.reopened

session.joined
session.left

provider.exhausted
provider.degraded
executor.failed

doc.generated
doc.invalidated
doc.reconciled

harness.changed
harness.benchmarked
harness.promoted

Current project state is a materialized view of that history.

This provides:

- replayability
- auditability
- crash recovery
- historical debugging
- multiple clients
- synchronization
- observability
- reproducibility

Nothing important should exist solely because some model remembers it.

---

6. The ticket is the work primitive

A ticket is not simply an issue.

It is a packet of work, context, authority, evidence, and lifecycle.

Conceptually:

Ticket
 ├─ id
 ├─ objective
 ├─ state
 ├─ parent
 ├─ children
 ├─ dependencies
 ├─ descendants
 ├─ authority
 ├─ resource claims
 ├─ executor requirements
 ├─ context references
 ├─ artifacts
 ├─ evidence
 ├─ decisions
 ├─ success predicates
 ├─ verification policy
 ├─ budgets
 ├─ retry policy
 ├─ failure history
 └─ event history

The natural-language objective can remain fuzzy.

The lifecycle surrounding it should not.

---

7. Authority is the deeper primitive

Ticketmaster does not fundamentally delegate prompts.

It delegates authority.

Example:

authority:
  repository:
    read:
      - "**"

    write:
      - src/auth/**
      - tests/auth/**

  git:
    commit: true
    merge: false

  tickets:
    create_children: true
    delegate_children: true
    modify_siblings: false

  project:
    modify_spec: false
    modify_milestones: false

  network:
    docs: true
    arbitrary: false

  resources:
    max_workers: 4

  budget:
    tokens: 180000
    dollars: 3.00

Authority is:

- explicit
- inspectable
- delegatable
- attenuating
- revocable
- lease-bound
- auditable

If executor A possesses authority Ω and delegates to B:

authority(B) ⊆ authority(A)

A child cannot manufacture powers the parent never possessed.

---

8. Leased authority

Authority should normally be temporary.

An executor acquires a ticket lease:

ticket: T-184

holder:
  agent:claude/a81

lease:
  acquired: 14:03
  heartbeat: 14:11
  expires: 14:23

If the worker disappears, the project does not become stuck behind a dead process.

The lease expires.

Authority returns to the project.

«Authority reversion is the return of leased execution authority to its parent or governing scope.»

At larger structural boundaries:

«Authority reconvergence is the process by which distributed authority collapses back into a higher governing scope after a phase, milestone, or project epoch ends.»

Genesis relies heavily on this.

---

9. Repository attachment

Ticketmaster must work with both new software and software that already exists.

A project can begin from:

new repository
existing local repository
existing Git remote
monorepo subtree
multiple related repositories

For a new project:

tm genesis

may create the repository itself.

For an existing one:

tm attach .

Ticketmaster analyzes the repository, constructs its initial code intelligence state, identifies existing conventions, imports relevant project metadata, and creates an initial model of the project without pretending that the absence of Ticketmaster history means the software has no history.

Existing Git history, architecture, tests, documentation, and conventions become evidence.

Ticketmaster adapts to the repository.

The repository does not need to be rewritten into some Ticketmaster-specific architecture.

---

10. Software Genesis

Ticketmaster must support software that does not exist yet.

This cannot begin with:

Please provide:
- your architecture document
- your issue hierarchy
- your test matrix
- your milestone plan

The user may give it:

semantic git history search.
like git blame but git why.
small local cli.
make it slap.

That should be enough.

Genesis transforms tiny human intent into increasingly structured reality.

---

11. Genesis Stage 0 — Seed

The input may be extremely small.

make GitHub Stories.
literal stories.
terminal and browser.
the joke is that they're completely normal stories.

Ticketmaster should strongly prefer action over interrogation.

Unless a missing decision truly prevents useful progress, Genesis makes a reasonable assumption and records it.

The seed produces:

Intent
  raw_prompt
  explicit_constraints
  inferred_constraints
  assumptions
  unresolved_questions

The original human language remains preserved.

Everything downstream can trace back to it.

---

12. Genesis Stage 1 — Vision

A strong model receives the seed with unusually broad conceptual authority.

Its job is not yet to create seventy-eight engineering tickets.

Its job is to answer:

«What is this thing?»

The result is a Vision.

The Vision establishes:

- product thesis
- user experience
- taste
- governing constraints
- identity
- non-goals
- architectural character
- what would make the project actually interesting
- what would make it technically valid but spiritually wrong

This prevents decomposition from destroying intent.

---

13. Genesis Stage 2 — Specification

Vision is compiled into a working specification.

It may include:

product requirements
architecture
interfaces
data model
constraints
technology choices
quality bar
security model
testing strategy
deployment assumptions
unknowns
milestones
definition of v0
definition of v1

The spec is durable.

But particularly during Genesis:

«The spec is not scripture.»

Reality is allowed to win.

---

14. Genesis Stage 3 — Graph compilation

The specification becomes executable project state.

Prose turns into:

tickets
milestones
dependencies
authority domains
verification policies
resource declarations
model roles
execution budgets

The initial graph is deliberately provisional.

It exists to create motion, not bureaucracy.

---

15. Genesis Stage 4 — Ignition

A new project has enormous uncertainty and almost no empirical information.

Trying to impose rigid long-horizon governance too early would optimize the wrong graph.

Genesis therefore enters Ignition.

During Ignition:

- strong models receive broader authority
- planning and implementation interleave
- tickets can be aggressively created, merged, split, reordered, or deleted
- architecture may still change materially
- assumptions are tested rapidly
- implementation begins almost immediately
- exploratory code is acceptable
- temporary scaffolding is acceptable
- agents can opportunistically fan out
- the system prioritizes reaching a coherent working artifact

The target is:

«Get to something real as fast as reasonably possible.»

Not a perfect backlog.

A fucking program.

---

16. Build while planning

Ticketmaster explicitly rejects:

understand everything
     ↓
plan everything
     ↓
write perfect issue graph
     ↓
begin implementation

Genesis behaves more like:

vision
  ↓
rough architecture
  ↓
begin implementation
  ↓
discover reality
  ├─────────────┐
  ↓             │
revise spec     │
  ↓             │
revise graph    │
  ↓             │
implement ◄─────┘

The graph is intentionally plastic.

---

17. V0 should happen offensively early

A V0 is not:

60% of the backend architecture exists

It is something the user can actually touch.

For a CLI:

the command runs
the core behavior exists
results appear

For an application:

it launches
the core loop works
a person can interact with it

This gives humans and agents vastly better information.

---

18. Genesis Stage 5 — Evaluation

Once something coherent exists, Ticketmaster enters an explicit evaluation window.

The user gets the thing.

They interact with it.

Feedback may:

- modify Vision
- supersede decisions
- alter architecture
- create tickets
- invalidate tickets
- promote unexpected behavior
- remove originally requested behavior
- redefine v1

Executing agents continue learning simultaneously.

This is joint project formation.

---

19. Genesis Stage 6 — Stabilization

Eventually:

- working end-to-end behavior exists
- architecture has met reality
- meaningful user feedback exists
- foundational uncertainties are resolved
- verification is meaningful
- the graph becomes trustworthy
- interfaces stabilize

Ticketmaster evaluates a maturity gate.

Once passed, Genesis ends.

---

20. Authority reconvergence

Exceptional Genesis authority expires.

Broad bootstrap leases close.

Control reconverges into ordinary project governance.

The project has grown bones.

Steady-state rules can now become stricter without crippling exploration.

---

21. Code Intelligence is core infrastructure

Ticketmaster should have an unusually strong understanding of source code because code retrieval is one of the highest-frequency operations in agentic engineering.

The default should not be:

agent
  ↓
grep some words
  ↓
read entire file
  ↓
grep again

Ticketmaster maintains a local Code Intelligence layer combining several retrieval modes.

---

22. Semantic repository search

A zvec-style local semantic index should be a first-class primitive.

Conceptually:

search.semantic("where do we recover abandoned worker leases?")

can find relevant code even when those exact words never appear.

The index can include:

- source chunks
- symbols
- comments
- tests
- configuration
- documentation
- important generated metadata

It should update incrementally as files change.

It should be local-first and cheap enough that dozens of concurrent agents can query it.

Readers should not fight over a heavyweight external vector service.

---

23. Exact search still matters

Semantic search does not replace grep.

Some questions are lexical:

MAX_RETRIES

or:

"authority.reverted"

or a specific error string.

Ticketmaster should expose exact search directly:

search.exact(...)
search.regex(...)

The harness should select the appropriate substrate based on intent.

Not dogmatically force one tool.

---

24. Symbol-aware navigation

LSP and static symbol information belong alongside semantic retrieval.

Useful primitives include:

symbol.definition()
symbol.references()
symbol.implementations()
symbol.callers()
symbol.callees()
symbol.type()
symbol.rename()

Ticketmaster should understand the structural relationships of code wherever language tooling permits it.

That allows agents to move through a codebase the way competent engineers do rather than approximating everything with text search.

---

25. Hybrid retrieval

The best search path often combines:

semantic relevance
+
lexical relevance
+
symbol relationships
+
path context
+
recent edits
+
ticket context
+
Git history

A query like:

where is retry policy actually enforced?

might return:

1. RetryPolicy.apply()
   semantic match
   exact symbol relationship

2. scheduler/recovery.ts
   called by RetryPolicy.apply()

3. T-481 patch
   recently changed same behavior

4. commit 71a9...
   introduced retry ceiling

Retrieval becomes a project capability rather than whatever one model can improvise with Bash.

---

26. Git history as active knowledge

For existing repositories, Git itself is a historical knowledge base.

Ticketmaster should be able to query:

- commits
- commit messages
- diff hunks
- deleted implementations
- renames
- old comments
- reversions
- historical tests
- prior architectural approaches

Semantically as well as lexically.

The same principle behind "git why" belongs naturally inside Ticketmaster:

«Current code tells you what exists. History often tells you why.»

History can therefore participate in context compilation and investigation tickets.

---

27. Code intelligence should be incremental

The index is not rebuilt constantly.

Repository changes produce incremental updates.

Conceptually:

file changed
   ↓
parse affected structure
   ↓
update symbols
   ↓
update semantic chunks
   ↓
invalidate changed embeddings
   ↓
refresh affected graph edges

Many workers should be able to read concurrently.

Index mutation should use a simple writer model or transactional update mechanism.

A hundred agents searching should not corrupt the project index.

---

28. Context compilation uses code intelligence

Before a worker gets an enormous pile of files, Ticketmaster can compile context from:

semantic retrieval
symbol graph
exact matches
dependency outputs
Git history
active decisions
recent relevant changes

This attacks one of the largest hidden costs in current coding agents:

repeatedly rediscovering the codebase.

---

29. Documentation is generated from authoritative state

Ticketmaster should aggressively avoid documentation drift.

The project has authoritative sources:

Vision
spec
decisions
ticket graph
code
schemas
interfaces
tests
configuration
deployment state

Documentation should be derived from or checked against those sources wherever possible.

That includes:

- architecture docs
- contributor docs
- API references
- subsystem maps
- generated diagrams
- setup instructions
- operational docs
- project status
- decision history

---

30. Never-drifting documentation

The standard software failure mode is:

implementation changes
       ↓
README remains confidently wrong for 11 months

Ticketmaster should treat documentation validity as project state.

A code change may invalidate documentation dependencies.

Example:

T-512 changes provider routing
       ↓
affects:
  docs/providers.md
  architecture/provider-fabric.md

Those documents become:

STALE

until regenerated, reviewed, or explicitly marked unaffected.

This need not mean every doc is fully generated.

It means the system knows when its factual basis may have changed.

---

31. Documentation has provenance

A generated or maintained doc should know what supports it.

architecture/provider-fabric.md

derived_from:
  src/providers/**
  D-019
  D-027
  config/provider_roles.toml

If one of those changes, Ticketmaster can assess whether the doc requires reconciliation.

This creates documentation that behaves more like compiled project knowledge and less like detached prose.

---

32. Human-written docs remain first-class

Ticketmaster should not replace good technical writing with sludge generated from source.

Humans may write authoritative prose.

The system can still track:

claims
linked decisions
code references
affected interfaces
last verified project state

The goal is not:

«AI writes all docs.»

The goal is:

«Docs should know when the world underneath them changed.»

---

33. Steady-state operation

After Genesis, Ticketmaster moves into long-horizon operation.

Normal changes follow clearer authority boundaries.

Milestones become harder to reopen.

Interfaces have ownership.

Verification becomes stricter.

Autonomous work can continue indefinitely.

The graph still evolves.

Loops still exist.

Architecture can still change.

But the cost of modifying established state now reflects its blast radius.

---

34. Directed graphs with explicit cycles

Real engineering is not a DAG.

Some regions are acyclic.

Others loop.

implement
   ↓
verify
   ↓
failure
   ↓
diagnose
   ↓
repair
   └──────► verify

Cycles carry:

attempt count
failure history
progress
remaining budget
exit predicate
escalation predicate

It is not:

«keep thinking until satisfied.»

---

35. Milestones are graph cuts

A milestone is a boundary across which assumptions become durable.

Once closed, downstream work can trust it.

Reopening requires an explicit transition with sufficient authority.

This prevents new agents from endlessly rediscovering and relitigating settled architecture.

---

36. Frontier intelligence is sparse

Ticketmaster spends expensive intelligence where it has leverage:

- Genesis Vision
- architecture
- ambiguous decomposition
- major restructuring
- repeated failure
- complex integration
- semantic review
- maturity evaluation
- high-blast-radius decisions

Not:

T24 dependencies finished.
Should T24 become READY?

Software knows.

---

37. The coding client

Ticketmaster must include an exceptional individual coding agent.

$ tm

should be competitive with the best standalone coding-agent clients.

It should support:

- semantic code search
- exact code search
- LSP navigation
- structured edits
- shell
- Git
- debugging
- tests
- browser use
- computer use
- artifacts
- worktrees
- long-running commands
- dependency inspection
- Git-history search

You should happily use it with no swarm at all.

---

38. Manual driving remains inside Core

Manual work never leaves orchestration.

If you start investigating code yourself, those changes can still attach to tickets, decisions, evidence, and verification.

Humans are executors.

---

39. Sessions are views, not truth

Conversation can remain live and useful.

But important state is promoted out of conversation into durable project objects.

A fresh worker should not need six hours of transcript to continue.

---

40. Multiplayer engineering

Multiple humans and agents share one project world.

Humans can:

observe
comment
pair
take over
delegate
review
approve
redirect

Agents can collaborate where useful.

Tickets, not personalities, organize the work.

---

41. Task-management integrations are mirrors

Ticketmaster should integrate with existing human project-management systems.

Examples might include:

Linear
GitHub Issues
Jira
GitLab Issues
other task systems

But this integration must preserve a critical architectural boundary:

«Ticketmaster's internal graph is authoritative. External task-management systems are projections of it.»

Ticketmaster cannot let the limitations of an external issue tracker define its internal orchestration semantics.

Its graph may contain:

- machine-only tickets
- ephemeral exploration nodes
- loops
- authority metadata
- leases
- verification nodes
- hidden implementation details
- fine-grained dependencies
- generated recovery work

It would be absurd to force every one of those into Linear.

---

42. Projection into external task systems

Instead, selected project state is mirrored outward.

For example:

Ticketmaster T-184
      │
      ├── internal children
      ├── verifier nodes
      ├── agent work
      └── artifacts
      │
      ▼
Linear ENG-412

The human-facing tracker might show:

Fix authentication refresh race
status: In Progress
owner: Allie

while internally Ticketmaster knows about twenty-seven relevant execution events.

Changes from the external system can flow back when semantically meaningful.

Examples:

status change
assignment
comment
priority
human-created issue

But the external tracker is never the orchestration database.

---

43. Basic native project canvas

Ticketmaster should also provide its own lightweight planning surface for teams that do not want a separate task manager.

The native UI can show:

backlog
active work
milestones
dependency graph
reviews
blocked work
recent decisions
active participants

Think of it less as replacing Linear and more as exposing the human-comprehensible surface of the internal graph.

Small teams can use it directly.

Larger teams can mirror the same information into their existing systems.

---

44. Provider Fabric

Ticketmaster should not equate roles with specific models.

Tickets request capabilities such as:

vision.frontier
planner.frontier
coder.fast
coder.deep
explorer.cheap
reviewer.semantic
synthesizer.long_context
computer_use

Provider Fabric maps roles to available models.

---

45. Quota-aware multi-provider execution

Provider Fabric tracks:

subscription limits
API quotas
token budgets
rate limits
concurrency
daily caps
monthly caps
provider health
latency
reliability
cost

A 24/7 system should route around exhausted providers instead of discovering quota exhaustion as an exception halfway through the night.

Scarce frontier capacity can be reserved for roles where it has leverage.

---

46. Graceful fallback

Provider failure can route:

preferred provider
       ↓ unavailable
equivalent provider
       ↓ unavailable
acceptable lower tier
       ↓
queue / wait

The policy depends on the task.

A formatting cleanup can degrade aggressively.

An architectural audit may wait.

---

47. Verification over self-report

Workers return evidence.

They do not certify themselves.

worker:
  what I changed

verifier:
  what mechanically happened

auditor:
  whether it actually satisfies intent

This separation is foundational.

---

48. Commands become durable artifacts

Expensive commands run once.

Full output is stored.

Agents can query the artifact later.

No repeated:

command | tail -3
command | tail -10
command | tail -50

because the harness forgot it already paid for the command.

Infrastructure remembers.

---

49. The harness itself belongs to the project

Ticketmaster's coding harness should not be a sacred invisible layer.

Its behavior is software.

Therefore, where practical, harness policy and configuration should live in or alongside the repository as versioned project state.

Examples:

tool preferences
search routing
context compilation policy
model-role mapping
verification policy
prompt fragments
command handling
editing behavior
project-specific retrieval rules

If the harness performs poorly on this project, improving it is legitimate engineering work.

---

50. Harness work is just work

Suppose agents repeatedly use exact search where semantic retrieval would have found the relevant subsystem much faster.

Or context compilation repeatedly includes 80k irrelevant tokens.

Or one particular verification loop causes needless retries.

Those observations may produce tickets:

T-901 Improve semantic search routing for architecture queries

T-902 Reduce redundant context around generated files

T-903 Cache test discovery artifacts

T-904 Improve Rust symbol retrieval fallback

These are ordinary tickets.

They have objectives.

They have evidence.

They can be benchmarked.

They can be reviewed.

They can be reverted.

The system can improve its own engineering environment without inventing a mystical self-modifying agent.

---

51. Harness epochs

One important constraint:

A live session should not mutate underneath itself unpredictably.

If a harness change is merged at 14:03, a coding agent that started at 13:30 should normally continue with the harness epoch it began with.

New sessions receive the new harness version.

Conceptually:

session S41
harness_epoch: 17

T-901 merged
harness_epoch: 18

session S41
  continues on 17

new session S42
  starts on 18

Long-running autonomous jobs can either remain pinned or restart at explicit safe boundaries.

This provides self-improvement without non-reproducible mid-session personality shifts.

---

52. Benchmark-driven harness improvement

Ticketmaster can devote a bounded amount of engineering capacity to improving its own efficacy.

The system can collect operational metrics such as:

time to ticket completion
tokens per successful ticket
tool calls per ticket
search calls before first relevant hit
verification failure rate
retry count
context bytes supplied
commands unnecessarily rerun
human intervention frequency
provider cost

This can expose obvious inefficiencies.

But measurement does not automatically rewrite the harness.

Instead:

observation
   ↓
candidate ticket
   ↓
implementation
   ↓
benchmark
   ↓
review
   ↓
promotion to new harness epoch

The harness improves the same way the product improves:

through engineering.

---

53. Repository-local benchmarks

Projects can carry representative benchmark tasks.

Examples:

find the retry implementation
fix seeded auth regression
trace symbol across packages
recover a failed build
modify API and update dependent docs

Ticketmaster can periodically replay those tasks under controlled conditions to compare harness changes.

Metrics might include:

success
cost
latency
tool count
context consumed
unnecessary reads
unnecessary edits
verification quality

That gives harness engineering an actual feedback loop.

---

54. Efficacy budgets

A project may optionally allocate some fraction of resources toward engineering the engineering system itself.

For example:

efficacy:
  enabled: true
  max_compute_fraction: 0.03
  require_benchmark_gain: true

Three percent of spare execution capacity might be available for investigating repeated inefficiencies.

This should remain bounded and subordinated to actual project goals.

Ticketmaster should not spend all night optimizing its own semantic-search prompt while the product burns down.

---

55. No mystical recursive self-improvement

The important distinction is:

BAD:

agent notices itself
agent rewrites itself
agent becomes better

versus:

GOOD:

system telemetry reveals inefficiency
      ↓
ticket created
      ↓
normal engineering process
      ↓
change benchmarked
      ↓
change reviewed
      ↓
new harness epoch

Ticketmaster can improve itself precisely because it refuses to mythologize self-improvement.

Its harness is just software.

---

56. Decisions are project objects

Architecture and policy should not disappear into chat transcripts.

A decision has:

subject
decision
reason
evidence
affected tickets
affected docs
author
timestamp
supersession history

Future workers inherit it.

---

57. Failure has type

Failure classes include:

executor_crash
provider_failure
tool_failure
verification_failure
incorrect_assumption
dependency_changed
resource_conflict
authority_insufficient
context_insufficient
nonprogress
budget_exhausted
retrieval_failure
documentation_inconsistency

Different failures create different transitions.

---

58. Oversight is authority-shaped

Human control should correspond to consequence boundaries, not endless command confirmation dialogs.

Example:

autonomous:
  repository.read
  src/parser.write
  tests.run
  child_tickets.create
  git.commit

approval_required:
  database.schema
  public_api.break
  production.deploy
  milestone.close
  spend_over: 10

---

59. Multiple clients, one world

Ticketmaster can expose:

- terminal
- IDE
- web
- mobile
- API
- rooms
- reviews
- project canvas

No client owns truth.

All interact with the same project.

---

60. Presence

Ticketmaster can show:

Allie     editing T-184
Maya      reviewing M-04
Claude    investigating T-192
Codex     running T-211

and:

src/parser/**
  leased by T-184

Human-human, human-agent, and agent-agent collision prevention use the same substrate.

---

61. Social topology comes from work

Ticketmaster should not manufacture pretend AI companies.

If two tickets share an interface, their participants have a reason to coordinate.

If a milestone contains ten tickets, a project room may naturally emerge.

The graph determines collaboration topology.

---

62. The complete lifecycle

                         INTENT
                           │
                           ▼
                         VISION
                           │
                           ▼
                          SPEC
                           │
                           ▼
                    GRAPH COMPILATION
                           │
                           ▼
                       IGNITION
                  broad leased authority
                           │
                           ▼
                     WORKING V0
                           │
                           ▼
                     EVALUATION
                           │
                           ▼
                    WORKING V1
                           │
                           ▼
                     STABILIZATION
                           │
                           ▼
                 MATURITY GATE PASSES
                           │
                           ▼
               AUTHORITY RECONVERGENCE
                           │
                           ▼
                    STEADY STATE
                           │
             ┌─────────────┼──────────────┐
             │             │              │
        autonomous       human        harness
           work           work       engineering
             │             │              │
             └─────────────┼──────────────┘
                           │
                      shared graph
                           │
                           ▼
                  milestones / releases
                           │
                           ▼
                       evolution

---

63. Example: starting from nothing

$ tm genesis

> I want a tiny semantic search tool for git history.
> git blame tells you who, git why tells you why.
> local only. CLI first. keep v1 tiny.

Ticketmaster:

1. preserves the seed
2. creates Vision
3. produces initial spec
4. initializes a Git repository
5. builds code intelligence state
6. compiles the provisional graph
7. begins Ignition
8. assigns broad leased Genesis authority
9. creates a thin working V0
10. lets the user try it
11. absorbs feedback
12. revises tickets and spec
13. reaches V1
14. passes maturity gate
15. reconverges authority
16. continues steady-state development

Later:

$ tm

git-why

SINCE YOU LEFT

12 tickets closed
 3 regressions repaired
 1 documentation set reconciled
 1 benchmark improved
 1 milestone passed

HARNESS

epoch 18 active
semantic architecture-search benchmark: +11% retrieval success
context overhead: -18%

NEEDS YOU

T-187 Choose default embedding model

ACTIVE

T-191 Windows packaging
T-193 incremental index repair
T-194 CLI output cleanup

You enter a ticket.

Make a decision.

Ask the coding agent to fix something unrelated.

Create another ticket.

Another agent picks it up.

You close the terminal.

The project continues.

---

64. Example: attaching to an existing repository

$ cd enormous-existing-project
$ tm attach .

Ticketmaster does not pretend Genesis means rewriting the project.

Instead it performs project assimilation:

inspect repository
      ↓
index code semantically
      ↓
build symbol graph
      ↓
inspect Git history
      ↓
discover existing docs
      ↓
discover test/build system
      ↓
identify external issue trackers
      ↓
construct initial project state
      ↓
ask only material unresolved questions

The first ticket may simply be:

T-001 Understand current architecture sufficiently
      to safely accept autonomous work.

Existing software joins Ticketmaster without losing its own identity.

---

65. Example: task-management mirroring

Internally:

M-12 Authentication reliability

├─ T-184 investigate refresh race
│  ├─ T-184a inspect Redis locking
│  └─ T-184b history search
│
├─ V-51 reproduce failure
├─ T-185 implementation
├─ V-52 regression verification
└─ A-19 semantic audit

Linear might simply see:

ENG-412 Fix authentication refresh race

Status: In Progress
Owner: Allie
Milestone: Authentication reliability

Ticketmaster retains the machine-level graph.

Linear receives the human abstraction.

That is the right direction of dependency.

---

66. Product invariants

Ticketmaster should preserve these principles:

The project, not the agent, is persistent.

Core owns project truth.

State belongs to infrastructure rather than context windows.

Authority is explicit, leased, scoped, attenuated, revocable, and auditable.

A tiny prompt should be capable of becoming working software.

Existing repositories are first-class.

Genesis is intentionally more fluid than mature operation.

Genesis optimizes aggressively for reaching a genuine V0/V1.

Exceptional bootstrap authority eventually expires.

Authority reconverges after maturity.

Inference resolves uncertainty; deterministic machinery handles known transitions.

Frontier intelligence is concentrated at high-leverage boundaries.

Code intelligence is foundational infrastructure, not an optional tool plugin.

Semantic search, exact search, symbol navigation, and Git history complement one another.

The system should not repeatedly rediscover its own codebase.

Documentation should know when the facts underneath it changed.

Human-written documentation remains valuable and first-class.

External task managers are mirrors, never the orchestration database.

Provider choice is infrastructure, not project identity.

Quota exhaustion is routable state, not catastrophe.

Humans and agents operate inside the same graph.

Manual work is not outside orchestration.

Workers provide evidence rather than certifying themselves.

Sessions are useful; durable state matters more.

The harness is software and may be improved through normal engineering.

Harness changes are benchmarked and promoted as explicit epochs.

Existing sessions do not unexpectedly mutate when the harness changes.

Self-improvement means measured software engineering, not recursive-agent mythology.

Agents are disposable participants in something larger than themselves.

---

67. The deepest inversion

Most current systems begin with:

SMART MODEL
     │
     ├── memory
     ├── subagents
     ├── tools
     ├── planning
     ├── retries
     ├── orchestration
     └── everything else

Ticketmaster begins with:

PERSISTENT COMPUTATIONAL ENGINEERING SYSTEM
     │
     ├── project state
     ├── authority
     ├── work graph
     ├── code intelligence
     ├── project knowledge
     ├── scheduler
     ├── collaboration
     ├── evidence
     ├── provider fabric
     ├── documentation
     ├── harness
     └── execution
             │
             └── cognition leased when useful

Intelligence is not the thing holding the system together.

Intelligence is a resource the system allocates.

The same is true of humans.

The same is true of tools.

The same is true of models.

The durable entity is the project itself.

---

Ticketmaster

One prompt can become a project.
An existing repo can become a project.
A project becomes a persistent machine.
Its graph lives beneath every interface.
Its code is searchable by meaning, symbol, text, and history.
Its docs remain tethered to reality.
Its harness can be engineered like everything else.
Its task trackers are mirrors, not masters.
Humans and agents enter and leave.
Authority moves through it.
Knowledge accumulates beneath it.
Intelligence appears where judgment matters.
Software handles everything else.

The agent isn't alive 24/7.

The software project is.
