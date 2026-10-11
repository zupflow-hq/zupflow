import { join } from "node:path";
import { defineConfig } from "@playwright/test";

const runDirectory = process.env.ZUPFLOW_BROWSER_RUN_DIR;
if (!runDirectory) throw new Error("Run hosted Connect tests with pnpm test:browser.");

export default defineConfig({
  testDir: "./browser",
  testMatch: "**/*.pw.ts",
  fullyParallel: false,
  workers: 1,
  retries: 0,
  forbidOnly: Boolean(process.env.CI),
  timeout: 45_000,
  globalTimeout: 12 * 60_000,
  expect: { timeout: 10_000 },
  quiet: true,
  outputDir: join(runDirectory, "results"),
  preserveOutput: "never",
  reporter: [["./browser/safe-reporter.ts"]],
  use: {
    browserName: "chromium",
    actionTimeout: 10_000,
    navigationTimeout: 15_000,
    trace: "off",
    video: "off",
    screenshot: "off",
  },
  projects: [
    { name: "desktop-light", use: { viewport: { width: 1280, height: 800 }, colorScheme: "light" } },
    { name: "desktop-dark", use: { viewport: { width: 1280, height: 800 }, colorScheme: "dark" } },
    { name: "mobile-light", use: { viewport: { width: 375, height: 812 }, colorScheme: "light" } },
    { name: "mobile-dark", use: { viewport: { width: 375, height: 812 }, colorScheme: "dark" } },
  ],
});
