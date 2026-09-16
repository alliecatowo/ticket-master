# D-001 — Build a minimal runtime, and make other harnesses first-class executors

- **Status:** accepted
- **Date:** 2026-09-16
- **Supersedes:** the implicit assumption in SPEC.md §11 that `tm-agent` is the only executor

## Subject

Whether Ticketmaster should build its own agent runtime, extend an existing harness, or delegate
entirely to third-party harnesses.

## Decision

All three, layered, with the **context pack as the boundary**:

1. Keep `tm-agent` as the *reference executor*, deliberately minimal. It exists to prove the
   context compiler works and to be the cheap, highly parallel workhorse. It does not try to win at
   interactive UX.
2. Make external harnesses first-class executors behind an `Executor` trait — `claude-code`,
   `codex`, `pi`, `opencode`, plus `human` — each normalizing its harness into `ExecutorOutcome`.
3. Generalize Provider Fabric into an executor fabric that routes compute placement (local,
   container, remote node, cloud sandbox) under the same quota, health and fallback rules.

## Reason

The project's advantage is not the agent loop. A tool-using loop is a few hundred lines and is
nobody's moat; Codex and pi are not winning on loop design. The advantage claimed in the brief —
deterministic prefetch and compiled context making cheaper models succeed more often in parallel —
lives in `tm-context` and `tm-codeintel`.

That advantage is *portable across harnesses*. Handing Claude Code a compiled context pack beats
Claude Code rediscovering the repository, so delegating does not forfeit the thesis; it exports it.
Conversely, building our own runtime in order to compete on loop quality would spend engineering
where we have no edge and create a maintenance burden (tool schemas, sandboxing, approvals,
streaming, session resume) that several teams already carry for us.

Keeping a minimal first-party runtime is still correct, because it is the only executor whose token
usage, context composition and parallelism we fully control — which is exactly what the cheap-model
thesis needs to be measurable. It is the instrument, not the product.

## Evidence

- `pi` ships interactive, print/JSON, RPC-over-stdio and SDK modes, with a "primitives, not
  features" extension model and no built-in MCP — trivially driveable, no standard to conform to.
- Codex is Rust and open source, with a mature sandbox and approval model worth learning from.
- There is no common agent protocol across these harnesses today, so per-harness adapters are
  required regardless of which one we prefer — the same conclusion already reached for issue
  trackers (§13.1) and model providers (§6).

## Consequences

- `tm-agent` is explicitly replaceable. If an external executor outperforms it, that is a success of
  the design, not a failure.
- Authority is enforced on **our** side of the boundary: external harnesses get a sandbox derived
  from granted authority, and their output is validated against that scope on return. We do not
  trust a third-party harness to honour our permission model.
- Verification stays separate regardless of executor. An external harness returns evidence, never a
  verdict.
- Adds an adapter maintenance surface, bounded by the capability declaration: an adapter that cannot
  do something says so, and the scheduler routes around it.

## Rejected alternatives

- **Extend a single existing harness as the base.** Couples the project's identity to one vendor's
  roadmap and still leaves the differentiated kernel — authority, leases, event-sourced project
  truth, documentation provenance, harness epochs — to be built anyway.
- **No first-party runtime at all.** Gives up direct control of context composition and token usage,
  which is the one thing the project claims to do better and must be able to measure.
- **A declared "agent teams" primitive.** Rejected on the brief's own principle that social topology
  comes from the work: if two tickets share an interface their participants have a reason to
  coordinate, and rooms plus presence already express that. A declared team invents structure the
  graph does not have and competes with it.
