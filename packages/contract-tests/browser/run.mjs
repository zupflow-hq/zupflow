import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// Also gate the subprocess's output: config/worker startup errors and DEBUG
// logging can bypass a reporter. No untrusted line ever reaches the terminal.
const projects = new Set(["desktop-light", "desktop-dark", "mobile-light", "mobile-dark"]);
const diagnostics = new Set([
  "consent-layout",
  "message-payload",
  "popup-cancel-close",
  "popup-unlisted-close",
  "popup-denied-close",
  "popup-denied-outcome",
  "browser-script-errors",
  "browser-csp-errors",
  "browser-credential-leak",
]);
const cases = new Set([
  "token-form",
  "token-stale-tab",
  "token-cancel",
  "invalid-and-expired-link",
  "oauth-connect-and-replay",
  "oauth-denied",
  "oauth-cancel",
  "oauth-bad-state",
  "oauth-bad-issuer",
  "oauth-expired-callback",
  "oauth-reconnect",
  "session-scope",
  "expiry-command-scope",
  "environment-scope",
  "popup-success",
  "popup-cancel",
  "popup-denied",
  "frame-allowlist",
  "return-allowlist",
]);
const args = process.argv.slice(2);
if (args.some((arg) => !arg.startsWith("--project=") || !projects.has(arg.slice(10)))) {
  process.stderr.write("Use pnpm test:browser [--project=desktop-light|desktop-dark|mobile-light|mobile-dark].\n");
  process.exitCode = 2;
} else {
  const directory = await mkdtemp(join(tmpdir(), "zupflow-connect-browser-run-"));
  const root = resolve(fileURLToPath(new URL("../../../", import.meta.url)));
  const env = {
    ...process.env,
    CARGO_INCREMENTAL: "0",
    ZUPFLOW_BROWSER_RUN_DIR: directory,
    // Browser profiles and crash/error artifacts belong to the same disposable
    // tree, including when a worker dies before its own fixture teardown.
    TMPDIR: directory,
    TMP: directory,
    TEMP: directory,
  };
  // Diagnostics and injected Node preloaders must not bypass the output gate.
  for (const key of Object.keys(env)) {
    if (
      key.startsWith("DEBUG") ||
      key.startsWith("PWDEBUG") ||
      key.startsWith("PW_TEST") ||
      (key.startsWith("PLAYWRIGHT_") && key !== "PLAYWRIGHT_BROWSERS_PATH") ||
      key === "NODE_OPTIONS"
    ) {
      delete env[key];
    }
  }
  let child;
  let finished = false;
  let interrupted = false;
  let passed = 0;
  let failed = 0;
  let skipped = 0;
  let ended = false;
  let runnerFailed = false;
  let pending = "";
  let discarding = false;

  function signalGroup(signal) {
    if (!child?.pid) return;
    try {
      process.kill(-child.pid, signal);
    } catch {
      /* Already stopped. */
    }
  }
  function interrupt() {
    interrupted = true;
    signalGroup("SIGTERM");
  }
  function accept(line) {
    const parts = line.split(" ");
    if (
      parts.length === 5 &&
      parts[0] === "CONNECT_BROWSER" &&
      parts[1] === "DETAIL" &&
      projects.has(parts[2]) &&
      cases.has(parts[3]) &&
      diagnostics.has(parts[4])
    ) {
      process.stdout.write(`DETAIL ${parts[2]} ${parts[3]} ${parts[4]}\n`);
      return;
    }
    if (
      parts.length === 4 &&
      parts[0] === "CONNECT_BROWSER" &&
      ["PASS", "FAIL", "SKIP"].includes(parts[1]) &&
      projects.has(parts[2]) &&
      cases.has(parts[3])
    ) {
      if (parts[1] === "PASS") passed++;
      if (parts[1] === "FAIL") failed++;
      if (parts[1] === "SKIP") skipped++;
      process.stdout.write(`${parts[1]} ${parts[2]} ${parts[3]}\n`);
    } else if (line === "CONNECT_BROWSER END PASS") {
      ended = true;
    } else if (line === "CONNECT_BROWSER END FAIL") {
      runnerFailed = true;
    } else if (line === "CONNECT_BROWSER ERROR runner_error" || line === "CONNECT_BROWSER ERROR unknown_case") {
      runnerFailed = true;
      process.stderr.write("FAIL browser runner (details withheld because diagnostics can contain credentials).\n");
    }
  }
  // Bound memory even if a dependency prints an enormous secret-bearing line.
  function read(chunk) {
    for (const character of chunk.toString("utf8")) {
      if (character === "\n") {
        if (!discarding) accept(pending.replace(/\r$/, ""));
        pending = "";
        discarding = false;
      } else if (!discarding) {
        if (pending.length >= 200) {
          pending = "";
          discarding = true;
        } else pending += character;
      }
    }
  }

  process.on("SIGINT", interrupt);
  process.on("SIGTERM", interrupt);
  const watchdog = setTimeout(() => {
    interrupt();
  }, 13 * 60_000);
  const killTimer = setInterval(() => {
    if (interrupted && !finished) signalGroup("SIGKILL");
  }, 5_000);
  try {
    child = spawn("pnpm", ["--filter", "@zupflow/contract-tests", "exec", "playwright", "test", ...args], {
      cwd: root,
      env,
      detached: true,
      stdio: ["ignore", "pipe", "pipe"],
    });
    child.stdout.on("data", read);
    child.stderr.resume();
    const code = await new Promise((done) => {
      child.once("error", () => done(1));
      child.once("close", (status) => done(status ?? 1));
    });
    finished = true;
    process.stdout.write(`Hosted Connect: ${passed} passed, ${failed} failed, ${skipped} skipped.\n`);
    const expected = cases.size * (args.length ? new Set(args).size : projects.size);
    if (code !== 0 || !ended || passed !== expected || failed || skipped || interrupted || runnerFailed) {
      process.stderr.write(
        "Hosted Connect failed. Inspect the named case in source; raw diagnostics are not retained.\n",
      );
      process.exitCode = 1;
    }
  } catch {
    process.stderr.write("Hosted Connect runner could not start. Install the pinned Playwright Chromium dependency.\n");
    process.exitCode = 1;
  } finally {
    clearTimeout(watchdog);
    clearInterval(killTimer);
    signalGroup("SIGKILL");
    process.off("SIGINT", interrupt);
    process.off("SIGTERM", interrupt);
    // Includes error-context files that Playwright can create independently
    // of traces, screenshots and video. No browser artifacts are uploaded.
    await rm(directory, { recursive: true, force: true });
  }
}
