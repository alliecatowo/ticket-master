# Vision addendum: System One decisions (Jev, Laya, laya-mlx)

> **Status:** proposed, 2026-09-23. Companion to
> `docs/decisions/D-020-system-one-decision-providers.md`. This is an addendum to
> `docs/vision.md`, not a replacement, and nothing in it is implemented yet. Claims are tagged
> throughout: **[verified]** means checked against a primary source (URL at the bottom),
> **[vendor]** means a number the vendor or model author published and nobody has independently
> reproduced, **[inference]** means our own reasoning from verified facts, and **[speculative]**
> means a bet.

## 0. First, what "jev / laya mlx" actually is

The owner's shorthand is "jev from typesafe, an MLX model". That covers two different things,
and the design depends on keeping them apart.

| Name | What it is | Weights | Runs on MLX? | Confidence |
|---|---|---|---|---|
| **Jev** (TypeSafe AI, SF) | TypeSafe's closed "System One" decision model, released 2026-09-15. Hosted API only. | Closed. Architecture and parameter count undisclosed. | **No.** It is a hosted API. | High [verified: typesafe.ai, MarkTechPost, Cloudflare, Pydantic docs] |
| **Laya** (Convai Innovations) | An open reproduction of the Jev *pattern*, released about three days later. A ModernBERT-large encoder plus a small decision head. | Apache-2.0, `convaiinnovations/laya` on HF, 421M params, about 808MB | Via the port below | High [verified: HF model card] |
| **laya-mlx** (`aac6fef`, repo `mizorewww/laya-mlx`) | An independent native MLX FP16 port of Laya, released 2026-09-19. Not an official Convai release. | Apache-2.0, `aac6fef/laya-mlx` and siblings on HF | **Yes.** This is "the MLX model". | High [verified: PyPI, GitHub README] |
| **jevmlx** (`bnsd55/jevmlx`) | A different approach: it scores constrained options from the logits of a normal MLX LLM (Qwen2.5 3B/7B, 4-bit) in one prefill, and serves a Jev-compatible `/v1/systemone` HTTP endpoint. | MIT (code); weights are Qwen's | Yes | High [verified: GitHub README] |

In short: **Jev is the hosted product, and Laya running through laya-mlx is the local, free,
open-weights path.** They share one contract, "state plus typed questions in, calibrated typed
answers out", so this addendum designs around the contract and treats both as interchangeable
backends. That is also what the owner asked for separately: a new kind of provider, local or
remote, in §4.

This is **not** Meta's VL-JEPA or LeJEPA, and it is **not** an embedding model or a text generator.

## 1. The idea

`docs/vision.md` says: *inference resolves uncertainty; software acts on the resolution.* Today
tm has only two tools for resolving uncertainty. Pure software can't handle any of it, and a
frontier LLM costs thousands of tokens, seconds of latency and a full agent turn every time.

Most of the uncertainty inside tm is **bounded**. Is this chat message a question, a command or a
job? Which of six ticket kinds is this? Is this shell command destructive? Did the page change the
way the click intended? Which of eleven tool families does this step need? Every one of those is
"pick from a known answer space", yet we currently answer each by sending a frontier model an
18KB tool-schema block and a context pack, and hoping it picks well.

System One models are a **third tier of intelligence**, sitting between deterministic software
and the LLM. They are typed, calibrated and fast, and by construction they cannot hallucinate
outside the schema: a Choice answer is always one of the offered options. That fits tm's thesis
exactly. The model resolves bounded uncertainty into a **typed value with a probability**. That
value becomes an **event**. **Pure code** acts on the event. When the probability is too low to
act on, the **cascade** hands off to the LLM or a human, with the machine's best guess attached.

The radical version: in steady state, **the frontier LLM stops being the router of its own
work**. The LLM writes code and handles genuine ambiguity. Everything bounded (routing, triage,
tool gating, perception checks, pre-screening) goes through a model that is 100 to 1000 times
cheaper, sits on the laptop, and answers in tens of milliseconds.

## 2. Verified facts, then inferences

### What the models take in and put out [verified]

- **Input:** a `state` (a string or JSON object) and a map of named `questions`. Text only.
  Pydantic's docs say plainly that Jev can't take image, audio, video or document inputs. Laya is
  a text encoder.
- **Three question types:**
  - `choice`: pick one of N options. Jev allows up to 255.
  - `score`: an ordinal rubric of 2 to 10 levels, returning a probability per level and an
    expected value.
  - `noul`: P(statement is true).
- **Output:** each answer carries the typed value, a probability per option, and a `confidence`
  from 0 to 1. The models never generate text.
- **Training:** both train with "Reinforcement Learning for Calibrated Decisions" (RLCD), a policy
  rewarded by strictly proper scoring rules, so honest probabilities are the optimal report.
  Laya's card describes this recipe; TypeSafe names RLCD.

### Jev (hosted) [verified unless marked]

- Endpoint: `POST https://api.typesafe.ai/v1/systemone`, with `TYPESAFE_API_KEY` as a bearer
  key. It is also on Cloudflare Workers AI (model page `typesafe/jev`), in Pydantic AI
  (`typesafe:jev-latest`), and in LangChain.
- Price: **$0.042 per million input tokens, and output is free.**
- Latency: 70 to 500ms end to end [vendor]. Rate limits are 250k tokens/s and 1,200 requests/min
  [vendor, via dev.to].
- Context: 32k tokens per the Cloudflare page. Another write-up says 64k combined, with a 32k
  per-question maximum.
- Availability: **early access behind a waitlist.** No free tier has been announced.
- Speed claims: "193.6x faster, 444.6x cheaper" than LLMs on System One tasks [vendor-run, not
  reproduced].
- Weaknesses Pydantic lists: arithmetic, counting, dates, multi-hop reasoning, free-text output,
  and more than 255 options.

### Laya and laya-mlx (local, open) [verified unless marked]

| | `laya` (English) | `laya-multilingual` | `laya-typed-decisions` |
|---|---|---|---|
| Encoder | ModernBERT-large | mmBERT-base | ModernBERT-large |
| Params | 421M | 322M | 421M |
| Context | **512 tokens** | 1024 (RoPE, up to 8k) | 1024 |
| Download | about 808MB | about 647MB | similar to root |
| laya-mlx P50, one short question, M3 Max | 13.4ms | 7.4ms | n/a |
| laya-mlx throughput, 50 questions | 147 q/s | 395 q/s | n/a |
| laya-mlx peak memory | **944 MiB** | **688 MiB** | n/a |

- **Zero-shot is weak.** The base checkpoints score **0.362** on the typed-decisions benchmark,
  near chance. After fine-tuning on 1,200 cases (6,000 decisions), the typed-decisions checkpoint
  scores **0.766**, against Jev's 0.727 on the same benchmark [author-run].
- **Many options collapse it.** Laya scores 0.425 on Banking77 (77 labels) against Jev's 0.870,
  because every option shares a fixed `head_max_len` of 192 to 256 tokens [author-run].
- **Calibration:** ECE is 0.081 after temperature scaling, against Jev's 0.144 raw
  [author-run]. Ordinal (`score`) questions are "the weakest primitive". The card says the
  act/escalate head's `act_probability` "carries no usable signal yet".
- **Fine-tuning is free and reproducible.** The model card links a Kaggle notebook,
  `laya_finetune_typed_decisions_2xT4_kaggle.ipynb`, that takes about 4 to 5 hours on Kaggle's
  free 2xT4.
- **laya-mlx** needs Python 3.11 or later and has no PyTorch dependency. It offers a Python API
  and a `laya-mlx predict` CLI, **with no server mode**, and supports batching, compile, prompt
  caching and fitted temperature. Every published benchmark is from an M3 Max with 128GB. There
  are **no M1/M2 or 8GB numbers**. The "50x faster than Jev" headline is unverified, per
  explainx.ai's fact-check.

### Inferences we are relying on

- **[inference]** On the owner's 8GB M-series Mac, laya-mlx fits: the multilingual model needs
  about 0.7GB and the English one about 0.95GB, beside Docker's 4GB. That is tight but plausible,
  and unified memory means there is no separate VRAM to budget. Latency on a base M1/M2 GPU will
  be several times the M3 Max figures, a guess of 30 to 80ms per short question. **Measure this
  before designing any hot path around it.**
- **[inference]** jevmlx's `quality` model (Qwen2.5-7B 4-bit, 4.5GB) doesn't fit beside Docker
  on 8GB, and the 3B model (2GB) is marginal. jevmlx is a zero-shot local fallback for bigger
  Macs, and a reference implementation of the wire contract. It isn't the default.
- **[inference]** Out of the box, Laya is useful **only after fine-tuning on tm's own labels**.
  Hosted Jev is the zero-shot path. So the realistic sequence is: collect labels in shadow mode,
  fine-tune Laya for free, then run locally.

## 3. The cascade: how a decision is made, recorded and trusted

Every use in §5 has the same shape:

```
state (text/JSON, deterministically assembled, redacted)
   │
   ▼
Decider (a DecisionProvider routed by the fabric: laya-local | jev | modal | mock)
   │   typed answers + per-option probabilities + confidence
   ▼
Calibration (per-site temperature and thresholds, fitted by `tm bench` from labels)
   │
   ├── confidence ≥ act_threshold      → deterministic action (pure code consumes the answer)
   ├── review band                     → LLM fallback, given the decider's top-k as a hint
   └── below abstain_threshold / error → the existing path, as if the decider did not exist
   ▼
event `classify.decided` { site, backend, model, model_revision, input_hash, questions_hash,
                           answers, calibrated_confidence, thresholds, disposition, latency_ms,
                           cost_micros }
```

Five invariants make this compatible with SPEC §0:

1. **A decision is an event, and replay reads the event, never the model.** The pure functions
   that consume a decision (scheduler planning, routing, pruning) take a
   `Option<&Classification>` argument read from the log. They never call the decider. MLX FP16
   isn't bit-reproducible across machines, which is one more reason replay must not re-infer.
   (`decision.*` is already taken by the `DecisionId` domain events, hence `classify.*`.)
2. **Tighten only, where authority is involved.** A decider may escalate `Allow` to
   `NeedsApproval`, exactly as `Oversight::review` does (D-009), but it can never soften a `Deny`
   and can never grant authority. `Authority::permits` stays pure and unchanged.
3. **The decider chooses the *need*; `Fabric::route` still chooses the candidate.** Routing under
   quota and health stays pure (SPEC §0). The decider only supplies the `Role` or `Need` that
   routing consumes.
4. **Abstention is always safe.** Every site's fallback is today's behavior, so a missing
   sidecar, an open breaker or low confidence can only cost the savings. It can never cost
   correctness.
5. **No network and no model in tests.** A `MockDecisionProvider` keyed by request hash, shaped
   like `MockProvider`, backs every workspace test.

**Shadow mode is the default for every new site.** The decider runs and records
`classify.decided { disposition: "shadow" }`, but nothing acts on it. `tm bench` scores the
shadow decisions against what actually happened. A site moves to acting only when its bench
numbers clear its gate (§7).

## 4. A new kind of provider: the decision fabric

This is the centerpiece. Today `tm_provider::Provider` (`crates/tm-provider/src/fabric.rs`)
requires `complete` and `embed`. A decider does neither, so it gets **its own trait next to
`Provider`**, and the rest of the fabric is reused as is: role routing, circuit breakers, quota,
the price ledger, redaction and events.

```rust
/// A backend that answers typed questions about a state with calibrated probabilities.
#[async_trait]
pub trait DecisionProvider: Send + Sync {
    fn id(&self) -> &str;                          // "laya-local", "typesafe", "modal-laya", "mock"
    fn limits(&self) -> DecideLimits;              // max_context_tokens, max_options, kinds supported
    async fn decide(&self, req: DecideRequest) -> Result<DecideResponse, ProviderError>;
}

pub struct DecideRequest {
    pub model: ModelId,
    pub state: StateDoc,                            // text or JSON, already redacted
    pub questions: BTreeMap<QuestionId, Question>,  // BTreeMap: stable order, stable hash
}
pub enum Question {
    Choice { instructions: String, options: Vec<OptionSpec> },
    Score  { instructions: String, levels: Vec<LevelSpec> },
    Noul   { statement: String },
}
pub struct Answer { pub value: AnswerValue, pub probabilities: Vec<f32>, pub confidence: f32 }
```

- **Role.** Add `Role::Decider` (`decider`) beside `Role::Embedder`, the precedent for a role
  that isn't a chat model. `Role::ALL` goes from 12 to 13, so run `mise run check-drift` after
  merging.
- **Routing uses `limits()`.** A 3,000-token state or an 80-option choice doesn't fit Laya's
  512-token, few-option budget. Candidates whose `limits()` can't hold the request are skipped,
  just as unhealthy ones are today. Under `Need.tolerance = Any` that falls through to hosted Jev.
  Under `Strict` (for example "never send this off-box") it abstains.
- **Redaction.** `redact_completion_request` only covers `CompletionRequest`. A decide request
  sends new data off the machine (state and question text), so it needs its own
  `redact_decide_request` through the same `SessionRedactor` (D-011). **This blocks every remote
  backend.**
- **`providers.toml`** gains the role, with local first for privacy and cost:

```toml
[[role.decider.candidates]]
provider = "laya-local"                  # bundled MLX sidecar
model    = "aac6fef/laya-multilingual-mlx@<pinned-revision>"
max_concurrency = 4

[[role.decider.candidates]]
provider = "typesafe"                    # hosted Jev
model    = "jev-latest"
max_concurrency = 8
degraded_ok = true
price = { input_per_mtok_micros = 42_000, output_per_mtok_micros = 0 }
```

### Three backend families, one contract

1. **Local native (bundled).** The laya-mlx sidecar.
   - "Bundled" realistically means a new verb, `tm model install laya`. It would:
     1. create an isolated Python 3.11+ environment under tm's data directory (via `uv`, not the
        user's Python);
     2. `pip install` a **pinned** `laya-mlx` version;
     3. fetch a **pinned HF revision** of the weights and verify its sha256;
     4. drop in a roughly 100-line `tm-decider` wrapper, because laya-mlx has no server mode.
   - tm spawns the wrapper as a child process speaking **JSON Lines over stdio**. The wire body is
     identical to the HTTP contract below. The sidecar dies with tm, needs no port and no auth,
     and runs no daemon.
   - Weights: 650 to 810MB. Python environment: a few hundred MB.
   - Later, the **native Rust** path removes Python entirely (§9).
2. **Self-hosted remote.** For example Laya on **Modal**, serving the HTTP contract. It suits
   *background* work (ticket triage, dedupe, nightly relabeling), where a cold start doesn't
   hurt. It doesn't suit the per-keystroke chat hot path: cold start plus loading about 800MB of
   weights is unmeasured and certainly not "1 to 2 seconds". Cost is in §9.
3. **Hosted API.** TypeSafe's Jev directly, Jev through Cloudflare Workers AI (tm already has
   `crates/tm-provider/src/providers/cloudflare.rs` and its `CF_API_TOKEN`/`CF_ACCOUNT_ID`
   plumbing), Vercel's gateway, or *anyone's* endpoint that speaks the contract, including a tm
   one (§8).

### The wire contract: adopt `/v1/systemone`, don't invent one

Of the existing standards, TypeSafe's own shape is the one that fits:

```
POST /v1/systemone
{ "model": "jev-latest",
  "state": "<string or JSON>",
  "questions": { "<id>": { "type": "choice" | "score" | "noul",
                           "instructions": "...", "criteria": [...] } } }
→ { "model": "jev-1.13.0",
    "answers": { "<id>": { "choice"|"score"|"noul": ..., "probabilities": [...], "confidence": 0.93 } },
    "usage": { "input_tokens": ..., "output_tokens": 0 } }
```

- Top-level field names are **[verified]** from Cloudflare's and Pydantic's docs. The exact
  per-question option field name (`criteria` versus `options`) is **not pinned**. Confirm it
  against a real response before building.
- jevmlx already serves `/v1/systemone` locally, so a local server, Modal, Jev and a future tm
  endpoint can all be swapped by changing a base URL.
- tm adds only an optional `x-tm-request-id` header for event correlation.
- **Why not the alternatives.** OpenAI `/embeddings` returns vectors, not decisions. HF TEI's
  `/predict` serves classifier heads whose labels are **fixed at training time**, while we need
  per-request option sets. TEI's `/rerank` *does* fit one site, context reranking (§5.6), so a
  cross-encoder behind TEI stays a legitimate alternative backend for that site alone.

## 5. The uses, ranked by leverage

Leverage = (calls per turn) × (tokens or turns saved) × (fits Laya's budget) × (labels available
today). Each entry gives: input, then questions, then decision; the seam; which backends fit; what
it saves; and how proven it is.

### 5.1 Chat intent routing (the highest leverage)

- **Input:** the user's chat message, plus a roughly 300-token deterministic digest (open ticket
  counts, the last turn's outcome, the current mode).
- **Questions:** `choice intent ∈ {answer_from_state, deterministic_command, start_ticket,
  converse, needs_frontier}`, plus `noul "the user asked to change files"`.
- **Decision:**
  - `answer_from_state` or `deterministic_command` with high confidence: run the matching pure
    handler (the `tm tickets --json` or `tm events` style reads, `/model`, `/compact`). **Zero
    LLM turns.**
  - `start_ticket`: pre-fill a ticket draft for the human to confirm. D-017 still holds:
    **chatting never creates a ticket on its own**, and the decider only proposes.
  - Everything else: a normal agent turn, now told the likely intent.
- **Seam:** `crates/tm-agent/src/session.rs` before the turn starts, and the TUI's send path.
- **Fits:** Laya once fine-tuned, since a message plus digest fits in 512 tokens. Jev zero-shot.
- **Saves:** a whole turn for every trivially answerable message. Commit `8a4b586` already cut a
  ticketless turn's context pack to 3k tokens, but schemas add about 4.5k tokens and output is
  extra, so roughly 8k tokens and 2 to 10 seconds per avoided turn **[inference]**.
- **Labels exist today:**
  - `Session.promote` already detects turns that ended in `ticket.create_child` or
    `ticket.delegate`.
  - A turn with zero tool calls means "answer".
  - A turn whose only calls were read-only means "deterministic candidate".
- **Status:** mechanics **[inference]**; savings **[speculative]** until shadow-measured.

### 5.2 Tool-set selection per step

- **Input:** the objective, the last step's tool results (truncated), and a deterministic "phase"
  hint.
- **Questions:** one `noul` per **tool family**, not one 71-way choice: "this step needs editing",
  "...needs the browser", "...needs ticket ops", "...needs shell", and so on for about 11
  families. That keeps Laya inside its few-options regime.
- **Decision:** send only the core set plus the families above threshold. An always-present
  `tools.expand(family)` meta-tool is the abstention valve: if the model reaches for a missing
  family it costs one cheap round trip, never a failure. Low confidence sends the full set, as
  today.
- **Seam:** `crates/tm-agent/src/agent_loop.rs`, where tool definitions are attached per step.
  This is also the capability-registration path the audit's A-01 recommends.
- **Fits:** Laya, with per-family `noul` questions. A single 71-option choice is **Jev-only**.
- **Saves:** the backlog measured about 18KB, roughly 4.5k tokens, of schemas on *every* step.
  Trimming to about 25 tools saves around 3k tokens per step, so about 90k input tokens over a
  30-step ticket **[inference]**.
- **Stated plainly:**
  - This is **not** what caused the 175k-token turn. `docs/backlog.md` attributes that mostly to
    edit-hash flailing.
  - Anthropic prompt caching already makes repeated schemas cheap in *dollars*. The win here is
    *context-window* headroom and fewer tool-choice mistakes, and dollars only on uncached
    providers.
- **Labels exist today:** every `StepRecord` records which tools the step actually called.

### 5.3 Ticket triage and routing

- **Input:** the objective, success criteria, the touched-path hints, and the parent's kind.
- **Questions:**
  - `choice kind ∈ TicketKind` (6 options);
  - `choice role ∈ {coder.fast, coder.deep, explorer.cheap, planner.frontier}`;
  - `choice executor_tier`;
  - `score priority` (4 levels);
  - `noul "needs decomposition before implementation"`;
  - `noul "duplicates candidate T-x"`, asked per candidate from a deterministic top-5 retrieved
    by `tm-codeintel` hybrid search over open tickets.
- **Decision:** fill defaults on `tm ticket new`, `ticket.create_child` and `POST /tickets`. A
  high-confidence duplicate becomes a *suggested* link or cancel for a human. Decomposition
  routes the ticket to planning first.
- **Seam:** `crates/tm-core` ticket creation, `crates/tm-scheduler/src/select.rs`, and the
  `Need` handed to `Fabric::route`.
- **Fits:** Laya, because an objective fits in 512 tokens. Dedupe is a batch of `noul` pairs.
- **Saves:** frontier spend on work that `coder.fast` could have done. It also saves the retry
  or escalation cycles caused by under-provisioned tickets (every escalation is a wasted
  attempt).
- **Labels exist today:** the final state, the attempt count, which role finished the ticket,
  verification failures, escalations and human retries.

### 5.4 Authority and risk pre-screening of tool calls (tighten only)

- **Input:** the tool name, the arguments (redacted), the ticket's authority summary, and the
  cwd.
- **Questions:**
  - `noul "irreversible or destructive"`;
  - `noul "reaches outside the ticket's declared scope"`;
  - `noul "exfiltrates data off the machine"`;
  - `score risk` (4 levels).
- **Decision:** above threshold, turn `Allow` into `NeedsApproval` and attach the decider's
  reason to `approval.requested`. The decider never allows anything, and `Authority::permits`
  runs first and alone decides `Deny`.
- **Seam:** `AgentLoop::drive`, right beside `Oversight::review` (D-009).
- **Fits:** Laya. This is LangChain's published "AutoMode" pattern with Jev.
- **Saves:** makes a looser static `oversight.toml` safe, so fewer blanket approvals are needed
  and fewer humans get interrupted. It is not a token saving; it is autonomy.
- **Labels exist today:** `approval.decided`, i.e. a human's approve or deny. These are
  **gold-standard labels**.

### 5.5 Verification pre-screening

- **Input:** the diff summary (per file, chunked to fit), the success criteria, and the test
  output tail.
- **Questions:** one `noul` per criterion ("the diff plausibly satisfies X"), plus `noul "tests
  failed for a reason unrelated to the change"`.
- **Decision:** a confident "fails criterion X" bounces the ticket back to rework *before* paying
  for the semantic reviewer or auditor, with X named. A pass never skips the judgment predicate.
  It only changes the order in which work is done.
- **Seam:** the verification ladder (SPEC §18.5) before `reviewer.semantic` or
  `auditor.semantic` runs.
- **Fits:** Jev for large diffs. Laya only on per-file chunks.
- **Saves:** frontier review calls on submissions that are obviously incomplete.
- **Status:** **[speculative].** Calibration on diffs is untested.

### 5.6 Context selection, reranking and compaction (select and drop, never summarize)

- **Input:** the objective, plus one candidate chunk at a time (a pack section, a search hit, or
  an old tool result).
- **Questions:** `noul "needed to make progress on the objective"`, batched across candidates.
  laya-mlx runs about 147 to 395 questions per second on an M3 Max.
- **Decision:** reorder or drop candidates inside `TokenBudget`. For compaction, *drop* the tool
  results the decider marks as dead, on top of `pruning.rs`'s deterministic staleness rules.
  **Neither Jev nor Laya can write a summary**, so summarizing stays with `summarizer.cheap`.
- **Seam:** `crates/tm-context` (pack assembly and `TokenBudget`) and
  `crates/tm-codeintel/src/hybrid.rs` (rerank stage).
- **Fits:** both. TEI `/rerank` with a cross-encoder is an alternative backend for this site.
- **Saves:** smaller context packs and fewer "read five more files" steps.
- **Labels:** noisy. Did the agent later read or edit a path that was in the pack?

### 5.7 Browser and computer-use perception (text trees, not pixels)

The models are text-only, which suits tm: SPEC §19.2 already makes the **accessibility tree, not
pixels**, the agent-facing surface. `tm-browser`'s `Snapshot::render_text` and `Snapshot::diff`
exist, and `tm-computer`'s `ComputerBackend::element_tree` walks macOS `AXUIElement`s.

- **"Did the UI change as expected"**
  - Input: the intended effect plus `Snapshot::diff`.
  - Question: `noul "the diff shows the intended effect"`, plus
    `choice {success, error_shown, nothing_happened, navigated_elsewhere, modal_blocking}`.
  - Decision: on success the agent simply continues. It never has to *look*, and it only sees the
    page again on failure.
- **Grounding** ("which ref is the Login button")
  - Deterministic role and name filtering narrows the candidates to 10 or fewer, then one
    `choice` over them.
  - A full-page choice over 255 refs is **Jev-only**.
- **State classification:** `choice page_state ∈ {logged_out, form, results, error, captcha,
  loading}` drives a deterministic playbook without an LLM step.
- **Screenshot-only surfaces are out of scope.** No image input exists.
- **Saves:** most of the perception turns in a browser task, which are **[speculative]** but
  plausibly the biggest multiplier in browser and computer use, where each step normally
  re-reads a large tree.

### 5.8 A fast local perception subagent tool, `perceive.ask`

A tool the **LLM** calls:

```
perceive.ask { source: "shell:last" | "browser:snapshot" | "file:<path>" | "diff" | "events:T-12",
               questions: { ... typed ... } }
```

- tm assembles the state deterministically from the named source, redacts it, and chunks it to
  fit the backend. The LLM gets back **typed answers and confidences, not the 40KB log**.
  - "Did any test fail? Choice: which crate?"
  - "Is the dev server up?"
  - "Does this log contain a panic?"
- **Decision:** the LLM decides what to read next. The decider replaces the reading itself.
- **Seam:** a new tool in `crates/tm-agent/src/tools.rs`, whose read-only authority class goes
  through the same capability registration.
- **Saves:** the largest tool-output tokens that enter the context, which are the ones
  `pruning.rs` later throws away anyway **[speculative, high ceiling]**.

### 5.9 Scheduler admission (advisory input, pure gate)

- **Input:** a ticket's triage record from §5.3.
- **Questions:** `score expected_verification_load`, `noul "likely to need human judgment"`.
- **Decision:** the result becomes a *recorded* weight read by the pure `AdmissionGate` (SPEC
  §21.4). A likely-to-need-review ticket counts more against `max_unverified_tickets`. The gate
  stays a pure function of `SchedulerView` plus recorded classifications.
- **Seam:** `crates/tm-scheduler/src/admission.rs`.
- **Status:** **[speculative]**, low volume.

### 5.10 Recovery triage

- **Input:** the last attempt's failure tail and the ticket.
- **Question:** `choice cause ∈ {flaky_env, missing_context, wrong_approach, spec_ambiguous,
  out_of_authority}`.
- **Decision:** a deterministic retry policy per cause: re-run, retry with a different context
  pack, escalate to planning, or escalate to a human with the cause named. Only the
  recovery-diagnosis inference site (SPEC §0) stays on the LLM, for the residue.

## 6. Labels come from our own event log

The event log is a labeling machine that is already running. `tm decide export-labels` (a
proposed verb) joins `classify.decided` events with the outcome events below and writes frozen
JSONL sets under `bench/decisions/`.

| Site | Label source (events that already exist) | Quality |
|---|---|---|
| Chat intent | `Session.promote` ticket detection; the tool calls per turn | good |
| Tool families | `StepRecord` tool names per step | good, abundant |
| Triage (kind, role, tier) | ticket end state, attempts, finishing role, escalations | good, slow to accrue |
| Risk | `approval.requested` → `approval.decided` | gold, sparse |
| Verification pre-screen | verification outcomes per predicate | good |
| Dedupe | human cancels whose reason names a duplicate | sparse |
| Context selection | later reads or edits of in-pack paths | noisy |

These sets are what the free Kaggle fine-tune (§9) trains on: a tm-specific Laya.

## 7. Evaluation through `tm bench`

- `tm bench` gains a decision-eval task type that replays a frozen label set through any
  `DecisionProvider`. It is deterministic under `MockDecisionProvider`, and live under `--live`.
- **Per-site metrics:**
  - accuracy;
  - **ECE** (calibration);
  - coverage at the act threshold (the share of calls acted on);
  - accuracy *within* coverage;
  - abstention rate;
  - fallback tokens saved;
  - p50/p95 latency.
- **Gate for leaving shadow mode** (per site, recorded in the harness epoch, SPEC §10):
  within-coverage accuracy ≥ the site's floor, ECE ≤ 0.1, and a `tm bench compare` showing no
  regression in end-to-end task success. Thresholds are fitted, not guessed.

## 8. Horizon: the tm decision API that others rely on [speculative]

- **`tm serve` exposes `/v1/systemone`**, backed by the decision fabric: any tool, editor plugin
  or agent can ask tm's routed decider typed questions. The web client and VS Code extension get
  local classification for free.
- **A hosted "tm decider"** would run a *tm-fine-tuned Laya*, available to other harnesses.
  - **License.** Laya is Apache-2.0 and may be served. Reselling **Jev** almost certainly
    conflicts with TypeSafe's terms (unverified; assume no).
  - **Provenance.** `aac6fef/*` is an unofficial port. Pin revisions and hashes, or convert from
    `convaiinnovations/*` ourselves before serving anyone else.
  - **Abuse.** Per-key auth and quotas (the fabric's `Limits`), a hard payload cap (Laya's 512 to
    1024 tokens caps it naturally), no persistence of `state` by default, redaction, and
    Cloudflare in front for rate limiting.
  - **Cost.** A CPU Modal container costs about $0.13 per warm hour (§9). At Laya's 200 to 460ms
    per decision on CPU, that is roughly $0.000007 to $0.000016 per decision while warm.
    Idle warm time dominates.
- **Beyond routing.** A decider-first agent loop, where every step begins with a free
  sub-10ms "what kind of step is this" and the frontier model is invoked only for generation.
  That is the "System One and System Two" split applied to tm's own loop.

## 9. Running it free or cheap

### Runtime paths from Rust

| Path | What | Pros | Cons | When |
|---|---|---|---|---|
| **A. laya-mlx Python sidecar** over stdio JSON Lines | the published port, used as is | works this week, fastest verified numbers | Python 3.11+ runtime and environment to manage, Apple-only; **unverified that laya-mlx can load or convert our own fine-tuned checkpoint** (it documents only the three published ones) | **first** |
| **B. Swift MLX sidecar** | port ModernBERT and the head to MLX Swift | one signed binary, no Python | a real port; no one has done it | only if Python hurts |
| **C. `mlx-rs`** (unofficial MLX bindings through mlx-c, v0.25) | in-process MLX from Rust | no sidecar | we write the model port; bindings are pre-1.0 | later |
| **D. candle**, whose `candle_transformers::models::modernbert` exists | pure Rust, Metal on Mac, **CPU on Linux and CI** | one static binary, works off-Apple | we port the decision head (two transformer layers, option-marker scorer) ourselves | the **native** end state |
| **E. Hosted** (Jev, Cloudflare, Modal) | HTTP | zero install | network, cost, redaction required | zero-shot and background work |

- **Non-Apple machines and CI.** Tests always use `MockDecisionProvider`. A Linux developer uses
  path D (CPU), the PyTorch `laya` package, or hosted Jev.
- **Download sizes:** Laya English about 808MB, multilingual about 647MB, the laya-mlx package
  117kB, and the Python environment about 200 to 400MB **[inference]**.

### Free and cheap options, with the arithmetic

- **Local laya-mlx: $0.** It costs about 1GB of RAM while loaded. Load it lazily and unload it
  when idle on the 8GB machine.
- **Hosted Jev: $0.042 per million input tokens.** A 1.5k-token triage call costs about $0.00006.
  10,000 decisions a day is about $0.60 a day. It is effectively free, but **waitlisted**.
- **Cloudflare Workers AI** lists Jev. Workers AI's free allowance is 10,000 neurons a day, then
  $0.011 per 1,000 neurons. **Whether Jev on Cloudflare draws from the neuron pool, and how many
  neurons a call costs, is unverified.** Pricing is "in the dashboard".
- **Modal: $30 a month in free credits** on the Starter plan.
  - Rates: T4 $0.000164/s, CPU $0.0000131 per core-second, memory $0.00000222 per GiB-second.
  - A **CPU-only** container with 2 cores and 4GiB costs about $0.12 an hour, so $30 buys about
    **240 warm hours a month**. That is enough for background triage with scale-to-zero.
  - A warm T4 costs about $0.75 an hour with CPU and memory, which blows through $30 in about 40
    hours. Use a T4 only for batch relabeling, not always-on serving.
  - Cold start plus weight load is **unmeasured**.
- **Kaggle: free 2xT4** for the fine-tune. The published recipe takes 4 to 5 hours.
- **Hugging Face:** host the fine-tuned weights free, private if preferred.

## 10. What this costs, stated plainly

- **A second model runtime.** Path A means tm now supervises a Python process on Apple machines:
  environment management, version pinning, crashes and memory. On an 8GB Mac that is about 1GB of
  RAM competing with Docker and `cargo -j 2`.
- **Laya is not useful out of the box.** It is near chance zero-shot. Every local win waits on
  labels (weeks of shadow mode) and a fine-tune. Until then the only zero-shot path is Jev, which
  is waitlisted, closed, off-box, and new enough that its prices and terms may change.
- **The fine-tune may not run where we want it.** The local plan is shadow labels, then a
  Kaggle fine-tune, then running it through laya-mlx. laya-mlx documents only the three published
  checkpoints. If it can't load or convert our fine-tuned one, the local win waits for the candle
  port (path D), or we run the fine-tune through PyTorch `laya` or Modal instead. Check this
  before collecting weeks of labels on the assumption.
- **Small context and few options.** 512 tokens and weak many-option accuracy force deterministic
  pre-chunking and pre-filtering at every site. That is design work at each site, not a drop-in.
- **Calibration is a claim until we measure it.** Every "high confidence" threshold is fitted per
  site, and drifts with harness epochs.
- **Wrong fast answers are still wrong.** "Zero hallucination" means schema-valid, not correct
  (TypeSafe's own words). A confident mis-route costs a wasted attempt, which is why
  authority-touching sites are tighten-only and everything starts in shadow.
- **SPEC §0 grows a new inference site.** That is a real philosophical amendment, contained by
  "decisions are events, replay never re-infers, pure code consumes recorded values".
- **Unofficial provenance.** laya-mlx and its weights are an independent port and could vanish or
  change. Pin revisions and hashes, and keep the convert-it-ourselves path open.
- **The token win is narrower than the pitch.** Schema trimming saves context, and dollars only
  where caching doesn't already. The 175k-token turn was mostly an editing bug. The big savings
  (§5.1, §5.7, §5.8) are **unmeasured**.
- **More surface to test.** A new trait, a role, events, bench tasks, a sidecar supervisor and a
  CLI verb.

## 11. The smallest first slice

**Shadow-mode chat intent plus tool-family selection, labels only, with no behavior change.**

1. Add `DecisionProvider`, `MockDecisionProvider` and `Role::Decider` in `tm-provider`, with a
   test that none of it touches the network.
2. Add the `classify.decided` event.
3. Add a laya-mlx stdio sidecar provider, built on demand and never in CI, and optionally a Jev
   HTTP provider behind `redact_decide_request`.
4. At two sites (turn start and step start) call the decider in shadow and record the answers.
   Nothing acts on them.
5. Add a `tm bench` decision task that scores shadow answers against `Session.promote` and
   `StepRecord` outcomes.

That produces the label corpus, the calibration data and a real latency number on the 8GB Mac
at zero risk. Every later slice turns a site from shadow to acting once its bench gate passes.

## 12. What the owner needs to provide or do

- [ ] **Decide the zero-shot path.** Join the TypeSafe waitlist at typesafe.ai; once admitted,
      create a key at `console.typesafe.ai`, stored as `TYPESAFE_API_KEY` in `.env`, never in
      the repo. Optional: shadow mode works with local Laya alone, just less accurately at first.
- [ ] **Accept the licenses.** Laya weights are Apache-2.0 (no gate). Read TypeSafe's terms
      before sending any project data to Jev, and before any idea of reselling it.
- [ ] **Install locally:** Python 3.11+ via mise or `uv`, then `pip install laya-mlx`, or let the
      proposed `tm model install laya` do it. Budget about **1.5GB of disk** (weights plus
      environment) and about **1GB of RAM** while loaded.
- [ ] **Hugging Face:** an `HF_TOKEN` is optional for the public weights, and is needed only to
      push a private fine-tuned checkpoint.
- [ ] **Kaggle account** with phone verification to unlock the free GPUs, for the fine-tune.
- [ ] **Modal (optional, for remote or background use):** create an account (Starter, $30 a month
      in free credits), `pip install modal`, then `modal token new`. Set a spend limit in the
      dashboard.
- [ ] **Cloudflare (optional):** the existing `CF_API_TOKEN` and `CF_ACCOUNT_ID` probably work for
      Jev on Workers AI. Check the dashboard for Jev's actual neuron cost.
- [ ] **Say yes or no to D-020** and the SPEC §0 and §6.1 amendments it proposes.

## Sources

- TypeSafe home: https://typesafe.ai/
- Cloudflare model page (Jev): https://developers.cloudflare.com/ai/models/typesafe/jev/
- Pydantic AI, TypeSafe: https://pydantic.dev/docs/ai/models/typesafe/
- MarkTechPost launch coverage: https://www.marktechpost.com/2026/09/19/typesafe-ai-releases-jev/
- Jev API guide (dev.to): https://dev.to/valyuai/how-to-use-jev-a-practical-guide-to-typesafes-system-one-model-g5e
- LangChain, harness with Jev: https://www.langchain.com/blog/building-a-harness-with-jev
- Laya model card: https://huggingface.co/convaiinnovations/laya
- Laya typed-decisions card: https://huggingface.co/convaiinnovations/laya-typed-decisions
- laya-mlx on PyPI: https://pypi.org/project/laya-mlx/
- laya-mlx on GitHub: https://github.com/mizorewww/laya-mlx
- Claims check: https://explainx.ai/blog/laya-mlx-jev-alternative-on-device-mlx-2026
- jevmlx: https://github.com/bnsd55/jevmlx
- mlx-rs: https://github.com/oxiglade/mlx-rs
- candle ModernBERT: https://docs.rs/candle-transformers/latest/candle_transformers/models/modernbert/index.html
- Modal pricing: https://modal.com/pricing
- Cloudflare Workers AI pricing: https://developers.cloudflare.com/workers-ai/platform/pricing/
