# `clients/web` — the project canvas

Vite + React + TypeScript, per `SPEC.md` §18.2. This is a **scaffolding-depth** V0: a real,
working vertical slice against the real `tm-server` API, not feature parity with the CLI.
Everything in this file is accurate as of this commit — run the commands yourself to re-verify.

## Deviation from SPEC.md §18.1: no `clients/ts/` yet

`SPEC.md` §18.1 describes a generated, shared `@ticketmaster/client` package at `clients/ts/`
that both the web app and the VS Code extension would consume as a `file:../ts` workspace
dependency. **That package does not exist in this repository** — there is no `clients/ts/`
directory, no `pnpm-workspace.yaml`, and no `GET /schema`-driven codegen wired up anywhere yet.

This task was scoped to `clients/web/` only ("only create/edit files under
`clients/web/`... never touch... another client"), so rather than create `clients/ts/` myself
(out of scope) or silently hand-roll `fetch` calls all over the app (against the spirit of
§18.1), I wrote a single, self-contained REST + SSE client and a pure `ProjectStore` inside
`src/api/` (`client.ts`, `store.ts`, `types.ts`). It is deliberately framework-agnostic (no React
imports) and structured the way `clients/ts/src/index.ts` would be, so extracting it into a real
`clients/ts/` package later is a copy, not a rewrite. Every mutation in this app goes through
`TicketmasterClient` in `src/api/client.ts` — nothing else calls `fetch` directly.

The wire types in `src/api/types.ts` are **hand-written**, not generated from `GET /schema`
(`tm-server`'s schema endpoint is itself only a hand-written partial document today — see
`crates/tm-server/src/routes.rs`'s `get_schema`). There is no `pnpm gen` step and no drift check.

## What is built

- **Status** (`src/views/StatusView.tsx`) — a "since you left" report (closed / repaired /
  reconciled / benchmarked / needs-you), built client-side from the live event log
  (`src/views/statusModel.ts`, unit-tested). `tm-server` has no dedicated report endpoint, so
  this is a heuristic grouping over `EventKind`s seen since the client connected — it does not
  see history from before that.
- **Backlog** (`src/views/BacklogView.tsx`) — ready / blocked / active groups, filterable by
  milestone, kind, and executor role (`src/views/backlogModel.ts`, unit-tested).
- **Ticket** (`src/views/TicketView.tsx`) — objective, state, authority, milestone, priority,
  attempts, failures, and this session's event history for the ticket. Presence and path leases
  scoped to the ticket via `PresenceBar`. Exposes the one live mutation (see below).
- **Live data**: on load, seeds a `ProjectStore` from `GET /state`, then keeps it live via
  `TicketmasterClient.subscribe(fromSeq)` — an async generator over `GET /events?from=`, parsing
  raw SSE frames (not the `EventSource` API, since that can't set an `Authorization` header) and
  reconnecting with the last-seen `seq` on any stream error.
- **One real mutation**: the Ticket view's Activate / Close / Reopen buttons call
  `POST /tickets/:id/transition` through `TicketmasterClient.transition`. The optimistic path is
  "do nothing locally, let the SSE stream's resulting `ticket.state_changed` event reconcile the
  store" — i.e. reconciliation is real (the same event-log path every other client update takes),
  there is no separate speculative local-state branch to accidentally diverge from it.
- **Presence and path leases** (`src/components/PresenceBar.tsx`) are rendered on both Status
  (project-wide) and Ticket (scoped to that ticket), polling `GET /presence` every 5s — presence
  is a TTL-swept snapshot server-side, not an event-sourced projection, so polling (not the event
  stream) is the correct source for it.
- **Routing** via `react-router-dom`; an actor id (`human:web` by default, editable in the nav and
  persisted to `localStorage`) is attached to every mutation as `actor`.

## What is explicitly NOT built

Routed as placeholders (`src/views/NotBuilt.tsx`) that state plainly they are not built, per
`SPEC.md` §18.2's table:

- **Graph** — dependency/parent-child graph, milestones as cuts, loop edges.
- **Rooms** — per-milestone activity, comments, live presence beyond the ticket-scoped bar above.
- **Review** — diffs and evidence awaiting audit, approve/reject.
- **Decisions** — decision log with supersession chains (`GET /decisions` is not called anywhere
  in this app).
- **Providers** — role→model routing, quota headroom, breaker state, spend
  (`GET /providers`/`GET /harness` are not called anywhere in this app).

Also not built, called out because it's easy to assume otherwise:

- No ticket creation, dependency editing, lease acquisition, evidence attachment, milestone,
  decision, or approval mutations — only the ticket lifecycle transitions listed above.
- No generated types / `clients/ts/` package (see the deviation note above).
- `ProjectStore.apply` only understands `ticket.created`, `ticket.state_changed`,
  `ticket.closed`, `ticket.cancelled`, and `ticket.escalated`; every other event kind is recorded
  in the recent-events buffer (for Status/history) but does not mutate a ticket's materialized
  fields. A `ticket.created` event for a ticket the store hasn't seen yet produces a minimal
  partial record (id/objective/state only) until the next full `GET /state` refetch.
- No offline/error UI beyond a connection-status dot and a generic mutation-error message.
- No auth UI: the client will send a bearer token if one is supplied via
  `new TicketmasterClient({ token })`, but nothing in the app currently prompts for or stores one
  (fine for the default loopback-bind, no-token `tm serve` story; would need work to use against
  a non-loopback bind).

## Running it

Against a real `tm serve`:

```
tm serve --addr 127.0.0.1:4173   # from the project root, in another terminal
pnpm install
pnpm dev                          # Vite dev server on 5173, proxying the API to :4173
```

Set `VITE_TM_SERVER` to point `pnpm dev`'s proxy at a different `tm serve` address.

Served by `tm serve` itself: `pnpm build` writes `dist/`, and `tm serve` serves it at `/app/`
(`/` redirects there; any other `/app/...` path gets `index.html`, so reloads and pasted links
work). The app is built with `base: "/app/"` and routes under `<BrowserRouter basename="/app">`,
so its `/graph`, `/decisions` and `/providers` pages never collide with the API's endpoints of the
same name. `tm serve` finds the build via `--web-dir`, then `TM_WEB_DIR`, then this directory's
`dist/` in the checkout `tm` was built from; `tm serve --open` opens it in a browser.

## Commands and results

```
pnpm install && pnpm test && pnpm build
```

- `pnpm test` (`vitest run`): **21 tests passed** across 4 files — `src/api/store.test.ts` (the
  `ProjectStore` event-application logic), `src/views/statusModel.test.ts`,
  `src/views/backlogModel.test.ts` (pure view-model logic), and `src/App.test.tsx` (a render
  smoke test: nav renders, Status/Backlog render with a seeded ticket, an unbuilt view shows its
  stub notice).
- `pnpm build` (`tsc --noEmit && vite build`): typechecks clean and builds `dist/` successfully.
