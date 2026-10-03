+++
title = "Quickstart"
weight = 2
+++

## Prerequisites

- A `tm` binary (see [Install](@/getting-started/install.md)).
- One configured provider, for example `ANTHROPIC_API_KEY`, or the DevPass trio
  (`DEVPASS_API_KEY` + `DEVPASS_BASE_URL` + `DEVPASS_MODEL`). See [Providers](@/reference/providers.md).

## First run

```sh
cd some-project   # any git repo works; without one tm falls back to a global project
tm init           # create a project (.tm/) in the current directory
tm                # open the TUI chat
```

| Want | Run |
| --- | --- |
| One-shot scriptable turn | `tm -p "explain this repo"` |
| Plain linear output (no TUI) | `tm --plain` |
| Tickets as JSON | `tm tickets --json` |
| Health check | `tm doctor` |
| HTTP API and web client | `tm serve --open` |

## Hand work to a background worker

Inside the chat, `/bg <task>` creates a ticket for a background worker. From the shell,
`tm ticket dispatch "..."` creates and activates a worker ticket; `tm ticket new` alone creates a
draft, which must be activated with `tm ticket activate`. Review the result on the tickets screen
(`/tickets`): peek at a submission, then accept (`tm ticket accept`) or reject it with a reason.

## Use tm as an MCP server

Initialize a project first (`tm mcp` resolves the project from the working directory), then:

```sh
tm init
claude mcp add --transport stdio tm -- tm mcp
```

`tm acp` serves the project as an Agent Client Protocol agent over stdio for editors such as Zed.

## Keyboard highlights

- `/` commands, `?` shortcuts, `!` shell mode, `@` file paths
- `Enter` send, `Shift+Enter` newline, up/down history, `Ctrl+R` search history
- `Ctrl+O` transcript, `Ctrl+T` tasks, `Shift+Tab` cycle auto/plan/ask, `Esc` interrupt
