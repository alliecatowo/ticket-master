import { defineConfig } from "vitest/config";

// Only the pure-logic modules ship *.test.ts files, and none of them import
// the `vscode` module, so this suite runs under plain Node with no VS Code
// host required. Host-integration code (tree/*Provider.ts,
// leases/*Provider.ts, codelens/*Provider.ts, extension.ts, commands/*.ts)
// is intentionally excluded from unit coverage here; see README.md.
export default defineConfig({
  test: {
    include: ["src/**/*.test.ts"],
    environment: "node",
  },
});
