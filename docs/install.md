# Installing tm

`tm` is a single Rust binary (`crates/tm-cli`, binary name `tm`) plus an optional built web
client. The GitHub repo (`alliecatowo/ticket-master`) and its release assets are public.

## Requirements

- macOS or Linux. `.github/workflows/release.yml` publishes both targets in its matrix
  (`aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`) automatically on every `vX.Y.Z` tag push;
  `mise run release` (`scripts/release.sh`) builds the same tarball layout locally, for a
  platform that matrix doesn't cover yet or to test a release before tagging. Anything outside
  those two targets needs the source install below. As of `v0.1.0` only `tm-aarch64-apple-darwin`
  (macOS arm64) is actually published, so on Linux, build from source (see below). `tm-computer`
  has compiled on Linux since `b01efda` (after `v0.1.0` was tagged), so the Linux asset attaches
  automatically from the next tag.
- [`gh`](https://cli.github.com/), authenticated (`gh auth login`), for `scripts/install.sh`, which
  downloads through `gh release download`. Release assets are public, so you can also fetch a
  tarball with a plain `curl` and pass its path to the script (see below).
- For the source install: Rust (see `rust-toolchain.toml`, currently 1.85+), a C compiler
  (`rusqlite` builds SQLite from source via its `bundled` feature).
- At least one configured model provider to actually run a turn — see "Provider setup" below.

## Install

### 1. Release tarball + install script (recommended)

```sh
gh auth login                 # once, if you haven't already (the script uses gh)
scripts/install.sh
```

This detects your OS/arch, downloads the matching `tm-<target-triple>.tar.gz` release
asset via `gh release download`, verifies its `.sha256`, and installs into
`${PREFIX:-$HOME/.local}` (`bin/tm`, `share/tm/web`).

To install from a tarball you already have — e.g. one built locally with `mise run release`, or
downloaded by hand — pass its path instead of downloading:

```sh
scripts/install.sh path/to/tm-<target-triple>.tar.gz
```

If `$PREFIX/bin` isn't already on your `PATH`, the script prints the line to add to your shell
profile.

### 2. From source with cargo

```sh
cargo install --git https://github.com/alliecatowo/ticket-master.git tm-cli --locked
```

`tm-cli` is the package name; it builds one binary, `tm`. This clones the whole workspace (its
path-dependency sibling crates need to be present to resolve) and builds just this package.

This path has **no bundled web client** — `tm serve` falls back to nothing to serve at `/app/`
unless you point it at a build with `--web-dir`/`TM_WEB_DIR` (see `clients/web/`).

### 3. Dev checkout with mise (for working on tm itself)

```sh
git clone https://github.com/alliecatowo/ticket-master.git
cd ticket-master
mise install
mise run build        # just the tm binary, or `mise run build:all` for the whole workspace
```

See this repo's own `CLAUDE.md` for the full set of `mise run` tasks (test, clippy, verify, the
TUI, `tm serve`, ...).

## Provider setup

`tm`'s interactive/chat path (bare `tm`, `tm -p "<prompt>"`) and the scheduler's dispatched ticket
turns (`tm run`, `tm sched run`) need at least one configured model provider for the `coder.fast`
role. Two ways to get one:

- **`ANTHROPIC_API_KEY`** — the default: `coder.fast` routes to `anthropic`/`claude-sonnet-5`.
- **DevPass** (any OpenAI-compatible gateway) — set **all three** of `DEVPASS_API_KEY`,
  `DEVPASS_BASE_URL`, `DEVPASS_MODEL` to make DevPass the default instead of Anthropic. A partial
  set of the three does nothing (falls straight through to the Anthropic-only default).

`tm-provider` supports many more backends beyond these two — OpenAI, OpenRouter, GitHub Models,
Groq, Cerebras, DeepSeek, Mistral, xAI, Together, Fireworks, Hugging Face, Gemini, Ollama/LM
Studio/llama.cpp (local), Azure OpenAI, Bedrock, Vertex, Cloudflare Workers AI — each with its own
env vars and a free-tier note. See `docs/providers.md` for the full reference; a role's
`providers.toml` candidate can name any of those slugs, not just the two above.

A `.env` file in the current directory is loaded automatically (via `dotenvy`, and never
overriding a variable already set in the real environment), so provider keys can live there
instead of your shell profile.

Check what's actually configured, and prove it works with a real (billed) call:

```sh
tm provider detect      # which backends this environment currently has credentials for
tm provider test        # one tiny real completion through each; `tm provider test <slug>` for just one
```

## First run

```sh
cd some-project          # any git repo works; without one tm falls back to a global project
tm                       # the interactive TUI chat
tm tickets                # straight to the tickets screen (Esc for a fresh chat)
tm doctor                 # sanity-check the environment
tm serve --open            # HTTP API + web client at /app/, opens a browser
```

To use `tm` as an MCP server from Claude Code, initialize a project first (`tm mcp` resolves the
project from the working directory the way every other subcommand does, so one has to already
exist there):

```sh
tm init
claude mcp add --transport stdio tm -- tm mcp
```

## Upgrade

Re-run `scripts/install.sh` (against a newer release, or a newer local tarball) — it replaces
`$PREFIX/bin/tm` and `$PREFIX/share/tm/web` in place. From a cargo install:
`cargo install --git https://github.com/alliecatowo/ticket-master.git tm-cli --locked --force`.

## Uninstall

```sh
rm -rf "${PREFIX:-$HOME/.local}/bin/tm" "${PREFIX:-$HOME/.local}/share/tm"
```

(`cargo uninstall tm-cli` instead, for a cargo install.)

This leaves `tm`'s own state alone: a repo-scoped project's `.tm/` directory inside that repo, and
global-scope state under `$TM_HOME` (default `$HOME/.tm`) — remove those by hand if you want a
clean slate.
