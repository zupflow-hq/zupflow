import { createInput, expect, must, test } from "./fixtures";
import { completed, consent, headers, layout, noStoredCredentials } from "./ui";

test.use({ mode: "oauth" });

test("oauth-connect-and-replay", async ({ stack, flow }, info) => {
  const board = must(stack.taskboard, "Taskboard");
  const { organization, workspaces } = stack.fixture;
  const workspace = workspaces.qaPlayground.id;
  const session = await stack.session(organization, workspace, createInput(board.app));
  let callback: string | undefined;
  flow.page.on("request", (request) => {
    if (new URL(request.url()).pathname === "/connect/oauth/callback") callback = request.url();
  });
  await headers(await flow.page.goto(session.url));
  await layout(flow.page, info.project.use.colorScheme === "dark" ? "dark" : "light");
  await consent(flow.page);
  const before = (await stack.owner.installations.list(workspace)).length;
  await flow.page.getByRole("button", { name: "Allow", exact: true }).click();
  await expect(flow.page.getByRole("heading", { name: "Taskboard connected", exact: true })).toBeVisible();
  const installation = await completed(stack, organization, workspace, session.id);
  expect(installation.connection?.account.email).toBe("robin@taskboard.example");
  expect(installation.connection?.granted_scopes).toEqual(["tasks:read", "tasks:write"]);
  expect((await stack.owner.installations.list(workspace)).length).toBe(before + 1);
  await layout(flow.page, info.project.use.colorScheme === "dark" ? "dark" : "light");
  await noStoredCredentials(flow.page);
  const replay = await flow.page.goto(must(callback, "OAuth callback"));
  expect(replay?.status()).toBe(400);
  await headers(replay);
  await expect(flow.page.getByRole("heading", { name: "This sign-in link isn't valid", exact: true })).toBeVisible();
  expect((await stack.owner.installations.get(workspace, installation.id)).connection).toEqual(installation.connection);
  expect((await stack.owner.installations.list(workspace)).length).toBe(before + 1);
});

test("oauth-denied", async ({ stack, flow }, info) => {
  const { organization, workspaces } = stack.fixture;
  const workspace = workspaces.qaPlayground.id;
  const session = await stack.session(organization, workspace, createInput(must(stack.taskboard, "Taskboard").app));
  const before = await stack.owner.installations.list(workspace);
  await flow.page.goto(session.url);
  await consent(flow.page);
  await flow.page.getByRole("button", { name: "Deny", exact: true }).click();
  await expect(flow.page.getByRole("heading", { name: "Access wasn't granted", exact: true })).toBeVisible();
  const result = await stack.owner.connectSessions.get(workspace, session.id);
  expect({ status: result.status, failure: result.failure_code }).toEqual({
    status: "failed",
    failure: "consent_denied",
  });
  expect(await stack.owner.installations.list(workspace)).toEqual(before);
  await layout(flow.page, info.project.use.colorScheme === "dark" ? "dark" : "light");
});

test("oauth-cancel", async ({ stack, flow }) => {
  const { organization, workspaces } = stack.fixture;
  const workspace = workspaces.qaPlayground.id;
  const session = await stack.session(organization, workspace, createInput(must(stack.taskboard, "Taskboard").app));
  const before = await stack.owner.installations.list(workspace);
  await flow.page.goto(session.url);
  await consent(flow.page);
  // Cancel in a second tab while consent remains open, then answer that old consent.
  const cancel = await flow.context.newPage();
  await cancel.goto(session.url);
  await cancel.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(cancel.locator("[data-state='cancelled']")).toBeVisible();
  await flow.page.getByRole("button", { name: "Allow", exact: true }).click();
  await expect(flow.page.getByRole("heading", { name: "This sign-in link isn't valid", exact: true })).toBeVisible();
  expect((await stack.owner.connectSessions.get(workspace, session.id)).status).toBe("cancelled");
  expect(await stack.owner.installations.list(workspace)).toEqual(before);
});

test("oauth-bad-state", async ({ stack, flow }) => {
  const { organization, workspaces } = stack.fixture;
  const workspace = workspaces.qaPlayground.id;
  const session = await stack.session(organization, workspace, createInput(must(stack.taskboard, "Taskboard").app));
  const before = await stack.owner.installations.list(workspace);
  await flow.page.goto(session.url);
  await consent(flow.page);
  // Playwright routes only the first request in a redirect chain. Navigate
  // the altered callback directly, while keeping the real consent open.
  const rejected = await flow.context.newPage();
  const callback = new URL("/connect/oauth/callback", stack.baseURL);
  callback.searchParams.set("state", "unknown-browser-state");
  callback.searchParams.set("error", "access_denied");
  callback.searchParams.set("iss", must(stack.taskboard, "Taskboard").origin);
  const response = await rejected.goto(callback.href);
  expect(response?.status()).toBe(400);
  await headers(response);
  await expect(rejected.getByRole("heading", { name: "This sign-in link isn't valid", exact: true })).toBeVisible();
  expect((await stack.owner.connectSessions.get(workspace, session.id)).status).toBe("pending");
  expect(await stack.owner.installations.list(workspace)).toEqual(before);
  await flow.page.getByRole("button", { name: "Allow", exact: true }).click();
  await expect(flow.page.locator("[data-state='completed']")).toBeVisible();
  await completed(stack, organization, workspace, session.id);
});

test("oauth-bad-issuer", async ({ stack, flow }, info) => {
  const { organization, workspaces } = stack.fixture;
  const workspace = workspaces.qaPlayground.id;
  const session = await stack.session(organization, workspace, createInput(must(stack.taskboard, "Taskboard").app));
  const before = await stack.owner.installations.list(workspace);
  await flow.page.goto(session.url);
  await consent(flow.page);
  const state = must(new URL(flow.page.url()).searchParams.get("state"), "pending OAuth state");
  const callback = new URL("/connect/oauth/callback", stack.baseURL);
  callback.searchParams.set("state", state);
  callback.searchParams.set("error", "access_denied");
  callback.searchParams.set("iss", "https://foreign.example");
  await flow.page.goto(callback.href);
  await expect(flow.page.getByRole("heading", { name: "Taskboard ended the sign-in", exact: true })).toBeVisible();
  expect((await stack.owner.connectSessions.get(workspace, session.id)).failure_code).toBe("authorization_failed");
  expect(await stack.owner.installations.list(workspace)).toEqual(before);
  await layout(flow.page, info.project.use.colorScheme === "dark" ? "dark" : "light");
});

test("oauth-expired-callback", async ({ stack, flow }, info) => {
  const { organization, workspaces } = stack.fixture;
  const workspace = workspaces.qaPlayground.id;
  const session = await stack.session(organization, workspace, createInput(must(stack.taskboard, "Taskboard").app));
  const before = await stack.owner.installations.list(workspace);
  await flow.page.goto(session.url);
  await consent(flow.page);
  expect(await stack.expire(organization, workspace, session.id)).toBe(0);
  await flow.page.getByRole("button", { name: "Allow", exact: true }).click();
  await expect(flow.page.getByRole("heading", { name: "This link isn't valid", exact: true })).toBeVisible();
  expect((await stack.owner.connectSessions.get(workspace, session.id)).status).toBe("expired");
  expect(await stack.owner.installations.list(workspace)).toEqual(before);
  await layout(flow.page, info.project.use.colorScheme === "dark" ? "dark" : "light");
});

test("oauth-reconnect", async ({ stack, flow }, info) => {
  const board = must(stack.taskboard, "Taskboard");
  const { organization, workspaces } = stack.fixture;
  const workspace = workspaces.qaPlayground.id;
  const session = await stack.session(organization, workspace, createInput(board.app));
  await flow.page.goto(session.url);
  await consent(flow.page);
  await flow.page.getByRole("button", { name: "Allow", exact: true }).click();
  await expect(flow.page.locator("[data-state='completed']")).toBeVisible();
  const installation = await completed(stack, organization, workspace, session.id);
  await expect
    .poll(async () => (await stack.owner.installations.tools(workspace, installation.id)).items.length)
    .toBe(3);
  await flow.page.goto(board.origin);
  await flow.page.getByRole("button", { name: "Revoke every token", exact: true }).click();
  await expect(flow.page.getByRole("heading", { name: "Done", exact: true })).toBeVisible();
  await stack.owner.installations.refreshTools(workspace, installation.id);
  await expect
    .poll(async () => (await stack.owner.installations.get(workspace, installation.id)).status)
    .toBe("reconnect_required");
  const count = (await stack.owner.installations.list(workspace)).length;
  const reconnect = await stack.session(organization, workspace, {
    ...createInput(board.app),
    intent: "reconnect",
    installation_id: installation.id,
  });
  await flow.page.goto(reconnect.url);
  await expect(flow.page.getByRole("heading", { name: "Reconnect Taskboard", exact: true })).toBeVisible();
  await expect(flow.page.getByText("robin@taskboard.example", { exact: false })).toBeVisible();
  await layout(flow.page, info.project.use.colorScheme === "dark" ? "dark" : "light");
  await consent(flow.page);
  await flow.page.getByRole("button", { name: "Allow", exact: true }).click();
  await expect(flow.page.locator("[data-state='completed']")).toBeVisible();
  const connected = await completed(stack, organization, workspace, reconnect.id);
  expect(connected.id === installation.id).toBe(true);
  expect(connected.status).toBe("connected");
  expect(connected.connection?.account).toEqual(installation.connection?.account);
  expect(must(connected.connection, "connection").credential_version).toBeGreaterThan(
    must(installation.connection, "connection").credential_version,
  );
  expect((await stack.owner.installations.list(workspace)).length).toBe(count);
});
