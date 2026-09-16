# Backlog

Work that is specified and agreed but not yet built, in rough priority order. Anything here is
real, scoped work — not aspiration. Items that turn out to be wrong get deleted, not quietly kept.

## Executors (D-001)

- `Executor` trait, `ExecutorTask` / `ExecutorOutcome` / `ExecutorCapabilities` in `tm-core`, with
  `tm-agent` reimplemented as the `builtin` adapter behind it.
- Adapters: `claude-code` (print mode / SDK), `codex` (exec / JSON), `pi` (JSON RPC over stdio),
  `opencode`, `human`.
- Sandbox derivation: turn a granted `Authority` into a concrete filesystem/network/command sandbox
  for an external process, and validate returned writes against that scope.
- Executor fabric: placement routing (local process, container, remote node, cloud sandbox) reusing
  the provider fabric's quota, health and fallback machinery. Free tiers modelled as ordinary
  capacity with a daily ceiling.

## Workflow definitions (SPEC §25)

- The definition format, its parser, and expansion into a validated ticket subgraph committed in one
  transaction.
- `for_each` fan-out over a prior node's structured output; explicit joins with timeout and merge
  rules; cycle budgets required at compile time.
- Versioned definitions with running instances pinned to the version they started on.
- A starter library: `review-change`, `migrate-sites`, `research-and-synthesize`, `harness-benchmark`.
- `tm doctor` warning for a workflow that is one node wide and one node deep.

## Project wiki (SPEC §26)

- Wiki assembly from authoritative sources, each page carrying provenance and rendered freshness.
- Staleness banner naming exactly what invalidated a page (ticket and/or decision).
- `/wiki` in `tm serve`, cross-linked with tickets, decisions and presence; plain markdown in-repo
  so it still works on GitHub with nothing running.
- Wiki pages as a ranked retrieval source in context compilation, so workers read the compiled
  explanation before the source.

## Project templates (SPEC §27)

- Template format: manifest, files with substitution, pinned `deps.lock`, `skill.md`, `verify.toml`,
  `bench/`.
- Selection during Genesis graph compilation instead of generating a scaffold from nothing.
- Starter set: VitePress, Zola, Astro, Next.js, Textual, Ink, Ratatui, Cobra, Axum, FastAPI, Hono,
  Tauri, SwiftUI, and the three library templates.
- Registry with version and checksum pinning; third-party templates treated as untrusted input
  (sandboxed verify, `skill.md` as data, no self-granted authority).
- CI that scaffolds every template and runs its `verify.toml`, so a template that does not build is
  caught as a bug in the template.

## Verification ladder (SPEC §18.5)

- `T-E2E-WEB` — Playwright end-to-end against a live `tm serve`.
- `T-VIS-REG` — visual regression across light/dark and Reduce Transparency, including the macOS
  Liquid Glass surfaces and recorded PTY screens.

## Stretch

- iOS simulator executor (SPEC §23).
- Additional model providers beyond Anthropic in the provider fabric.
- Additional tracker adapters beyond the shipped four.
