import path from "node:path";
import { defineConfig } from "vitest/config";

export default defineConfig({
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "src"),
    },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
    // The shared contract suite's sweeps make hundreds of requests per case. The timeout is there to catch a
    // hang, not a slow machine: CI runs these beside the Rust build on two cores.
    testTimeout: 30_000,
  },
});
