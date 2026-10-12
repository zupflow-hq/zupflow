import type { OrganizationId, WorkspaceId, ZupflowClient } from "@zupflow/sdk";
import { createInput, expect, must, reference, type Stack, test } from "./fixtures";
import { apiFailure, completed, layout, token } from "./ui";

async function tokenApp(stack: Stack, organization: OrganizationId, workspace: WorkspaceId, client: ZupflowClient) {
  const source = await stack.owner.workspaceApps.get(
    stack.fixture.workspaces.qaPlayground.id,
    stack.fixture.apps.echoServer,
  );
  const app = await client.workspaceApps.create(workspace, {
    kind: "mcp",
    name: "Echo test server",
    key: "browser_echo",
    description: "Local browser fixture",
    auth_type: "token",
  });
  expect(app.organization_id === organization && app.owner_workspace_id === workspace).toBe(true);
  await client.workspaceApps.update(workspace, app.id, { auth: source.auth });
  await client.workspaceApps.updateConfiguration(workspace, app.id, { mcp_server_url: "https://echo.example/mcp" });
  return app.id;
}

async function foreignWorkspace(stack: Stack) {
  const workspaces = await stack.outsider.workspaces.list(stack.fixture.otherOrganization, {
    q: "harbor-sandbox",
    limit: 10,
  });
  return must(
    workspaces.items.find((workspace) => workspace.slug === "harbor-sandbox"),
    "foreign Sandbox workspace",
  ).id;
}

test("session-scope", async ({ stack, flow }) => {
  const { organization, otherOrganization, workspaces, apps } = stack.fixture;
  const own = workspaces.qaPlayground.id;
  const sibling = workspaces.acmeSandbox.id;
  const foreign = await foreignWorkspace(stack);
  const siblingApp = await tokenApp(stack, organization, sibling, stack.owner);
  const foreignApp = await tokenApp(stack, otherOrganization, foreign, stack.outsider);
  const session = await stack.session(organization, own, createInput(apps.echoServer));
  const siblingSession = await stack.session(organization, sibling, createInput(siblingApp));
  const foreignSession = await stack.outsider.connectSessions.create(foreign, createInput(foreignApp));
  const siblingBefore = await stack.owner.installations.list(sibling);
  const foreignBefore = await stack.outsider.installations.list(foreign);
  await apiFailure(() => stack.owner.connectSessions.get(sibling, session.id), 404, "not_found");
  await apiFailure(() => stack.outsider.connectSessions.get(own, session.id), 404, "not_found");
  await apiFailure(() => stack.owner.connectSessions.get(foreign, foreignSession.id), 404, "not_found");
  await apiFailure(() => stack.session(organization, sibling, createInput(apps.echoServer)), 404, "not_found");
  // A bearer link authorizes its own session. Query parameters cannot retarget
  // it to another organization, workspace, environment or app.
  const target = new URL(session.url);
  target.searchParams.set("organization_id", otherOrganization);
  target.searchParams.set("workspace_id", foreign);
  target.searchParams.set("app_id", foreignApp);
  target.searchParams.set("environment", "production");
  await flow.page.goto(target.href);
  await token(flow.page);
  await expect(flow.page.locator("[data-state='completed']")).toBeVisible();
  const connected = await completed(stack, organization, own, session.id);
  expect(connected.workspace_id === own && connected.app.id === apps.echoServer).toBe(true);
  await apiFailure(() => stack.owner.installations.get(sibling, connected.id), 404, "not_found");
  await apiFailure(() => stack.outsider.installations.get(own, connected.id), 404, "not_found");
  expect(await stack.owner.installations.list(sibling)).toEqual(siblingBefore);
  expect(await stack.outsider.installations.list(foreign)).toEqual(foreignBefore);
  expect((await stack.owner.connectSessions.get(sibling, siblingSession.id)).status).toBe("pending");
  expect((await stack.outsider.connectSessions.get(foreign, foreignSession.id)).status).toBe("pending");
});

test("expiry-command-scope", async ({ stack, flow }) => {
  const { organization, otherOrganization, workspaces, apps } = stack.fixture;
  const own = workspaces.qaPlayground.id;
  const sibling = workspaces.acmeSandbox.id;
  const foreign = await foreignWorkspace(stack);
  const foreignApp = await tokenApp(stack, otherOrganization, foreign, stack.outsider);
  const siblingApp = await tokenApp(stack, organization, sibling, stack.owner);
  const victim = await stack.session(organization, own, createInput(apps.echoServer));
  const sameWorkspace = await stack.session(organization, own, createInput(apps.echoServer));
  const otherWorkspace = await stack.session(organization, sibling, createInput(siblingApp));
  const otherTenant = await stack.outsider.connectSessions.create(foreign, createInput(foreignApp));
  const readAll = async () =>
    Promise.all([
      stack.owner.connectSessions.get(own, victim.id),
      stack.owner.connectSessions.get(own, sameWorkspace.id),
      stack.owner.connectSessions.get(sibling, otherWorkspace.id),
      stack.outsider.connectSessions.get(foreign, otherTenant.id),
    ]);
  const before = await readAll();
  expect(await stack.malformedExpiry()).not.toBe(0);
  expect(await readAll()).toEqual(before);
  // Test each missing predicate separately, using valid real IDs.
  expect(await stack.expire(otherOrganization, own, victim.id)).not.toBe(0);
  expect(await readAll()).toEqual(before);
  expect(await stack.expire(organization, sibling, victim.id)).not.toBe(0);
  expect(await readAll()).toEqual(before);
  expect(await stack.expire(organization, foreign, otherTenant.id)).not.toBe(0);
  expect(await readAll()).toEqual(before);
  expect(await stack.expire(otherOrganization, own, otherTenant.id)).not.toBe(0);
  expect(await readAll()).toEqual(before);
  expect(await stack.expire(organization, own, victim.id)).toBe(0);
  const after = await readAll();
  expect(after[0]?.status).toBe("expired");
  expect(after.slice(1)).toEqual(before.slice(1));
  expect(await stack.expire(organization, own, victim.id)).not.toBe(0);
  // The neighboring reference still works in a real page after the command.
  await flow.page.goto(sameWorkspace.url);
  await token(flow.page);
  await expect(flow.page.locator("[data-state='completed']")).toBeVisible();
  await completed(stack, organization, own, sameWorkspace.id);
  // Expiry must not shorten a completed link's outcome window or alter a cancellation.
  const finished = await stack.owner.connectSessions.get(own, sameWorkspace.id);
  expect(await stack.expire(organization, own, sameWorkspace.id)).not.toBe(0);
  expect(await stack.owner.connectSessions.get(own, sameWorkspace.id)).toEqual(finished);
  await stack.anonymous.connect.cancel(reference(otherWorkspace.url));
  const cancelled = await stack.owner.connectSessions.get(sibling, otherWorkspace.id);
  expect(await stack.expire(organization, sibling, otherWorkspace.id)).not.toBe(0);
  expect(await stack.owner.connectSessions.get(sibling, otherWorkspace.id)).toEqual(cancelled);
});

test("environment-scope", async ({ stack, flow }, info) => {
  const { organization, workspaces, apps } = stack.fixture;
  const sandbox = workspaces.qaPlayground.id;
  const production = workspaces.acme.id;
  const app = must(
    (await stack.owner.workspaceApps.list(production)).find((candidate) => candidate.key === "acme_publishing"),
    "Production app",
  );
  const sandboxKey = await stack.key(organization, "sandbox");
  const productionKey = await stack.key(organization, "production");
  const neighbor = await stack.session(organization, sandbox, createInput(apps.echoServer));
  const before = await stack.owner.installations.list(sandbox);
  await apiFailure(() => sandboxKey.connectSessions.create(production, createInput(app.id)), 404, "not_found");
  await apiFailure(() => productionKey.connectSessions.create(sandbox, createInput(apps.echoServer)), 404, "not_found");
  await apiFailure(
    () => stack.owner.connectSessions.create(production, createInput(app.id)),
    403,
    "console_read_only_in_production",
  );
  const session = await productionKey.connectSessions.create(production, createInput(app.id));
  await flow.page.goto(session.url);
  await expect(flow.page.getByText("Sandbox", { exact: true })).toHaveCount(0);
  await layout(flow.page, info.project.use.colorScheme === "dark" ? "dark" : "light");
  // Fixture publishing templates use the same single token field with a
  // provider-specific label; its browser input remains accessible by label.
  const input = flow.page.locator("input[data-key]");
  await expect(input).toHaveCount(1);
  await expect(input).toHaveAccessibleName(/.+/);
  await input.fill("production-browser-fixture-token");
  await flow.page.getByRole("button", { name: "Connect", exact: true }).click();
  await expect(flow.page.locator("[data-state='completed']")).toBeVisible();
  const result = await productionKey.connectSessions.get(production, session.id);
  expect(result.status).toBe("completed");
  expect(result.environment).toBe("production");
  await apiFailure(() => sandboxKey.connectSessions.get(production, session.id), 404, "not_found");
  await apiFailure(() => productionKey.connectSessions.get(sandbox, neighbor.id), 404, "not_found");
  expect((await stack.owner.connectSessions.get(sandbox, neighbor.id)).status).toBe("pending");
  expect(await stack.owner.installations.list(sandbox)).toEqual(before);
});
