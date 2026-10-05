+++
title = "Oversight and approvals"
weight = 4
description = "Permission modes, the oversight.toml approval policy, the sh -c shell.string class added in 0.1.4, and the trust gate on repo-committed config."
+++

Three separate layers decide what an agent may do. They compose; a later layer never loosens an earlier one.

1. **Authority** answers "is this possible at all?" Every ticket carries one, and a worker's is narrow
   (see [Tickets and workers](@/concepts/tickets-and-workers.md)). A denial is final.
2. **Oversight** answers "does a human need to see this first?" It is the `oversight.toml` policy below.
3. **Permission mode** is the interactive chat's own dial, for your live session.

## Permission modes (the chat)

`Shift+Tab` cycles the chat through three modes, and `/permissions auto|plan|ask` sets one directly:

| Mode | Behavior |
| --- | --- |
| `auto` | acts on its own: edits, commands and git run without asking |
| `plan` | read-only: investigates and proposes a plan, changes nothing |
| `ask` | asks before every edit, command, git operation or pty keystroke |

When the chat asks, the prompt takes `1`, `2` or `3`.

## oversight.toml

`oversight.toml` lives at the **root of your repository**, next to your code (not inside `.tm/`), so it is
reviewed in pull requests like any other config. It lists **action classes** that must stop and ask a human
before they run, plus an optional spend limit:

```toml
approval_required = ["shell.string", "git.push", "git.merge"]
spend_over_micros = 5000000   # ask before a single spend over $5.00
```

A class matches by exact name or by dotted prefix, so `"git"` covers every `git.*` action. With no
`oversight.toml`, nothing extra is asked: authority still applies, but oversight is off.

Unknown keys are an error, not a no-op. A misspelled `approval_requird` is rejected rather than silently
producing a policy that gates nothing.

### Action classes

| Class | What it covers |
| --- | --- |
| `repository.read`, `repository.write` | reading and writing paths |
| `shell.run` | running a command (argv form) |
| `shell.string` | running a `sh -c "..."` string (see below) |
| `git.commit`, `git.branch`, `git.merge`, `git.push`, `git.force_push` | git operations |
| `network.fetch`, `browser.navigate` | fetching a URL, driving the headless browser |
| `tickets.*`, `project.*` | ticket and project changes, such as `tickets.reopen` |
| `budget.spend` | spending (also gated by `spend_over_micros`) |
| `resources.spawn_worker` | starting another worker |
| `computer.input`, `computer.capture`, `computer.clipboard` | desktop control |
| `pty.spawn`, `pty.send`, `pty.control` | interactive terminal sessions and keystrokes |

### shell.string: approving `sh -c`

New in **0.1.4**. A command given as an argv list (`["cargo", "test"]`) can be checked segment by segment
against a worker's shell allow-list. A `sh -c "..."` string is different: it is a whole program, so
redirections, expansions, quoting and `$(...)` substitution are invisible to a per-command check. Listing
the pseudo-class `shell.string` makes every `sh`, `bash`, `zsh`, `dash` or `ash` `-c` command stop for a
human first, with the reason

```
shell.run: shell string (`sh -c`) needs human approval
```

The argv form is unchanged, and a denial is never softened into an approval request. Without
`shell.string` in your policy, a `sh -c` line still has to pass the allow-list on every one of its
segments; the pseudo-class is how you require a person on top of that.

Separately, `tm serve` now wraps its routes in a 60 second request timeout (HTTP 408), except the
event stream and the approvals long-poll.

### How an approval flows

When a tool call hits a gated class, the run records an `approval.requested` event and **suspends**. The
tickets screen shows the ticket under **Needs input**, and a desktop notification fires (set `TM_NOTIFY=0`
to silence it). You answer in the TUI, or over HTTP (`GET /approvals` lists pending requests and
`POST /approvals/{id}/decide` answers one); the run then resumes. A class you approve "for the session" is
remembered in memory only and never written to `oversight.toml`.

## The trust gate

A cloned repository can commit `hooks.toml`, `acp.toml` and `oversight.toml`, and the first two can run
commands. So none of them take effect until you have reviewed and trusted them:

```
$ tm trust --status
Not trusted (oversight.toml). Run `tm trust` after reviewing them.
$ tm trust
--- oversight.toml ---
approval_required = ["shell.string", "git.push"]
spend_over_micros = 5000000

Trusted oversight.toml for ~/api. Editing any of them (for example with git pull) asks again.
```

Trust is recorded against a hash of the files in your `$TM_HOME`, never in the repo. Any later edit,
including a `git pull`, puts the workspace back to untrusted: the output reads "Changed since you
trusted them" until you review again. `tm trust --revoke` withdraws it. An untrusted `oversight.toml`
refuses to load rather than falling back to the more permissive default.
