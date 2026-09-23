// The `test` every spec imports: Playwright's own, plus a real `tm serve` per worker.
//
//   import { test, expect } from "./fixtures";
//
// - `tmServer` (worker): one scratch project and `tm serve` per Playwright worker, shared by
//   every test that worker runs, so tests must not assume an empty project.
// - `apiURL` (worker): that server's API origin.
// - `api` (worker): a `TmApi` for seeding and inspecting tickets over HTTP.
// - `baseURL`: overridden to the server's `/app/` URL, so `page.goto("")` opens the home screen.
// - `freshServer` (test, lazy): a second, dedicated server for a test that needs an empty
//   project. Only started when a test asks for it.
import { test as base, expect } from "@playwright/test";
import path from "node:path";
import { TmApi } from "./helpers/api";
import { startTmServer, type TmServer } from "./helpers/server";

type WorkerFixtures = {
  tmServer: TmServer;
  apiURL: string;
  api: TmApi;
};

type TestFixtures = {
  freshServer: TmServer;
};

export const test = base.extend<TestFixtures, WorkerFixtures>({
  tmServer: [
    async ({}, use, workerInfo) => {
      const server = await startTmServer({
        logPath: path.join(
          workerInfo.project.outputDir,
          `tm-serve-worker-${workerInfo.parallelIndex}-${workerInfo.workerIndex}.log`,
        ),
      });
      try {
        await use(server);
      } finally {
        await server.stop();
      }
    },
    { scope: "worker", timeout: 60_000 },
  ],

  apiURL: [async ({ tmServer }, use) => use(tmServer.apiURL), { scope: "worker" }],

  api: [async ({ tmServer }, use) => use(new TmApi(tmServer.apiURL)), { scope: "worker" }],

  baseURL: async ({ tmServer }, use) => use(tmServer.appURL),

  freshServer: async ({}, use, testInfo) => {
    const server = await startTmServer({ logPath: testInfo.outputPath("tm-serve-fresh.log") });
    try {
      await use(server);
    } finally {
      await server.stop();
    }
  },
});

export { expect };
export { HUMAN, WORKER, TmApi, TmApiError, type Ticket, type TicketState } from "./helpers/api";
export { startTmServer, type TmServer } from "./helpers/server";
