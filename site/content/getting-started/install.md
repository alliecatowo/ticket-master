+++
title = "Install"
weight = 1
+++

`tm` is a single Rust binary (package `tm-cli`, binary name `tm`) plus an optional built web client.
It is **not** published to crates.io. Install it with Homebrew (`brew install alliecatowo/tap/ticket-master`), from a GitHub release, or from source.

## Requirements

- macOS or Linux.
- At present only `v0.1.0` is released, and it ships a single asset, `tm-aarch64-apple-darwin.tar.gz`
  (macOS arm64). On Linux, build from source. The release workflow builds both macOS arm64 and
  Linux x86_64 on each `vX.Y.Z` tag, so the Linux asset appears with the next tag.
- For the source install: Rust 1.85+ and a C compiler (SQLite is built from source).
- At least one configured model provider to run a turn; see [Providers](@/reference/providers.md).

## Option A: release tarball

```sh
curl -fLO https://github.com/alliecatowo/ticket-master/releases/download/v0.1.0/tm-aarch64-apple-darwin.tar.gz
tar -xzf tm-aarch64-apple-darwin.tar.gz
./tm-aarch64-apple-darwin/tm --version
```

Or clone the repo and run `scripts/install.sh` (needs an authenticated [`gh`](https://cli.github.com/)).
It detects your OS and architecture, downloads the matching asset, verifies its `.sha256` when the release
carries one, and installs into `${PREFIX:-$HOME/.local}` (`bin/tm`, `share/tm/web`). Pass a tarball path
to install from a local file instead.

## Option B: from source with cargo

```sh
cargo install --git https://github.com/alliecatowo/ticket-master.git tm-cli --locked
```

This path has no bundled web client; `tm serve` needs `--web-dir` (or `TM_WEB_DIR`) pointing at a build
of `clients/web/`.

## Option C: development checkout

```sh
git clone https://github.com/alliecatowo/ticket-master.git
cd ticket-master
mise install
mise run build
./target/debug/tm init
```

## Provider setup

Two quick ways to get a working provider:

- **`ANTHROPIC_API_KEY`**: the default; `coder.fast` routes to Anthropic.
- **DevPass** (any OpenAI-compatible gateway): set all three of `DEVPASS_API_KEY`, `DEVPASS_BASE_URL`
  and `DEVPASS_MODEL`. A partial set does nothing.

A `.env` file in the current directory is loaded automatically and never overrides variables already
set in your environment.

```sh
tm provider detect   # which backends have credentials in this environment
tm provider test     # one tiny real (billed) completion through each
```

## Upgrade and uninstall

Re-run `scripts/install.sh` against a newer release to upgrade in place, or re-run the `cargo install`
command with `--force`. To uninstall:

```sh
rm -rf "${PREFIX:-$HOME/.local}/bin/tm" "${PREFIX:-$HOME/.local}/share/tm"   # cargo install: cargo uninstall tm-cli
```

`tm`'s own state (`.tm/` in a repo, or `$TM_HOME`, default `$HOME/.tm`) is left alone.
