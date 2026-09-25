# D-020 — System One decision providers (Jev, Laya) as a new fabric provider kind

**Status:** accepted and wired for ticket-creation shadow triage · **Date:** 2026-09-23 · **Supersedes:** nothing (amends SPEC.md §6.1; §0 not yet amended)

## Context

TypeSafe AI released **Jev** on 2026-09-15. It is a closed, hosted "System One" model
(`POST https://api.typesafe.ai/v1/systemone`, waitlisted, $0.042 per million input tokens, free
output) that takes a text or JSON `state` plus typed questions (`choice`, `score`, `noul`) and
returns a typed answer with per-option probabilities and a calibrated confidence. It never
generates text. **Laya** (`convaiinnovations/laya`, Apache-2.0, 421M, ModernBERT-large) is an
open reproduction, and **laya-mlx** is an independent MLX FP16 port. laya-mlx runs in about 1GB,
answers one short question in 7 to 13ms on an M3 Max, and is the "MLX model" the owner meant.

Laya is near chance zero-shot (0.362 on its own typed-decisions benchmark). It reaches 0.766
after a free, roughly 4-hour Kaggle fine-tune. Its context is 512 to 1024 tokens, and it is weak
when a question has many options.

tm currently resolves every bounded judgment (is this a question or a job, which tool family,
is this command risky, did the page change) by sending a frontier LLM a full turn. That turn
includes the roughly 18KB tool-schema block `docs/backlog.md` measured on every step. The full
research, with sources and verified-versus-inferred tags, is in
`docs/vision/system-one-decisions.md`.

## Decision

1. **A new provider kind.** Add a `DecisionProvider` trait in `tm-provider` next to `Provider`:
   `decide(DecideRequest) -> DecideResponse`, plus `limits()`, which reports max context tokens,
   max options and supported question kinds. `Provider` requires `complete` and `embed`, which a
   decider implements neither of.
   - **Reused from the fabric:** role routing, breakers, `Limits`, the price ledger and events.
   - **New role:** `Role::Decider` (`decider`).
   - **Routing:** candidates whose `limits()` can't hold a request are skipped, exactly like
     unhealthy candidates.
2. **One wire contract.** Adopt TypeSafe's `/v1/systemone` request and response shape; don't
   invent one. jevmlx already serves it locally. Backends:
   - a bundled **laya-mlx stdio sidecar** (JSON Lines, same body);
   - self-hosted HTTP (for example Modal);
   - hosted Jev directly or through Cloudflare Workers AI;
   - `MockDecisionProvider` for every test.
3. **Decisions are events.** Every call records `classify.decided` with site, backend, model
   revision, input and question hashes, answers, calibrated confidence, thresholds, disposition
   (`acted`/`fallback`/`abstained`/`shadow`), latency and cost. (`decision.*` is already the
   `DecisionId` domain's namespace.) **Replay reads the event and never re-infers.** Pure
   consumers take the recorded classification as data.
4. **A cascade with safe abstention.** If calibrated confidence clears the site's act threshold,
   a deterministic action runs. In the review band, the LLM runs with the decider's top-k as a
   hint. Otherwise, or on error, today's path runs unchanged.
5. **Tighten only around authority.** A decider may escalate `Allow` to `NeedsApproval`
   alongside `Oversight::review` (D-009). It may never soften `Deny` or grant authority.
   `Authority::permits` and `Fabric::route` stay pure.
6. **Shadow first.** Every new site starts in shadow mode. It moves to acting only when a
   `tm bench` decision-eval clears the site's within-coverage accuracy floor, keeps ECE at or
   below 0.1, and shows no end-to-end regression.
7. **Redaction before anything leaves the machine.** Add `redact_decide_request` through the
   existing `SessionRedactor` (D-011). No remote decision backend ships without it.

SPEC.md §6.1 names the `decider` role and `DecisionProvider` trait. At project open, `tm-cli`
loads the effective role table and adapts a configured non-mock decider into the store's
`TriageDecider` hook. Ticket creation records successful answers as `classify.decided` shadow
events; provider errors are logged and ticket creation continues. The default mock is disabled,
unless `TM_DECIDER_SHADOW=1` opts in. This wiring currently covers ticket-creation triage only;
turn-start and step-start sites remain future work. SPEC.md §0 does not yet name "System One
decisions" as an inference site; that amendment remains a follow-up.

## Why

- **It is the thesis, sharpened.** "Inference resolves uncertainty; software acts on the
  resolution" (`docs/vision.md`). Most of tm's runtime uncertainty is bounded, and a
  schema-typed, calibrated answer is exactly the form software can act on. The LLM stays for
  generation and genuine ambiguity.
- **Cost and latency.**
  - Local Laya is free and answers in tens of milliseconds.
  - Hosted Jev is about $0.00006 for a 1.5k-token decision.
  - A frontier turn costs thousands of tokens and seconds.
  - Chat-intent routing, tool-family selection and browser "did it work" checks are the
    highest-volume sites.
- **Labels are free.** The event log already records the outcome of every site. `Session.promote`
  covers ticket-creating turns, `StepRecord` the tools actually used, `approval.decided` human
  risk judgments, and ticket end states the triage quality. That is enough to fine-tune a
  tm-specific Laya on a free GPU.
- **Why a separate trait rather than folding into `Provider`.** The request shape, limits and
  response are different in kind. Forcing `complete` and `embed` stubs on a decider, or `decide`
  stubs on 21 LLM backends, would put lies in the trait.
- **Why `/v1/systemone` rather than OpenAI `/embeddings` or HF TEI.** Embeddings return vectors,
  and TEI's classifiers have labels fixed at training time. Per-request typed options are the
  whole point. TEI `/rerank` stays a valid alternative backend for the context-rerank site only.

## What this costs, stated plainly

- **A second runtime.** On Apple machines tm must supervise a Python 3.11+ sidecar (environment,
  pins, crashes, about 1GB of RAM on an 8GB machine that already runs Docker). The pure-Rust
  answer means porting Laya's decision head ourselves, using candle's existing `modernbert`
  module or `mlx-rs`. That is not done.
- **No zero-shot local win.** Laya needs weeks of shadow labels and a fine-tune before it helps.
  The zero-shot path is Jev: closed, waitlisted, off-box, and too new to trust its prices or
  terms to stay put.
- **The fine-tune may not load on MLX.** laya-mlx documents only the three published
  checkpoints. Whether it can load or convert a tm fine-tuned Laya is unverified. If it can't,
  local fine-tuned inference waits for a candle port, or runs through PyTorch `laya` or Modal.
- **Small context and few options** (512 to 1024 tokens, weak beyond about 10 options). Every
  site needs deterministic chunking or pre-filtering, which is design work per site.
- **SPEC §0 gains an inference site.** It is contained by the event and replay rule, but it is a
  real amendment, and the hygiene and "no model in pure code" discipline must police it.
- **Wrong fast answers are still wrong.** "Zero hallucination" means schema-valid, not correct.
  Hence tighten-only authority, shadow first, and fitted thresholds that must be refitted as
  harness epochs change.
- **Unofficial provenance.** laya-mlx and `aac6fef/*` weights are a third-party port. Revisions
  and hashes must be pinned, and we must keep the ability to convert from
  `convaiinnovations/*` ourselves.
- **The token claim is narrower than it sounds.** Trimming tool schemas saves context-window
  space, but Anthropic prompt caching already makes those tokens cheap in dollars. The 175k-token
  turn in `docs/backlog.md` was mostly an editing bug. The large savings are unmeasured until the
  shadow slice runs.

## Consequences

- **First slice:** the trait, the mock, `Role::Decider`, the `classify.decided` event, a laya-mlx
  stdio sidecar provider, and shadow calls at turn start and step start. Plus a `tm bench`
  decision task scored against `Session.promote` and `StepRecord`. Nothing acts on the decisions.
- `Role::ALL` grows from 12 to 13. Run `mise run check-drift` after merging.
- New CLI verbs (`tm model install`, `tm decide export-labels`) get CLAUDE.md lines when they
  land, not before.
