#!/usr/bin/env node
/**
 * Holds a build of the Console to two things, after `vite build`:
 *
 * - **No mock API:** no mock service worker, and none of the mock's code.
 *   Each marker below is a string only the mock has; a build with the mock
 *   contains every one of them. A build made with `VITE_ZUPFLOW_MOCK=1`
 *   contains it on purpose, and passes only when also run with
 *   `--allow-mock`, with a warning.
 * - **Its Content-Security-Policy:** `index.html` carries the policy, and no
 *   inline script it would have to allow.
 *
 *   node scripts/check-build.mjs [dist] [--allow-mock]
 */
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";

const args = process.argv.slice(2);
const allowMock = args.includes("--allow-mock");
const dist = path.resolve(import.meta.dirname, "..", args.find((arg) => !arg.startsWith("--")) ?? "dist");

const MARKERS = [
  // The mock's session key (src/mocks/http.ts).
  "zupflow.mock.session",
  // The mock's control route (src/mocks/handlers.ts).
  "/__mock/reset",
  // The service worker the mock starts (src/mocks/browser.ts).
  "mockServiceWorker",
  // The mock's simulated sign-in (src/features/auth/mock-sign-in.tsx).
  "Continue as Jordan Lee",
  // Mock Service Worker itself.
  "[MSW]",
];

/** Directives the policy must carry, exactly as built in `vite.config.ts`. */
const REQUIRED_DIRECTIVES = [
  "default-src 'self'",
  "script-src 'self'",
  "object-src 'none'",
  "base-uri 'none'",
  "form-action 'self'",
];

if (!existsSync(dist)) {
  console.error(`check-build: ${dist} doesn't exist; run vite build first.`);
  process.exit(1);
}

const html = readFileSync(path.join(dist, "index.html"), "utf8");
const policy = /<meta http-equiv="Content-Security-Policy" content="([^"]*)"/
  .exec(html)?.[1]
  ?.replace(/&#39;/g, "'")
  .replace(/&quot;/g, '"')
  .replace(/&amp;/g, "&");
const problems = [];
if (!policy) {
  problems.push("index.html has no Content-Security-Policy meta tag");
} else {
  const directives = policy.split(";").map((directive) => directive.trim());
  for (const required of REQUIRED_DIRECTIVES) {
    if (!directives.includes(required)) problems.push(`the Content-Security-Policy lacks ${JSON.stringify(required)}`);
  }
  if (!directives.some((directive) => directive.startsWith("connect-src 'self'"))) {
    problems.push("the Content-Security-Policy lacks a connect-src");
  }
}
if (/<script(?![^>]*\bsrc=)[^>]*>/.test(html)) problems.push("index.html has an inline script the policy would block");
if (problems.length > 0) {
  console.error("check-build: this build's Content-Security-Policy isn't right:");
  for (const line of problems) console.error(`  ${line}`);
  process.exit(1);
}

if (process.env.VITE_ZUPFLOW_MOCK === "1") {
  if (!allowMock) {
    console.error(
      "check-build: built with VITE_ZUPFLOW_MOCK=1, so this build contains the mock API. Pass --allow-mock if that's what you meant.",
    );
    process.exit(1);
  }
  console.warn(
    "check-build: warning: this build contains the mock API (VITE_ZUPFLOW_MOCK=1 --allow-mock). Never deploy it.",
  );
  process.exit(0);
}

function files(directory) {
  return readdirSync(directory).flatMap((name) => {
    const full = path.join(directory, name);
    return statSync(full).isDirectory() ? files(full) : [full];
  });
}

const found = [];
for (const file of files(dist)) {
  const relative = path.relative(dist, file);
  if (path.basename(file) === "mockServiceWorker.js") {
    found.push(`${relative}: the mock's service worker`);
    continue;
  }
  if (!/\.(js|mjs|css|html|json|map)$/.test(file)) continue;
  const text = readFileSync(file, "utf8");
  for (const marker of MARKERS) {
    if (text.includes(marker)) found.push(`${relative}: contains ${JSON.stringify(marker)}`);
  }
}

if (found.length > 0) {
  console.error("check-build: this production build contains the mock API:");
  for (const line of found) console.error(`  ${line}`);
  console.error("Keep the mock behind __ZUPFLOW_MOCK__ (see src/features/auth/session.ts).");
  process.exit(1);
}
console.log("check-build: a Content-Security-Policy, and no mock API in this build.");
