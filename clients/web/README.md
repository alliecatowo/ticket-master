# `clients/web`: the project canvas

Vite + React + TypeScript, per `SPEC.md` §18.2. A browser surface for working a project end to
end against the real `tm-server` API: dispatch work, watch it move, review and accept it, retry
what escalated. Everything here was checked against a live `tm serve`. Run the commands below to
check it again.

## Deviation from SPEC.md §18.1: no `clients/ts/` yet

`SPEC.md` §18.1 describes a generated `@ticketmaster/client` package at `clients/ts/`, shared with
the VS Code extension. This app doesn't consume it. It carries its own hand-written, framework-agnostic REST +
SSE client in `src/api/` (`client.ts`, `live.ts`, `store.ts`, `types.ts`, `identity.ts`), laid
out so that moving it into a shared package later is a copy, not a rewrite. Only `client.ts`
calls `fetch`. The wire types are written by hand from `crates/tm-server/src/routes.rs` and
`crates/tm-events/src/payload.rs`. There is no `pnpm gen` step and no drift check, because
`GET /schema` is itself only a partial, hand-written document.

## The model: the TUI's tickets screen, in a browser

The home screen is the same model as the terminal tickets screen (D-019 §2, "Implemented: tickets
screen"): Claude Code's agent view, with tickets as its rows. `src/views/ticketsModel.ts` is a
port of `crates/tm-cli/src/tickets/overview.rs` (`group_for`, `summary_for`) and of the peek's
`choices` in `crates/tm-cli/src/tui/tickets_view.rs`, so both surfaces group and word a ticket
the same way:

| Group | States |
| --- | --- |
| Needs input | `escalated`; `leased`/`running` while an approval request is open |
| Working | `leased`, `running`, `verifying`, `auditing` |
| Ready for review | `submitted` |
| Queued | `draft`, `blocked`, `ready`, `rework`, `replan`, `recovery` |
| Completed | `closed`, `cancelled` |

| State | Actions (numbered as in the TUI) |
| --- | --- |
| `submitted` | 1 Accept · 2 Reject (requires a reason) |
| `escalated` | 1 Retry · 2 Retry with guidance (requires guidance) |
| `draft` | 1 Queue it |
| any non-terminal state | Cancel ticket… (asks for confirmation; the reason is optional) |

Differences from the TUI, on purpose:
- Row titles get 44 columns instead of 32.
- A queued ticket always reads "waiting for a worker". The client doesn't read `workers` from
  `GET /health` yet (see "Not built").

## What is built

- **Tickets** (`/`, `src/views/TicketsView.tsx`): the five groups, each row showing its worker
  glyph, id, title, one-line summary, state and age. Completed folds after five.
- **Dispatch box**: "Describe a task for a background worker". Enter sends; Shift+Enter adds a
  newline.
  - It `POST /tickets` with only `{kind: "work", objective, actor}`, then activates the ticket.
  - If the ticket is created but activation fails, the box says so and links the draft. The
    ticket isn't lost.
- **Ticket** (`/ticket/:id`, `src/views/TicketView.tsx`): the full page for one ticket.
  - The state's actions come first, in a panel, with `1`/`2` as keyboard shortcuts when no text
    field has focus.
  - Then: the objective, state and attempts (`n of max`), each failure with its class and
    detail, and the submission summary with its evidence.
  - The event timeline hides bookkeeping events unless you ask for all of them.
  - Details (worker, waiting time, parent, children, dependencies) and presence sit alongside.
- **Review** (`/review`, `src/views/ReviewView.tsx`): every submission waiting for a human,
  oldest first. Each shows its summary, evidence and objective, with Accept and Reject inline.
  Escalated tickets are listed underneath with links. The nav shows a count badge.
- **Identity** (`src/api/identity.ts`):
  - The client acts as `human:web` by default. Click the "acting as" chip to set a name, which
    is kept in `localStorage` under `tm.web.actor`.
  - `normalizeHandle` strips a pasted `human:` prefix and control characters, joins words with
    `-`, and caps the handle at 40 characters.
  - `TicketmasterClient` is the only place that turns the handle into an actor, so every body it
    sends is `human:<handle>`. Accept, reject and retry are human-only on the server, and the web
    client can't send them as anyone else.
- **Live updates** (`src/api/live.ts`, `src/api/store.ts`):
  - `LiveProject` reads `GET /state`, then streams `GET /events?from=<cursor>`. It uses `fetch`
    and parses SSE frames itself, because `EventSource` can't send a bearer token.
  - When the stream drops or ends, the connection pill turns to "Reconnecting". A banner
    appears with a countdown (backoff 1s, 2s, 3s, then every 5s) and a "Retry now" button.
  - Each reconnect re-reads `/state` and resumes from the last event it applied.
  - `ProjectStore` keeps two cursors. Tickets, leases and evidence come from the snapshot and
    are patched only by events after it. History (the timeline, submission summaries, escalation
    reasons) is folded from the log exactly once, so a reconnect never duplicates a timeline
    row. Until the replay reaches the first snapshot's head (`historyReady`), an empty timeline or
    missing summary reads "Reading the event log…", not "none".
  - Mutations don't update local state optimistically. The resulting events arrive over the
    stream.
- **Errors**:
  - A refused action shows the server's own sentence next to the button, e.g. "Not allowed: …"
    for a 403 or "Can't do that now: …" for a 409.
  - A network failure reads "Couldn't reach tm serve".
- **Kept from before**:
  - **Status**: "since you left", built from events seen since the client connected.
  - **Backlog**: ready, blocked and active tickets, with filters.
  - **Presence bar**: polls `GET /presence` every 5s.

## Not built

- These routes are placeholders (`src/views/NotBuilt.tsx`) that say they're unbuilt:
  - **Graph**
  - **Rooms**
  - **Decisions** (`GET /decisions` isn't called)
  - **Providers** (`GET /providers` and `/harness` aren't called)
- No mutations beyond dispatch and the ticket actions above: no dependency editing, milestones,
  decisions, approvals, evidence attachment or lease handling.
- No diff or artifact viewer. Evidence shows its kind, artifact id and summary only.
- No token UI. `new TicketmasterClient({ token })` works, but nothing in the app asks for a token
  (fine for the default loopback, no-token `tm serve`).
- Every page load replays the whole event log from `seq` 0 to build the timelines, which gets
  slower as the log grows. The server now has what's needed to stop that; the client hasn't
  adopted it yet:
  - `GET /tickets/{id}/events?after=<seq>&limit=<n>` returns `{events, next}`: one page of that
    ticket's own events, oldest first, in the same wire shape as the other event lists. Pass
    `next` back as `after` until it is `null`. `after` is exclusive, like `/events?from=`.
  - `GET /health` and `GET /state` include `workers: true|false`: whether this `tm serve` works
    ready tickets. It is `false` under `--no-workers`, and also when the runner couldn't start.
    With it, "waiting for a worker" could say "no worker is running" instead.
  - `GET /events` honors a `Last-Event-ID` header over `?from=`, and sends `:` keep-alive
    comments while idle. `parseSseFrame` already ignores those.
  - A `reject` with a blank reason is now a 400. The UI already blocks it.
- If the client reconnects to a *different* project whose log is longer than the one it was
  reading, it keeps the old timelines. It only detects a server log that is behind its cursor.

## Running it

```
pnpm install
pnpm build                     # writes dist/, which tm serve serves at /app/
tm serve --open                # from the project; works tickets in-process
```

To develop against a running server, use `tm serve --addr 127.0.0.1:4173`, then `pnpm dev`. The
Vite dev server on port 5173 proxies the API to port 4173. Set `VITE_TM_SERVER` to point the
proxy somewhere else. The app is built with `base: "/app/"` and routes under
`<BrowserRouter basename="/app">`, so its `/graph`, `/decisions` and `/providers` pages never
collide with the API endpoints of the same names.

## Commands and results

```
pnpm test && pnpm typecheck && pnpm build
```

- `pnpm test` (`vitest run`): 85 tests in 9 files.
  - `src/views/ticketsModel.test.ts`: the group for all 14 states, approval overrides, the
    actions each state allows, Cancel availability, summaries, ordering, and ages.
  - `src/api/client.test.ts`: the exact `POST /tickets/:id/transition` body for every action,
    the dispatch create-then-activate sequence, SSE frame parsing across chunk boundaries, and
    `describeError` for 403, 409, 422 and network errors.
  - `src/api/identity.test.ts`: the `human:web` default, normalization, and storage.
  - `src/api/live.test.ts`: reconnecting after a dropped stream without duplicating history,
    and "Retry now".
  - `src/api/store.test.ts`: event folding.
  - `src/views/timelineModel.test.ts`: how timeline events read.
  - `src/App.test.tsx`: the rendered flows (home groups, dispatch, retry with guidance, cancel
    confirmation, inline review, a refused action, the empty state).
- `pnpm typecheck` and `pnpm build` (`tsc --noEmit && vite build`): clean.
- Live check against `tm serve` with `TM_TEST_MOCK_PROVIDER=1`, driven with Playwright and
  system Chrome:
  - Dispatch works. The mock worker fails three attempts and the ticket escalates. Retry with
    guidance appends the guidance to the objective.
  - Queue a draft, accept from Review, and reject (blocked while the reason is empty) all work.
    The name set in the chip shows up as the actor in the log.
  - Stopping and restarting the server shows the banner, then the client goes Live again with an
    identical timeline.
  - At 390px wide there is no horizontal overflow.
