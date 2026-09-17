# Ticketmaster for VS Code

Editor surface for Ticketmaster (SPEC.md section 18.4): a tree view of
milestones → tickets → children with state badges, editor decorations that
show when the open file is covered by an active path lease, a CodeLens for
decisions affecting the open file, and commands to open/claim/submit/record
against a running `tm-server`.

## Important caveat: `clients/ts/` does not exist yet

SPEC.md 18.1 specifies a shared `@ticketmaster/client` package at
`clients/ts/` — generated types from `tm-server`'s `/schema` endpoint, a
`TicketmasterClient` with SSE `subscribe()` reconnect semantics, and a pure
`ProjectStore` materialized view — that both `clients/web/` and this
extension are supposed to consume, with the explicit rule "nothing else may
hand-roll `fetch`". That package is not present anywhere in this repository
(`clients/ts/` doesn't exist), and this task was scoped to only touch
`clients/vscode/`, so it could not be built as part of this change.

To keep the extension honest to that rule as far as possible, every network
call is centralized in one module, `src/ticketmaster/client.ts`, rather than
scattered `fetch` calls across the codebase — but it is a hand-written,
minimal stand-in, not the generated package:

- `src/ticketmaster/types.ts` — hand-written TypeScript types mirroring the
  Rust structs in SPEC.md sections 2-4 (`Ticket`, `Milestone`, `Decision`,
  `Lease`, ...), not generated from `tm-server`'s `/schema`.
- `src/ticketmaster/client.ts` — a small `fetch`-based `TicketmasterClient`
  covering only the REST calls this extension needs (`GET /state`,
  `GET /tickets`, `GET /tickets/:id`, `GET /milestones`, `GET /decisions`,
  `POST /tickets/:id/lease`, `POST /tickets/:id/transition`,
  `POST /decisions`). **No SSE `subscribe()`, no reconnect/exactly-once
  guarantee, no `ProjectStore`.** The extension refreshes by re-fetching
  `/state` on demand (command run, refresh button, editor switch), not by
  following the live event stream.
- This client has **not been exercised against a real `tm-server`** — there
  is no integration test and no `tm-server` was running during this work.
  It is untested beyond `tsc` type-checking.

When `clients/ts/` lands, delete `src/ticketmaster/{types,client}.ts` and
have the rest of this extension import `@ticketmaster/client` instead; the
pure logic modules below don't know or care which client module they're fed
by, since they take plain data in and return plain data out.

## What is built

- **Tree view** (`src/tree/`): `treeModel.ts` is pure logic that builds a
  milestones → tickets → child-tickets tree from flat `Ticket[]` /
  `Milestone[]` arrays, with a state → badge (icon + text) mapping for all
  14 `TicketState` values from SPEC.md 4.3. Handles tickets with no
  milestone (grouped under a synthetic "Unassigned" node), dangling child
  references, and same-ticket cycles defensively. `ticketTreeProvider.ts`
  adapts this onto `vscode.TreeDataProvider`.
- **Path-lease decorations** (`src/leases/`): `leaseDecorations.ts` is pure
  logic — `findLeasesForPath` matches a lease's `ResourceClaim` glob
  patterns (SPEC.md 2.4 `**`/`*` semantics, reimplemented in
  `src/shared/glob.ts`) against the open file's workspace-relative path, and
  `leaseToRange`/`computeLeaseDecoration` map the matching leases onto a
  whole-document range plus a human-readable message (e.g. "Leased by T-184
  (agent:claude/a81)"). `leaseDecorationProvider.ts` adapts this onto a
  `vscode.TextEditorDecorationType`, refreshed on editor switch and on
  demand.
- **Decision CodeLens** (`src/codelens/`): `decisionCodeLens.ts` is pure
  logic — filters decisions whose `affectedDocs` glob patterns match the
  open file (excluding superseded ones) and produces a CodeLens entry per
  decision, anchored at line 0 (decisions in SPEC.md 4.6 are file-scoped,
  not line-scoped). `decisionCodeLensProvider.ts` adapts this onto
  `vscode.CodeLensProvider`.
- **Commands** (`src/commands/commands.ts`, registered in
  `src/extension.ts`): `ticketmaster.openTicket` (quick-pick + JSON
  preview), `ticketmaster.claimTicket` (acquires a lease as
  `human:$USER`), `ticketmaster.submitWithEvidence` (prompts for an
  evidence summary, submits a `HumanAttestation`), `ticketmaster.recordDecision`
  (prompts for subject/decision/reason, tags the active file as an affected
  doc; also used to show an existing decision's detail when invoked from a
  CodeLens), `ticketmaster.runOnSelection` (sends the selection to a `tm`
  terminal — this shells out to the `tm` CLI from SPEC.md 15, it does not
  call `tm-server` directly), and `ticketmaster.refresh`.
- **Activity bar view + `package.json` contributes**: a "Ticketmaster"
  activity bar container with the milestones tree view, all five commands
  above, context-menu wiring (open/claim on tree items, run-on-selection on
  editor selections), and a `ticketmaster.serverUrl` setting.
- **Tests**: `vitest` unit tests for every pure module —
  `src/shared/glob.test.ts`, `src/tree/treeModel.test.ts`,
  `src/leases/leaseDecorations.test.ts`,
  `src/codelens/decisionCodeLens.test.ts` — 30 tests, none of them import
  `vscode` or require a host.

## What is not built

- The generated `@ticketmaster/client` package itself (see caveat above) —
  this extension has its own narrow, hand-written, unverified-against-a-real-server
  substitute instead.
- SSE subscription / live updates. The extension only ever re-fetches
  `/state`; there is no `subscribe(fromSeq)`, no reconnect-with-
  `Last-Event-ID`, no exactly-once delivery guarantee across a drop.
- `ProjectStore` materialized view.
- Presence beyond what leases already imply (no separate presence indicator
  UI; `GET /presence` is not called).
- `Lease.acquire`/`heartbeat`/`release` beyond the initial claim — the
  extension calls `POST /tickets/:id/lease` to claim a ticket but never
  heartbeats or releases it; a claimed lease will simply expire server-side.
- Any VS Code Extension Test Runner ("host integration") tests. SPEC.md 18.4
  says host integration should be exercised through the extension test
  runner; none were added here — `*Provider.ts` and `extension.ts` are
  wired by hand and covered only indirectly, by `tsc` type-checking the
  `vscode` API calls and by unit-testing the pure logic each one delegates
  to. They have not been run inside an actual VS Code instance.
- A `.vscodeignore` / packaging (`vsce package`) step, an icon, a
  changelog, or marketplace metadata.
- Error handling beyond a warning toast on the initial `/state` fetch
  failing at activation; subsequent per-command failures propagate as
  unhandled rejections rather than user-facing messages in most commands.

## Layout

```
src/
  ticketmaster/         domain types + REST client (see caveat above)
  shared/glob.ts         PathPattern glob matcher shared by leases + CodeLens
  tree/                  treeModel.ts (pure) + ticketTreeProvider.ts (host)
  leases/                leaseDecorations.ts (pure) + leaseDecorationProvider.ts (host)
  codelens/              decisionCodeLens.ts (pure) + decisionCodeLensProvider.ts (host)
  commands/commands.ts   command implementations (host)
  extension.ts           activation: wires client, providers, commands
```

## Commands run and their results

```
$ pnpm install
Packages: +48, done. (esbuild's postinstall script was ignored by pnpm's
build-script sandbox; approved separately with `pnpm approve-builds esbuild`
so the vitest dep tree installs cleanly — this is a pnpm packaging quirk,
not an extension issue.)

$ pnpm test        # -> vitest run
 ✓ src/codelens/decisionCodeLens.test.ts (6 tests)
 ✓ src/leases/leaseDecorations.test.ts (8 tests)
 ✓ src/shared/glob.test.ts (9 tests)
 ✓ src/tree/treeModel.test.ts (7 tests)
 Test Files  4 passed (4)
      Tests  30 passed (30)

$ pnpm run compile  # -> tsc -p ./
(no output, exit 0 — strict mode, noUnusedLocals/Parameters all clean)
```

Not run: the extension inside an actual VS Code window (no VS Code host
available in this environment), and anything against a live `tm-server`
(none was running).
