/**
 * Isolation through the API alone: one organization's real objects, aimed at
 * from another organization or another workspace, must answer 404 and change
 * nothing. "Nothing" is what the victim can read about itself, before and
 * after.
 */
import { TERMINAL_RUN_STATUSES } from "@zupflow/contract";
import { ENVIRONMENTS, type OrganizationId, ZupflowApiError, type ZupflowClient } from "@zupflow/sdk";
import { expect } from "vitest";
import { must } from "./fixture";
import { notServedIn } from "./served";

/** What `read` answers, or undefined from a target that doesn't serve its operation yet. */
async function ifServed<T>(read: () => Promise<T>): Promise<T | undefined> {
  try {
    return await read();
  } catch (error) {
    if (notServedIn(error)) return undefined;
    throw error;
  }
}

/** One workspace's real objects, read through the API by a member of its organization. */
export async function loadTenant(
  client: ZupflowClient,
  org: OrganizationId,
  workspaceSlug: string,
  sandboxSlug: string,
) {
  const find = async (slug: string) =>
    must(
      (await client.workspaces.list(org, { q: slug, limit: 100 })).items.find((w) => w.slug === slug),
      `workspace ${slug}`,
    );
  const workspace = await find(workspaceSlug);
  // Connect is refused to Console sessions in Production, so Connect attempts start from Sandbox.
  const sandbox = await find(sandboxSlug);
  const w = workspace.id;
  const agents = await client.agents.list(w);
  const grants = (await Promise.all(agents.map((a) => client.grants.list(w, a.id)))).flat();
  const grant = must(grants[0], `grant in ${workspaceSlug}`);
  const runs = (await client.runs.list(w, { limit: 100 })).items;
  return {
    org,
    workspace,
    sandbox,
    agent: must(
      agents.find((a) => a.id === grant.agent_id),
      `agent with a grant in ${workspaceSlug}`,
    ),
    grant,
    // A finished run, so a refused cancel can't be confused with one that came too late.
    // Of the granted agent, so a filter by that agent has something to leak.
    run: must(
      runs.find((r) => r.agent_id === grant.agent_id && TERMINAL_RUN_STATUSES.includes(r.status)),
      `finished run of the granted agent in ${workspaceSlug}`,
    ),
    installation: must((await client.installations.list(w))[0], `installation in ${workspaceSlug}`),
    session: must((await client.connectSessions.list(w))[0], `Connect session in ${workspaceSlug}`),
    // A catalog app ready in the Sandbox, so Connect attempts that use it reach the check under test.
    orgApp: must(
      (await client.workspaceApps.available(sandbox.id)).find((a) => a.app.owner_workspace_id === null),
      `catalog app ready in ${sandboxSlug}`,
    ).app,
    workspaceApp: must(
      (await client.workspaceApps.list(w)).find((a) => a.owner_workspace_id === w),
      `workspace app of ${workspaceSlug}`,
    ),
    key: must((await client.apiKeys.list(org))[0], "API key"),
    // Model providers, where the target serves them; their attempts are left out otherwise.
    connection: await ifServed(async () => must((await client.modelConnections.list(org))[0], "model connection")),
    model: await ifServed(async () => must((await client.models.list(org, { limit: 1 })).items[0], "model")),
    hook: must((await client.webhooks.list(org))[0], "webhook"),
    member: must((await client.members.list(org))[0], "member"),
  };
}
export type Tenant = Awaited<ReturnType<typeof loadTenant>>;

/**
 * Everything the victim can read about the tenant, with what changes on its
 * own (a live run's progress, members' last activity) left out.
 */
export async function snapshotOf(client: ZupflowClient, t: Tenant): Promise<string> {
  const w = t.workspace.id;
  const runs = (await client.runs.list(w, { limit: 100 })).items.map((r) => ({
    id: r.id,
    agent: r.agent_id,
    // Only queued and running move on their own (the fixture has none at reset); a waiting run stays
    // waiting until someone decides.
    status: r.status === "queued" || r.status === "running" ? "live" : r.status,
  }));
  const agents = await client.agents.list(w);
  const orgApps = await client.organizationApps.list(t.org);
  const workspaceApps = await client.workspaceApps.list(w);
  return JSON.stringify([
    await client.organizations.get(t.org),
    (await client.workspaces.list(t.org, { limit: 1 })).pagination.total,
    (await client.members.list(t.org)).map(({ last_active_at: _seen, ...m }) => m),
    await client.workspaces.get(w),
    agents,
    await Promise.all(agents.map((a) => client.grants.list(w, a.id))),
    runs,
    await client.runs.events(w, t.run.id),
    await client.installations.list(w),
    await Promise.all((await client.installations.list(w)).map((i) => client.installations.tools(w, i.id))),
    await client.connectSessions.list(w),
    orgApps,
    await Promise.all(
      orgApps.flatMap((a) => ENVIRONMENTS.map((env) => client.organizationApps.getConfiguration(t.org, a.id, env))),
    ),
    workspaceApps,
    await Promise.all(workspaceApps.map((a) => client.workspaceApps.getConfiguration(w, a.id))),
    await Promise.all(ENVIRONMENTS.map((env) => client.connectSettings.get(t.org, env))),
    await client.toolPolicy.get(t.org),
    await Promise.all(orgApps.map((a) => client.toolPolicy.levels(t.org, a.id, { limit: 100 }))),
    await client.apiKeys.list(t.org),
    await ifServed(() => client.modelConnections.list(t.org)),
    await ifServed(() => client.models.list(t.org, { limit: 100 })),
    await ifServed(() => Promise.all(ENVIRONMENTS.map((env) => client.models.getDefault(t.org, env)))),
    await client.webhooks.list(t.org),
    await client.audit.list(t.org, { limit: 100 }),
  ]);
}

/** Every route class, aimed at `victim`'s real IDs, directly and through the attacker's own organization. */
export function attempts(r: ZupflowClient, own: Tenant, victim: Tenant): [string, () => Promise<unknown>][] {
  const v = victim;
  const w = v.workspace.id;
  const callId = "call_031DCp7XSYxLgDmAT3Dc7r" as const;
  // Model providers, only where the target serves them (loadTenant found them then).
  const { connection, model } = v;
  const victimModelAttempts: [string, () => Promise<unknown>][] =
    connection && model
      ? [
          ["[victim org] list model connections", () => r.modelConnections.list(v.org)],
          [
            "[victim org] connect a model provider",
            () =>
              r.modelConnections.create(v.org, {
                environment: "sandbox",
                kind: "openai",
                label: "x",
                api_key: "sk-test-long-attacker-0000-xx",
              }),
          ],
          ["[victim org] get model connection", () => r.modelConnections.get(v.org, connection.id)],
          [
            "[victim org] rename model connection",
            () => r.modelConnections.update(v.org, connection.id, { label: "x" }),
          ],
          [
            "[victim org] replace model key",
            () => r.modelConnections.replaceKey(v.org, connection.id, { api_key: "sk-test-long-attacker-0000-xx" }),
          ],
          ["[victim org] delete model connection", () => r.modelConnections.delete(v.org, connection.id)],
          ["[victim org] provider models", () => r.modelConnections.providerModels(v.org, connection.id)],
          ["[victim org] list models", () => r.models.list(v.org)],
          [
            "[victim org] add model",
            () => r.models.create(v.org, { connection_id: connection.id, provider_model_id: model.provider_model_id }),
          ],
          ["[victim org] get model", () => r.models.get(v.org, model.id)],
          ["[victim org] rename model", () => r.models.update(v.org, model.id, { name: "Owned" })],
          ["[victim org] remove model", () => r.models.delete(v.org, model.id)],
          ["[victim org] default model", () => r.models.getDefault(v.org, model.environment)],
          ["[victim org] set default model", () => r.models.setDefault(v.org, model.environment, { model_id: null })],
        ]
      : [];
  const ownModelAttempts: [string, () => Promise<unknown>][] =
    connection && model
      ? [
          ["[own org] model connection via own org", () => r.modelConnections.get(own.org, connection.id)],
          [
            "[own org] rename model connection via own org",
            () => r.modelConnections.update(own.org, connection.id, { label: "Owned" }),
          ],
          [
            "[own org] replace model key via own org",
            () => r.modelConnections.replaceKey(own.org, connection.id, { api_key: "sk-test-long-attacker-0000-xx" }),
          ],
          ["[own org] delete model connection via own org", () => r.modelConnections.delete(own.org, connection.id)],
          ["[own org] provider models via own org", () => r.modelConnections.providerModels(own.org, connection.id)],
          [
            "[own org] add model from victim connection",
            () =>
              r.models.create(own.org, { connection_id: connection.id, provider_model_id: model.provider_model_id }),
          ],
          ["[own org] model via own org", () => r.models.get(own.org, model.id)],
          ["[own org] rename model via own org", () => r.models.update(own.org, model.id, { name: "Owned" })],
          ["[own org] remove model via own org", () => r.models.delete(own.org, model.id)],
        ]
      : [];
  return [
    // The victim's workspace.
    ["[victim workspace] get workspace", () => r.workspaces.get(w)],
    ["[victim workspace] rename workspace", () => r.workspaces.update(w, { name: "Owned" })],
    ["[victim workspace] delete workspace", () => r.workspaces.delete(w)],
    ["[victim workspace] list agents", () => r.agents.list(w)],
    [
      "[victim workspace] create agent",
      () => r.agents.create(w, { name: "x", description: "", model_id: v.agent.model_id, instructions: "" }),
    ],
    ["[victim workspace] get agent", () => r.agents.get(w, v.agent.id)],
    ["[victim workspace] pause agent", () => r.agents.update(w, v.agent.id, { status: "paused" })],
    ["[victim workspace] delete agent", () => r.agents.delete(w, v.agent.id)],
    ["[victim workspace] list grants", () => r.grants.list(w, v.agent.id)],
    [
      "[victim workspace] create grant",
      () => r.grants.create(w, v.agent.id, { installation_id: v.installation.id, tools: [], approval_required: [] }),
    ],
    [
      "[victim workspace] update grant",
      () => r.grants.update(w, v.agent.id, v.grant.id, { tools: [], approval_required: [] }),
    ],
    ["[victim workspace] delete grant", () => r.grants.delete(w, v.agent.id, v.grant.id)],
    ["[victim workspace] list runs", () => r.runs.list(w)],
    ["[victim workspace] start run", () => r.runs.create(w, v.agent.id, { input: "hi" })],
    ["[victim workspace] get run", () => r.runs.get(w, v.run.id)],
    ["[victim workspace] run events", () => r.runs.events(w, v.run.id)],
    ["[victim workspace] cancel run", () => r.runs.cancel(w, v.run.id)],
    [
      "[victim workspace] approve",
      () => r.runs.resolveApproval(w, v.run.id, { call_id: callId, decision: "approved" }),
    ],
    ["[victim workspace] list installations", () => r.installations.list(w)],
    ["[victim workspace] get installation", () => r.installations.get(w, v.installation.id)],
    ["[victim workspace] rename installation", () => r.installations.update(w, v.installation.id, { label: "Owned" })],
    ["[victim workspace] disconnect", () => r.installations.disconnect(w, v.installation.id)],
    ["[victim workspace] delete installation", () => r.installations.delete(w, v.installation.id)],
    ["[victim workspace] list installation tools", () => r.installations.tools(w, v.installation.id)],
    ["[victim workspace] refresh installation tools", () => r.installations.refreshTools(w, v.installation.id)],
    [
      "[victim workspace] accept tool change",
      () => r.installations.acceptToolChange(w, v.installation.id, { name: "x", latest_hash: "0".repeat(64) }),
    ],
    ["[victim workspace] list connect sessions", () => r.connectSessions.list(w)],
    ["[victim workspace] get connect session", () => r.connectSessions.get(w, v.session.id)],
    [
      "[victim workspace] create connect session",
      () =>
        r.connectSessions.create(w, {
          app_id: v.orgApp.id,
          intent: "create",
          label: "x",
          installation_id: null,
          return_url: null,
        }),
    ],
    ["[victim workspace] list workspace apps", () => r.workspaceApps.list(w)],
    ["[victim workspace] get workspace app", () => r.workspaceApps.get(w, v.workspaceApp.id)],
    ["[victim workspace] rename workspace app", () => r.workspaceApps.update(w, v.workspaceApp.id, { name: "Owned" })],
    ["[victim workspace] workspace app configuration", () => r.workspaceApps.getConfiguration(w, v.workspaceApp.id)],
    [
      "[victim workspace] set workspace app configuration",
      () => r.workspaceApps.updateConfiguration(w, v.workspaceApp.id, { mcp_server_url: null }),
    ],
    ["[victim workspace] available apps", () => r.workspaceApps.available(w)],
    // The victim's organization.
    ["[victim org] get organization", () => r.organizations.get(v.org)],
    ["[victim org] rename organization", () => r.organizations.update(v.org, { name: "Owned" })],
    ["[victim org] usage", () => r.organizations.usage(v.org, "production")],
    ["[victim org] list members", () => r.members.list(v.org)],
    ["[victim org] invite member", () => r.members.invite(v.org, { email: "x@attacker.example", role: "owner" })],
    ["[victim org] change member role", () => r.members.update(v.org, v.member.id, { role: "owner" })],
    ["[victim org] remove member", () => r.members.remove(v.org, v.member.id)],
    ["[victim org] list workspaces", () => r.workspaces.list(v.org)],
    [
      "[victim org] create workspace",
      () => r.workspaces.create(v.org, { name: "x", slug: "x-owned", environment: "sandbox", external_id: null }),
    ],
    ["[victim org] list org apps", () => r.organizationApps.list(v.org)],
    ["[victim org] get org app", () => r.organizationApps.get(v.org, v.orgApp.id)],
    ["[victim org] rename org app", () => r.organizationApps.update(v.org, v.orgApp.id, { name: "Owned" })],
    ["[victim org] org app configuration", () => r.organizationApps.getConfiguration(v.org, v.orgApp.id, "production")],
    [
      "[victim org] set org app configuration",
      () => r.organizationApps.updateConfiguration(v.org, v.orgApp.id, "production", { mcp_server_url: null }),
    ],
    ["[victim org] org app installations", () => r.organizationApps.installations(v.org, v.orgApp.id)],
    ["[victim org] org runs", () => r.organizationRuns.list(v.org)],
    ["[victim org] audit", () => r.audit.list(v.org)],
    ["[victim org] connect settings", () => r.connectSettings.get(v.org, "production")],
    [
      "[victim org] set connect settings",
      () => r.connectSettings.update(v.org, "production", { return_urls: [], allowed_origins: [] }),
    ],
    ["[victim org] tool policy", () => r.toolPolicy.get(v.org)],
    ["[victim org] set tool policy", () => r.toolPolicy.update(v.org, { mode: "manual", new_tool_level: "deny" })],
    ["[victim org] tool levels", () => r.toolPolicy.levels(v.org, v.orgApp.id)],
    [
      "[victim org] set tool levels",
      () => r.toolPolicy.setLevels(v.org, v.orgApp.id, { levels: [{ tool: "x", level: "allow" }] }),
    ],
    ["[victim org] list keys", () => r.apiKeys.list(v.org)],
    ["[victim org] create key", () => r.apiKeys.create(v.org, { name: "x", environment: "sandbox" })],
    ["[victim org] revoke key", () => r.apiKeys.revoke(v.org, v.key.id)],
    ["[victim org] list webhooks", () => r.webhooks.list(v.org)],
    ["[victim org] update webhook", () => r.webhooks.update(v.org, v.hook.id, { enabled: false })],
    ["[victim org] test webhook", () => r.webhooks.sendTest(v.org, v.hook.id)],
    ["[victim org] delete webhook", () => r.webhooks.delete(v.org, v.hook.id)],
    ...victimModelAttempts,
    // The victim's child IDs, through the attacker's own organization and workspace.
    ["[own org] org app via own org", () => r.organizationApps.get(own.org, v.orgApp.id)],
    [
      "[own org] org app config via own org",
      () => r.organizationApps.getConfiguration(own.org, v.orgApp.id, "production"),
    ],
    ["[own org] key via own org", () => r.apiKeys.revoke(own.org, v.key.id)],
    ["[own org] webhook via own org", () => r.webhooks.update(own.org, v.hook.id, { enabled: false })],
    ["[own org] webhook test via own org", () => r.webhooks.sendTest(own.org, v.hook.id)],
    ["[own org] delete webhook via own org", () => r.webhooks.delete(own.org, v.hook.id)],
    ["[own org] rename org app via own org", () => r.organizationApps.update(own.org, v.orgApp.id, { name: "Owned" })],
    [
      "[own org] set org app config via own org",
      () => r.organizationApps.updateConfiguration(own.org, v.orgApp.id, "production", { mcp_server_url: null }),
    ],
    ["[own org] org app installations via own org", () => r.organizationApps.installations(own.org, v.orgApp.id)],
    ["[own org] tool levels of victim app via own org", () => r.toolPolicy.levels(own.org, v.orgApp.id)],
    [
      "[own org] set tool levels of victim app via own org",
      () => r.toolPolicy.setLevels(own.org, v.orgApp.id, { levels: [{ tool: "x", level: "allow" }] }),
    ],
    ...ownModelAttempts,
    ["[own org] member via own org", () => r.members.update(own.org, v.member.id, { role: "owner" })],
    ["[own org] remove member via own org", () => r.members.remove(own.org, v.member.id)],
    ["[own workspace] agent via own workspace", () => r.agents.get(own.workspace.id, v.agent.id)],
    [
      "[own workspace] pause agent via own workspace",
      () => r.agents.update(own.workspace.id, v.agent.id, { status: "paused" }),
    ],
    ["[own workspace] delete agent via own workspace", () => r.agents.delete(own.workspace.id, v.agent.id)],
    ["[own workspace] grants of victim agent", () => r.grants.list(own.workspace.id, v.agent.id)],
    [
      "[own workspace] grant to victim agent",
      () =>
        r.grants.create(own.workspace.id, v.agent.id, {
          installation_id: own.installation.id,
          tools: own.grant.tools.slice(0, 1),
          approval_required: [],
        }),
    ],
    [
      "[own workspace] update victim grant",
      () => r.grants.update(own.workspace.id, v.agent.id, v.grant.id, { tools: [], approval_required: [] }),
    ],
    ["[own workspace] delete victim grant", () => r.grants.delete(own.workspace.id, v.agent.id, v.grant.id)],
    // Through the attacker's Sandbox, so Production's read-only refusal can't answer first.
    ["[own workspace] run victim agent", () => r.runs.create(own.sandbox.id, v.agent.id, { input: "hi" })],
    ["[own workspace] cancel victim run", () => r.runs.cancel(own.workspace.id, v.run.id)],
    ["[own workspace] run via own workspace", () => r.runs.get(own.workspace.id, v.run.id)],
    ["[own workspace] run events via own workspace", () => r.runs.events(own.workspace.id, v.run.id)],
    ["[own workspace] installation via own workspace", () => r.installations.get(own.workspace.id, v.installation.id)],
    [
      "[own workspace] rename victim installation",
      () => r.installations.update(own.workspace.id, v.installation.id, { label: "Owned" }),
    ],
    [
      "[own workspace] disconnect victim installation",
      () => r.installations.disconnect(own.workspace.id, v.installation.id),
    ],
    ["[own workspace] delete victim installation", () => r.installations.delete(own.workspace.id, v.installation.id)],
    ["[own workspace] victim installation tools", () => r.installations.tools(own.workspace.id, v.installation.id)],
    [
      "[own workspace] accept victim tool change",
      () =>
        r.installations.acceptToolChange(own.workspace.id, v.installation.id, {
          name: "x",
          latest_hash: "0".repeat(64),
        }),
    ],
    // Through the attacker's Sandbox, so Production's read-only refusal can't answer first.
    [
      "[own workspace] refresh victim installation tools",
      () => r.installations.refreshTools(own.sandbox.id, v.installation.id),
    ],
    ["[own workspace] session via own workspace", () => r.connectSessions.get(own.workspace.id, v.session.id)],
    ["[own workspace] workspace app via own workspace", () => r.workspaceApps.get(own.workspace.id, v.workspaceApp.id)],
    [
      "[own workspace] rename victim workspace app",
      () => r.workspaceApps.update(own.workspace.id, v.workspaceApp.id, { name: "Owned" }),
    ],
    [
      "[own workspace] victim workspace app configuration",
      () => r.workspaceApps.getConfiguration(own.workspace.id, v.workspaceApp.id),
    ],
    [
      "[own workspace] set victim workspace app configuration",
      () => r.workspaceApps.updateConfiguration(own.workspace.id, v.workspaceApp.id, { mcp_server_url: null }),
    ],
    [
      "[own workspace] grant on victim installation",
      () =>
        r.grants.create(own.workspace.id, own.agent.id, {
          installation_id: v.installation.id,
          tools: own.grant.tools.slice(0, 1),
          approval_required: [],
        }),
    ],
    ["[own workspace] victim grant under own agent", () => r.grants.delete(own.workspace.id, own.agent.id, v.grant.id)],
    [
      "[own workspace] connect with victim app",
      () =>
        r.connectSessions.create(own.sandbox.id, {
          app_id: v.workspaceApp.id,
          intent: "create",
          label: "x",
          installation_id: null,
          return_url: null,
        }),
    ],
    [
      "[own org] connect with victim catalog app",
      () =>
        r.connectSessions.create(own.sandbox.id, {
          app_id: v.orgApp.id,
          intent: "create",
          label: "x",
          installation_id: null,
          return_url: null,
        }),
    ],
    [
      "[own workspace] reconnect victim installation",
      () =>
        r.connectSessions.create(own.sandbox.id, {
          app_id: own.orgApp.id,
          intent: "reconnect",
          label: "x",
          installation_id: v.installation.id,
          return_url: null,
        }),
    ],
  ];
}

export interface SealOptions {
  /** False for an attacker who may also use the victim organization's own routes. */
  victimOrgPaths?: boolean;
  /** Both tenants are workspaces of one organization: only the victim's IDs through the attacker's own workspace are attacks. */
  sameOrganization?: boolean;
}

/** Every attempt answers 404, and the victim reads the same state before and after. */
export async function expectSealed(
  attacker: ZupflowClient,
  own: Tenant,
  victim: Tenant,
  victimClient: ZupflowClient,
  { victimOrgPaths = true, sameOrganization = false }: SealOptions = {},
) {
  const before = await snapshotOf(victimClient, victim);
  for (const [what, attempt] of attempts(attacker, own, victim)) {
    if (!victimOrgPaths && what.startsWith("[victim org]")) continue;
    if (sameOrganization && !what.startsWith("[own workspace]")) continue;
    let answer: string = "succeeded";
    try {
      await attempt();
    } catch (error) {
      if (!(error instanceof ZupflowApiError)) throw error;
      answer = `${error.status} ${error.code}`;
    }
    expect(answer, what).toBe("404 not_found");
  }
  expect(await snapshotOf(victimClient, victim), "the victim's state after the attempts").toBe(before);
  // A workspace's own filter naming the victim's agent finds nothing, even within one organization.
  const byAgent = await attacker.runs.list(own.workspace.id, { agent_id: victim.agent.id, limit: 100 });
  expect(JSON.stringify(byAgent), "runs filtered by the victim's agent").not.toContain(victim.run.id);
  expect(byAgent.items).toEqual([]);
  // Filters naming the victim's workspace find nothing of it (within one organization, they rightly do).
  if (sameOrganization) return;
  // Nor does the attacker's Sandbox offer the victim's apps.
  const offered = JSON.stringify(await attacker.workspaceApps.available(own.sandbox.id));
  expect(offered).not.toContain(victim.orgApp.id);
  const filtered = JSON.stringify([
    await attacker.organizationRuns.list(own.org, { workspace_id: victim.workspace.id }),
    await attacker.audit.list(own.org, { workspace_id: victim.workspace.id }),
  ]);
  expect(filtered).not.toContain(victim.workspace.id);
  expect(filtered).not.toContain(victim.run.id);
}
