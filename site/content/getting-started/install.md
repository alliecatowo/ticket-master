+++
title = "Install"
weight = 1
+++

`tm` is a single Rust binary (package `tm-cli`, binary name `tm`) plus an optional built web client. The
current release is **v0.1.4**. It is **not** published to crates.io: install it with Homebrew, from a GitHub
release, or from source with `cargo install --git`.

## Requirements

- macOS or Linux.
- Each GitHub release ships `tm-<target>.tar.gz` for `aarch64-apple-darwin`, `x86_64-apple-darwin`,
  `aarch64-unknown-linux-gnu` and `x86_64-unknown-linux-gnu`, and Homebrew installs the right one.
- For the source install: Rust 1.85+ and C and C++ compilers (SQLite is built from source).
- At least one configured model provider to run a turn; see [Providers](@/reference/providers.md).

## Homebrew (macOS and Linux)

```sh
brew install alliecatowo/tap/ticket-master
tm --version
```

The formula installs the prebuilt binary and the built web client, so `tm serve` finds it with no extra flags.

## From source with cargo

```sh
cargo install --locked --git https://github.com/alliecatowo/ticket-master tm-cli
tm --version
```

`tm-cli` is the package; it installs the `tm` binary into `~/.cargo/bin`. This path has no bundled web
client; `tm serve` needs `--web-dir` (or `TM_WEB_DIR`) pointing at a build of `clients/web/`.

## Release tarball

```sh
curl -fLO https://github.com/alliecatowo/ticket-master/releases/download/v0.1.4/tm-aarch64-apple-darwin.tar.gz
tar -xzf tm-aarch64-apple-darwin.tar.gz
./tm-aarch64-apple-darwin/bin/tm --version
```

Each tarball has a `.sha256` beside it. Or clone the repo and run `scripts/install.sh` (needs an authenticated
[`gh`](https://cli.github.com/)). It detects your OS and architecture, downloads the matching asset, verifies its
checksum and installs into `${PREFIX:-$HOME/.local}` (`bin/tm`, `share/tm/web`). Pass a tarball path to install
from a local file instead.

## Development checkout

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

Upgrade with `brew upgrade ticket-master`, re-run `scripts/install.sh` against a newer release, or re-run the
`cargo install` command with `--force`. To uninstall, `brew uninstall ticket-master`, `cargo uninstall tm-cli`, or for the script install:

```sh
rm -rf "${PREFIX:-$HOME/.local}/bin/tm" "${PREFIX:-$HOME/.local}/share/tm"
```

`tm`'s own state (`.tm/` in a repo, or `$TM_HOME`, default `$HOME/.tm`) is left alone.
