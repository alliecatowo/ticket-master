/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import { configDefaults } from "vitest/config";
import react from "@vitejs/plugin-react";

// Vite + React config for the Ticketmaster project canvas.
//
// This app is served two ways, both under `/app/` so its routes never collide with the API's:
//  - `tm serve --open`: `tm serve` serves the built `dist/` at `/app/` and the API from the same
//    origin, so `apiBase` defaults to `""` (same-origin, relative paths).
//  - `pnpm dev`: the Vite dev server proxies `/health`, `/events`, `/tickets`, etc. to a `tm
//    serve` instance running on VITE_TM_SERVER (default http://127.0.0.1:4173) so the same
//    same-origin client code works unmodified in dev.
const TM_SERVER = process.env.VITE_TM_SERVER ?? "http://127.0.0.1:4173";
const API_PATHS = [
  "/health",
  "/events",
  "/state",
  "/schema",
  "/tickets",
  "/graph",
  "/decisions",
  "/milestones",
  "/artifacts",
  "/docs",
  "/approvals",
  "/sessions",
  "/presence",
  "/leases",
  "/providers",
  "/harness",
  "/metrics",
];

export default defineConfig({
  base: "/app/",
  plugins: [react()],
  server: {
    proxy: Object.fromEntries(
      API_PATHS.map((path) => [
        path,
        { target: TM_SERVER, changeOrigin: true, ws: path === "/events" },
      ]),
    ),
  },
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/setupTests.ts"],
    // e2e/ is Playwright's (pnpm e2e), run against a real tm serve, not vitest's jsdom.
    exclude: [...configDefaults.exclude, "e2e/**"],
  },
});
