<div align="center">

# 🎟️ Ticketmaster (`tm`)

### The interactive coding agent with a ticket-powered background crew

[![CI](https://github.com/alliecatowo/ticket-master/actions/workflows/ci.yml/badge.svg)](https://github.com/alliecatowo/ticket-master/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/alliecatowo/ticket-master)](https://github.com/alliecatowo/ticket-master/releases/latest)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green)](#-license)

Chat with an agent live in your terminal — then hand follow-up work to background
workers as **tickets**, review their submissions, and keep shipping.

**[🌐 Website](https://alliecatowo.github.io/ticket-master/)** · **[▶ Live demo](https://alliecatowo.github.io/ticket-master/demo/)** · **[📸 Showcase](#-showcase)** · **[⚡ Quickstart](#-quickstart)** · **[🎛️ Slash commands](#%EF%B8%8F-slash-commands)**
· **[🤖 Providers](#-providers)** · **[🗺️ Docs map](#%EF%B8%8F-docs-map)**

</div>

---

## 📸 Showcase

> Every clip below is the **real `tm` binary** (`v0.1.1`) driven against a disposable
> project with a **live provider** — no mocks, no scripted turns.
> Full session + reproduction notes: [`docs/showcase/`](docs/showcase/).

![Live turn: @calc.py completion, file reads, live crash repro, ticket-aware answer](docs/showcase/tui-live.gif)

<details>
<summary><b>🎬 More clips — accept a submission, dispatch a background worker</b></summary>
<br/>

| Review: peek + accept | Dispatch: worker runs live |
| --- | --- |
| ![Tickets screen: peek T-2's submission, press 1, it closes](docs/showcase/tui-accept.gif) | ![Dispatch T-3, watch its worker run doctest and pytest live in the row](docs/showcase/tui-dispatch.gif) |
| `/tickets` → `Space` peek → `1` accept | dispatch input → worker verifies in the row |

Prefer video? Same clips as WebM:
[`tui-live.webm`](docs/showcase/tui-live.webm) ·
[`tui-accept.webm`](docs/showcase/tui-accept.webm) ·
[`tui-dispatch.webm`](docs/showcase/tui-dispatch.webm) ·
full session [`tui-live.cast`](docs/showcase/tui-live.cast) (`asciinema play` it).

</details>

---

## 📦 Install

**Homebrew (macOS and Linux):**

```sh
brew install alliecatowo/tap/ticket-master   # installs the `tm` CLI
```

<details open>
<summary><b>Option A: download a release</b></summary>
<br/>

Every [GitHub release](https://github.com/alliecatowo/ticket-master/releases) ships a tarball per
platform (`tm-aarch64-apple-darwin`, `tm-x86_64-apple-darwin`, `tm-aarch64-unknown-linux-gnu`,
`tm-x86_64-unknown-linux-gnu`) with a `.sha256` beside each. The tarball holds `bin/tm` and the
built web client in `share/tm/web/`, which `tm serve` finds on its own:

```sh
curl -fLO https://github.com/alliecatowo/ticket-master/releases/latest/download/tm-aarch64-apple-darwin.tar.gz
tar -xzf tm-aarch64-apple-darwin.tar.gz
./tm-aarch64-apple-darwin/bin/tm --version && ./tm-aarch64-apple-darwin/bin/tm init
```

Or let `scripts/install.sh` do the download, checksum verification and layout for you, into
`${PREFIX:-$HOME/.local}`:

```sh
gh auth login   # once: the script downloads via the gh CLI
scripts/install.sh
```

`mise run release` builds the same tarball layout locally. See [`docs/install.md`](docs/install.md)
for the full guide: upgrading, uninstalling, and installing from a local tarball.

</details>

<details>
<summary><b>Option B — build from source</b></summary>

Requires Rust 1.85+ ([`mise`](https://mise.jdx.dev/) provides it: `mise install`):

```sh
git clone https://github.com/alliecatowo/ticket-master.git && cd ticket-master
mise install
mise run build            # debug build of just the `tm` binary
./target/debug/tm init
```

Prefer an optimized binary? `mise run build:release` (≈ `cargo build --release -p tm-cli`).

</details>

---

## ⚡ Quickstart

<details open>
<summary><b>Prerequisites</b></summary>

- Rust 1.85+ (managed via [`mise`](https://mise.jdx.dev/): `mise install`)
- One configured provider — e.g. `ANTHROPIC_API_KEY`, or the DevPass trio
  (`DEVPASS_API_KEY` + `DEVPASS_BASE_URL` + `DEVPASS_MODEL`). See [`docs/providers.md`](docs/providers.md).

</details>

```sh
mise run build            # build just the `tm` binary
./target/debug/tm init    # create a project in the current directory
./target/debug/tm         # open the TUI on the chat
```

| Want… | Run… |
| --- | --- |
| One-shot scriptable turn | `tm -p "explain this repo"` |
| Plain linear output (no TUI) | `tm --plain` |
| Tickets as JSON | `tm tickets --json` |
| Health check | `tm doctor` |
| Full gate before you call it done | `mise run verify` |

---

## 🎛️ Slash commands

<details>
<summary><b>Click to expand the full command list</b></summary>

| Command | What it does |
| --- | --- |
| `/help` | Keyboard shortcuts |
| `/clear`, `/resume`, `/compact` | Fresh chat · continue a past one · summarize to free context |
| `/model [name]` | Show or switch the model |
| `/level fast|deep` | Choose the fast or deep chat role |
| `/connect`, `/provider`, `/config` | Set up providers, inspect routing, and view harness settings |
| `/status` | Model, provider, directory, mode and ticket |
| `/cost` | Tokens this session used, turn by turn |
| `/init` | Write an `AGENTS.md` for this project |
| `/bg [task]` | Hand work to a background worker as a ticket |
| `/context` | Token use by section, plus the attached ticket's prefetched context |
| `/todos` | Toggle the task checklist |
| `/tickets` | Background tickets and workers |
| `/attach <ticket>`, `/detach` | Tie this chat to a ticket (or untie it) |
| `/decide <text>` | Record a project decision |
| `/exit` | Quit |

</details>

`/level` is an interactive chat command only; there is no noninteractive CLI tier-setting
command. Use `/level fast` or `/level deep` in chat; deep requires a configured `coder.deep`
provider/model route and reports an actionable error when none exists. `tm ticket dispatch "..."` creates
and activates a worker ticket; `tm ticket new` alone creates a draft, which must be activated.

<details>
<summary><b>⌨️ Keyboard highlights</b></summary>

- `/` commands · `?` shortcuts · `!` shell mode · `@` file paths · `←` `←` tickets
- `Enter` send · `Shift+Enter` newline · `↑`/`↓` history · `Ctrl+R` search history
- `Ctrl+O` transcript · `Ctrl+T` tasks · `Shift+Tab` cycle auto/plan/ask · `Esc` interrupt

</details>

---

## 🤖 Providers

`tm` routes turns through a provider fabric — **21 backends** (Anthropic, OpenAI,
OpenRouter, GitHub Models, Gemini, Ollama/LM Studio/llama.cpp local, Azure, Bedrock,
Vertex, Cloudflare, DevPass, …).

```sh
tm provider detect   # which backends are configured in THIS environment
tm provider list     # role → candidate routing from providers.toml
tm provider default                  # show the project's default model
tm provider default ollama/qwen2.5-coder  # set it; `clear` removes it
tm provider test devpass  # one tiny billed smoke test
```

`tm init` creates project-scoped `harness.toml` (harness behavior) and `providers.toml`
(provider role routing). `tm harness set` validates and persists a candidate; promote the resulting
harness epoch to apply it. Chat model defaults are stored separately in `default-model.json`.
Sessions and background workers use the same project provider table;
local backends require a concrete model route in `providers.toml`; tm does not guess a local model.
`tm provider list` reports configured routing candidates, not whether credentials are present
or a model can answer; use `tm provider detect` for environment availability and `tm provider test`
for a real (potentially billed) round trip. Saving a default model does not verify service access.

Full matrix and configuration details: [`docs/providers.md`](docs/providers.md).

---

## 🗺️ Docs map

<details>
<summary><b>Where everything lives</b></summary>

| Path | What's there |
| --- | --- |
| [`SPEC.md`](SPEC.md) | Binding project philosophy (§0) + full spec |
| [`docs/install.md`](docs/install.md) | Full install guide: release download, `scripts/install.sh`, source, provider setup |
| [`docs/decisions/`](docs/decisions/) | Architecture decision records (`D-NNN`) |
| [`docs/providers.md`](docs/providers.md) | Provider matrix + wire-name mapping |
| [`docs/showcase/`](docs/showcase/) | Real TUI recordings + friction log |
| [`docs/wiki/`](docs/wiki/) | Generated docs (regenerate: `mise run docs:wiki`) |
| [`docs/backlog.md`](docs/backlog.md) | What's parked / what's next |
| [`CLAUDE.md`](CLAUDE.md) | Dev-workflow bible: mise tasks, worktrees, TUI reference |

</details>

## 📄 License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

---

<div align="center">

`mise run verify` is the gate: fmt check + `clippy -D warnings` + workspace tests + hygiene.
Run it before considering any change done. ✅

</div>
