# @ticketmaster/client

The shared TypeScript client for the Ticketmaster server API (`SPEC.md` §18.1): typed REST
bindings, a reconnecting SSE event subscription, and a pure in-memory `ProjectStore`
materialized view. Meant to be the one place any surface (web app, VS Code extension) talks to
`tm-server`'s HTTP API.

## What's here

- `src/generated.ts` — written by `pnpm gen` from `GET /schema`. **`GET /schema` is itself
  partial today** (see the doc comment on `tm_server::routes::get_schema`): it covers only
  `TicketId`, `ParticipantId`, `TicketKind`, `TicketState`, `CreateTicketRequest` and
  `TransitionRequest`. This file renders exactly that, no more — it does not invent shape the
  schema doesn't assert.
- `src/domain.ts` — everything the rest of the wire domain needs (`Ticket`, `Lease`, `Decision`,
  `Milestone`, `Artifact`, `Evidence`, the full `EventKind`/payload catalogue, request bodies,
  `ErrorBody`/`TicketmasterApiError`), **hand-transcribed** from `tm-core`'s Rust structs and
  `tm-events`'s payload catalogue, not generated. When `GET /schema` grows to cover one of
  these, the hand-written type should move out in favor of the generated one — keeping both
  would be exactly the drift this package exists to prevent.
- `src/client.ts` — `TicketmasterClient`: one method per route in
  `tm-server/src/routes.rs::router`, plus `subscribe(fromSeq)`.
- `src/sse.ts` — the SSE frame parser and the reconnect-with-`Last-Event-ID` loop
  `subscribe` is built on. Reconnects by sending both `Last-Event-ID` and the `from=<seq>` query
  parameter the server actually implements (`tm-server/src/sse.rs` only reads `from` today), and
  locally dedupes on `seq` the same way the server's own `ResumableStream` does, so a reconnect
  overlap never yields a duplicate.
- `src/store.ts` — `ProjectStore`: a pure reducer over `WireEvent[]`, transcribed arm-by-arm from
  `crates/tm-core/src/materialize.rs::apply` (including its `TicketDefaults` placeholders and its
  documented gaps, e.g. `artifact.created`'s payload not carrying `bytes_len`/`hash`).

## What's *not* here / known gaps

- `ProjectStore` only materializes the subset of event kinds `tm-core::materialize::apply` itself
  materializes (tickets, leases, decisions, milestones, artifacts, evidence, session/presence
  participants) — the same scope `tm-core` covers, not more. Authority, resource-claim,
  provider, doc, harness and genesis events are received (and can be handled by a caller
  directly off `WireEvent`) but are no-ops in the store, matching `tm-core`'s own `_ => {}` arm.
- `Authority`, `Budget`, and `Predicate` are typed as `unknown` in `src/domain.ts` — their Rust
  wire shapes weren't pinned down for this pass; callers needing them should widen those types
  locally until `GET /schema` (or a follow-up) covers them.
- There is no CI wiring in this repo yet that runs `pnpm gen` against a live server and fails on
  drift; `pnpm gen` itself supports that (falls back to `schema/snapshot.json` when no server
  answers), but no CI job invokes it. That's the "CI check regenerates and fails on drift" half
  of §18.1's requirement, not built here.
- No browser build/bundling step (this is a plain `tsc` build to `dist/`, ESM only, targeting
  Node's `fetch`/`ReadableStream`, which are also present in every evergreen browser this project
  is likely to target — but nothing here has been run in an actual browser).

## Commands run and their results

```
$ pnpm install
Already up to date.

$ pnpm test
 ✓ test/store.test.ts (13 tests)
 ✓ test/sse-reconnect.test.ts (3 tests)
 ✓ test/client.test.ts (3 tests)
 Test Files  3 passed (3)
      Tests  19 passed (19)

$ pnpm build
$ tsc -p tsconfig.json
(clean exit, dist/ populated)
```

No test opens a real socket: every test that needs a "server" injects a fake `fetch` (and a fake
`sleep` for the reconnect backoff) via the `fetch`/`sleep` options `TicketmasterClient` and
`subscribeToEvents` both accept for exactly this reason.

## Regenerating `src/generated.ts`

```
pnpm gen                                   # tries http://127.0.0.1:4173/schema, else the snapshot
TM_SERVER_URL=http://host:port pnpm gen    # against a specific running tm-server
```

`schema/snapshot.json` is a checked-in copy of the current `GET /schema` response, used only as
a fallback when no server answers within 500ms — keep it in sync by hand if the server's
hand-written schema document changes shape before it grows a real `schemars`-style derive.
