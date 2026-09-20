# D-016 — A `SubscriptionOAuth` auth adapter over the real Codex CLI's ChatGPT session

**Status:** provisional — the code, its pure-logic tests, and the full verify gate are all real and
passing, but the live endpoint call this change exists to prove has not actually been executed
against the real backend from any session that built it (see "What this costs" for exactly why and
what running `mise run test:live-codex-auth` on an unsandboxed machine would need to confirm
before this graduates to `accepted`) · **Date:** 2026-09-20 · **Supersedes:** nothing

## Context

`crates/tm-auth` (built earlier this session) defines `AuthAdapter`/`Credential`/`Entitlement` and
ships three adapters: `EnvApiKey`, `KeychainApiKey` (shape only, not implemented), and
`DeviceCodeOAuth` (a real RFC 8628 device-code flow this workspace drives itself). All three either
read a bare secret or run their own OAuth dance. None of them cover the fourth real-world shape
`SPEC.md` §28.2's `CredentialKind::SubscriptionOAuth` variant names in passing: a credential another
already-authenticated program produced, that this workspace only ever *reads*.

This task's brief was to prove that shape end to end, concretely: read the real, already-logged-in
Codex CLI's stored ChatGPT-subscription OAuth session on this machine (`~/.codex/auth.json`, mode
`0600`, `codex login status` reporting "Logged in using ChatGPT"), expose it as a real
`AuthAdapter`, build a real `Provider` that uses it, and drive a real completion through this
workspace's own `Fabric`/`AgentLoop` — not `codex` itself acting as an external executor. The user
explicitly authorized reading and using this real local credential file for exactly this purpose.

Two things had to be established before writing any code:

1. **What is this token actually valid against?** A JWT's header/payload are base64url-encoded, not
   encrypted — only the signature segment is unforgeable — so decoding them locally to read `iss`/
   `aud`/`client_id`/`scp`/`exp` reveals the token's *shape* without bypassing or needing to verify
   its signature. Decoding this machine's real `tokens.access_token` (redacting PII-shaped claims —
   `sub`, the nested `profile.email`, account/org UUIDs — from every intermediate output this
   session produced, never printing the raw token itself anywhere) showed:
   - `iss: "https://auth.openai.com"`, `client_id: "app_EMoamEEZ73f0CkXaXp7hrann"`.
   - `aud: ["https://api.openai.com/v1"]`.
   - `scp: [openid, profile, email, offline_access, api.connectors.read, api.connectors.invoke]` —
     notably **no** scope that grants the public Chat Completions/Responses API at
     `api.openai.com`. This token is not a drop-in `OPENAI_API_KEY` replacement.

   Cross-referenced against `developers.openai.com/codex/auth/ci-cd-auth` (redirects to
   `learn.chatgpt.com/docs/auth/ci-cd-auth`), `codex doctor`/`codex --help`'s own output on this
   machine, and third-party reverse-engineering writeups
   (`github.com/simonw/llm-openai-via-codex`, `gist.github.com/ravidsrk/...`, a `docs.rs/skg-
   provider-codex` crate description, and general web search corroboration), the real backend this
   token authenticates against is **`POST https://chatgpt.com/backend-api/codex/responses`** — an
   undocumented, Codex-CLI-specific endpoint speaking the (public) Responses API wire shape, not
   Chat Completions. Headers: `Authorization: Bearer <access_token>`, `ChatGPT-Account-ID:
   <tokens.account_id>` (read straight off the same file, not decoded from the JWT), and —
   corroborated less strongly, from a search result describing the backend's model catalog as
   gated on it — `originator: codex_cli_rs`. Refresh: `POST https://auth.openai.com/oauth/token`,
   `grant_type=refresh_token`, the stored `refresh_token`, `client_id=app_EMoamEEZ73f0CkXaXp7hrann`
   — the same token endpoint and client id the JWT's own `iss`/`client_id` claims name.

2. **Is this sanctioned for third-party reuse?** No source claims it is, and one is explicit that
   it isn't: `learn.chatgpt.com/docs/auth/ci-cd-auth` scopes its own guidance to "Codex-managed
   ChatGPT auth" and says plainly to treat `auth.json` "like a password: it contains access tokens.
   Don't commit it, paste it into tickets, or share it in chat" — guidance about *handling* the
   file, not an endorsement of programmatic reuse against an undocumented backend. See "What this
   costs" below for what this means for this change's status.

## Decision

- **`crates/tm-auth/src/codex_subscription.rs`**: `CodexSubscriptionOAuth`, a fourth `AuthAdapter`,
  `kind() == CredentialKind::SubscriptionOAuth`. `default_codex_home()` resolves `$CODEX_HOME`
  (non-empty) else `$HOME/.codex`, mirroring the real `codex` binary's own resolution (`codex
  doctor`'s "disk" section reports exactly this path); the actual decision is a pure function
  (`resolve_codex_home`) unit-tested without touching real environment variables. `credential()`
  reads `auth.json` fresh on every call (the real `codex` process may have refreshed it
  independently since the last read), decodes the access token's `exp` claim (no signature
  verification — see Context; a token with no decodable `exp` is treated as never expiring, the
  same convention `DeviceCodeOAuth`'s `OAuthTokens::is_expired` already uses), and returns it
  as-is if still fresh. `account_id()` is a separate, non-secret accessor reading
  `tokens.account_id` directly off the file — `Credential` deliberately has no field for it (see
  its own docs on why it carries only `{secret, refresh}`), so a caller threads it through
  separately at provider-construction time.
- **Refresh reuses, not re-derives, `DeviceCodeOAuth`'s RFC 6749 §6 merge logic.**
  `crate::device_code::interpret_token_response` and `tokens_from_refresh` (the latter's
  visibility bumped from private to `pub(crate)` for this) are the same functions
  `DeviceCodeOAuth::refresh` uses, including the "a refresh response may omit `refresh_token`
  when it isn't rotated" fallback this session's own earlier work on `DeviceCodeOAuth` fixed as a
  real bug (losing the refresh token on the *first* refresh, then failing the *second* expiry with
  `NoRefreshToken`). `CodexSubscriptionOAuth::refresh` then writes the refreshed tokens back to
  `auth.json` atomically (temp file in the same directory, `chmod 0600` on Unix, then `rename`),
  re-reading the file immediately before writing to narrow (not eliminate — see "What this costs")
  the race against a concurrently-running real `codex` process.
- **`crates/tm-provider/src/providers/codex_chatgpt.rs`**: `CodexChatGptProvider`, a twelfth
  provider module, self-contained (does not build on `compat.rs`'s Chat Completions dialect —
  different wire family) but reuses `compat::classify_status` for HTTP status/error-envelope
  classification, since the Responses API's `{"error": {"message": ...}}` shape matches closely
  enough that re-deriving a second classifier would be pure duplication. Always requests
  `stream: true` regardless of `CompletionRequest::stream` (every third-party source calling this
  specific backend does so streaming; none confirms `stream: false` works here) and reassembles
  the single `Completion` this crate's `Provider::complete` contract promises from the *terminal*
  `response.completed`/`incomplete`/`failed` SSE event, which — unlike Chat Completions SSE —
  carries the entire final response object, so this is "extract the terminal event," not delta
  accumulation. Falls back to parsing the body as plain JSON if it contains no SSE `data:` framing
  at all, a defensive allowance given the documented wire-shape uncertainty.
- **The live proof, not yet run (see "What this costs")**: `crates/tm-agent/src/agent_loop.rs`'s
  own `tests` module gains a `live_codex_auth` submodule with one `#[tokio::test]`,
  `real_agent_loop_turn_reaches_the_real_codex_backend_and_submits`, reusing the file's existing
  `LiveHarness` (a real `tm_core::Store`, `ToolRegistry::standard`, a seeded ticket) exactly the
  way every other test in that file does, substituting `MockProvider` for a real
  `CodexChatGptProvider::from_local_session`. The task's objective directly asks the model to call
  `ticket.submit` with `summary` set to exactly the word `authloopworks`; the test *would* assert
  `AgentOutcome::Submitted`'s evidence summary contains it, once actually run against the real
  backend. Three independent layers keep this out of `cargo test --workspace`/`mise run verify`: a
  `live-codex-auth` Cargo feature (the module does not exist in the compiled binary without it —
  mirrors `tm-cli`'s `otel` feature, D-010), `#[ignore]` (needs `-- --ignored` even with the
  feature on), and a `TM_LIVE_CODEX_AUTH=1` runtime check (a clear skip message rather than a hard
  failure for a deliberate-but-unintended invocation). `mise.toml`'s `test:live-codex-auth` task
  runs it with the env var already set — **note the inverse is not safe to rely on**: a
  hand-invoked `cargo test -p tm-agent --features live-codex-auth -- --ignored` *without*
  `TM_LIVE_CODEX_AUTH=1` prints `ok` too, because the runtime check's early `return` on an unset
  var is itself a passing test outcome, not a skip cargo reports separately the way `#[ignore]`
  does. A green run only means something when the env var was actually set.

## Why

- Reusing `DeviceCodeOAuth`'s refresh-merge logic rather than rewriting it is not just DRY: it is
  the concrete way this change inherits a bug fix this same session already made once (the
  dropped-refresh-token bug), instead of risking reintroducing it in a sibling adapter that looks
  superficially different (file-backed vs. in-memory `TokenStore`) but has the identical RFC 6749
  §6 hazard.
- `compat.rs` was deliberately not stretched to cover this: it is built around eleven backends
  that all speak (a close dialect of) Chat Completions, and its own module docs already flag the
  one backend-specific gap (`max_tokens` vs `max_completion_tokens`) that exists *within* that one
  dialect. The Responses API is a structurally different wire shape (a flat `input` array of typed
  items, not `messages`; `instructions` not a `system` message; `max_output_tokens` not
  `max_tokens`; tool calls as top-level input items, not nested in an assistant message) — forcing
  it through `compat.rs`'s abstractions would have meant either widening `CompatConfig` with a
  second protocol's worth of special cases or silently mistranslating something. A second,
  self-contained wire module, reusing only the one genuinely shared piece
  (`classify_status`'s error-envelope shape), was the more honest boundary.
- Routing the live proof through a real `ticket.submit` tool call (not a bare text reply) is what
  makes it a real `AgentLoop` proof rather than a `Fabric::execute` proof with extra steps:
  `AgentLoop::drive` only reaches `AgentOutcome::Submitted` via a dispatched tool call — a
  text-only turn is `Failed` ("model ended turn without submitting") — so asserting on the
  submitted evidence's summary exercises the full chain the task asked for: `AuthAdapter::
  credential` → `Provider::complete` → `Fabric::execute` → `AgentLoop::drive`'s tool-dispatch path
  → `Store` → `AgentOutcome::Submitted`.

## What this costs, stated plainly

- **The wire shape was not empirically verified against the live endpoint from inside the session
  that built it.** The development sandbox's own auto-mode classifier blocked every attempt at a
  live network call using this credential — both a raw `curl` probe (attempted first, per plan, to
  nail the shape before writing Rust) and, later, actually running the gated
  `TM_LIVE_CODEX_AUTH=1 cargo test ... live_codex_auth` test itself. Both were blocked identically,
  even though the user had explicitly pre-authorized this exact action. The wire shape implemented
  here is therefore cross-referenced from public documentation and third-party sources, not
  confirmed firsthand — `crates/tm-provider/src/providers/codex_chatgpt.rs`'s own module docs say
  this plainly, and its unit tests cover the pure translation functions against literal
  recorded-shaped JSON, not a real captured response. **Running `mise run test:live-codex-auth` on
  a machine outside this sandbox, with a real logged-in Codex CLI session, is the actual
  verification this change is still waiting on** — if the shape is wrong in some way, that test's
  failure (a real server error message, a parse failure naming the field it choked on) is what
  will say so, but a first failure should not be read too literally: there are several
  independently-uncertain links in this chain (the `originator` value, whether `stream: true` is
  actually required, the SSE terminal-event shape, the Responses API item shapes, and whether
  tool-calling round-trips through this specific backend at all), and a 400 caused by one can look
  similar to a 400 caused by another from the outside. If the first live run fails, the useful next
  step is bisecting — retry with a bare text-only prompt and no `tools` first, to separate a plain
  auth/transport/text-response failure from anything specific to the tool-calling translation —
  rather than assuming the error string alone localizes which link broke.
- **The `originator: codex_cli_rs` header is the least-confirmed piece of the whole contract.** It
  is included because one search result described the backend's model catalog as gated on it, not
  because any source gave a canonical, cited value. If it turns out to be wrong or unnecessary,
  the live test's own failure mode (a different or degraded model catalog, or an outright auth
  failure) is the mechanism that will surface it, not a code review of this doc.
- **This is not obviously sanctioned for the use this change puts it to, and this doc does not
  resolve that question.** No source found says a ChatGPT-subscription session is meant to be
  reused by a third-party program calling an undocumented backend endpoint directly; the closest
  official statement (`learn.chatgpt.com/docs/auth/ci-cd-auth`) treats `auth.json` as sensitive
  and scopes its own guidance to Codex-managed usage, without addressing third-party reuse either
  way. The user explicitly authorized *building and testing this locally*, on this machine, for
  this stated purpose — that is a narrower and different question from whether this should ever
  ship as a general, always-on product feature (auto-detected the way `DevPassProvider`/
  `OpenAiProvider` are, say). This change deliberately does **not** wire `CodexChatGptProvider`
  into `providers::registry::Registry`'s autodetection or `role_config::RoleTable::default_table`
  — it exists, is real, and is reachable by name, but nothing in this workspace routes to it by
  default. Whether to promote it further is a call for a human to make explicitly later, not one
  this change makes for them by omission.
- **The concurrent-writer hazard on `auth.json` is narrowed, not closed.** `codex` itself may be
  running and refreshing the same file independently. `CodexSubscriptionOAuth::refresh` re-reads
  immediately before writing and writes atomically (temp file + `rename`), which shrinks the race
  window to "between that re-read and the rename" — but there is no real cross-process lock (an
  `flock` on a sibling `.lock` file was considered and left out of this change's scope; it adds
  real complexity — stale-lock handling, a second failure mode — for a race this narrow, on a
  token that, per the machine this was built on, is valid for roughly ten days at a time, making
  simultaneous refreshes from two processes an edge case rather than a routine occurrence). Two
  processes racing to refresh at the exact same instant can still each persist a token the other
  doesn't know about.
- **`account_id` is read once at `CodexChatGptProvider` construction, not per call.** If the
  underlying Codex session's account ever changed mid-process (switching ChatGPT accounts without
  restarting whatever holds the provider), the header would go stale silently rather than erroring
  — judged acceptable since nothing in this workspace's own process lifecycle does that, but worth
  naming as a real, if narrow, gap.
- **The live test's assertion never actually exercises the refresh path on this machine.** The
  real token this was built against (`iat`/`exp` roughly ten days apart, issued days before this
  session) is not near expiry, so `real_agent_loop_turn_reaches_the_real_codex_backend_and_submits`
  proves the credential→provider→loop chain on a *fresh* token, not the refresh branch. The refresh
  merge logic itself is unit-tested purely (inherited directly from `DeviceCodeOAuth`'s own tests,
  see Decision above), but nothing here exercises a real HTTP round-trip against
  `https://auth.openai.com/oauth/token` — consistent with this workspace's existing rule that no
  test performs a real network call, and with `DeviceCodeOAuth::refresh`'s own live path being
  equally untested by a real call for the identical reason.
