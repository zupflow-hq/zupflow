import type { TestCase, TestResult, TestStep } from "@playwright/test/reporter";
import { describe, expect, it, vi } from "vitest";
import SafeReporter, { diagnosticLine, resultLine } from "./safe-reporter";

describe("hosted Connect reporter", () => {
  it("prints only registered project and case names", () => {
    expect(resultLine("desktop-light", "token-form", "passed")).toBe("CONNECT_BROWSER PASS desktop-light token-form");
    const bearer = "http://localhost/connect/private-reference?state=private-state&code=private-code";
    expect(resultLine("desktop-light", bearer, "failed")).toBe("CONNECT_BROWSER ERROR unknown_case");
    expect(resultLine(bearer, "token-form", "passed")).toBe("CONNECT_BROWSER ERROR unknown_case");
    expect(resultLine("desktop-light", "token-form", bearer)).toBe("CONNECT_BROWSER FAIL desktop-light token-form");
  });

  it("keeps script, policy and credential failures distinguishable", () => {
    for (const code of ["browser-script-errors", "browser-csp-errors", "browser-credential-leak"]) {
      expect(diagnosticLine("mobile-light", "popup-denied", code)).toBe(
        `CONNECT_BROWSER DETAIL mobile-light popup-denied ${code}`,
      );
    }
  });

  it("identifies both cancellation close checks without exposing their errors", () => {
    for (const code of ["popup-cancel-close", "popup-unlisted-close"]) {
      expect(diagnosticLine("mobile-dark", "popup-cancel", code)).toBe(
        `CONNECT_BROWSER DETAIL mobile-dark popup-cancel ${code}`,
      );
    }
  });

  it("rejects unknown or untrusted diagnostic names in every field", () => {
    const untrusted = "http://localhost/connect/private-reference?state=private-state&code=private-code";
    for (const value of ["unknown", untrusted, "consent-layout\nprivate-value", "consent-layout private-value"]) {
      expect(diagnosticLine(value, "popup-denied", "consent-layout")).toBeNull();
      expect(diagnosticLine("mobile-light", value, "consent-layout")).toBeNull();
      expect(diagnosticLine("mobile-light", "popup-denied", value)).toBeNull();
    }
  });

  it("emits only failed explicit steps and never their raw error or attachments", () => {
    const reporter = new SafeReporter();
    const test = {
      title: "popup-denied",
      parent: { project: () => ({ name: "mobile-light" }) },
    } as TestCase;
    const result = {} as TestResult;
    const untrusted = "http://localhost/connect/private-reference?state=private-state&code=private-code";
    const step = {
      title: "consent-layout",
      category: "test.step",
      error: { message: untrusted, stack: untrusted },
      attachments: [{ name: untrusted, path: untrusted }],
    } as TestStep;
    const output = vi.spyOn(process.stdout, "write").mockImplementation(() => true);
    try {
      reporter.onStepEnd(test, result, step);
      reporter.onStepEnd(test, result, { ...step, error: undefined });
      reporter.onStepEnd(test, result, { ...step, category: "pw:api" });
      reporter.onStepEnd(test, result, { ...step, title: untrusted });
      reporter.onStdOut();
      reporter.onStdErr();
      expect(output.mock.calls).toEqual([["CONNECT_BROWSER DETAIL mobile-light popup-denied consent-layout\n"]]);
    } finally {
      output.mockRestore();
    }
  });
});
