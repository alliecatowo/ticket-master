+++
title = "The thesis"
weight = 1
+++


> The industry's current answer is: give the agent bash, some API keys, and a pile of if-else
> statements that load markdown files — and it'll figure it out.

That is the whole state of the art, and it is beneath what these models can do. Frontier agents are
extraordinarily capable right now, and the harnesses around them are shooting them in the foot:
1970s tools, brittle prompt scaffolding, and a hope that the model compensates.

**Build tools as intelligent as the agents using them.** Everything below follows from that.

## 1. Context is resolved, not discovered

A conventional agent begins every task ignorant and spends its opening turns rediscovering the
repository — `ls`, `grep`, open a file, guess again. It pays for that exploration in the most
expensive currency there is, because **context is re-sent on every turn**. Eight turns of groping
around before the first real edit means paying for that context eight times before any work happens.

Fewer turns is cheaper *even at identical context size*. That is the economics, and it is a summation,
not a constant.

So: given a ticket, we resolve the needed context **up front, deterministically**. The agent's first
turn already contains nearly everything it needs. No skills to author, no rules files to maintain, no
MCP servers to wire, no "context engineering" — which is a brittle discipline that exists only
because retrieval is bad.

This is the difference between *configuring* an agent and *engineering* a system.

## 2. The navigation ladder

When the agent does need to explore — and it will, because coding is the core task — it should
navigate the way a good engineer does, not the way a shell script does.

**Stage 1 — semantic, when neither location nor symbol is known.** AST-aware, symbol-aware chunking
and retrieval. Not naive embeddings over arbitrary text windows: chunks that respect the structure of
code, queried semantically, seeded by what the ticket already claims.

**Stage 2 — symbol map over what came back.** Not raw file dumps. *There is a class here, with these
methods, implementing this trait.* Structure before content.

**Stage 3 — LSP traversal, the way a real engineer reads code.** Go to definition. Find
implementations. Hover for the type. And above all: **callers and callees.**

That last one is the most important tool in the system, because it is the only one that addresses
*the agent does not know what it does not know*. A model cannot grep for a caller it has never heard
of. It can always ask "who calls this?" Callers and callees convert an unknown-unknown into a
traversal, and that is what makes navigation fast, accurate and cheap instead of speculative.

**Stage 4 — literal search, if you genuinely need it.** `rg` stays available. We do not restrict the
agent and we do not add friction; restriction is a confession that your tools are not good enough to
win on merit. But it should be *painfully obvious* that the higher rungs are better, so reaching for
string matching is a rare, deliberate act rather than a first instinct.

**Stage 5 — edit, with a REPL and live LSP feedback.** The agent should see type errors as it writes
them, the way an engineer does, not after a full rebuild in a later turn.

LSP and symbolic knowledge are first-class, not an optimisation. They are the substrate.

## 3. The agent generates; the system does the rest

Why is an agent running `git` commands at all? Why is it composing a commit message inside a
200k-token session that is also holding the entire implementation in its head?

**Given a ticket, you get code. Code becomes a PR by a deterministic process.**

The agent signals completion. The system takes the diff and does the mechanical work — and where a
model *is* needed for a mechanical step, it is a fresh, stateless, near-free call: *here is a diff,
write a commit message.* *Here is a merged change, write a PR description.* Those calls cost almost
nothing because they carry no history. Pipe the result through the app. **Like a real engineer.**

The exact sequence is not the point and must not be hardcoded. The concept is: the agent does the
irreducibly intelligent part — reason about the problem, write the code, decide what new work exists
— and everything deterministic stays deterministic.

## 4. Exploit statelessness

Models are stateless. We keep behaving as though they are not.

Long-horizon sessions are a workaround for systems with nowhere to put state. **We have somewhere to
put state**: an event-sourced log, a ticket graph, a goal loop whose steps are events. So we should
prefer many cheap, short, stateless calls over one enormous accumulating session, and use turn-based
agentic sessions only where the work genuinely requires continuity.

Every long session is re-paying for context that a durable system would have simply looked up.

## 5. Verification is a gate, not an opinion

The agent's claim of success is an input, never a conclusion.

- Validation is deterministic wherever it can be: it compiles, the tests pass, the invariants hold.
- Iteration is cheap, and can be done **independently of whoever wrote the original implementation**
  — a fresh worker with the failing evidence and the compiled context is often better than the
  author, and always cheaper than a long session.
- Fulfilment is genuinely *determined*, not asserted. Then the state machine moves.
- It gets audited later regardless, and bugs that accumulated surface at the next cadence.

## 6. The loop

```
ticket ──▶ deterministic context resolution
       ──▶ agent produces a diff or artifact (minimal exploration, free to explore more)
       ──▶ deterministic validation
       ──▶ cheap iteration until fulfilled, by anyone, not necessarily the author
       ──▶ fulfilment determined, not claimed
       ──▶ PR by deterministic process ──▶ review and acceptance (a gate)
       ──▶ code lands, ticket state changes, the state machine advances
       ──▶ state changes drive cadences
       ──▶ cadences alter authority and the graph itself — new tickets, redispatched work,
           re-prioritised direction — from both deterministic and agentic mechanisms
       ──▶ repeat
```

A human is not a special case in this loop. A person checking out a branch and coding on their laptop
is **another input and output of the same state machine** — the same ticket, the same criteria, the
same gate, the same state transition. Not a parallel process that the system merely tolerates.

## 7. The client stands alone

The TUI has to win on its own. Installed by itself, with no ecosystem, no plugins and nothing else
configured, it must absolutely crush coding tasks and beat Codex head to head.

The ecosystem should be epic. But an ecosystem is what you build *after* the thing is already good,
never the reason it is good. A product whose value depends on installing thirty plugins has admitted
the core is not enough.

## What this rejects

- **Prompt scaffolding as architecture.** Loading markdown by if-else is not engineering.
- **Restriction as safety.** Never fence the agent in. Make the good path obviously better.
- **Long sessions as memory.** That is a workaround for having no state.
- **The agent as orchestrator.** Deterministic machinery beneath nondeterministic intelligence.
- **Agent-authored ceremony.** Commits, PR text, status updates — mechanical work belongs to the
  machine, or to a stateless call that costs nothing.

## Why it can work here

Every point above needs something a transcript-based agent does not have: durable project state, an
authority algebra, deterministic retrieval, and a scheduler that is a pure function. That is what
this repository is. The thesis is not that we prompt better. It is that **the model should not be the
orchestration system** — and once it isn't, all of this becomes ordinary engineering.
