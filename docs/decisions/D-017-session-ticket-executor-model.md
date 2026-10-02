# D-017 — Sessions, tickets, and executors: how they actually relate

**Status:** accepted · **Date:** 2026-09-20 · **Supersedes:** nothing (a synthesis and one
correction of drift, not a new design)

## Context

Raised directly by the project owner, worried about the coherence of the whole thing after a long
session of feature work: *"tm=claude. tickets and shit is all in the background. right now home
makes session = ticket. sessions can create tickets. have none. or have multiple tickets. they are
not like one session = ticket."* And, more fully, the model they actually want:

> the tickets are internal sorta tickets. that can be synced with ticket providers but they're an
> internal and sometimes divergent primitive. it has its own tickets, linear or wherever is just
> another ticket workspace. you have a session. you ask it do something, it makes a plan. it turns
> that plan into tickets (should we make this tickets? if not in auto mode. also it should have
> permissive autonomous permissions by default). it acts on them. it manipulates ticket state, etc.
> thats the tui sorta version. but then if you do it from home or whatever like as you get more
> advanced. you just manage ticket state. but the tui agent is this sorta ephemeral disconnected
> in between / client. [...] the coding agent is just another executor, like codex / claude but
> better. the tui is both a tui interface for the ticket master *server*. but the tui agent is
> just, again, a tui agent like opencode but better in every way. and tickets can be worked on
> autonomously, in the background, but the tui agent is how you then manipulate and steer etc,
> break off and *detach* to do more focused work or disconnected stuff, then you sorta reattach
> when you're done and new tickets are created or some are closed and the loop / state goes on.

This document exists because that description needed checking against what's actually written
down, not assumed. It turns out **most of it is already specified**, in three separate places
that don't visibly reference each other: `SPEC.md` §14/§17, `docs/backlog.md`'s "Remote control
and teleport" section, and `docs/decisions/D-001-executor-strategy.md`. The gap was never in the
vision. It's that (a) nothing ties the three together in one place, (b) one real implementation
detail — the plan-to-tickets decision point — was never specified at all, and (c) the actual
running code currently violates the vision in one concrete, already-diagnosed way.

## The model, stated once, with what already backs each claim

**A session is an ephemeral, disposable *view*, never the persistent thing.** `SPEC.md`
Invariant #1: "The project — not any agent or session — is the persistent entity." Invariant #12:
"Sessions are views; a fresh worker can always continue from durable state alone." §14: "Sessions
are views: a session holds a transcript and a pinned harness epoch, but anything that matters is
promoted to durable objects... A test asserts a fresh worker can complete a handed-off ticket
using only durable state, with the prior transcript deleted." This was already the design. It is
not yet what the code does — see "The current violation" below.

**Tickets are Ticketmaster's own durable, internal-first primitive.** Not a cache of an external
tracker, and not required to exist for a session to do something. Invariant #9: "External trackers
are mirrors, never the orchestration database." `crates/tm-mirror` is the sync adapter layer
(GitHub/Linear/Jira/GitLab today) — it pushes/pulls against *our* event-sourced tickets, which
remain authoritative even when a mirror drifts or is offline. "Linear or wherever is just another
ticket workspace" is exactly invariant #9, restated.

**The coding agent is just another executor.** `D-001`: `tm-agent`/`AgentLoop` (this project's
own built-in worker) is "the reference executor, deliberately minimal... explicitly replaceable."
External harnesses — Claude Code, Codex, Gemini CLI, Goose via ACP (`crates/tm-acp`, this
session's own B-12 work), `pi`, `opencode`, `human` — are first-class executors behind the same
`Executor` trait. Our own executor's job is to be a genuinely better version of what Codex/Claude
Code already do, per D-001's own framing ("nobody's moat," "the advantage... is *portable across
harnesses*") — not to be architecturally special. This is already built, not aspirational.

**The TUI serves two roles at once, and both are real.** First, it is a client to the
Ticketmaster *server* — the durable, event-sourced project state, which `tm serve` (§14) exposes
and which outlives any one machine or session. Second, standing alone, it is simply a coding-agent
TUI in the same category as opencode's, meant to be better in every way *as a standalone tool*,
with zero setup required (D-003's whole point: bare `tm` in a fresh directory just works, writes
nothing into your workspace, and the ticket/event machinery underneath is discoverable, not
mandatory). Both framings are true simultaneously and are not in tension: the "massive ecosystem"
(tickets, mirrors, scheduler, multi-executor routing) is what's running underneath even the
simplest bare invocation; a human who never looks at it never has to.

**Work gets done two ways, and a session steers, it doesn't have to drive.** Tickets can be worked
on autonomously in the background (`tm sched run`, this session's own `--worktree` isolation work,
the scheduler dispatching to whichever executor a ticket's `ExecutorRequirements` names) — this is
already built. A session (the TUI agent, or a plain `-p`/interactive loop) is how a human
*manipulates and steers* that ongoing process: checks status, nudges a ticket, attaches/detaches.
**Detach-and-reattach itself — walking away to do focused, disconnected work and coming back to
find new tickets created, others closed, the loop having continued — is real, specified, and not
yet built.** `docs/backlog.md`'s "Remote control and teleport (SPEC §14, §18)" section already
names this precisely: attach/detach as ordinary operations resuming from a known `seq` (never
gapping, never replaying from zero, since state is already an event log), push vs. pull as
separate verbs with separate attenuated `Authority` grants, multiple concurrent viewers with
presence. Competitive grounding for the same shape, checked live this session: Devin's own
framing is explicit about this — "the developer stepping out of the critical path and re-entering
at defined checkpoints" — and Cursor/Claude Code's background-agent modes are the same pattern:
task assignment is either explicit (a real issue, a real description) or the agent's own judgment
call for real multi-step work, never "every chat message becomes a tracked unit."

## What was genuinely missing: the plan-to-tickets decision point

Nowhere in this codebase is it specified *when* a session's plan should become real, tracked
tickets versus just... happening. The user's own framing is the actual spec for this, and it
wasn't written down anywhere before this document:

- A session asks the model to do something. The model forms a plan.
- **Whether that plan becomes real tickets is a decision, not an automatic step** — in interactive
  (non-autonomous) mode, this should be something the session can be asked about or decide
  deliberately; in autonomous mode, it just happens without asking.
- **The default posture is permissive/autonomous**, not conservative-by-default. This part is
  already correctly built: `crates/tm-types/src/action.rs`'s `Oversight::autonomous()` ("nothing
  requires approval") is `AgentLoop`'s actual default oversight policy today (wired for real this
  session, `docs/decisions/D-009-oversight-policy-wiring.md`) — `Oversight::conservative()` exists
  as an opt-in, not the default. This one part of the ask was already true; it just wasn't
  connected to this framing anywhere.
- Once real tickets exist, the session acts on them the ticket-centric way this whole system
  already does: authority-scoped tool calls, budget tracking, state-machine-governed transitions.

This is now the specified behavior. It is **not yet implemented** — today, per `crates/tm-cli/src/
agent.rs`'s `AgentSession::resolve_ticket`, a scratch ticket is created unconditionally on a
session's first turn, with no decision point and no "zero tickets" outcome at all. That's the next
section.

## The current violation, named precisely against the invariants above

`AgentSession::resolve_ticket` eagerly creates a real, persisted scratch `Ticket` the first time
any turn runs in a session with no `attached_ticket` — regardless of whether the prompt is real,
trackable work or a trivial question, and with no mechanism for a session to ever have zero
tickets. It does not recreate one per message (`self.attached_ticket` is cached for the rest of
that session's turns), so this is "exactly one ticket per session, created eagerly, no way to
opt out" rather than "one ticket per message" — but it is still a direct violation of Invariant #1
("the project... not any... session... is the persistent entity") and #12 ("sessions are views"):
a session's *identity* is a ticket right now, which is precisely backwards.

Investigated, not guessed: `crates/tm-agent/src/outcome.rs`'s `AgentTask.ticket` is a required,
non-optional field, and it is not merely an attribution label — `crates/tm-agent/src/
agent_loop.rs` genuinely reads a real ticket back from the `Store` for goal-seeding
(`view.tickets.get(&task.ticket)`), goal-state re-orientation (`store.goal_state`), and a
per-ticket event-count budget cap (`store.event_count_for`), and bakes the ticket id into the
rendered prompt. Making `AgentTask.ticket` `Option<TicketId>` would ripple a real "what does this
mean with no ticket" question through every one of those call sites, in the one part of this
system meant to be boring and correct. This is a real, multi-step fix, not a quick decoupling —
see `docs/archive/AGENT_HANDOFF.md`'s top section for the concrete next-session starting point (stop the eager
create; decide what a ticketless turn's `AgentTask.ticket` should be, given those real reads;
build the promotion path, since `ticket.create_child` requires a parent a ticketless session
doesn't have).

**2026-09-22 addendum — the cited evidence for "not attribution-only" is softer than recorded
above, on a second, closer reading of the actual `tm-core` signatures (not the `agent_loop.rs`
call sites alone).** This narrows the open question in step 2 below; it does not reverse the
"don't quick-decouple this" conclusion, and no code changed for this addendum:

- `tm_core::Store::record_usage`'s own signature is `record_usage(ticket: Option<&TicketId>,
  session: Option<&SessionId>, ...)` — the store layer already anticipates a ticket-less usage
  record (it falls back to session/project-level budget scopes when `ticket` is `None`). Passing
  `None` here when `AgentTask` has no ticket is threading an existing capability through, not
  inventing new store-side behavior.
- `Store::event_count_for` takes a generic `subject: &Id`, not `&TicketId` — it works identically
  whether that `Id` resolves to a real ticket row or not, so the event backstop needs no special
  casing either way.
- `crate::prompt::render_task_prompt(ticket: &TicketId, pack: &ContextPack)` — checked directly —
  uses the ticket id *only* to print `"# Ticket {ticket}\n"` as a header. That one call site
  genuinely is attribution-only, contrary to the blanket claim above.

**What is still genuinely ticket-row-dependent, unchanged from the original finding**:
`Store::goal_state`/`set_goal`/`reorient_goal` (all take `&TicketId`, not `Option`, and read a
real row), `Store::claim_goal_complete` (called right before every `ticket.submit` dispatch), and
`Store::budget_handoff` (hands off a specific ticket's lease — meaningless with no ticket to hand
off). So the honest, narrower statement of step 2's open question: three of the four originally
cited call sites are already ticket-optional or purely cosmetic; the real remaining design
question is narrower than recorded — what should goal-tracking and budget-handoff *mean* for a
ticketless turn (most likely: skip goal-tracking entirely, since there's no ticket `objective` to
seed one from; on budget exhaustion, produce `AgentOutcome::BudgetExhausted` directly without a
`budget_handoff` call, since there's no lease to release). Still not attempted in this pass — see
`docs/archive/AGENT_HANDOFF.md`'s architectural-gap section for the pointer.

## Implemented, 2026-09-22

The violation above is fixed, along with two worse bugs found on the way that the original
write-up missed:

- **Every message after the first was silently dropped.** `run_turn_streaming` only used the
  prompt to create the scratch ticket, so turn 2+ re-sent turn 1's objective. The model never saw
  what the user just said, and there was no conversation memory at all.
- **A normal reply was reported as a failure.** With no `ticket.submit` possible from a `Draft`
  scratch ticket, every everyday chat turn ended `failed (Other)`.

What changed:
- `AgentTask.conversation: Option<Conversation>` carries the turn's user message plus every prior
  turn's message and steps. With it set, a turn that ends in plain text is
  `AgentOutcome::Replied`. With it unset (scheduler and executor ticket work), behavior is
  byte-for-byte unchanged.
- `AgentTask.ticket` and `CallContext.ticket` are `Option`. Per the addendum above, usage records
  against session and project scopes, and goal tracking and the per-ticket event backstop are
  skipped. Budget exhaustion ends the turn without a lease handoff. Events land on the session as
  their subject. Tools that inherently act on a ticket (`ticket.submit`, `evidence.attach`,
  `ticket.comment` without an explicit id) refuse with a clear message. `ticket.create_child`
  with no attached ticket creates a root ticket, which is how a session creates its first one.
  The browser, computer, and pty session registries key on `(Option<TicketId>, SessionId)`.
- `AgentSession` no longer creates a scratch ticket. It runs against the attached ticket if there
  is one, and otherwise against `tm_context::compile_session`: retrieval and wiki hits for the
  message, `AGENTS.md` conventions and skills, and an index of open tickets.
- Acceptance test, as this document asked: `a_trivial_question_in_a_fresh_session_creates_no_ticket`
  (`crates/tm-cli/src/agent.rs`). The pty-driven `tui_turn.rs` asserts the same through the real
  binary.

Still open: the plan-to-tickets decision point is guidance to the model, not a mechanism. The
detach and reattach flow is unchanged.

## What this costs, stated plainly

Fixing the violation above properly costs real kernel-touching engineering effort across
`tm-agent`, not a quick patch — deliberately not attempted in the same pass as this document, per
the project's own `advisor()`-reviewed judgment that a session already burning down toward a usage
limit should not start a multi-step kernel change it can't finish and verify. Writing the vision
down clearly here, ahead of the fix, is what keeps the fix honest when it's actually attempted:
the acceptance test for that future work is literally "does a trivial Q&A session end with zero
tickets," which this document now makes an explicit, checkable claim rather than an implicit one.

The remote-control/teleport half of "detach and reattach" remains unbuilt, and this document does
not change that — it only clarifies that it is not a separate, optional nice-to-have, but the
other half of the same model this whole document describes: a session that can't detach and
reattach against durable server state isn't actually a view, it's just a shorter-lived version of
the same session=ticket conflation this document exists to name.
