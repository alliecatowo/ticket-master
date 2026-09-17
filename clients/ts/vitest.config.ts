import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "node",
    include: ["test/**/*.test.ts"],
    // No network in tests: every test that needs a "server" injects a fake fetch/EventSource
    // implementation rather than talking to a real socket.
    restoreMocks: true,
  },
});
