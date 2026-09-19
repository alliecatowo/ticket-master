# Working in this repo

## Always use mise tasks

This repo's `mise.toml` defines the canonical way to build, test, lint and verify — run
`mise run <task>` (or `mise tasks` to list them) instead of hand-rolling the equivalent `cargo`
invocation. They exist so build-concurrency limits, `-p` scoping, and which gate actually runs
never drift between sessions:

- `mise run build` — build just the `tm` binary (fast default).
- `mise run build:all` — build the whole workspace.
- `mise run test` — `cargo test --workspace`.
- `mise run test:crate -- <crate>` — one crate's tests.
- `mise run clippy` — workspace lint, `-D warnings`, matching CI.
- `mise run fmt` — apply rustfmt everywhere (`cargo xtask fmt`).
- `mise run hygiene` — the fast standalone hygiene scan (non-determinism, unwrap/expect,
  network-in-tests, stray `.tm` literals — see `crates/xtask/src/hygiene.rs`).
- `mise run verify` — the full gate (fmt check + clippy + `cargo test --workspace` + hygiene).
  **Run this before considering any change done**, not just a crate-scoped test pass.
- `mise run clean` — `rm -rf target`. This machine runs tight on disk; do this after a verify
  pass lands, not mid-build. `mise run worktree:clean` sweeps every worktree under
  `.claude/worktrees/` the same way.
- `mise run tui` — build and launch the ratatui TUI against the current directory's project.
- `mise run dev` — same, with `RUST_LOG=tm=debug,tm_core=debug,tm_agent=debug` piped to
  `/tmp/tm-dev.log` instead of the alt-screen (so debug output doesn't corrupt the TUI's frame).
- `mise run doctor` — `tm doctor` against the current directory.

Every build/test/clippy call in these tasks is already capped at `-j 2` — this is an 8GB Mac,
concurrent full-workspace compiles have caused real disk-space incidents. If you're driving
several agents/worktrees at once, don't override that cap.

## Codebase search: zvec-grep is indexed for this repo

This workspace has a persistent zvec-grep index (`.zvec-grep/`, gitignored, local embedding
model — no network dependency). Use it exactly per the global routing rules in
`~/.claude/CLAUDE.md`: `zvec_grep_rg` for an exact symbol/string/path, `zvec_grep_search` for
"where does X happen" / architecture / cross-file questions. The index has a live watcher, so it
stays current across edits without a manual rebuild. Don't rebuild or drop it without asking —
that rule is global, not repo-specific, and still applies here.

## Logging and debugging

- `RUST_LOG` controls `tracing` output on stderr (`tracing_subscriber::EnvFilter`, see
  `crates/tm-cli/src/main.rs`'s `install_tracing`). `mise run dev` sets a sane default; for a
  narrower trace use e.g. `RUST_LOG=tm_agent=trace,tm_scheduler=debug`.
- The TUI runs in the alt screen, so raw `println!`/`eprintln!` debugging will corrupt the frame
  — use `tracing::debug!`/`info!` (routed to stderr, invisible inside the alt screen, visible
  when redirected to a file as `mise run dev` does) rather than print statements when working on
  `tm-tui`/`tm-cli`'s TUI path.
- Every session's real state lives under `Project.state_dir` (see
  `docs/decisions/D-003-project-scope.md`) — `sqlite3 <state_dir>/project.db` is a legitimate way
  to inspect what actually got written, and `tm events` reads the same log a UI would.

## Navigation

- `docs/audit-2026-09-18-fable.md` is a comprehensive, file:line-anchored implementation status
  audit — read it before assuming a SPEC section is done or missing.
- `SPEC.md` section 0 is binding project philosophy, not aspirational prose: deterministic
  machinery stays pure/unit-testable with no network and no model calls; the hygiene task
  enforces the sharpest edges of this mechanically.
- Prefer `symbol.*`/`search.hybrid`-shaped retrieval (or zvec-grep, above) over blind
  multi-file `Read` sweeps — this workspace's own `tm-codeintel` crate exists because that's
  faster and more precise than grepping cold.
