import { createInput, expect, reference, test } from "./fixtures";
import { completed, headers, layout, noStoredCredentials, token } from "./ui";

test("token-form", async ({ stack, flow }, info) => {
  const { organization, workspaces, apps } = stack.fixture;
  const workspace = workspaces.qaPlayground.id;
  const session = await stack.session(organization, workspace, createInput(apps.echoServer));
  const { page } = flow;
  await headers(await page.goto(session.url));
  await expect(page.getByRole("heading", { name: "Connect Echo test server", exact: true })).toBeVisible();
  await expect(page.getByText("Sandbox", { exact: true })).toBeVisible();
  await layout(page, info.project.use.colorScheme === "dark" ? "dark" : "light");
  const input = page.getByLabel("Token", { exact: true });
  await expect(input).toHaveAttribute("type", "password");
  await input.focus();
  await page.keyboard.press("Tab");
  await expect(page.getByRole("button", { name: "Connect", exact: true })).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(input).toHaveAttribute("aria-invalid", "true");
  await expect(page.locator("[data-error-for='token']")).toBeVisible();
  await token(page, "invalid-browser-fixture-token");
  await expect(input).toHaveAttribute("aria-invalid", "true");
  await expect(page.locator("[data-error-for='token']")).toHaveText("Rejected by the provider.");
  await expect(page.locator("[data-form-error]")).toBeHidden();
  expect((await stack.owner.connectSessions.get(workspace, session.id)).status).toBe("pending");
  const before = (await stack.owner.installations.list(workspace)).length;
  await token(page);
  await expect(page.getByRole("heading", { name: "Echo test server connected", exact: true })).toBeVisible();
  const installation = await completed(stack, organization, workspace, session.id);
  expect(installation.status).toBe("connected");
  expect(installation.app.id === apps.echoServer && installation.workspace_id === workspace).toBe(true);
  expect((await stack.owner.installations.list(workspace)).length).toBe(before + 1);
  await layout(page, info.project.use.colorScheme === "dark" ? "dark" : "light");
  await noStoredCredentials(page);
  expect(
    await page.evaluate((secret) => document.documentElement.outerHTML.includes(secret), reference(session.url)),
  ).toBe(false);
});

test("token-stale-tab", async ({ stack, flow }) => {
  const { organization, workspaces, apps } = stack.fixture;
  const workspace = workspaces.qaPlayground.id;
  const session = await stack.session(organization, workspace, createInput(apps.echoServer));
  const second = await flow.context.newPage();
  await flow.page.goto(session.url);
  await second.goto(session.url);
  const before = (await stack.owner.installations.list(workspace)).length;
  await token(flow.page);
  await expect(flow.page.locator("[data-state='completed']")).toBeVisible();
  await token(second, "another-browser-fixture-token");
  await expect(second.locator("[data-state='completed']")).toBeVisible();
  expect((await stack.owner.installations.list(workspace)).length).toBe(before + 1);
});

test("token-cancel", async ({ stack, flow }, info) => {
  const { organization, workspaces, apps } = stack.fixture;
  const workspace = workspaces.qaPlayground.id;
  const session = await stack.session(organization, workspace, createInput(apps.echoServer));
  const before = await stack.owner.installations.list(workspace);
  await flow.page.goto(session.url);
  await flow.page.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(flow.page.getByRole("heading", { name: "Connect was cancelled", exact: true })).toBeVisible();
  expect((await stack.owner.connectSessions.get(workspace, session.id)).status).toBe("cancelled");
  expect(await stack.owner.installations.list(workspace)).toEqual(before);
  await layout(flow.page, info.project.use.colorScheme === "dark" ? "dark" : "light");
});

test("invalid-and-expired-link", async ({ stack, flow }, info) => {
  const { organization, workspaces, apps } = stack.fixture;
  const workspace = workspaces.qaPlayground.id;
  const session = await stack.session(organization, workspace, createInput(apps.echoServer));
  const unknown = await flow.page.goto(`${stack.baseURL}/connect/unknown-browser-reference`);
  expect(unknown?.status()).toBe(404);
  await headers(unknown);
  await expect(flow.page.getByRole("heading", { name: "This link isn't valid", exact: true })).toBeVisible();
  const generic = await flow.page.locator("body").innerText();
  expect(await stack.expire(organization, workspace, session.id)).toBe(0);
  const expired = await flow.page.goto(session.url);
  expect(expired?.status()).toBe(404);
  await headers(expired);
  expect((await flow.page.locator("body").innerText()) === generic).toBe(true);
  expect((await stack.owner.connectSessions.get(workspace, session.id)).status).toBe("expired");
  await layout(flow.page, info.project.use.colorScheme === "dark" ? "dark" : "light");
});
