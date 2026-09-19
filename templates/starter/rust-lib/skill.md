# Rust library crate conventions

This project is a plain Rust library crate: no binary, no framework, just a public API meant to
be depended on. Conventions for working in this codebase:

- **Every public function gets a unit test.** A library's whole contract is its public API;
  untested public functions are the highest-risk code in a crate this shape, since callers
  depend on behavior no integration test elsewhere in the workspace will catch a regression in.
- **Prefer `Option`/`Result` over panicking.** A library that panics on bad input takes down
  whatever embeds it; `checked_add` in `src/lib.rs` is the pattern (`checked_*`/`try_*` over the
  panicking equivalent) to follow for anything that can fail predictably.
- **No `unwrap()`/`expect()` in non-test code without a comment justifying why the case is
  actually unreachable.** This mirrors the workspace-wide hygiene rule the rest of this
  repository is held to (`cargo run -p xtask -- hygiene`); a scaffolded library should start
  clean against it, not accumulate debt from its first commit.
- **`pub` is a promise.** Keep the public surface small and intentional; something not meant for
  external use should be private or `pub(crate)`, not `pub` by default.
- **Formatting and linting**: `cargo fmt` and `cargo clippy --all-targets -- -D warnings` are
  expected to pass clean; this scaffold's `verify.toml` runs `cargo build`/`cargo test` and
  assumes both already pass.
