# D-008 — OpenTelemetry tracing export, opt-in behind `otel`

**Status:** accepted · **Date:** 2026-09-20 · **Supersedes:** nothing

## Context

`docs/audit-2026-09-18-fable.md`'s M-04 ("Ecosystem table stakes") names OpenTelemetry export as
a gap against the field; its "done looks like" is specific: "`tracing-opentelemetry` behind a
feature flag with `TM_OTEL_ENDPOINT`". `crates/tm-cli/src/main.rs`'s `install_tracing` was, before
this change, a fixed three-line `tracing_subscriber::fmt()` builder writing to stderr under
`RUST_LOG` — the only tracing subscriber this codebase has ever installed. Other crates already
emit real `tracing::debug!`/`info!` events against that one subscriber (`CLAUDE.md`'s "Logging and
debugging" section) — bare events only, not span-shaped instrumentation; see "What this costs"
for why that distinction turned out to matter. The task was to add an export path *alongside*
whatever tracing already existed, not to touch any of those call sites.

Two things had to be settled before writing code:

1. **Which crate versions actually resolve together.** `tracing-opentelemetry` doesn't version in
   lockstep with the `opentelemetry` family (its own README says so explicitly, linking
   [tokio-rs/tracing-opentelemetry#170](https://github.com/tokio-rs/tracing-opentelemetry/issues/170)).
   As of this writing, `opentelemetry`/`opentelemetry_sdk` just cut 0.33.0 (two days before this
   decision), but `tracing-opentelemetry` 0.33.0 — published a month earlier — still declares
   `opentelemetry = "^0.32.0"`, and `opentelemetry-otlp` follows suit at its own 0.32.0 (its 0.33.0
   requires `opentelemetry ^0.33`, which `tracing-opentelemetry` 0.33.0 does not accept). Naively
   pinning all four crates' own latest-stable numbers independently would have produced a graph
   Cargo cannot resolve to one `opentelemetry` version — or, worse, one that resolves to two
   incompatible `opentelemetry` majors whose types don't unify, so `tracing-opentelemetry`'s
   `Tracer` silently wouldn't implement the trait `opentelemetry-otlp`'s exporter needs. Confirmed
   by downloading and inspecting the actual published `Cargo.toml`s (crates.io's dependency-graph
   API), not by assuming a version scheme from memory — see the pins in
   `crates/tm-cli/Cargo.toml`'s comment for the exact chain.
2. **Which OTLP transport.** `opentelemetry-otlp` supports gRPC (`tonic`) or HTTP
   (`http-proto`/`http-json`, via `reqwest`). Picked HTTP: it is the crate's own documented
   default, avoids adding a `tonic`/`h2`/gRPC stack this workspace has never needed for anything
   else, and needs no `protoc` binary on the build machine (confirmed by inspecting the actual
   published crate contents — `opentelemetry-proto`'s protobuf bindings are checked in
   pre-generated, no `build.rs` anywhere in this dependency chain, so there was never a real risk
   here, just one worth ruling out rather than assuming).

## Decision

- **New `otel` Cargo feature on `tm-cli` only**, gating four new `optional = true` dependencies
  (`opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`, `tracing-opentelemetry`), off by
  default. Crate-local rather than promoted to `[workspace.dependencies]`, since nothing else in
  the tree touches OpenTelemetry — matches this crate's own existing precedent for
  `tm-browser`/`tm-computer`/`tm-notify` (pathed directly rather than promoted "so this crate does
  not require editing the workspace root").
- **`crates/tm-cli/src/otel.rs`** (itself `#[cfg(feature = "otel")]`-gated via its `mod otel;` in
  `main.rs`), split into a pure half and an I/O half:
  - `endpoint_from_env()` reads `TM_OTEL_ENDPOINT`, treating unset or all-whitespace as disabled.
  - `build_provider(endpoint: &str)` constructs the OTLP/HTTP `SpanExporter` and wraps it in a
    `SdkTracerProvider` with a batch processor. Pure in the sense that matters for testing: no
    network I/O happens at construction (the HTTP client connects lazily on first export), so this
    is exercised directly by unit tests with no live collector, per `SPEC.md` §0 / the hygiene
    task's network-in-tests rule.
  - `install(endpoint: &str)` calls `build_provider`, builds a `tracing_opentelemetry::layer()`
    from it, and installs the combined `EnvFilter` + stderr `fmt` layer + OTel layer as the
    process's one global subscriber via `try_init()` — the only half that can't be called more
    than once per test binary, so it's kept separate and untested directly (the construction it
    wraps is what's actually tested).
- **`install_tracing` in `main.rs`**: when the `otel` feature is compiled in and `TM_OTEL_ENDPOINT`
  is set, calls `otel::install`; on any failure (malformed endpoint, subscriber already installed),
  prints one line to stderr naming the var and the cause, then falls back to exactly the original
  stderr-only `fmt()` builder — unchanged from before this decision, byte-for-byte, for every other
  combination (feature off; feature on but the var unset or empty). `TM_OTEL_ENDPOINT` is passed
  to the exporter as a programmatic endpoint (`WithExportConfig::with_endpoint`), which
  `opentelemetry-otlp` uses verbatim with **no** `/v1/traces` suffix appended (confirmed by reading
  `resolve_http_endpoint`'s source — that auto-append only fires for the *default* endpoint or the
  generic `OTEL_EXPORTER_OTLP_ENDPOINT` env var, not a code-supplied one) — so the intended value
  is the full traces endpoint, e.g. `http://localhost:4318/v1/traces`, not a bare host:port.
- **Flush before exit, not via `Drop`.** `main()` always exits through `std::process::exit`, which
  skips destructors entirely. `install_tracing` now returns a `TracingGuard` (a no-op empty struct
  when the feature is off) whose own `Drop` impl is invoked explicitly by `main` (`drop(guard)`,
  not left implicit) — running the OTel provider's `shutdown_with_timeout(EXPORT_TIMEOUT)`, which
  flushes the batch processor with a bound rather than `shutdown()`'s unbounded wait — immediately
  before the one remaining `std::process::exit` call (the two previous exit sites collapsed into
  one to give the flush a single, always-reached place to run from).
- Service name sent on every span is the literal `"tm"` (the binary name), not the crate name
  `tm-cli`.

## Why

- Reusing the *existing* subscriber composition rather than replacing it satisfies the audit's own
  framing directly: "OTel export should layer on top of the same spans/events, not require every
  call site to change" (source of this task). Every `tracing` call site already in the workspace
  reaches both layers with zero changes elsewhere, because `tracing`'s global dispatcher is
  process-wide and both layers are added at the one place it's installed — but "reaches the OTel
  layer" and "gets exported" are not the same claim; see the first item under "What this costs"
  for why that distinction actually matters here, not just as a hedge.
- Splitting `build_provider`/`install` mirrors this codebase's established pure/I/O split
  (`tm-notify`'s `decision`/`notifier`, `tm-scheduler`'s `plan()`/`SchedulerLoop`) for exactly the
  same reason: the interesting, verifiable behavior (endpoint parsing, error propagation, that
  construction genuinely doesn't need a live collector) is unit tested directly; the
  can't-call-twice global mutation is kept small, isolated, and understood as untestable-by-
  construction rather than smeared across the module.
- `default-features = false` on all four new dependencies (keeping only `"trace"` and, on
  `opentelemetry-otlp`, `"http-proto"`/`"reqwest-blocking-client"`) drops metrics/logs support and
  `tracing-opentelemetry`'s `log`-crate bridge neither `install_tracing` nor anything else in this
  workspace asks for — smaller opt-in build, not smaller default build (see costs below for what
  it does *not* avoid).

## What this costs, stated plainly

- **As of this decision, a `tm` invocation with this feature on exports nothing to the
  collector, because the workspace has zero spans to export.** `tracing_opentelemetry::
  OpenTelemetryLayer` is span-shaped: it turns `#[instrument]`/`*_span!`-created spans into OTel
  spans and turns events *inside* a span into that span's OTel events, but drops a bare event
  with no enclosing span outright. Checked directly rather than assumed:
  `rg '#\[instrument|_span!\('  crates/` returns zero matches in this codebase's real (non-test)
  code (excluding `otel.rs` itself, whose doc comment mentions the pattern in prose and whose own
  unit tests deliberately emit sample events) against 61 bare `tracing::info!`/`debug!`/`warn!`/
  `error!` call sites across 23 files (same exclusion; `rg -c 'tracing::(info|debug|warn|error)!
  \('  crates/` minus `otel.rs`'s own count). So today, this decision
  is exactly and only the export plumbing — subscriber wiring, env-var gate, exporter/provider
  construction, flush-before-exit — verified correct and inert-until-opted-in on its own terms;
  what it would carry to a real collector is a separate, deliberate follow-up (adding
  `#[instrument]`/`*_span!` at the handful of places actually worth tracing, e.g. `tm-scheduler`'s
  dispatch loop or `tm-agent`'s tool-call loop), not something this change does or was scoped to
  do. Don't read "layer on top of the same spans/events, not require every call site to change"
  (this task's own framing) as "the workspace already has meaningful spans" — it doesn't yet.
- **A second `reqwest` major version in the dependency graph, only when `otel` is enabled.**
  `opentelemetry-otlp` 0.32 requires `reqwest ^0.13.1`; this workspace's own `reqwest = "0.12"`
  (`rustls-tls`) is used everywhere else. Cargo resolves both concurrently rather than erroring —
  verified via `Cargo.lock` after building with `--features otel`, which shows `reqwest 0.12.28`
  and `reqwest 0.13.5` as separate `[[package]]` entries — which means two full HTTP-client stacks
  compile when the feature is on. Confirmed *not* to affect the default (no-flags) build: the only
  `Cargo.lock` changes from a plain `cargo build --workspace -j 2` are new entries reserved for the
  four optional `otel`-gated deps and their own transitive dependencies, plus `serial_test`/
  `serial_test_derive` (an existing `[workspace.dependencies]` entry with no prior real use
  anywhere in the tree until this change's tests, added as a normal dev-dependency, not gated by
  `otel`) (plus a few pre-existing entries' dependency-list lines relabelled from `"reqwest"`
  to `"reqwest 0.12.28"` for disambiguation now that a second version exists in the lock graph at
  all) — no existing crate's resolved version changed, and `cargo tree -p tm-cli` with default
  features shows zero `opentelemetry*` crates.
- **The version pin is load-bearing and will go stale.** The 0.32/0.33 split recorded above is a
  snapshot of what resolves *today*; `opentelemetry`/`opentelemetry_sdk` 0.33.0 already exists and
  `tracing-opentelemetry` will presumably catch up to it eventually. Bumping any one of the four
  crates without re-checking all three others' declared requirements (not just trusting "latest"
  independently per crate) risks silently reintroducing the exact unresolvable/dual-major graph
  this decision worked around.
- **HTTP/protobuf transport only — no gRPC.** A collector that only accepts OTLP/gRPC on 4317
  won't take input from this exporter; the audit didn't specify a transport, and adding `tonic` as
  a second opt-in path is a real but separate future change, not folded in here.
- **No metrics or logs export**, only traces — `default-features = false` deliberately drops both.
  The audit's "done looks like" named tracing specifically; extending to metrics/logs is a
  follow-up, not implied by this decision.
- **`TM_OTEL_ENDPOINT` semantics are stricter than some other OTel SDKs' conventions.** Passing a
  bare `host:port` (as OpenTelemetry's own `OTEL_EXPORTER_OTLP_ENDPOINT` env var would accept, with
  `/v1/traces` auto-appended) silently produces a working exporter that posts to the wrong path
  instead of erroring — because a *programmatic* endpoint is used verbatim by this version of
  `opentelemetry-otlp`. Users need the full path; this is documented in `otel.rs`'s doc comment on
  `TM_OTEL_ENDPOINT_VAR` and here, not enforced in code (no validation beyond "is this a parseable
  URI at all").
- **No test exercises an actual span reaching a real collector** (by design — this task's own
  instructions and `SPEC.md` §0 / the hygiene task's network-in-tests rule all rule that out for
  `cargo test`; a non-routable address that hangs on connect would be real network I/O from test
  code even though it can never succeed, so that path isn't exercised by an automated test either,
  only by hand, below). What `crates/tm-cli/src/otel.rs`'s unit tests (`cargo test -p tm-cli
  --features otel`) do verify: `endpoint_from_env`'s three states (unset, blank, set);
  exporter/provider construction succeeding for a well-formed endpoint and failing correctly for a
  malformed one, with neither needing a live collector; and, directly rather than by inference,
  that installing the combined stderr-`fmt` + OTel subscriber produces byte-identical `fmt` output
  to the original stand-alone `fmt()` builder for the same events (`tracing::subscriber::
  with_default`-scoped, so it never touches process-global state and is safe to run repeatedly).
  What's still only manually confirmed, not automated: a full `tm` invocation, end to end, with
  `--features otel` and `TM_OTEL_ENDPOINT` pointed at `http://127.0.0.1:0/v1/traces` (a port that
  refuses instantly rather than hanging) exits cleanly with `--json` output unaffected — checked
  once by hand, not re-checked by any test that runs in `verify` or `mise run test:otel`. And
  because the workspace currently has zero spans (see this section's first item), the one thing
  that can't be checked *even by hand* right now is what happens when a span genuinely queued for
  export hits an unreachable collector at `shutdown` time: `EXPORT_TIMEOUT` (`otel.rs`, 3s, used
  both as the exporter's own request timeout and as `SdkTracerProvider::shutdown_with_timeout`'s
  bound in `TracingGuard::drop`, replacing the SDK's own unbounded `shutdown()`) exists specifically
  so that path doesn't hang a `tm` invocation once real spans exist, but it is unreachable through
  the CLI today and therefore unverified beyond reading the two APIs it calls.
