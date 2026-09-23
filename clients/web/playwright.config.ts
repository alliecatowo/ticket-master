// Playwright end-to-end tests for the web client, against a real `tm serve` (see e2e/README.md).
//
// `pnpm -C clients/web e2e` builds `dist/` and runs everything here. Each worker starts its own
// scratch project and `tm serve` (e2e/fixtures.ts), so specs run in parallel safely.
import { chromium, defineConfig, devices } from "@playwright/test";
import { existsSync } from "node:fs";

// Browser: never download one if something usable is installed.
// - `E2E_CHANNEL=chrome` (or `msedge`, ...) forces an installed branded browser.
// - Otherwise Playwright's own Chromium, when this Playwright version's build is already in
//   ~/Library/Caches/ms-playwright.
// - Otherwise the installed Google Chrome.
// Only if none of these exist does `pnpm exec playwright install chromium` become necessary.
function browserChannel(): string | undefined {
  if (process.env.E2E_CHANNEL) return process.env.E2E_CHANNEL;
  if (existsSync(chromium.executablePath())) return undefined;
  if (
    existsSync("/Applications/Google Chrome.app") ||
    existsSync("/usr/bin/google-chrome") ||
    existsSync("/usr/bin/google-chrome-stable")
  ) {
    return "chrome";
  }
  return undefined;
}

const channel = browserChannel();

export default defineConfig({
  testDir: "./e2e",
  globalSetup: "./e2e/global-setup.ts",
  outputDir: "./test-results",
  // Each worker runs a tm serve plus a browser; this is an 8GB machine.
  workers: process.env.E2E_WORKERS ? Number(process.env.E2E_WORKERS) : 2,
  // Seeding an escalated ticket waits on three failed attempts (about 6 to 10 seconds).
  timeout: 60_000,
  expect: { timeout: 10_000 },
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : "list",
  use: {
    headless: true,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    {
      name: channel ? `chromium (${channel})` : "chromium",
      use: { ...devices["Desktop Chrome"], ...(channel ? { channel } : {}) },
    },
  ],
});
