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
- `mise run docs:wiki` — regenerate `docs/wiki/` (`tm wiki generate`); pass `-- --dry-run` to
  preview without writing (see "Navigation" below).

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

- `mise run lsp` runs a full-workspace `rust-analyzer diagnostics` dump — informational, not a
  gate (it exits non-zero on *any* diagnostic, including the benign `#[cfg(test)]`
  "inactive-code" note every test module produces, so read the output rather than the exit
  code). `rust-analyzer` itself is a real LSP server available in this toolchain
  (`mise install rust-analyzer` if it's ever missing) for anything that wants go-to-definition
  or type-aware navigation beyond what `tm-codeintel`'s heuristics give you.

## Navigation

- `docs/audit-2026-09-18-fable.md` is a comprehensive, file:line-anchored implementation status
  audit — read it before assuming a SPEC section is done or missing.
- `SPEC.md` section 0 is binding project philosophy, not aspirational prose: deterministic
  machinery stays pure/unit-testable with no network and no model calls; the hygiene task
  enforces the sharpest edges of this mechanically.
- Prefer `symbol.*`/`search.hybrid`-shaped retrieval (or zvec-grep, above) over blind
  multi-file `Read` sweeps — this workspace's own `tm-codeintel` crate exists because that's
  faster and more precise than grepping cold.
- `docs/wiki/` is generated documentation (`SPEC.md` §26, B-14): architecture/<crate>, decisions,
  history/<path>, tickets, and glossary pages assembled from live project state. Regenerate it
  with `mise run docs:wiki` (`tm wiki generate`; add `--dry-run` to preview without writing) —
  don't hand-edit a page unless you also flip its front matter to `mode = "maintained"`/`"human"`,
  or the next regeneration silently overwrites it.

## Keep documentation honest as you change things

A change that lands without its documentation landing with it is half-finished, not done. When
you touch behavior this repo documents, update the same turn, not a follow-up:

- A new/changed decision (an architecture choice, a tradeoff, something a future session would
  otherwise have to re-derive) gets a `docs/decisions/D-NNN-*.md` in the existing format
  (`docs/decisions/D-002-terminal-ui-stack.md` is the template: Status/Date/Supersedes, then
  Context/Decision/Why/"What this costs, stated plainly").
- A SPEC.md section whose actual implementation now disagrees with what's written gets corrected
  or pointed at the decision doc that supersedes it — don't let SPEC.md silently drift out of
  sync with reality the way parts of it already have (see the fable audit's "Part 3:
  corrections to the baseline").
- `docs/audit-2026-09-18-fable.md` is a point-in-time snapshot, not a living document — don't
  edit it after the fact to mark things done; a stale audit is still useful as history, a
  silently-edited one isn't. Track new status in the SPEC/decision docs instead.
- New CLI verbs, mise tasks, or dev-workflow changes get a line in this file, not just in the
  code's own `--help` output — this file is what a fresh session reads first.

## This harness should keep improving itself

Treat friction — a repeated manual fix, a stray file some track created by mistake, a check that
should have caught something but didn't, a convention two agents independently reinvented
slightly differently — as a signal to fix the harness itself, not just the immediate symptom.
That means: add the missing hygiene check, write the missing `SKILL.md`, add the missing mise
task, correct this file, rather than only patching the one instance. This is a standing
directive, not a one-time cleanup — it doesn't expire when the current backlog does.
