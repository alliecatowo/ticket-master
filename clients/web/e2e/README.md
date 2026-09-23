# `clients/web/e2e`: Playwright against a real `tm serve`

These tests drive the built web client in a headless browser against a real `tm serve`. Each
Playwright worker gets its own scratch project and server, so spec files run in parallel.

```
pnpm -C clients/web e2e                  # pnpm build, then playwright test
pnpm -C clients/web e2e:nobuild          # reuse the dist/ you already built
pnpm -C clients/web e2e -- smoke         # one file (args go to playwright test)
pnpm -C clients/web e2e --repeat-each=3  # shake out flakes
```

## What it needs

- **`dist/`**, from `pnpm build`. `e2e` builds it first. `e2e:nobuild` fails fast in
  `global-setup.ts` if it's missing.
- **A `tm` binary.** Nothing here runs cargo. `helpers/paths.ts`'s `resolveTmBin` looks in this
  order:
  1. `$TM_BIN`
  2. this checkout's `target/debug/tm`
  3. the primary checkout's `target/debug/tm`, found through `git rev-parse --git-common-dir`, so
     a worktree without its own `target/` still works
  4. `tm` on `$PATH`

  The run's first lines print which binary it picked. It is used as-is, so rebuild it
  (`mise run build`) when the server changes.
- **A browser.** Nothing is downloaded. `@playwright/test` is pinned to `1.63.0`, whose Chromium
  (revision 1243) is already in `~/Library/Caches/ms-playwright` on this machine.
  `playwright.config.ts` uses that, else the installed Google Chrome (`channel: "chrome"`).
  `E2E_CHANNEL=chrome` forces Chrome. Only on a machine with neither is
  `pnpm exec playwright install chromium` needed.

Settings:
- `E2E_WORKERS` (default 2). Each worker is a `tm serve` plus a browser, and this is an 8GB
  machine.
- `E2E_KEEP_TMP=1` keeps the scratch directories.
- Server output goes to `test-results/tm-serve-worker-<n>-<i>.log`.
- Failures keep a trace and a screenshot in `test-results/`.

## The scratch project

`helpers/server.ts`'s `startTmServer()`:
1. Makes a fresh `git` repo (with an empty initial commit) and a separate `TM_HOME`. Both are
   `realpath`'d `mkdtemp` directories under `$TMPDIR`.
2. Runs `tm init` there.
3. Starts `tm serve --addr 127.0.0.1:0 --web-dir <abs clients/web/dist>` with `TM_NOTIFY=0` and
   `TM_TEST_MOCK_PROVIDER=1`.
4. Reads the port from the "Serving the API at http://…" line, then waits for `GET /health` and
   for `/app/` to serve HTML.

To stop, it sends SIGINT (`tm serve` only shuts down gracefully on ctrl-c). If the server is still
up 1.5 s later it sends SIGKILL, because an open SSE stream holds graceful shutdown. Then it
deletes both directories. A `process.on("exit")` hook kills any server a crashed worker left
behind. Nothing ever runs in this checkout.

**The mock model fails every attempt.** A ticket activated on a server with workers gets leased
within about 2 s, fails three times, and is `escalated` after roughly 6 to 10 s. That is the only
state the workers reach on their own. Seed `submitted` over HTTP with `seedSubmitted`.

## Fixtures (`e2e/fixtures.ts`)

Import `test` and `expect` from `./fixtures`, not from `@playwright/test`:

```ts
import { expect, test } from "./fixtures";

test("accept from review", async ({ page, api }) => {
  const { id } = await api.seedSubmitted("Add the retry helper");
  await page.goto("review");
  // ...
});
```

| Fixture | Scope | What it is |
| --- | --- | --- |
| `tmServer` | worker | The worker's `TmServer`: `apiURL`, `appURL`, `repoDir`, `tmHome`, `logPath`, `env`, `tmBin`, `runTm(args)` (run another `tm` command against the same project), `stop()` |
| `apiURL` | worker | `tmServer.apiURL`, e.g. `http://127.0.0.1:53412` |
| `api` | worker | A `TmApi` bound to that server (below) |
| `baseURL` | test | `tmServer.appURL`, e.g. `http://127.0.0.1:53412/app/` |
| `freshServer` | test | A second, dedicated server, started only for tests that ask for it. Use it when a test needs an empty project: `page.goto(freshServer.appURL)`, `new TmApi(freshServer.apiURL)` |

**`baseURL` ends in `/app/`. Navigate with relative paths:**
- `page.goto("")` opens the home screen.
- `page.goto("review")` and `page.goto(`ticket/${id}`)` open those pages.
- **A leading slash (`page.goto("/review")`) resolves to the API origin, not the app.**

**The worker's server is shared.** Every test a worker runs uses the same project, so tickets pile
up across tests:
- Never assert on totals, group counts or "the first row".
- Find your own ticket by the id a helper returned: `page.getByTestId(`row-${id}`)`.
- Row titles are shortened objectives. `shortTitle` cuts at the first `,` `:` `;` `(` or
  sentence end, then to 44 columns. Match a row by id, not by its full objective.
- Tests that need an empty project use `freshServer`.

## `TmApi` (`e2e/helpers/api.ts`)

Everything goes over HTTP with the wire shapes in `crates/tm-server/src/routes.rs`:
- Human actions are sent as `HUMAN` (`"human:e2e"`).
- Seeded worker steps are sent as `WORKER` (`"agent:e2e/worker"`). A participant id must be
  `human:<handle>`, `agent:<provider>/<id>` or `system`.
- A non-2xx answer throws `TmApiError`, with `status` and the server's `body`.

| Method | Does |
| --- | --- |
| `createTicket(objective, {kind?, priority?, parent?, actor?})` → `id` | `POST /tickets` with the same worker defaults as `tm ticket new`. Leaves a `draft`. |
| `activate(id)` | `{"activate": …}`: draft → ready. With workers on, it gets picked up within about 2 s. |
| `cancel(id, reason?)` | `{"cancel": …}` as the human. |
| `accept(id, note?)` / `reject(id, reason)` / `retry(id, guidance?)` | The human-only review actions. The server refuses them from a non-human actor. |
| `transition(id, body)` | Any externally tagged transition body, e.g. `{trigger: {trigger: "work_started", actor}}`. |
| `createArtifact(content, {kind?, mediaType?, ticket?, actor?})` → `id` | `POST /artifacts` with inline bytes (a JSON byte array). Default kind `patch`. |
| `waitForState(id, state \| state[], {timeoutMs?, intervalMs?})` → ticket | Polls `GET /tickets/:id` (default 30 s, every 200 ms). On timeout it throws with the last state seen. |
| `seedEscalated(objective, {timeoutMs?})` → `id` | Creates and activates a ticket, then waits (default 45 s) while the workers fail it three times into `escalated`. |
| `seedSubmitted(objective, {summary?, evidence?})` → `{id, artifact, lease}` | Drives draft → ready → leased → running → submitted by hand, with a stored artifact as evidence (below). |
| `getTicket(id)`, `listTickets()`, `getState()` | Read-only. `getState()` is `GET /state`: tickets, leases, artifacts, evidence and `head`. |
| `request(method, path, body?)` | Any other endpoint. |

### How `seedSubmitted` works

The steps, in order:
1. `activate` as the human.
2. `POST /tickets/:id/lease` for `WORKER`, with a 3600 s TTL. This is ready → leased.
3. The `work_started` trigger: leased → running.
4. `POST /artifacts`.
5. `{"submit": {summary, evidence: [artifact]}}` as `WORKER`: running → submitted.

The submit also records a `review` evidence row ("submission evidence").

Things to know:
- **Race with the scheduler.** The lease is requested right after activation, before the
  in-process scheduler's next tick (every 2 s). If the scheduler wins anyway, the lease gets a
  409. The helper then cancels that ticket and seeds a new one, up to three tries, so a rare run
  leaves an extra cancelled row. This is one more reason to find rows by id.
- **The seeded lease stays live.** The server refuses to release a lease once the ticket is
  `submitted` ("no transition from Submitted on LeaseReleased").
- **Accept closes the ticket**: submitted → verifying → … → closed, all in one call.
- **Reject sends it back to work.** Right after the call it already reads `ready` (it goes
  through `rework`). The workers then pick it up and the mock fails it. The seeded attempt counts
  toward the three, so it reaches `escalated` within a few seconds. Wait for that state rather
  than asserting a transient one.

## Specs

- `smoke.spec.ts`:
  1. `/app/` loads and shows the "Tickets" heading.
  2. Dispatching a task from the box creates a ticket on the server and activates it.
  3. Its row appears.
- `seeding.spec.ts`: checks the helpers themselves:
  - `seedSubmitted` shows in Ready for review, with its evidence.
  - `seedEscalated` shows in Needs input after 3 attempts.
  - `createTicket` + `cancel` + `waitForState`.
  - `freshServer` is empty.
