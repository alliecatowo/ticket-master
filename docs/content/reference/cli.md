+++
title = "CLI reference"
weight = 1
+++

Bare `tm` opens the interactive agent. `tm -p "<prompt>"` runs one prompt to completion and exits
(exit code 0 on a reply, 2 when the agent failed the task, 4 when it ran out of budget, 64 for a
malformed command line). Run `tm --help` or `tm <command> --help` for every flag.

Global flags: `--json`, `--quiet`, `--no-color`, `--plain`, and `--project <root>`.
`--continue` resumes the most recent conversation and `--resume <id>` a saved one.

| Command | What it does |
| --- | --- |
| `tm init` | Create a project (`.tm/`) in the current directory |
| `tm attach` | Bring an existing repository into a new or existing project |
| `tm genesis` | Turn a natural-language prompt into a running project |
| `tm status` | "Since you left" report: what changed and what needs attention |
| `tm stats` | Usage rollups by ticket, day, model or tool |
| `tm doctor` | Invariants, hash-chain check, index health, permission probes |
| `tm ticket` | Ticket lifecycle: list, show, new, dispatch, edit, activate, accept, reject, retry, close, cancel, reopen |
| `tm tickets` | Open the tickets view (or print tickets with `--json`) |
| `tm dep`, `tm milestone`, `tm decision` | Dependency edges, milestones, durable decision records |
| `tm sched`, `tm lease` | Scheduler loop and ticket/resource leases |
| `tm run <ticket>` | Execute one ticket to completion in the foreground |
| `tm search`, `tm symbol`, `tm history` | Code index search, symbol-level intelligence, git history queries |
| `tm docs` | Documentation as project state: staleness check and reconciliation |
| `tm provider` | `detect`, `list`, `status`, `default`, `test`, `reset` |
| `tm auth` | Interactive login for a provider credential |
| `tm harness` | Harness configuration and epoch history |
| `tm bench` | Repository-local benchmark tasks |
| `tm workflow` | Reusable parameterized recipes that expand into a ticket graph |
| `tm mirror` | External tracker mirrors (GitHub, Linear, Jira, GitLab) |
| `tm templates` | Project scaffolding templates |
| `tm serve` | HTTP/SSE API, optionally serving the web client at `/app/` |
| `tm mcp`, `tm acp` | Serve the project over MCP or Agent Client Protocol on stdio |
| `tm events` | Tail, inspect, replay and verify the durable event log |
| `tm browser`, `tm computer` | Drive a headless browser or the real desktop |
| `tm project` | Show where project state lives; list global projects |
| `tm wiki` | Generate wiki pages from live project state |

## Slash commands (in the chat)

| Command | What it does |
| --- | --- |
| `/help` | Keyboard shortcuts |
| `/clear`, `/resume`, `/compact` | Fresh chat, continue a past one, summarize to free context |
| `/model [name]` | Show or switch the model |
| `/level fast\|deep` | Choose the fast or deep chat role (deep needs a configured `coder.deep` route) |
| `/connect`, `/provider`, `/config` | Set up providers, inspect routing, view harness settings |
| `/status` | Model, provider, directory, mode and ticket |
| `/cost` | Tokens used this session, turn by turn |
| `/init` | Write an `AGENTS.md` for this project |
| `/bg [task]` | Hand work to a background worker as a ticket |
| `/context` | Token use by section |
| `/todos` | Toggle the task checklist |
| `/tickets` | Background tickets and workers |
| `/attach <ticket>`, `/detach` | Tie this chat to a ticket, or untie it |
| `/decide <text>` | Record a project decision |
| `/exit` | Quit |
