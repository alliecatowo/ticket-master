# Axum service conventions

This project is an HTTP service built with [Axum](https://github.com/tokio-rs/axum) on Tokio.
Conventions for working in this codebase:

- **Build the `Router` in its own function** (`router()` in `src/main.rs`), separate from
  `main()`'s `#[tokio::main]` entry point. This is what makes the service testable: `cargo test`
  drives the router directly through `tower::ServiceExt::oneshot`, with no real TCP socket or
  running process involved.
- **Handlers return `impl IntoResponse`** (here, `Json<Value>`); avoid reaching for
  `.unwrap()`/`.expect()` inside a handler body — a panicking handler takes the whole worker
  thread's task down, not just the request. Prefer mapping errors to a proper status code.
- **`/health` is the liveness contract.** Every service scaffolded from this template ships one;
  keep it dependency-free (no database ping, no downstream call) so it answers even when a real
  dependency is down and orchestrators can tell "process is up" apart from "service is healthy".
- **Configuration in, not hardcoded.** The listen port comes from this template's `port`
  parameter, substituted at scaffold time; a real service should read it (and anything else
  environment-specific) from the process environment at startup instead of a compile-time
  constant — this scaffold keeps it a constant only because "read config" is exactly the kind of
  project-specific work `SPEC.md` §27.1 says a template should leave for the agent to do, not
  pre-decide.
- **Formatting and linting**: `cargo fmt` and `cargo clippy --all-targets -- -D warnings` are
  expected to pass clean; this scaffold's `verify.toml` runs `cargo build`/`cargo test` and
  assumes both already pass.
