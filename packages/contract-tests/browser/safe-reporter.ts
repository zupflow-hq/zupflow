import type { FullResult, Reporter, TestCase, TestResult, TestStep } from "@playwright/test/reporter";

// A failing Playwright action can include its URL, form value and DOM in its
// error, stack, call log and attachments. Do not redact those after formatting:
// report only these source-controlled names and enum outcomes instead.
export const CASES = [
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
] as const;

const PROJECTS = ["desktop-light", "desktop-dark", "mobile-light", "mobile-dark"];
const DIAGNOSTICS = [
  "consent-layout",
  "message-payload",
  "popup-cancel-close",
  "popup-unlisted-close",
  "popup-denied-close",
  "popup-denied-outcome",
  "browser-script-errors",
  "browser-csp-errors",
  "browser-credential-leak",
];

export function diagnosticLine(project: string, name: string, code: string): string | null {
  if (!PROJECTS.includes(project) || !CASES.some((item) => item === name) || !DIAGNOSTICS.includes(code)) return null;
  return `CONNECT_BROWSER DETAIL ${project} ${name} ${code}`;
}

export function resultLine(project: string, name: string, status: string): string {
  if (!PROJECTS.includes(project) || !CASES.some((item) => item === name)) {
    return "CONNECT_BROWSER ERROR unknown_case";
  }
  const outcome = status === "passed" ? "PASS" : status === "skipped" ? "SKIP" : "FAIL";
  return `CONNECT_BROWSER ${outcome} ${project} ${name}`;
}

export default class SafeReporter implements Reporter {
  printsToStdio() {
    return true;
  }

  onTestEnd(test: TestCase, result: TestResult) {
    process.stdout.write(`${resultLine(test.parent.project()?.name ?? "", test.title, result.status)}\n`);
  }

  onStepEnd(test: TestCase, _result: TestResult, step: TestStep) {
    if (step.category !== "test.step" || !step.error) return;
    const line = diagnosticLine(test.parent.project()?.name ?? "", test.title, step.title);
    if (line) process.stdout.write(`${line}\n`);
  }

  // Intentionally ignore all test stdout/stderr, raw errors and attachments.
  onStdOut() {}
  onStdErr() {}
  onError() {
    process.stdout.write("CONNECT_BROWSER ERROR runner_error\n");
  }

  onEnd(result: FullResult) {
    const status = result.status === "passed" ? "PASS" : "FAIL";
    process.stdout.write(`CONNECT_BROWSER END ${status}\n`);
  }
}
