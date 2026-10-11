import type { Page, Response } from "@playwright/test";
import type { ConnectSessionId, OrganizationId, WorkspaceId } from "@zupflow/sdk";
import { ZupflowApiError } from "@zupflow/sdk";
import { expect, must, type Stack, test } from "./fixtures";

export async function layout(page: Page, scheme: "light" | "dark") {
  const checks = await page.evaluate(
    (dark) => ({
      fits: document.documentElement.scrollWidth <= window.innerWidth,
      theme:
        getComputedStyle(document.documentElement).getPropertyValue("--background").trim() ===
        (dark ? "#0d1417" : "#f7f9fb"),
      media: matchMedia("(prefers-color-scheme: dark)").matches === dark,
    }),
    scheme === "dark",
  );
  expect(checks).toEqual({ fits: true, theme: true, media: true });
  await expect(page.getByRole("heading", { level: 1 })).toBeVisible();
}

export async function headers(response: Response | null, framed = false) {
  const values = must(response, "hosted page response").headers();
  expect(values["cache-control"]).toBe("no-store");
  expect(values["referrer-policy"]).toBe("no-referrer");
  const csp = values["content-security-policy"] ?? "";
  for (const directive of [
    "default-src 'none'",
    "script-src 'self'",
    "connect-src 'self'",
    "form-action 'self'",
    "base-uri 'none'",
    "require-trusted-types-for 'script'",
    "trusted-types 'none'",
  ]) {
    expect(csp.includes(directive)).toBe(true);
  }
  if (!framed) {
    expect(csp.includes("frame-ancestors 'none'")).toBe(true);
    expect(values["x-frame-options"]).toBe("DENY");
  }
}

export async function noStoredCredentials(page: Page) {
  expect(await page.evaluate(() => localStorage.length + sessionStorage.length)).toBe(0);
  expect((await page.context().cookies()).length).toBe(0);
}

export async function token(page: Page, value = "browser-fixture-token") {
  await page.getByLabel("Token", { exact: true }).fill(value);
  await page.getByRole("button", { name: "Connect", exact: true }).click();
}

export async function consent(page: Page) {
  await page.getByRole("button", { name: "Continue to Taskboard", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Zupflow wants to use your Taskboard", exact: true })).toBeVisible();
  await expect(page.getByText("robin@taskboard.example", { exact: false })).toBeVisible();
  await test.step("consent-layout", async () => {
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  });
}

export async function completed(
  stack: Stack,
  organization: OrganizationId,
  workspace: WorkspaceId,
  session: ConnectSessionId,
) {
  const client = stack.member(organization, organization === stack.fixture.organization ? "owner" : "outsider");
  await expect.poll(async () => (await client.connectSessions.get(workspace, session)).status).toBe("completed");
  const outcome = await client.connectSessions.get(workspace, session);
  return client.installations.get(workspace, must(outcome.installation_id, "completed installation"));
}

export async function apiFailure(action: () => Promise<unknown>, status: number, code: string) {
  let matched = false;
  try {
    await action();
  } catch (error) {
    matched = error instanceof ZupflowApiError && error.status === status && error.code === code;
  }
  // API messages and thrown causes may contain provider text. Assert only a
  // boolean, never the rejected value or a response body.
  expect(matched).toBe(true);
}
