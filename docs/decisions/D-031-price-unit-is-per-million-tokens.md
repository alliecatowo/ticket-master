# D-031 — `Price` is denominated per 1,000,000 tokens, not per token

**Status:** accepted · **Date:** 2026-09-25 · **Supersedes:** nothing

## Context

`crate::role_config::Price` stored `input_micros_per_token`/`output_micros_per_token` as `u64`
micro-dollars *per token*. `crate::fabric::cost_micros` computed a completion's real spend as
`tokens * micros_per_token`, both integers, no rounding.

Any model priced under $1 per 1,000,000 tokens is a fraction of a micro-dollar per token —
$0.20/M tokens is 0.2 micro-dollars/token — which floors to `0` the moment it's stored as an
integer `micros_per_token`. That's exactly the range most of the cheap models this project cares
about (`tel-completion-cost-field`'s own sub-$1/M example) live in, and it silently zeroed
`usage.recorded`'s `dollars_micros`, `tm stats` dollar figures, and budget tier-down for every one
of them — while `default_table_with`, the built-in default `providers.toml`, priced *no* candidate
at all (`price: None` on every entry), so the bug had never actually been exercised by a real
default. `workflow wf_87a85411-848`'s critic result flagged both: `rg 'price:\s*Some'` found only
a test fixture, and `role_config.rs:48` documented the per-token integer unit that can't represent
a realistic sub-cent-per-token rate.

## Decision

`Price`'s two fields are renamed and reinterpreted as **micro-dollars per 1,000,000 tokens**:
`input_micros_per_million_tokens`/`output_micros_per_million_tokens`. `fabric::cost_micros`
multiplies token counts by the per-million rate in `u128` (a straight `u64 * u64` product can
already approach `u64::MAX` for a large token count against a normal price), sums, divides back
down by `1_000_000`, and saturates to `u64::MAX` rather than wrapping.

`Price` deserializes via a `#[serde(try_from = "PriceToml")]` shim (`PriceToml`) that accepts
either the new per-million field names, or the old per-token names scaled up by `1_000_000` —
never a mix of both families, and never only one field of a pair, both of which are a parse error
rather than a silent partial price. This means an old `providers.toml`/test fixture written
against the pre-this-task unit still parses, to the same real price, instead of erroring or
silently misreading a per-token integer as a per-million one. Serialization (round-tripping a
parsed table back to TOML) always emits only the new, per-million field names, so the legacy shape
never propagates forward on its own.

`RoleTable::default_table_with` now prices every Anthropic candidate it configures —
`claude-opus-5-5` ($4/$20 per 1,000,000 input/output tokens), `claude-sonnet-5` ($2/$10), and
`claude-haiku-4-5` ($1/$5), Anthropic's published first-party API rates for those model ids as of
this task — and the `text-embedding-3-small` embedder candidate ($0.02/1,000,000 input tokens,
OpenAI's published rate; embeddings have no output tokens to price), instead of leaving every
default candidate unpriced. A real `providers.toml` can still override any of these per candidate.
DevPass's primary candidate is deliberately left unpriced (`primary.price = None` after the
DevPass substitution): DevPass is a subscription pass-through, not a per-token API rate, so
reporting Anthropic's direct-API price for it would misattribute cost to the wrong provider.

## Why

Per-million-token integer micro-dollars is the smallest fixed-point change that fixes the real
failure (truncation to zero) without introducing a float into a budget-accounting path (SPEC §0's
deterministic-machinery rule) or requiring every call site to reason about rounding error from a
float re-conversion. `u128` intermediates avoid a second overflow class (`tokens * price`
overflowing `u64`) that a naive per-million change would otherwise reintroduce at realistic token
counts. The legacy-field-name compat shim exists specifically because this task's file ownership
did not include `crates/tm-agent/src/agent_loop.rs`, which has a test asserting a specific
`dollars_micros` value against a TOML literal using the old field names
(`run_records_usage_recorded_with_the_completions_actual_spend`); accepting the old names (scaled
up) rather than renaming outright keeps that file's test passing unmodified, since
`(100 * 42_000_000 + 50 * 84_000_000) / 1_000_000 == 100 * 42 + 50 * 84 == 8400` either way.

## What this costs, stated plainly

Every in-repo consumer of `Price`'s fields this task could reach was updated in the same change:
`crates/tm-provider/src/route.rs`'s price-ceiling admission check and
`crates/tm-context/src/sections.rs`'s budget tier-menu now divide by `1_000_000` after multiplying
by the per-million rate, and both files' test fixtures were switched to the new field names (the
compat shim means they didn't strictly have to be, but the new names are the ones any *new* code
should write). No repo-tracked `crates/tm-provider/providers.toml` exists — `providers.toml` is
scaffolded per-project by `tm init` from `RoleTable::default_table()`, per `docs/providers.md` —
so there is no tracked fixture file to migrate.

The legacy-alias shim is a permanent surface, not a deprecation window with an end date: nothing
in this task set a timeline for removing `PriceToml`'s two legacy fields, and doing so later is
itself a breaking change against whatever `providers.toml`/fixtures still use them by then.

Pricing every default candidate also means a project that never touches `providers.toml` now has a
real, finite dollar figure behind `tm stats`, the context pack's budget tier menu, and budget
tier-down/handoff for the first time — any test elsewhere in the workspace that asserts on those
surfaces against an unpriced default table (constructed via `RoleTable::default_table()` /
`default_table_with`) may now see non-placeholder output instead. This task's own file ownership
found and fixed the one such assertion inside `crates/tm-context/src/sections.rs`
(`build_budget_tier_menu_prices_a_metered_role`, whose expected `$0.0200/call` string is unchanged
by the unit fix since the underlying dollar amount is the same). `crates/tm-context/src/pack.rs`
calls `RoleTable::default_table()` at roughly fifteen call sites but was not exhaustively audited
for a same assumption; a file this task could not touch at all (`crates/tm-cli/src/ops.rs`,
`crates/tm-cli/src/project.rs`, `crates/tm-cli/src/sched.rs`, `crates/tm-core/src/store.rs`) with
the same assumption is not confirmed clean either.
