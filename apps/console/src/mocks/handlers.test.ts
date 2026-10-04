/**
 * Contract tests for the mock API. They pin the isolation and Connect rules
 * the Console relies on, so the real backend can be held to the same cases.
 */
import {
  type AgentId,
  type AppId,
  createClient,
  createHttpTransport,
  ID_PREFIXES,
  type OrganizationId,
  type WorkspaceId,
  ZupflowApiError,
} from "@zupflow/sdk";
import { setupServer } from "msw/node";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { db, resetDb } from "./db";
import { createHandlers } from "./handlers";
import { setLatency } from "./http";
import { hashReference, isId, newId, newReference, uuidOf } from "./ids";
import { mulberry32 } from "./random";
import { runEventsKey, timelineOf } from "./scope";
import { DEMO_USER_ID, seed } from "./seed";

const API = "http://zupflow.test/api";
const server = setupServer(...createHandlers(API));

/**
 * A Console session for `userId`, acting in `organization` (Northwind unless
 * given). Resolved per request, because every test reseeds the database.
 */
function clientFor(userId: string | null, organization: () => OrganizationId | undefined = northwindId) {
  return createClient(
    createHttpTransport({
      baseUrl: API,
      getToken: async () => (userId ? `mock_${userId}` : null),
      getOrganization: () => organization() ?? null,
    }),
  );
}

const northwindId = () => db().organizations.find((o) => o.slug === "northwind")?.id;

async function failure(promise: Promise<unknown>) {
  try {
    await promise;
  } catch (error) {
    if (error instanceof ZupflowApiError) return error;
    throw error;
  }
  throw new Error("Expected the request to fail");
}

/** The hosted Connect reference: the last segment of the session's link, as the hosted page reads it. */
const referenceOf = (session: { url: string }) => must(session.url.split("/").pop(), "Connect reference");

/** The base62 body for a UUID, written independently of the code under test. */
function uuidToBase62(uuid: string): string {
  const alphabet = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
  let n = BigInt(`0x${uuid.replaceAll("-", "")}`);
  let out = "";
  while (n > 0n) {
    out = alphabet[Number(n % 62n)] + out;
    n /= 62n;
  }
  return out.padStart(22, "0");
}

/** An active API key of the workspace's organization and environment: where a real approval comes from. */
function approvalKeyFor(workspaceId: string) {
  const workspace = must(
    db().workspaces.find((w) => w.id === workspaceId),
    "workspace",
  );
  return must(
    db().api_keys.find(
      (k) =>
        k.organization_id === workspace.organization_id && k.environment === workspace.environment && !k.revoked_at,
    ),
    "API key",
  ).id;
}

/** The value, or a failed test: replaces `?? ""` placeholders that would hide a missing fixture. */
function must<T>(value: T | null | undefined, what = "fixture"): T {
  if (value === null || value === undefined) throw new Error(`Missing ${what}`);
  return value;
}

const owner = clientFor(DEMO_USER_ID);
const ws = (slug: string) => {
  const w = db().workspaces.find((x) => x.slug === slug);
  if (!w) throw new Error(`No workspace ${slug}`);
  return w;
};
const app = (slug: string) => {
  const a = db().apps.find((x) => x.slug === slug);
  if (!a) throw new Error(`No app ${slug}`);
  return a;
};

beforeAll(() => {
  setLatency([0, 0]);
  server.listen({ onUnhandledFrame: "error" });
});
afterAll(() => server.close());
beforeEach(() => resetDb());

describe("authentication and membership", () => {
  it("answers 401 without a credential", async () => {
    const error = await failure(clientFor(null).me.get());
    expect(error.status).toBe(401);
    expect(error.code).toBe("unauthenticated");
  });

  it("checks the credential before parsing an ID, so a signed-out caller always gets 401", async () => {
    const anonymous = clientFor(null);
    const org = must(db().organizations[0]?.id, "organization");
    for (const call of [
      () => anonymous.workspaces.get("ws_short"),
      () => anonymous.workspaces.get(ws("acme").id),
      () => anonymous.members.list("org_short"),
      () => anonymous.members.list(org),
      () => anonymous.runs.get(ws("acme").id, "run_short"),
    ]) {
      expect((await failure(call())).status).toBe(401);
    }
  });

  it("hides another organization's workspace as 404", async () => {
    const outsider = newId("user");
    db().users.push({ id: outsider, name: "Out Sider", email: "o@outsider.example", avatar_url: null });
    const error = await failure(clientFor(outsider).workspaces.get(ws("acme").id));
    expect(error.status).toBe(404);
  });

  it("refuses Production keys to developers but allows Sandbox keys", async () => {
    const sam = db().users.find((u) => u.email === "sam@northwind.example");
    const orgId = must(db().organizations[0]?.id, "organization");
    const dev = clientFor(must(sam?.id, "Sam"));
    const denied = await failure(dev.apiKeys.create(orgId, { name: "x", environment: "production" }));
    expect(denied.status).toBe(403);
    const created = await dev.apiKeys.create(orgId, { name: "x", environment: "sandbox" });
    expect(created.secret.startsWith("zf_test_")).toBe(true);
    expect(created.key).not.toHaveProperty("secret");
  });
});

describe("apps and environments", () => {
  it("rejects an unsupported app kind instead of coercing it", async () => {
    const orgId = must(db().organizations[0]?.id, "organization");
    const error = await failure(
      owner.organizationApps.create(orgId, {
        // @ts-expect-error: the contract only accepts "mcp"
        kind: "openapi",
        name: "Other",
        slug: "other",
        description: "",
        auth_type: "token",
      }),
    );
    expect(error.code).toBe("unsupported_app_kind");
  });

  it("never lets a workspace change environment", async () => {
    const transport = createHttpTransport({
      baseUrl: API,
      getToken: async () => `mock_${DEMO_USER_ID}`,
      getOrganization: () => northwindId() ?? null,
    });
    const error = await failure(
      transport.request({ method: "PATCH", path: `/v1/workspaces/${ws("acme").id}`, body: { environment: "sandbox" } }),
    );
    expect(error.code).toBe("environment_immutable");
  });

  it("offers only apps ready in the workspace's own environment, with no fallback", async () => {
    // Linear is ready in Sandbox and incomplete in Production.
    const prod = await owner.workspaceApps.available(ws("acme").id);
    const sandbox = await owner.workspaceApps.available(ws("acme-sandbox").id);
    expect(prod.some((a) => a.app.slug === "linear")).toBe(false);
    expect(sandbox.some((a) => a.app.slug === "linear")).toBe(true);

    // Notion is in the catalog but configured in neither environment.
    const error = await failure(
      owner.connectSessions.create(ws("acme-sandbox").id, {
        app_id: app("notion").id,
        intent: "create",
        label: "Docs",
        installation_id: null,
        return_url: null,
      }),
    );
    expect(error.code).toBe("configuration_not_ready");
  });

  it("keeps a workspace app invisible to every other workspace", async () => {
    // The echo server belongs to the QA playground; Acme's Sandbox can't see it.
    const echo = app("echo-server");
    const other = ws("acme-sandbox");
    const available = await owner.workspaceApps.available(other.id);
    expect(available.some((a) => a.app.id === echo.id)).toBe(false);
    expect((await failure(owner.workspaceApps.get(other.id, echo.id))).status).toBe(404);
    const error = await failure(
      owner.connectSessions.create(other.id, {
        app_id: echo.id,
        intent: "create",
        label: "Echo",
        installation_id: null,
        return_url: null,
      }),
    );
    expect(error.status).toBe(404);
  });

  it("refuses private and plain-HTTP MCP endpoints", async () => {
    const orgId = must(db().organizations[0]?.id, "organization");
    const id = app("notion").id;
    const local = await failure(
      owner.organizationApps.updateConfiguration(orgId, id, "sandbox", {
        mcp_server_url: "https://169.254.169.254/mcp",
      }),
    );
    expect(local.code).toBe("destination_not_allowed");
    const http = await failure(
      owner.organizationApps.updateConfiguration(orgId, id, "sandbox", { mcp_server_url: "http://mcp.example.com" }),
    );
    expect(http.code).toBe("insecure_url");
  });

  it("never returns a stored client secret", async () => {
    const orgId = must(db().organizations[0]?.id, "organization");
    const config = await owner.organizationApps.updateConfiguration(orgId, app("notion").id, "production", {
      mcp_server_url: "https://mcp.notion.com/mcp",
      oauth_client_id: "client",
      oauth_client_secret: "super-secret-value-1234",
    });
    expect(JSON.stringify(config)).not.toContain("super-secret-value");
    expect(config.oauth?.client_secret).toEqual(expect.objectContaining({ set: true, last4: "1234" }));
    expect(config.status).toBe("ready");
  });
});

describe("Connect", () => {
  const start = (
    workspaceId: WorkspaceId,
    appId: AppId,
    extra: Partial<Parameters<typeof owner.connectSessions.create>[1]> = {},
  ) =>
    owner.connectSessions.create(workspaceId, {
      app_id: appId,
      intent: "create",
      label: "Echo 2",
      installation_id: null,
      return_url: null,
      ...extra,
    });

  it("only returns to allowlisted URLs", async () => {
    const error = await failure(
      start(ws("qa-playground").id, app("echo-server").id, { return_url: "https://evil.example/callback" }),
    );
    expect(error.code).toBe("return_url_not_allowed");
    const ok = await start(ws("qa-playground").id, app("echo-server").id, {
      return_url: "http://localhost:3000/settings/integrations",
    });
    expect(ok.status).toBe("pending");
  });

  it("completes once and refuses replay", async () => {
    const session = await start(ws("qa-playground").id, app("echo-server").id);
    const result = await owner.connect.submitCredentials(referenceOf(session), { values: { token: "abc123" } });
    expect(result.installation.workspace_id).toBe(ws("qa-playground").id);
    expect(result.session.status).toBe("completed");
    const replay = await failure(
      owner.connect.submitCredentials(referenceOf(session), { values: { token: "abc123" } }),
    );
    expect(replay.code).toBe("session_not_pending");
  });

  it("keeps the session open when the provider rejects a credential", async () => {
    const session = await start(ws("qa-playground").id, app("echo-server").id);
    const error = await failure(
      owner.connect.submitCredentials(referenceOf(session), { values: { token: "invalid" } }),
    );
    expect(error.code).toBe("credential_rejected");
    expect((await owner.connect.get(referenceOf(session))).status).toBe("pending");
  });

  it("refuses to reconnect a different account", async () => {
    const sandbox = ws("acme-sandbox");
    const installation = db().installations.find(
      (i) => i.workspace_id === sandbox.id && i.label === "Marketing (test)",
    );
    const session = await owner.connectSessions.create(sandbox.id, {
      app_id: app("google-workspace").id,
      intent: "reconnect",
      label: "ignored",
      installation_id: installation?.id ?? null,
      return_url: null,
    });
    const transport = createHttpTransport({ baseUrl: API, getToken: async () => null });
    const error = await failure(
      transport.request({
        method: "POST",
        path: `/v1/connect/sessions/${referenceOf(session)}/simulate-consent`,
        body: { decision: "approved", email: "someone-else@acme.example" },
      }),
    );
    expect(error.code).toBe("account_mismatch");
  });
});

describe("grants and runs", () => {
  it("cannot grant an installation from another workspace", async () => {
    const acme = ws("acme");
    const betaInstallation = db().installations.find((i) => i.workspace_id === ws("beta-labs").id);
    const agent = db().agents.find((a) => a.workspace_id === acme.id && a.status === "active");
    const error = await failure(
      owner.grants.create(acme.id, must(agent?.id), {
        installation_id: must(betaInstallation?.id),
        tools: ["search_files"],
        approval_required: [],
      }),
    );
    expect(error.status).toBe(404);
  });

  it("rejects tools the app doesn't have and approvals outside the grant", async () => {
    const qa = ws("qa-playground");
    const agent = await owner.agents.create(qa.id, {
      name: "Probe",
      description: "",
      model: "claude-haiku-4-5-20251001",
      instructions: "",
    });
    const installation = db().installations.find((i) => i.workspace_id === qa.id && i.status === "connected");
    const unknown = await failure(
      owner.grants.create(qa.id, agent.id, {
        installation_id: must(installation?.id),
        tools: ["rm_rf"],
        approval_required: [],
      }),
    );
    expect(unknown.code).toBe("unknown_tool");
    const outside = await failure(
      owner.grants.create(qa.id, agent.id, {
        installation_id: must(installation?.id),
        tools: ["echo"],
        approval_required: ["echo_write"],
      }),
    );
    expect(outside.code).toBe("approval_outside_grant");
  });

  it("does not read another workspace's run through this workspace's path", async () => {
    const betaRun = db().runs.find((r) => r.workspace_id === ws("beta-labs").id);
    const error = await failure(owner.runs.get(ws("acme").id, must(betaRun?.id)));
    expect(error.status).toBe(404);
  });

  it("refuses runs for a paused agent", async () => {
    const qa = ws("qa-playground");
    const agent = await owner.agents.create(qa.id, {
      name: "Paused",
      description: "",
      model: "claude-haiku-4-5-20251001",
      instructions: "",
    });
    await owner.agents.update(qa.id, agent.id, { status: "paused" });
    const error = await failure(owner.runs.create(qa.id, agent.id, { input: "hi" }));
    expect(error.code).toBe("agent_paused");
  });

  it("never lets the Console act on Production accounts", async () => {
    const acme = ws("acme");
    const agent = db().agents.find((a) => a.workspace_id === acme.id && a.status === "active");
    const run = await failure(owner.runs.create(acme.id, must(agent?.id), { input: "hi" }));
    expect(run.status).toBe(403);
    expect(run.code).toBe("console_read_only_in_production");
    const connect = await failure(
      owner.connectSessions.create(acme.id, {
        app_id: app("google-workspace").id,
        intent: "create",
        label: "Sales",
        installation_id: null,
        return_url: null,
      }),
    );
    expect(connect.code).toBe("console_read_only_in_production");
  });
});

describe("approvals", () => {
  it("leaves a denied action out of the run's result", async () => {
    const run = db().runs.find((r) => r.status === "waiting_for_approval");
    expect(run).toBeDefined();
    if (!run) return;
    const { resolveApproval, advanceRun } = await import("./runner");
    // The call actually waiting for a decision, not merely the plan's first call.
    const requested = [...timelineOf(db(), run)].reverse().find((e) => e.type === "approval.requested");
    const callId = must(requested?.type === "approval.requested" ? requested.data.call_id : null, "pending call");
    const pending = run.plan.find((s) => s.kind === "tool_call" && s.call.call_id === callId);
    const tool = must(pending?.kind === "tool_call" ? pending.call.tool : null, "pending tool");
    resolveApproval(db(), run, callId, "denied", approvalKeyFor(run.workspace_id), new Date().toISOString());
    for (let i = 0; i < 10 && run.status !== "completed"; i++) {
      advanceRun(db(), run, new Date().toISOString(), () => 0.5);
    }
    expect(run.status).toBe("completed");
    const events = timelineOf(db(), run);
    expect(events.some((e) => e.type === "tool_call.started" && e.data.tool === tool)).toBe(false);
    expect(run.output).not.toMatch(/published the post/);
  });
});

describe("scale and environments", () => {
  it("pages and searches workspaces on the server, one environment at a time", async () => {
    const orgId = must(db().organizations[0]?.id, "organization");
    const first = await owner.workspaces.list(orgId, { environment: "production", limit: 25 });
    expect(first.items).toHaveLength(25);
    expect(first.items.every((w) => w.environment === "production")).toBe(true);
    expect(first.pagination.total).toBeGreaterThan(1000);
    expect(first.pagination.next_cursor).not.toBeNull();

    const second = await owner.workspaces.list(orgId, {
      environment: "production",
      limit: 25,
      cursor: first.pagination.next_cursor ?? undefined,
    });
    const seen = new Set(first.items.map((w) => w.id));
    expect(second.items.some((w) => seen.has(w.id))).toBe(false);

    const found = await owner.workspaces.list(orgId, { q: "tenant_8c1f2a" });
    expect(found.items.map((w) => w.slug).sort()).toEqual(["acme", "acme-sandbox"]);

    const busiest = await owner.workspaces.list(orgId, { environment: "production", sort: "activity", limit: 1 });
    expect(busiest.items[0]?.stats.runs_last_24h).toBeGreaterThan(0);
  });

  it("lists runs across workspaces of one environment only", async () => {
    const orgId = must(db().organizations[0]?.id, "organization");
    const runs = await owner.organizationRuns.list(orgId, { environment: "sandbox", limit: 100 });
    expect(runs.items.length).toBeGreaterThan(0);
    expect(runs.items.every((r) => r.workspace.environment === "sandbox")).toBe(true);
    const outsider = newId("user");
    db().users.push({ id: outsider, name: "O", email: "o2@outsider.example", avatar_url: null });
    expect((await failure(clientFor(outsider).organizationRuns.list(orgId))).status).toBe(404);
  });

  it("records no end-user identity: decisions carry only the API key that sent them", async () => {
    let checked = 0;
    for (const workspace of [ws("acme"), ws("acme-sandbox")]) {
      const runs = await owner.runs.list(workspace.id, { limit: 100 });
      for (const run of runs.items) {
        for (const e of await owner.runs.events(workspace.id, run.id)) {
          if (e.type !== "approval.resolved") continue;
          expect(e.data).not.toHaveProperty("resolved_by");
          // The key belongs to the run's organization and environment, and existed when it sent the decision.
          const key = must(
            db().api_keys.find((k) => k.id === e.data.api_key_id),
            "approval key",
          );
          expect(key.organization_id).toBe(workspace.organization_id);
          expect(key.environment).toBe(workspace.environment);
          expect(Date.parse(key.created_at)).toBeLessThanOrEqual(Date.parse(e.created_at));
          checked++;
        }
      }
      const sessions = await owner.connectSessions.list(workspace.id);
      const installations = await owner.installations.list(workspace.id);
      expect(JSON.stringify([runs, sessions, installations])).not.toMatch(/"subject"|user_[0-9a-f]{8}/);
    }
    expect(checked).toBeGreaterThan(0);
  });

  it("refuses approvals from a Console session", async () => {
    const run = db().runs.find((r) => r.status === "waiting_for_approval");
    const pending = run?.plan.find((s) => s.kind === "tool_call");
    const error = await failure(
      owner.runs.resolveApproval(must(run?.workspace_id), must(run?.id), {
        call_id: must(pending?.kind === "tool_call" ? pending.call.call_id : null),
        decision: "approved",
      }),
    );
    expect(error.status).toBe(403);
    expect(error.code).toBe("approval_requires_api_key");
    expect(db().runs.find((r) => r.id === run?.id)?.status).toBe("waiting_for_approval");
  });

  it("gives every Connect session its hosted URL", async () => {
    const session = await owner.connectSessions.create(ws("qa-playground").id, {
      app_id: app("echo-server").id,
      intent: "create",
      label: "Echo 3",
      installation_id: null,
      return_url: null,
    });
    expect(session.url).toMatch(/\/connect\/[0-9A-Za-z]{32}$/);
    expect(session.url).not.toContain(session.id);
  });
});

describe("identifiers", () => {
  it("issues prefixed, base62 IDs that sort by creation time", () => {
    const first = newId("run", { at: Date.UTC(2026, 9, 3, 12) });
    const second = newId("run", { at: Date.UTC(2026, 9, 3, 12, 0, 0, 1) });
    expect(first).toMatch(/^run_[0-9A-Za-z]{22}$/);
    expect(first < second).toBe(true);
    expect(isId("run", first)).toBe(true);
    expect(isId("agent", first)).toBe(false);
  });

  it("encodes the UUID losslessly", () => {
    const id = newId("workspace", { at: Date.UTC(2026, 9, 3) });
    const uuid = uuidOf(id);
    expect(uuid).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    expect(uuidOf(`ws_${uuidToBase62(uuid ?? "")}`)).toBe(uuid);
  });

  it("seeds the same simulated runs whatever the clock says", () => {
    // IDs carry their creation time; nothing in the demo may depend on that.
    const shape = (nowMs: number) =>
      seed(nowMs).runs.map((r) => [r.status, ...r.plan.map((s) => (s.kind === "tool_call" ? s.call.tool : s.kind))]);
    expect(shape(Date.UTC(2026, 9, 3, 12))).toEqual(shape(Date.UTC(2026, 11, 24, 7, 13)));
  });

  it("registers each prefix once", () => {
    const prefixes = Object.values(ID_PREFIXES);
    expect(new Set(prefixes).size).toBe(prefixes.length);
    expect(prefixes.every((p) => /^[a-z]+$/.test(p))).toBe(true);
  });

  it("gives every seeded object a prefixed ID and leaks no bare UUID", async () => {
    const uuid = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i;
    const acme = ws("acme");
    const responses = await Promise.all([
      owner.me.get(),
      owner.workspaces.get(acme.id),
      owner.agents.list(acme.id),
      owner.installations.list(acme.id),
      owner.runs.list(acme.id, { limit: 5 }),
      owner.audit.list(acme.organization_id, { limit: 50 }),
    ]);
    expect(JSON.stringify(responses)).not.toMatch(uuid);
    expect(acme.id).toMatch(/^ws_[0-9A-Za-z]{22}$/);
  });

  it("answers 404 for a malformed ID, a bare UUID or another object's ID", async () => {
    const acme = ws("acme");
    const agent = must(db().agents.find((a) => a.workspace_id === acme.id));
    const bare = must(uuidOf(acme.id));
    for (const bad of [bare, `ws_${"z".repeat(22)}`, "ws_short", agent.id]) {
      const error = await failure(owner.workspaces.get(bad as WorkspaceId));
      expect(error.status).toBe(404);
      expect(error.code).toBe("not_found");
    }
    const wrongType = await failure(owner.agents.get(acme.id, acme.id as unknown as AgentId));
    expect(wrongType.status).toBe(404);
  });

  it("refuses an installation on a create session, including another workspace's", async () => {
    const beta = must(db().installations.find((i) => i.workspace_id === ws("beta-labs").id));
    for (const installation_id of [beta.id, "inst_garbage" as const]) {
      const error = await failure(
        owner.connectSessions.create(ws("qa-playground").id, {
          app_id: app("echo-server").id,
          intent: "create",
          label: "Echo X",
          installation_id,
          return_url: null,
        }),
      );
      expect(error.status).toBe(400);
    }
  });

  it("keeps hosted Connect inside the session's own tenant, even for a corrupted session", async () => {
    // A session whose app belongs to another organization must not show or use that app.
    const foreignApp = {
      ...structuredClone(app("echo-server")),
      id: newId("app"),
      organization_id: newId("organization"),
      owner_workspace_id: null,
      name: "Foreign Secret App",
    };
    db().apps.push(foreignApp);
    const reference = newReference();
    const qa = ws("qa-playground");
    const planted = {
      ...structuredClone(must(db().connect_sessions[0], "a seeded Connect session")),
      organization_id: qa.organization_id,
      workspace_id: qa.id,
      environment: qa.environment,
      id: newId("connect_session"),
      reference_hash: await hashReference(reference),
      app_id: foreignApp.id,
      intent: "create" as const,
      installation_id: null,
      status: "pending" as const,
      expires_at: new Date(Date.now() + 60_000).toISOString(),
    };
    db().connect_sessions.push(planted);
    expect(JSON.stringify(await owner.connect.get(reference))).not.toContain("Foreign Secret App");
    const error = await failure(owner.connect.submitCredentials(reference, { values: { token: "abc123" } }));
    expect(error.code).toBe("app_not_eligible");
    expect(db().installations.some((i) => i.app_id === foreignApp.id)).toBe(false);
  });

  it("keeps Connect sessions inside their workspace", async () => {
    const qa = ws("qa-playground");
    // A reconnect can't name another workspace's installation.
    const beta = must(db().installations.find((i) => i.workspace_id === ws("beta-labs").id));
    const reconnect = await failure(
      owner.connectSessions.create(qa.id, {
        app_id: app("echo-server").id,
        intent: "reconnect",
        label: "x",
        installation_id: beta.id,
        return_url: null,
      }),
    );
    expect(reconnect.status).toBe(404);
    // Another workspace's session isn't readable through this workspace's path.
    const elsewhere = must(db().connect_sessions.find((x) => x.workspace_id !== qa.id));
    expect((await failure(owner.connectSessions.get(qa.id, elsewhere.id))).status).toBe(404);
  });

  it("refuses an expired session reached by its reference", async () => {
    const session = await owner.connectSessions.create(ws("qa-playground").id, {
      app_id: app("echo-server").id,
      intent: "create",
      label: "Echo 7",
      installation_id: null,
      return_url: null,
    });
    const row = must(db().connect_sessions.find((x) => x.id === session.id));
    row.expires_at = new Date(Date.now() - 1000).toISOString();
    const error = await failure(owner.connect.submitCredentials(referenceOf(session), { values: { token: "abc123" } }));
    expect(error.status).toBe(409);
    expect(error.code).toBe("session_not_pending");
  });

  it("stores only a hash of the Connect reference", async () => {
    const session = await owner.connectSessions.create(ws("qa-playground").id, {
      app_id: app("echo-server").id,
      intent: "create",
      label: "Echo 6",
      installation_id: null,
      return_url: null,
    });
    expect(JSON.stringify(db())).not.toContain(referenceOf(session));
  });

  it("authorizes hosted Connect by the reference, never by the session ID", async () => {
    const session = await owner.connectSessions.create(ws("qa-playground").id, {
      app_id: app("echo-server").id,
      intent: "create",
      label: "Echo 4",
      installation_id: null,
      return_url: null,
    });
    expect((await owner.connect.get(referenceOf(session))).id).toBe(session.id);
    // The workspace's own view reads the session by ID, as the Console does while it waits.
    expect((await owner.connectSessions.get(ws("qa-playground").id, session.id)).status).toBe("pending");
    await owner.connect.submitCredentials(referenceOf(session), { values: { token: "abc123" } });
    expect((await owner.connectSessions.get(ws("qa-playground").id, session.id)).status).toBe("completed");
    const listed = await owner.connectSessions.list(ws("qa-playground").id);
    expect(listed.some((s) => s.id === session.id && s.status === "completed")).toBe(true);
    // Only the creating response carries the link; reads never hand the reference out again.
    expect(JSON.stringify(listed)).not.toContain(referenceOf(session));
    expect(JSON.stringify(await owner.connectSessions.get(ws("qa-playground").id, session.id))).not.toContain(
      referenceOf(session),
    );
    expect(JSON.stringify(await owner.connect.get(referenceOf(session)))).not.toMatch(/"url"/);
    const byId = await failure(owner.connect.get(session.id));
    expect(byId.status).toBe(404);
    const other = await owner.connectSessions.create(ws("qa-playground").id, {
      app_id: app("echo-server").id,
      intent: "create",
      label: "Echo 5",
      installation_id: null,
      return_url: null,
    });
    expect(referenceOf(other)).not.toBe(referenceOf(session));
  });
});

describe("two organizations", () => {
  const larkspurId = () => db().organizations.find((o) => o.slug === "larkspur")?.id;
  const rowan = () => clientFor(must(db().users.find((u) => u.email === "rowan@larkspur.example")).id, larkspurId);

  type Client = ReturnType<typeof clientFor>;

  /** One organization's real objects, read from the seed. */
  function tenant(orgSlug: string, workspaceSlug: string, sandboxSlug: string) {
    const org = must(
      db().organizations.find((o) => o.slug === orgSlug),
      orgSlug,
    ).id;
    const workspace = ws(workspaceSlug);
    // Connect is refused to Console sessions in Production, so Connect attempts start from Sandbox.
    const sandbox = ws(sandboxSlug);
    const here = <T extends { organization_id: string; workspace_id: string }>(rows: T[]) =>
      must(
        rows.find((r) => r.organization_id === org && r.workspace_id === workspace.id),
        `${orgSlug} row`,
      );
    const agent = here(db().agents);
    return {
      org,
      workspace,
      sandbox,
      agent,
      run: here(db().runs),
      installation: here(db().installations),
      session: here(db().connect_sessions),
      grant: must(
        db().grants.find((g) => g.organization_id === org && g.agent_id === agent.id),
        "grant",
      ),
      orgApp: must(db().apps.find((a) => a.organization_id === org && a.owner_workspace_id === null)),
      workspaceApp: must(db().apps.find((a) => a.organization_id === org && a.owner_workspace_id === workspace.id)),
      key: must(db().api_keys.find((k) => k.organization_id === org)),
      hook: must(db().webhooks.find((h) => h.organization_id === org)),
      member: must(db().memberships.find((m) => m.organization_id === org)),
    };
  }
  type Tenant = ReturnType<typeof tenant>;

  /** Everything an organization owns, to prove a refused request changed nothing. */
  function stateOf(org: string) {
    const d = db();
    const mine = <T extends { organization_id: string }>(rows: T[]) => rows.filter((r) => r.organization_id === org);
    const runIds = new Set(mine(d.runs).map((r) => r.id));
    return JSON.stringify([
      d.organizations.filter((o) => o.id === org),
      mine(d.workspaces),
      mine(d.apps),
      mine(d.configurations),
      mine(d.installations),
      mine(d.connect_sessions),
      mine(d.agents),
      mine(d.grants),
      mine(d.runs).map(({ plan: _plan, ...run }) => run),
      Object.entries(d.run_events).filter(([key]) => runIds.has(key.split("/")[2] as never)),
      mine(d.connect_settings),
      mine(d.api_keys),
      mine(d.webhooks),
      mine(d.audit),
      mine(d.memberships).map(({ last_active_at: _seen, ...m }) => m),
    ]);
  }

  /** Every route class, aimed at `victim`'s real IDs, directly and through the attacker's own organization. */
  function attempts(r: Client, own: Tenant, victim: Tenant): [string, () => Promise<unknown>][] {
    const v = victim;
    const w = v.workspace.id;
    const callId = "call_031DCp7XSYxLgDmAT3Dc7r" as const;
    return [
      // The victim's workspace.
      ["[victim workspace] get workspace", () => r.workspaces.get(w)],
      ["[victim workspace] rename workspace", () => r.workspaces.update(w, { name: "Owned" })],
      ["[victim workspace] delete workspace", () => r.workspaces.delete(w)],
      ["[victim workspace] list agents", () => r.agents.list(w)],
      [
        "[victim workspace] create agent",
        () => r.agents.create(w, { name: "x", description: "", model: "claude-sonnet-5-5", instructions: "" }),
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
      [
        "[victim workspace] rename installation",
        () => r.installations.update(w, v.installation.id, { label: "Owned" }),
      ],
      ["[victim workspace] disconnect", () => r.installations.disconnect(w, v.installation.id)],
      ["[victim workspace] delete installation", () => r.installations.delete(w, v.installation.id)],
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
      [
        "[victim workspace] rename workspace app",
        () => r.workspaceApps.update(w, v.workspaceApp.id, { name: "Owned" }),
      ],
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
      [
        "[victim org] org app configuration",
        () => r.organizationApps.getConfiguration(v.org, v.orgApp.id, "production"),
      ],
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
      ["[victim org] list keys", () => r.apiKeys.list(v.org)],
      ["[victim org] create key", () => r.apiKeys.create(v.org, { name: "x", environment: "sandbox" })],
      ["[victim org] revoke key", () => r.apiKeys.revoke(v.org, v.key.id)],
      ["[victim org] list webhooks", () => r.webhooks.list(v.org)],
      ["[victim org] update webhook", () => r.webhooks.update(v.org, v.hook.id, { enabled: false })],
      ["[victim org] test webhook", () => r.webhooks.sendTest(v.org, v.hook.id)],
      ["[victim org] delete webhook", () => r.webhooks.delete(v.org, v.hook.id)],
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
      ["[own org] member via own org", () => r.members.update(own.org, v.member.id, { role: "owner" })],
      ["[own org] remove member via own org", () => r.members.remove(own.org, v.member.id)],
      ["[own workspace] agent via own workspace", () => r.agents.get(own.workspace.id, v.agent.id)],
      ["[own workspace] run via own workspace", () => r.runs.get(own.workspace.id, v.run.id)],
      ["[own workspace] run events via own workspace", () => r.runs.events(own.workspace.id, v.run.id)],
      [
        "[own workspace] installation via own workspace",
        () => r.installations.get(own.workspace.id, v.installation.id),
      ],
      ["[own workspace] session via own workspace", () => r.connectSessions.get(own.workspace.id, v.session.id)],
      [
        "[own workspace] workspace app via own workspace",
        () => r.workspaceApps.get(own.workspace.id, v.workspaceApp.id),
      ],
      [
        "[own workspace] grant on victim installation",
        () =>
          r.grants.create(own.workspace.id, own.agent.id, {
            installation_id: v.installation.id,
            tools: ["list_issues"],
            approval_required: [],
          }),
      ],
      [
        "[own workspace] victim grant under own agent",
        () => r.grants.delete(own.workspace.id, own.agent.id, v.grant.id),
      ],
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

  /**
   * Every attempt must answer 404 and change nothing. Within one organization only the
   * attempts that use the victim's IDs through the attacker's own workspace are attacks;
   * a member may use their organization's other workspaces directly.
   */
  async function expectSealed(
    attacker: Client,
    own: Tenant,
    victim: Tenant,
    { victimOrgPaths = true, sameOrganization = false } = {},
  ) {
    const before = stateOf(victim.org);
    for (const [what, attempt] of attempts(attacker, own, victim)) {
      // A member of both organizations may use the victim organization's own routes; those name it in the path.
      if (!victimOrgPaths && what.startsWith("[victim org]")) continue;
      if (sameOrganization && !what.startsWith("[own workspace]")) continue;
      const error = await failure(attempt());
      expect(error.status, what).toBe(404);
    }
    expect(stateOf(victim.org)).toBe(before);
    // Filters naming the victim's workspace find nothing of it (within one organization, they rightly do).
    if (sameOrganization) return;
    const filtered = JSON.stringify([
      await attacker.organizationRuns.list(own.org, { workspace_id: victim.workspace.id }),
      await attacker.audit.list(own.org, { workspace_id: victim.workspace.id }),
    ]);
    expect(filtered).not.toContain(victim.org);
    expect(filtered).not.toContain(victim.run.id);
  }

  it("answers 404 and changes nothing when Larkspur uses Northwind's real IDs", async () => {
    await expectSealed(
      rowan(),
      tenant("larkspur", "harbor", "harbor-sandbox"),
      tenant("northwind", "acme", "acme-sandbox"),
    );
  });

  it("answers 404 and changes nothing when Northwind uses Larkspur's real IDs", async () => {
    await expectSealed(
      owner,
      tenant("northwind", "acme", "acme-sandbox"),
      tenant("larkspur", "harbor", "harbor-sandbox"),
    );
  });

  it("holds for a member of both organizations: the named organization decides", async () => {
    const larkspur = must(larkspurId());
    db().memberships.push({
      id: newId("member"),
      organization_id: larkspur,
      user_id: DEMO_USER_ID,
      role: "owner",
      status: "active",
      invited_at: new Date().toISOString(),
      joined_at: new Date().toISOString(),
      last_active_at: null,
    });
    const harbor = ws("harbor");
    const acme = ws("acme");
    const asNorthwind = clientFor(DEMO_USER_ID, northwindId);
    const asLarkspur = clientFor(DEMO_USER_ID, larkspurId);
    expect((await failure(asNorthwind.workspaces.get(harbor.id))).status).toBe(404);
    expect((await failure(asLarkspur.workspaces.get(acme.id))).status).toBe(404);
    expect((await asLarkspur.workspaces.get(harbor.id)).id).toBe(harbor.id);
    expect((await asNorthwind.workspaces.get(acme.id)).id).toBe(acme.id);
    // And acting as Northwind, Larkspur's children stay out of reach.
    await expectSealed(
      asNorthwind,
      tenant("northwind", "acme", "acme-sandbox"),
      tenant("larkspur", "harbor", "harbor-sandbox"),
      { victimOrgPaths: false },
    );
  });

  it("ignores rows whose organization doesn't match their workspace", async () => {
    // Corrupted rows: the scope columns exist so that a row labelled with the wrong
    // organization is never counted or used, even inside the right workspace.
    const acme = ws("acme");
    const larkspur = must(larkspurId());
    const larkspurApp = must(db().apps.find((a) => a.organization_id === larkspur && a.owner_workspace_id === null));
    const run = must(db().runs.find((r) => r.workspace_id === acme.id));
    const strayRun = { ...structuredClone(run), id: newId("run"), organization_id: larkspur };
    db().runs.push(strayRun);
    const installation = must(db().installations.find((i) => i.workspace_id === acme.id && i.status === "connected"));
    installation.app_id = larkspurApp.id;

    const runs = await owner.runs.list(acme.id, { limit: 100 });
    expect(runs.items.some((r) => r.id === strayRun.id)).toBe(false);
    expect((await failure(owner.runs.get(acme.id, strayRun.id))).status).toBe(404);
    const orgRuns = await owner.organizationRuns.list(must(northwindId()), { limit: 100 });
    expect(orgRuns.items.some((r) => r.id === strayRun.id)).toBe(false);

    const shown = await owner.installations.get(acme.id, installation.id);
    expect(shown.app.name).not.toBe(larkspurApp.name);
    const { planRun } = await import("./runner");
    const agent = must(db().agents.find((a) => a.workspace_id === acme.id));
    const plan = planRun(db(), { ...run, agent_id: agent.id });
    expect(JSON.stringify(plan)).not.toContain(larkspurApp.name);
  });

  it("leaves mislabelled rows out of every count, summary and lookup", async () => {
    // Each row below sits in a real workspace but names the wrong organization
    // (or the reverse). None may move a number or appear anywhere.
    const org = must(northwindId());
    const larkspur = must(larkspurId());
    const acme = ws("acme");
    const harbor = ws("harbor");
    const sandbox = ws("acme-sandbox");
    const agent = must(db().agents.find((a) => a.workspace_id === acme.id && a.status === "active"));
    const run = must(db().runs.find((r) => r.workspace_id === acme.id));
    const google = must(db().apps.find((a) => a.organization_id === org && a.slug === "google-workspace"));
    const harborInstallation = must(db().installations.find((i) => i.workspace_id === harbor.id));

    const snapshot = async () => {
      const [usage, byActivity, workspace, app, me, agentNow, events, audit] = await Promise.all([
        owner.organizations.usage(org, "production", 14),
        owner.workspaces.list(org, { environment: "production", sort: "activity", limit: 100 }),
        owner.workspaces.get(acme.id),
        owner.organizationApps.get(org, google.id),
        owner.me.get(),
        owner.agents.get(acme.id, agent.id),
        owner.runs.events(acme.id, run.id),
        owner.audit.list(org, { limit: 100 }),
      ]);
      return {
        usage: usage.totals,
        order: byActivity.items.map((w) => w.id),
        stats: workspace.stats,
        installs: app.installation_count,
        setup: me.organizations.find((o) => o.id === org)?.setup,
        agent: [agentNow.runs_last_24h, agentNow.grant_count],
        events: events.map((e) => e.id),
        auditWorkspaces: audit.items.map((e) => e.workspace?.id ?? null),
      };
    };
    const before = await snapshot();

    const now = new Date().toISOString();
    // A run, an agent and a grant in Acme labelled as Larkspur's.
    db().agents.push({ ...structuredClone(agent), id: newId("agent"), organization_id: larkspur });
    // Recent runs, labelled as Larkspur's, in a Northwind workspace that has had none: it mustn't climb the activity order.
    const quiet = must(
      db().workspaces.find(
        (w) =>
          w.organization_id === org &&
          w.environment === "production" &&
          !db().runs.some((r) => r.workspace_id === w.id),
      ),
      "a quiet workspace",
    );
    for (let i = 0; i < 5; i++) {
      db().runs.push({
        ...structuredClone(run),
        id: newId("run"),
        organization_id: larkspur,
        workspace_id: quiet.id,
        created_at: now,
      });
    }
    db().runs.push({
      ...structuredClone(run),
      id: newId("run"),
      organization_id: larkspur,
      agent_id: agent.id,
      created_at: now,
    });
    db().grants.push({
      ...structuredClone(must(db().grants.find((g) => g.workspace_id === acme.id))),
      id: newId("grant"),
      organization_id: larkspur,
      agent_id: agent.id,
    });
    // An installation in Acme labelled as Larkspur's, using Northwind's app.
    db().installations.push({
      ...structuredClone(harborInstallation),
      id: newId("installation"),
      workspace_id: acme.id,
      app_id: google.id,
    });
    // A run event under Acme's run's own key, labelled as Larkspur's.
    const events = must(db().run_events[runEventsKey(run)]);
    events.push({
      ...structuredClone(must(events[0])),
      id: newId("run_event"),
      organization_id: larkspur,
      sequence: events.length + 1,
    });
    // An audit event of Northwind naming Larkspur's workspace.
    db().audit.unshift({
      id: newId("audit_event"),
      organization_id: org,
      workspace_id: harbor.id,
      environment: "production",
      actor: { type: "system", id: null, name: "Test" },
      action: "workspace.updated",
      target: { type: "workspace", id: harbor.id, name: "x" },
      created_at: now,
    });

    const after = await snapshot();
    expect(after.usage).toEqual(before.usage);
    expect(after.order).toEqual(before.order);
    expect(after.stats).toEqual(before.stats);
    expect(after.installs).toBe(before.installs);
    expect(after.setup).toEqual(before.setup);
    expect(after.agent).toEqual(before.agent);
    expect(after.events).toEqual(before.events);
    expect(after.auditWorkspaces.slice(1)).toEqual(before.auditWorkspaces.slice(0, after.auditWorkspaces.length - 1));
    expect(after.auditWorkspaces[0]).toBeNull();

    // A brand-new organization's setup can't be completed by another organization's rows.
    const fresh = await owner.organizations.create({ name: "Stray Check", slug: "stray-check" });
    const freshWorkspace = await owner.workspaces.create(fresh.id, {
      name: "Only",
      slug: "only",
      environment: "sandbox",
      external_id: null,
    });
    db().runs.push({ ...structuredClone(run), id: newId("run"), workspace_id: freshWorkspace.id });
    db().agents.push({ ...structuredClone(agent), id: newId("agent"), workspace_id: freshWorkspace.id });
    // A configuration of the new organization that names Larkspur's ready app.
    const larkspurConfig = must(
      db().configurations.find(
        (c) => c.organization_id === larkspur && c.workspace_id === null && c.environment === "production",
      ),
    );
    db().configurations.push({ ...structuredClone(larkspurConfig), organization_id: fresh.id });
    // Another organization's label, in the new organization's workspace.
    db().installations.push({
      ...structuredClone(harborInstallation),
      id: newId("installation"),
      organization_id: larkspur,
      workspace_id: freshWorkspace.id,
      status: "connected",
    });
    // The new organization's own label, but in another organization's workspace.
    const harborWs = ws("harbor");
    db().runs.push({ ...structuredClone(run), id: newId("run"), organization_id: fresh.id, workspace_id: harborWs.id });
    db().agents.push({
      ...structuredClone(agent),
      id: newId("agent"),
      organization_id: fresh.id,
      workspace_id: harborWs.id,
    });
    db().installations.push({
      ...structuredClone(harborInstallation),
      id: newId("installation"),
      organization_id: fresh.id,
      workspace_id: harborWs.id,
    });
    // Its own app, with a ready configuration labelled for another workspace.
    const larkspurApp = must(db().apps.find((a) => a.organization_id === larkspur && a.owner_workspace_id === null));
    const freshApp = {
      ...structuredClone(larkspurApp),
      id: newId("app"),
      organization_id: fresh.id,
      owner_workspace_id: null,
    };
    db().apps.push(freshApp);
    db().configurations.push({
      ...structuredClone(larkspurConfig),
      app_id: freshApp.id,
      organization_id: fresh.id,
      workspace_id: harborWs.id,
    });
    const setup = (await owner.me.get()).organizations.find((o) => o.id === fresh.id)?.setup;
    expect(setup?.has_run).toBe(false);
    expect(setup?.has_agent).toBe(false);
    expect(setup?.has_installation).toBe(false);
    expect(setup?.has_api_key).toBe(false);
    expect(setup?.has_ready_configuration).toBe(false);

    // Hosted Connect: a session in Acme's Sandbox, labelled Northwind, that names Larkspur's workspace or installation.
    const reference = newReference();
    const base = structuredClone(must(db().connect_sessions[0], "a seeded Connect session"));
    db().connect_sessions.push({
      ...base,
      id: newId("connect_session"),
      organization_id: org,
      workspace_id: harbor.id,
      reference_hash: await hashReference(reference),
      environment: "production",
      app_id: google.id,
      intent: "reconnect",
      installation_id: harborInstallation.id,
      status: "pending",
      expires_at: new Date(Date.now() + 60_000).toISOString(),
    });
    const hosted = await owner.connect.get(reference);
    expect(hosted.expected_account).toBeNull();
    const refused = await failure(owner.connect.submitCredentials(reference, { values: { token: "abc123" } }));
    expect(refused.code).toBe("app_not_eligible");
    expect(sandbox.organization_id).toBe(org);
  });

  it("builds every scoped row with id, organization_id and workspace_id first", async () => {
    // Rows created through the API, not only seeded ones.
    const qa = ws("qa-playground");
    const session = await owner.connectSessions.create(qa.id, {
      app_id: app("echo-server").id,
      intent: "create",
      label: "Order check",
      installation_id: null,
      return_url: null,
    });
    const { installation } = await owner.connect.submitCredentials(referenceOf(session), {
      values: { token: "abc123" },
    });
    const agent = await owner.agents.create(qa.id, {
      name: "Order",
      description: "",
      model: "claude-sonnet-5-5",
      instructions: "",
    });
    const tool = must(app("echo-server").tools[0]).name;
    const grant = await owner.grants.create(qa.id, agent.id, {
      installation_id: installation.id,
      tools: [tool],
      approval_required: [],
    });
    const run = await owner.runs.create(qa.id, agent.id, { input: "Order check" });
    // Creating an app creates its configurations on first read.
    const catalogApp = await owner.organizationApps.create(qa.organization_id, {
      kind: "mcp",
      name: "Order",
      slug: "order-check",
      description: "",
      auth_type: "token",
    });
    expect(db().configurations.some((c) => c.app_id === catalogApp.id)).toBe(true);
    const created = new Set<string>([session.id, installation.id, agent.id, grant.id, run.id]);
    const d = db();
    const tables = {
      installations: d.installations,
      connect_sessions: d.connect_sessions,
      agents: d.agents,
      grants: d.grants,
      runs: d.runs,
      run_events: Object.values(d.run_events).flat(),
    };
    expect(
      Object.values(tables)
        .flat()
        .filter((r) => created.has(r.id)).length,
    ).toBe(created.size);
    expect(tables.run_events.some((e) => e.run_id === run.id)).toBe(true);
    // Configurations, keyed naturally, start with their scope.
    for (const c of d.configurations) {
      expect(Object.keys(c).slice(0, 3), "configuration").toEqual(["organization_id", "workspace_id", "app_id"]);
    }
    // Organization-scoped rows that may name a workspace start with their organization.
    for (const event of d.audit) expect(Object.keys(event).slice(0, 2), "audit").toEqual(["id", "organization_id"]);
    for (const [name, rows] of Object.entries(tables)) {
      expect(rows.length, name).toBeGreaterThan(0);
      for (const row of rows) {
        expect(Object.keys(row).slice(0, 3), name).toEqual(["id", "organization_id", "workspace_id"]);
      }
    }
  });

  /**
   * Every read an organization's Console makes, for every workspace given:
   * if any of it moves when rows are added that belong to nobody visible,
   * some statement checked one layer instead of two.
   */
  async function everythingSeenBy(client: Client, org: string, workspaceIds: string[]) {
    const o = org as OrganizationId;
    const out: unknown[] = [];
    const me = await client.me.get();
    out.push(me.organizations.map(({ setup, id, role }) => ({ setup, id, role })));
    out.push(await client.organizations.get(o));
    for (const env of ["sandbox", "production"] as const) {
      out.push((await client.organizations.usage(o, env, 14)).totals);
      out.push(await client.connectSettings.get(o, env));
      for (const sort of ["name", "activity"] as const) {
        out.push(
          (await client.workspaces.list(o, { environment: env, sort, limit: 100 })).items.map((w) => [w.id, w.stats]),
        );
      }
      out.push((await client.organizationRuns.list(o, { environment: env, limit: 100 })).items.map((r) => r.id));
    }
    // Last-active moves on every request the caller makes; everything else must hold still.
    out.push((await client.members.list(o)).map(({ last_active_at: _seen, ...m }) => m));
    out.push((await client.audit.list(o, { limit: 100 })).items.map((e) => [e.id, e.workspace?.id ?? null]));
    out.push(await client.apiKeys.list(o));
    out.push(await client.webhooks.list(o));
    const apps = await client.organizationApps.list(o);
    out.push(apps);
    for (const app of apps) {
      out.push(
        (await client.organizationApps.installations(o, app.id, { limit: 100 })).items.map((r) => r.installation.id),
      );
    }
    for (const w of workspaceIds as WorkspaceId[]) {
      out.push(await client.workspaces.get(w));
      out.push(await client.workspaceApps.available(w));
      const workspaceApps = await client.workspaceApps.list(w);
      out.push(workspaceApps);
      for (const app of workspaceApps) out.push(await client.workspaceApps.getConfiguration(w, app.id));
      const agents = await client.agents.list(w);
      out.push(agents);
      for (const agent of agents) out.push(await client.grants.list(w, agent.id));
      out.push(await client.installations.list(w));
      out.push(await client.connectSessions.list(w));
      const runs = (await client.runs.list(w, { limit: 100 })).items;
      out.push(runs);
      for (const run of runs.slice(0, 10)) out.push(await client.runs.events(w, run.id));
    }
    return JSON.parse(JSON.stringify(out)) as unknown;
  }

  it("ignores every row labelled with the other organization, in every read and by every ID", async () => {
    const northwind = must(northwindId());
    const larkspur = must(larkspurId());
    const nwWorkspaces = ["acme", "acme-sandbox", "qa-playground", "beta-labs"].map((slug) => ws(slug).id);
    const lkWorkspaces = ["harbor", "harbor-sandbox"].map((slug) => ws(slug).id);
    const rowanClient = rowan();
    const seenBefore = [
      await everythingSeenBy(owner, northwind, nwWorkspaces),
      await everythingSeenBy(rowanClient, larkspur, lkWorkspaces),
    ];
    // Reading is stable on its own.
    expect(await everythingSeenBy(owner, northwind, nwWorkspaces)).toEqual(seenBefore[0]);

    // Two kinds of mislabelled copy of every workspace-scoped row, put first so a lookup that
    // checks one layer would find the copy before the real row:
    //  - same workspace, the other organization's label;
    //  - same organization, sitting in the other organization's workspace.
    const d = db();
    const otherOrg = (org: string) => (org === northwind ? larkspur : northwind) as OrganizationId;
    const partner = new Map<string, WorkspaceId>([
      [ws("acme").id, ws("harbor").id],
      [ws("beta-labs").id, ws("harbor").id],
      [ws("acme-sandbox").id, ws("harbor-sandbox").id],
      [ws("qa-playground").id, ws("harbor-sandbox").id],
      [ws("harbor").id, ws("acme").id],
      [ws("harbor-sandbox").id, ws("acme-sandbox").id],
    ]);
    const ours = (row: { organization_id: string; workspace_id: string }) =>
      (row.organization_id === northwind || row.organization_id === larkspur) && partner.has(row.workspace_id);
    type Copy = { kind: string; id: string; agent?: string };
    const copies: Copy[] = [];
    const variants = <T extends { organization_id: OrganizationId; workspace_id: WorkspaceId }>(row: T): T[] => [
      { ...structuredClone(row), organization_id: otherOrg(row.organization_id) },
      { ...structuredClone(row), workspace_id: must(partner.get(row.workspace_id)) },
    ];
    const mirror = <T extends { id: string; organization_id: OrganizationId; workspace_id: WorkspaceId }>(
      kind: string,
      rows: T[],
      fresh: () => T["id"],
      tweak: (copy: T) => T = (copy) => copy,
    ) => {
      for (const row of rows.filter(ours)) {
        for (const variant of variants(row)) {
          const copy = tweak({ ...variant, id: fresh() });
          rows.unshift(copy);
          copies.push({ kind, id: copy.id, agent: (row as { agent_id?: string }).agent_id });
        }
      }
    };
    mirror(
      "installation",
      d.installations,
      () => newId("installation"),
      (c) => ({ ...c, label: "MISLABELLED" }),
    );
    mirror(
      "connect_session",
      d.connect_sessions,
      () => newId("connect_session"),
      (c) => ({ ...c, label: "MISLABELLED" }),
    );
    mirror(
      "agent",
      d.agents,
      () => newId("agent"),
      (c) => ({ ...c, name: "MISLABELLED" }),
    );
    mirror("grant", d.grants, () => newId("grant"));
    for (const run of d.runs.filter(ours)) {
      for (const variant of variants(run)) {
        const copy = { ...variant, id: newId("run"), input: "MISLABELLED" };
        d.runs.unshift(copy);
        d.run_events[runEventsKey(copy)] = timelineOf(d, run).map((e) => ({
          ...structuredClone(e),
          id: newId("run_event"),
          run_id: copy.id,
          organization_id: copy.organization_id,
          workspace_id: copy.workspace_id,
        }));
        copies.push({ kind: "run", id: copy.id });
      }
      // And stray events inside the real run's own timeline, first in line.
      const timeline = d.run_events[runEventsKey(run)];
      const first = timeline?.[0];
      if (timeline && first) {
        for (const variant of variants(first)) timeline.unshift({ ...variant, id: newId("run_event"), sequence: 9999 });
      }
    }
    for (const app of d.apps.filter((a) => a.owner_workspace_id !== null && partner.has(a.owner_workspace_id))) {
      if (app.organization_id !== northwind && app.organization_id !== larkspur) continue;
      const home = must(app.owner_workspace_id);
      const placements: [OrganizationId, WorkspaceId][] = [
        [otherOrg(app.organization_id), home],
        [app.organization_id, must(partner.get(home))],
      ];
      for (const [organization_id, owner_workspace_id] of placements) {
        const copy = {
          ...structuredClone(app),
          id: newId("app"),
          organization_id,
          owner_workspace_id,
          name: "MISLABELLED",
        };
        d.apps.unshift(copy);
        for (const c of d.configurations.filter((x) => x.app_id === app.id)) {
          d.configurations.unshift({
            ...structuredClone(c),
            app_id: copy.id,
            organization_id,
            workspace_id: owner_workspace_id,
          });
        }
        copies.push({ kind: "workspace_app", id: copy.id });
      }
    }
    // Configurations of the real apps, labelled with the other organization or another workspace, first in line.
    for (const c of d.configurations.filter((x) => x.organization_id === northwind || x.organization_id === larkspur)) {
      d.configurations.unshift({
        ...structuredClone(c),
        organization_id: otherOrg(c.organization_id),
        mcp_server_url: null,
      });
      if (c.workspace_id) {
        d.configurations.unshift({
          ...structuredClone(c),
          workspace_id: must(partner.get(c.workspace_id)),
          mcp_server_url: null,
        });
      }
    }

    expect(await everythingSeenBy(owner, northwind, nwWorkspaces)).toEqual(seenBefore[0]);
    expect(await everythingSeenBy(rowanClient, larkspur, lkWorkspaces)).toEqual(seenBefore[1]);

    // Nor can anyone reach a copy by its ID, through any workspace.
    const clients: [Client, WorkspaceId[]][] = [
      [owner, nwWorkspaces as WorkspaceId[]],
      [rowanClient, lkWorkspaces as WorkspaceId[]],
    ];
    for (const copy of copies) {
      for (const [client, workspaces] of clients) {
        for (const w of workspaces) {
          const id = copy.id as never;
          const agent = copy.agent as never;
          const tries: Record<string, (() => Promise<unknown>)[]> = {
            installation: [
              () => client.installations.get(w, id),
              () => client.installations.update(w, id, { label: "x" }),
              () => client.installations.delete(w, id),
            ],
            connect_session: [() => client.connectSessions.get(w, id)],
            agent: [() => client.agents.get(w, id), () => client.agents.update(w, id, { name: "x" })],
            grant: copy.agent
              ? [
                  () => client.grants.update(w, agent, id, { tools: ["x"], approval_required: [] }),
                  () => client.grants.delete(w, agent, id),
                ]
              : [],
            run: [() => client.runs.get(w, id), () => client.runs.events(w, id)],
            workspace_app: [() => client.workspaceApps.get(w, id), () => client.workspaceApps.getConfiguration(w, id)],
          };
          for (const attempt of tries[copy.kind] ?? []) {
            expect((await failure(attempt())).status, `${copy.kind} ${copy.id}`).toBe(404);
          }
        }
      }
    }
    expect(await everythingSeenBy(owner, northwind, nwWorkspaces)).toEqual(seenBefore[0]);
    expect(await everythingSeenBy(rowanClient, larkspur, lkWorkspaces)).toEqual(seenBefore[1]);
  }, 60_000);

  it.each([
    ["another organization", "organization"],
    ["a sibling workspace", "workspace"],
  ] as const)("never runs with an agent, grant or installation labelled with %s", async (_label, layer) => {
    const { advanceRun, enqueueRun, planRun } = await import("./runner");
    const larkspur = must(larkspurId());
    const acme = ws("acme");
    // Move a row out of Acme at one layer only: the other organization's label, or Northwind's other workspace.
    const relabel = <T extends { organization_id: OrganizationId; workspace_id: WorkspaceId }>(row: T): T =>
      layer === "organization"
        ? Object.assign(row, { organization_id: larkspur })
        : Object.assign(row, { workspace_id: ws("beta-labs").id });
    const grant = must(
      db().grants.find(
        (g) =>
          g.workspace_id === acme.id &&
          g.approval_required.length === 0 &&
          db().installations.some((i) => i.id === g.installation_id && i.status === "connected"),
      ),
      "a grant without approvals on a connected installation",
    );
    const agent = must(db().agents.find((a) => a.id === grant.agent_id));
    const template = must(db().runs.find((r) => r.workspace_id === acme.id));
    const run = {
      ...structuredClone(template),
      id: newId("run"),
      agent_id: agent.id,
      status: "queued" as const,
      input: "Do the thing",
      output: null,
      error: null,
      started_at: null,
      finished_at: null,
    };
    // Fixed seeds, so the test never depends on how a random plan falls.
    const seeds = Array.from({ length: 40 }, (_, i) => mulberry32(i + 1));
    run.plan = must(
      seeds.map((rng) => planRun(db(), run, rng)).find((plan) => plan.some((s) => s.kind === "tool_call")),
      "a plan that calls a tool",
    );
    const planned = run.plan.flatMap((s) => (s.kind === "tool_call" ? [s.call.installation_id] : []));
    expect(planned.length).toBeGreaterThan(0);

    // A mislabelled grant giving this agent an installation it isn't granted is never planned with.
    const ungranted = must(
      db().installations.find(
        (i) =>
          i.workspace_id === acme.id &&
          i.status === "connected" &&
          !db().grants.some((g) => g.agent_id === agent.id && g.installation_id === i.id),
      ),
      "an installation the agent isn't granted",
    );
    const ungrantedTool = must(db().apps.find((a) => a.id === ungranted.app_id)?.tools[0]).name;
    db().grants.unshift(
      relabel({
        ...structuredClone(grant),
        id: newId("grant"),
        installation_id: ungranted.id,
        tools: [ungrantedTool],
        approval_required: [],
      }),
    );
    for (let i = 0; i < 25; i++) {
      const plan = planRun(db(), run, mulberry32(100 + i));
      expect(plan.some((s) => s.kind === "tool_call" && s.call.installation_id === ungranted.id)).toBe(false);
    }
    db().runs.push(run);
    db().run_events[runEventsKey(run)] = [];
    enqueueRun(db(), run);
    // Corrupt the rows the run will use: same IDs, mislabelled at one layer.
    relabel(agent);
    for (const i of db().installations) if (planned.includes(i.id)) relabel(i);
    for (let step = 0; step < 30 && !["completed", "failed", "cancelled"].includes(run.status); step++) {
      advanceRun(db(), run, new Date().toISOString(), () => 0.5);
    }
    // Planning again with the corrupted rows uses none of them, however the plan falls.
    for (let i = 0; i < 25; i++) {
      const replanned = planRun(db(), run, mulberry32(200 + i));
      expect(replanned.some((s) => s.kind === "tool_call" && planned.includes(s.call.installation_id))).toBe(false);
    }
    // And the run's agent no longer resolves for display either.
    expect((await owner.runs.get(acme.id, run.id)).agent_name).toBe("Deleted agent");
    const events = timelineOf(db(), run);
    const started = events.find((e) => e.type === "run.started");
    expect(started?.type === "run.started" ? started.data.model : null).toBe("unknown");
    expect(events.some((e) => e.type === "tool_call.completed")).toBe(false);
    expect(run.status).toBe("failed");
  });

  it("resumes and cancels runs only with their full scope", async () => {
    const { scheduleRun, cancelScheduledRun, isRunScheduled } = await import("./db");
    const larkspur = must(larkspurId());
    const run = must(db().runs.find((r) => r.workspace_id === ws("acme").id && r.plan.length > 0) ?? db().runs[0]);
    run.plan = run.plan.length > 0 ? run.plan : [{ kind: "start" }];
    expect(scheduleRun(run)).toBe(true);
    // A mismatched scope neither schedules anything nor touches the real run's timer.
    expect(scheduleRun({ id: run.id, organization_id: larkspur, workspace_id: run.workspace_id })).toBe(false);
    expect(scheduleRun({ id: run.id, organization_id: run.organization_id, workspace_id: ws("harbor").id })).toBe(
      false,
    );
    cancelScheduledRun({ id: run.id, organization_id: larkspur, workspace_id: run.workspace_id });
    expect(isRunScheduled(run)).toBe(true);
    cancelScheduledRun(run);
    expect(isRunScheduled(run)).toBe(false);
  });

  it("deletes a workspace's rows and no other organization's", async () => {
    const larkspur = must(larkspurId());
    const qa = ws("qa-playground");
    const d = db();
    const strays = {
      agent: {
        ...structuredClone(must(d.agents.find((a) => a.workspace_id === qa.id))),
        id: newId("agent"),
        organization_id: larkspur,
      },
      installation: {
        ...structuredClone(must(d.installations.find((i) => i.workspace_id === qa.id))),
        id: newId("installation"),
        organization_id: larkspur,
      },
      app: {
        ...structuredClone(must(d.apps.find((a) => a.owner_workspace_id === qa.id))),
        id: newId("app"),
        organization_id: larkspur,
      },
    };
    d.agents.push(strays.agent);
    d.installations.push(strays.installation);
    d.apps.push(strays.app);
    d.configurations.push({
      ...structuredClone(must(d.configurations.find((c) => c.workspace_id === qa.id))),
      app_id: strays.app.id,
      organization_id: larkspur,
    });
    const strayRun = {
      ...structuredClone(must(d.runs.find((r) => r.organization_id !== larkspur))),
      id: newId("run"),
      organization_id: larkspur,
      workspace_id: qa.id,
    };
    d.runs.push(strayRun);
    d.run_events[runEventsKey(strayRun)] = [];
    const straySession = {
      ...structuredClone(must(d.connect_sessions[0])),
      id: newId("connect_session"),
      organization_id: larkspur,
      workspace_id: qa.id,
    };
    d.connect_sessions.push(straySession);
    const strayGrant = {
      ...structuredClone(must(d.grants[0])),
      id: newId("grant"),
      organization_id: larkspur,
      workspace_id: qa.id,
    };
    d.grants.push(strayGrant);

    await owner.workspaces.delete(qa.id);
    const after = db();
    expect(after.agents.some((a) => a.id === strays.agent.id)).toBe(true);
    expect(after.installations.some((i) => i.id === strays.installation.id)).toBe(true);
    expect(after.apps.some((a) => a.id === strays.app.id)).toBe(true);
    expect(after.configurations.some((c) => c.app_id === strays.app.id)).toBe(true);
    expect(after.runs.some((r) => r.id === strayRun.id)).toBe(true);
    expect(after.run_events[runEventsKey(strayRun)]).toBeDefined();
    expect(after.connect_sessions.some((s) => s.id === straySession.id)).toBe(true);
    expect(after.grants.some((g) => g.id === strayGrant.id)).toBe(true);
    expect(after.workspaces.some((w) => w.id === qa.id)).toBe(false);
  });

  it("keeps membership rules inside the organization", async () => {
    const org = must(northwindId());
    // Someone who belongs to another organization can still be invited here.
    const invited = await owner.members.invite(org, { email: "rowan@larkspur.example", role: "developer" });
    expect(invited.organization_id).toBe(org);
    // Another organization's owners don't count towards this one's last owner.
    const self = must(db().memberships.find((m) => m.organization_id === org && m.user_id === DEMO_USER_ID));
    expect((await failure(owner.members.update(org, self.id, { role: "admin" }))).code).toBe("last_owner");
    expect((await failure(owner.members.remove(org, self.id))).code).toBe("last_owner");
    // Connect settings belong to their organization alone.
    const larkspurSettings = JSON.stringify(
      await Promise.all([
        rowan().connectSettings.get(must(larkspurId()), "sandbox"),
        rowan().connectSettings.get(must(larkspurId()), "production"),
      ]),
    );
    expect(larkspurSettings).not.toContain("northwind");
  });

  it("keeps uniqueness, settings and Connect rules inside each organization", async () => {
    const larkspur = must(larkspurId());
    const r = rowan();
    // Names another organization already uses are free here.
    const appSlug = await r.organizationApps.create(larkspur, {
      kind: "mcp",
      name: "Google Workspace",
      slug: "google-workspace",
      description: "",
      auth_type: "token",
    });
    expect(appSlug.slug).toBe("google-workspace");
    const workspaceSlug = await r.workspaces.create(larkspur, {
      name: "Acme",
      slug: "acme",
      environment: "sandbox",
      external_id: null,
    });
    expect(workspaceSlug.slug).toBe("acme");
    // Another organization's return-URL allowlist doesn't apply here.
    const northwindSandbox = await owner.connectSettings.get(must(northwindId()), "sandbox");
    const allowedElsewhere = must(northwindSandbox.return_urls[0]);
    const harborSandbox = ws("harbor-sandbox");
    const linear = must(db().apps.find((a) => a.organization_id === larkspur && a.slug === "linear"));
    const refused = await failure(
      r.connectSessions.create(harborSandbox.id, {
        app_id: linear.id,
        intent: "create",
        label: "x",
        installation_id: null,
        return_url: allowedElsewhere,
      }),
    );
    expect(refused.code).toBe("return_url_not_allowed");
    // Writing this organization's settings never touches another's.
    const before = JSON.stringify(await owner.connectSettings.get(must(northwindId()), "sandbox"));
    await r.connectSettings.update(larkspur, "sandbox", {
      return_urls: ["https://app.larkspur.example/back"],
      allowed_origins: [],
    });
    expect(JSON.stringify(await owner.connectSettings.get(must(northwindId()), "sandbox"))).toBe(before);
  });

  it("keeps grants and hosted reconnects inside their organization", async () => {
    const larkspur = must(larkspurId());
    const acme = ws("acme");
    const d = db();
    // A grant labelled as Larkspur's, on Acme's agent and installation.
    const installation = must(d.installations.find((i) => i.workspace_id === acme.id && i.status === "connected"));
    const agent = must(d.agents.find((a) => a.workspace_id === acme.id));
    const strayGrant = {
      ...structuredClone(must(d.grants[0])),
      id: newId("grant"),
      organization_id: larkspur,
      workspace_id: acme.id,
      agent_id: agent.id,
      installation_id: installation.id,
    };
    d.grants.unshift(strayGrant);
    // It doesn't block a real grant for the same pair...
    for (const g of d.grants.filter(
      (g) => g.organization_id !== larkspur && g.agent_id === agent.id && g.installation_id === installation.id,
    )) {
      await owner.grants.delete(acme.id, agent.id, g.id);
    }
    const tool = must(d.apps.find((a) => a.id === installation.app_id)?.tools[0]).name;
    const created = await owner.grants.create(acme.id, agent.id, {
      installation_id: installation.id,
      tools: [tool],
      approval_required: [],
    });
    expect(created.installation_id).toBe(installation.id);
    // ...and isn't removed with the real installation or agent.
    await owner.installations.delete(acme.id, installation.id);
    expect(db().grants.some((g) => g.id === strayGrant.id)).toBe(true);
    await owner.agents.delete(acme.id, agent.id);
    expect(db().grants.some((g) => g.id === strayGrant.id)).toBe(true);

    // A hosted reconnect whose installation carries another organization's label finds nothing to reconnect.
    const sandbox = ws("acme-sandbox");
    const google = must(
      db().apps.find((a) => a.organization_id === sandbox.organization_id && a.slug === "google-workspace"),
    );
    const real = must(db().installations.find((i) => i.workspace_id === sandbox.id && i.app_id === google.id));
    const stray = { ...structuredClone(real), id: newId("installation"), organization_id: larkspur };
    db().installations.unshift(stray);
    const reference = newReference();
    db().connect_sessions.push({
      ...structuredClone(must(db().connect_sessions[0])),
      id: newId("connect_session"),
      organization_id: sandbox.organization_id,
      workspace_id: sandbox.id,
      reference_hash: await hashReference(reference),
      environment: "sandbox",
      app_id: google.id,
      intent: "reconnect",
      installation_id: stray.id,
      status: "pending",
      expires_at: new Date(Date.now() + 60_000).toISOString(),
    });
    expect((await owner.connect.get(reference)).expected_account).toBeNull();
    const transport = createHttpTransport({ baseUrl: API, getToken: async () => null });
    const consent = await failure(
      transport.request({
        method: "POST",
        path: `/v1/connect/sessions/${reference}/simulate-consent`,
        body: { decision: "approved", email: "someone@acme.example" },
      }),
    );
    expect(consent.status).toBe(404);
  });

  it("keeps one workspace's IDs out of another workspace of the same organization", async () => {
    const beta = ws("beta-labs");
    await owner.workspaceApps.create(beta.id, {
      kind: "mcp",
      name: "Beta tools",
      slug: "beta-tools",
      description: "",
      auth_type: "token",
    });
    const acmeTenant = tenant("northwind", "acme", "acme-sandbox");
    const betaTenant = tenant("northwind", "beta-labs", "qa-playground");
    await expectSealed(owner, acmeTenant, betaTenant, { sameOrganization: true });
    await expectSealed(owner, betaTenant, acmeTenant, { sameOrganization: true });
  });

  it("deletes one workspace and leaves the rest of its organization intact", async () => {
    const org = must(northwindId());
    const qa = ws("qa-playground");
    const rest = () => {
      const d = db();
      const elsewhere = <T extends { organization_id: string; workspace_id: string }>(rows: T[]) =>
        rows
          .filter((r) => r.organization_id === org && r.workspace_id !== qa.id)
          .map((r) => (r as { id?: string }).id ?? r);
      const qaRuns = new Set(d.runs.filter((r) => r.workspace_id === qa.id).map((r) => r.id));
      return JSON.stringify([
        d.workspaces.filter((w) => w.organization_id === org && w.id !== qa.id).length,
        elsewhere(d.installations),
        elsewhere(d.connect_sessions),
        elsewhere(d.agents),
        elsewhere(d.grants),
        elsewhere(d.runs),
        Object.keys(d.run_events).filter((key) => key.startsWith(`${org}/`) && !qaRuns.has(key.split("/")[2] as never)),
        d.apps.filter((a) => a.organization_id === org && a.owner_workspace_id !== qa.id).map((a) => a.id),
        d.configurations.filter((c) => c.organization_id === org && c.workspace_id !== qa.id).length,
        d.api_keys.filter((k) => k.organization_id === org).length,
        d.webhooks.filter((h) => h.organization_id === org).length,
      ]);
    };
    // A Larkspur workspace row that happens to share the ID must survive too.
    const larkspur = must(larkspurId());
    db().workspaces.unshift({ ...structuredClone(ws("harbor")), id: qa.id, organization_id: larkspur });
    const before = rest();
    await owner.workspaces.delete(qa.id);
    expect(rest()).toBe(before);
    expect(db().workspaces.some((w) => w.id === qa.id && w.organization_id === larkspur)).toBe(true);
  });

  it("keeps hosted Connect inside the session's own workspace", async () => {
    const sandbox = ws("acme-sandbox");
    const qa = ws("qa-playground");
    const org = sandbox.organization_id;
    const google = must(db().apps.find((a) => a.organization_id === org && a.slug === "google-workspace"));
    // A reconnect naming an installation of the organization's other workspace.
    const elsewhere = must(
      db().installations.find((i) => i.workspace_id === qa.id || i.workspace_id === ws("acme").id),
    );
    const reference = newReference();
    const base = structuredClone(must(db().connect_sessions[0]));
    db().connect_sessions.push({
      ...base,
      id: newId("connect_session"),
      organization_id: org,
      workspace_id: sandbox.id,
      reference_hash: await hashReference(reference),
      environment: "sandbox",
      app_id: google.id,
      intent: "reconnect",
      installation_id: elsewhere.id,
      status: "pending",
      expires_at: new Date(Date.now() + 60_000).toISOString(),
    });
    expect((await owner.connect.get(reference)).expected_account).toBeNull();
    const transport = createHttpTransport({ baseUrl: API, getToken: async () => null });
    const consent = await failure(
      transport.request({
        method: "POST",
        path: `/v1/connect/sessions/${reference}/simulate-consent`,
        body: { decision: "approved", email: "someone@acme.example" },
      }),
    );
    expect(consent.status).toBe(404);
    // A session naming the other workspace's own app is neither shown nor usable.
    const echo = app("echo-server");
    expect(echo.owner_workspace_id).toBe(qa.id);
    const second = newReference();
    db().connect_sessions.push({
      ...base,
      id: newId("connect_session"),
      organization_id: org,
      workspace_id: sandbox.id,
      reference_hash: await hashReference(second),
      environment: "sandbox",
      app_id: echo.id,
      intent: "create",
      installation_id: null,
      status: "pending",
      expires_at: new Date(Date.now() + 60_000).toISOString(),
    });
    expect((await owner.connect.get(second)).app.name).not.toBe(echo.name);
    const refused = await failure(owner.connect.submitCredentials(second, { values: { token: "abc123" } }));
    expect(refused.code).toBe("app_not_eligible");
  });

  it("serves a workspace's own apps only through that workspace", async () => {
    const org = must(northwindId());
    const publishing = must(db().apps.find((a) => a.organization_id === org && a.owner_workspace_id === ws("acme").id));
    for (const attempt of [
      () => owner.organizationApps.get(org, publishing.id),
      () => owner.organizationApps.update(org, publishing.id, { name: "x" }),
      () => owner.organizationApps.getConfiguration(org, publishing.id, "production"),
    ]) {
      expect((await failure(attempt())).status).toBe(404);
    }
    // Slugs are unique per owner: the organization's catalog, or one workspace.
    const inBeta = await owner.workspaceApps.create(ws("beta-labs").id, {
      kind: "mcp",
      name: "Publishing",
      slug: publishing.slug,
      description: "",
      auth_type: "token",
    });
    expect(inBeta.slug).toBe(publishing.slug);
    const inCatalog = await owner.organizationApps.create(org, {
      kind: "mcp",
      name: "Publishing",
      slug: publishing.slug,
      description: "",
      auth_type: "token",
    });
    expect(inCatalog.slug).toBe(publishing.slug);
    // A workspace app's installations are counted in its own workspace only.
    const countOf = async () =>
      must((await owner.workspaceApps.list(ws("acme").id)).find((a) => a.id === publishing.id)).installation_count;
    const countBefore = await countOf();
    db().installations.unshift({
      ...structuredClone(must(db().installations.find((i) => i.workspace_id === ws("beta-labs").id))),
      id: newId("installation"),
      app_id: publishing.id,
    });
    expect(await countOf()).toBe(countBefore);
    // The workspace app's environment is its own workspace's, whatever another organization's rows say.
    const acme = ws("acme");
    db().workspaces.unshift({
      ...structuredClone(ws("harbor-sandbox")),
      id: acme.id,
      organization_id: must(larkspurId()),
    });
    const listed = await owner.workspaceApps.list(acme.id);
    expect(listed.find((a) => a.id === publishing.id)?.configurations.map((c) => c.environment)).toEqual([
      "production",
    ]);
  });

  it("treats an invited member as not yet a member", async () => {
    const org = must(northwindId());
    const invited = must(db().memberships.find((m) => m.organization_id === org && m.status === "invited"));
    const client = clientFor(invited.user_id, northwindId);
    expect((await client.me.get()).organizations.map((o) => o.id)).not.toContain(org);
    expect((await failure(client.members.list(org))).status).toBe(404);
    expect((await failure(client.workspaces.get(ws("acme").id))).status).toBe(404);
    // And an invited owner doesn't count towards the last owner.
    await owner.members.invite(org, { email: "next-owner@northwind.example", role: "owner" });
    const self = must(db().memberships.find((m) => m.organization_id === org && m.user_id === DEMO_USER_ID));
    expect((await failure(owner.members.update(org, self.id, { role: "admin" }))).code).toBe("last_owner");
    expect((await failure(owner.members.remove(org, self.id))).code).toBe("last_owner");
    // Only an owner may uninvite an owner.
    const admin = must(
      db().memberships.find((m) => m.organization_id === org && m.role === "admin" && m.status === "active"),
    );
    const asAdmin = clientFor(admin.user_id, northwindId);
    const invitedOwner = await owner.members.invite(org, {
      email: "admin-cant-remove@northwind.example",
      role: "owner",
    });
    expect((await failure(asAdmin.members.remove(org, invitedOwner.id))).status).toBe(403);
    expect((await failure(asAdmin.members.remove(org, self.id))).status).toBe(403);
    // But an invited owner can be demoted or uninvited by the active one.
    const pending = await owner.members.invite(org, { email: "maybe-owner@northwind.example", role: "owner" });
    expect((await owner.members.update(org, pending.id, { role: "admin" })).role).toBe("admin");
    const another = await owner.members.invite(org, { email: "uninvited@northwind.example", role: "owner" });
    await owner.members.remove(org, another.id);
    expect(db().memberships.some((m) => m.id === another.id)).toBe(false);
  });

  it("plans a run only with apps of the run's own organization", async () => {
    // An installation in Acme's workspace, labelled Larkspur's and using Larkspur's app, under a real Acme grant.
    const { planRun } = await import("./runner");
    const larkspur = must(larkspurId());
    const acme = ws("acme");
    const larkspurApp = must(db().apps.find((a) => a.organization_id === larkspur && a.owner_workspace_id === null));
    const grant = must(db().grants.find((g) => g.workspace_id === acme.id && g.approval_required.length === 0));
    const installation = must(db().installations.find((i) => i.id === grant.installation_id));
    Object.assign(installation, { organization_id: larkspur, app_id: larkspurApp.id, label: "FOREIGN APP" });
    grant.tools = larkspurApp.tools.map((t) => t.name);
    const run = {
      ...structuredClone(must(db().runs.find((r) => r.workspace_id === acme.id))),
      agent_id: grant.agent_id,
    };
    for (let i = 0; i < 25; i++) {
      const plan = planRun(db(), run, mulberry32(300 + i));
      expect(JSON.stringify(plan)).not.toContain("FOREIGN APP");
      expect(plan.some((s) => s.kind === "tool_call" && s.call.installation_id === installation.id)).toBe(false);
    }
  });

  it("keeps grants inside their workspace", async () => {
    const acme = ws("acme");
    const beta = ws("beta-labs");
    const d = db();
    const installation = must(d.installations.find((i) => i.workspace_id === acme.id && i.status === "connected"));
    const agent = must(d.agents.find((a) => a.workspace_id === acme.id));
    // A grant of the same organization, filed under Beta Labs, naming Acme's agent and installation.
    const stray = {
      ...structuredClone(must(d.grants[0])),
      id: newId("grant"),
      organization_id: acme.organization_id,
      workspace_id: beta.id,
      agent_id: agent.id,
      installation_id: installation.id,
    };
    d.grants.unshift(stray);
    for (const g of d.grants.filter(
      (g) => g.workspace_id === acme.id && g.agent_id === agent.id && g.installation_id === installation.id,
    )) {
      await owner.grants.delete(acme.id, agent.id, g.id);
    }
    const tool = must(d.apps.find((a) => a.id === installation.app_id)?.tools[0]).name;
    const created = await owner.grants.create(acme.id, agent.id, {
      installation_id: installation.id,
      tools: [tool],
      approval_required: [],
    });
    expect(created.workspace_id).toBe(acme.id);
    await owner.installations.delete(acme.id, installation.id);
    expect(db().grants.some((g) => g.id === stray.id)).toBe(true);
    await owner.agents.delete(acme.id, agent.id);
    expect(db().grants.some((g) => g.id === stray.id)).toBe(true);
  });

  it("stores, numbers and deletes run events within their run's scope", async () => {
    const larkspur = must(larkspurId());
    // Every timeline is stored under its run's organization, workspace and ID.
    for (const [key, events] of Object.entries(db().run_events)) {
      for (const e of events) expect(key).toBe(`${e.organization_id}/${e.workspace_id}/${e.run_id}`);
    }
    // A stray event with another organization's label, filed under a real run's key.
    const qa = ws("qa-playground");
    const run = must(
      db().runs.find((r) => r.workspace_id === qa.id && !["completed", "failed", "cancelled"].includes(r.status)) ??
        db().runs.find((r) => r.workspace_id === qa.id),
    );
    const key = runEventsKey(run);
    const stray = {
      ...structuredClone(must(timelineOf(db(), run)[0])),
      id: newId("run_event"),
      organization_id: larkspur,
      sequence: 500,
    };
    const list = must(db().run_events[key]);
    list.unshift(stray);
    // And one with the right scope but another run's ID.
    const otherRun = {
      ...structuredClone(stray),
      id: newId("run_event"),
      organization_id: run.organization_id,
      run_id: newId("run"),
    };
    list.unshift(otherRun);
    // It neither numbers nor shows in the run's own timeline.
    const { cancelRun } = await import("./runner");
    const before = timelineOf(db(), run).length;
    cancelRun(db(), run, "Test", new Date().toISOString());
    const shown = await owner.runs.events(qa.id, run.id);
    expect(shown.map((e) => e.sequence)).toEqual(shown.map((_, i) => i + 1));
    expect(shown.length).toBe(before + 1);
    expect(shown.some((e) => e.id === stray.id)).toBe(false);
    // And deleting the workspace removes the run's own events, not the stray.
    await owner.workspaces.delete(qa.id);
    expect(db().run_events[key]?.map((e) => e.id)).toEqual([stray.id]);
    expect(shown.some((e) => e.id === otherRun.id)).toBe(false);
  });

  it("checks the named organization against both the membership and the workspace", async () => {
    const harbor = ws("harbor");
    const acme = ws("acme");
    // A Northwind member naming Larkspur isn't a member there.
    const spoof = clientFor(DEMO_USER_ID, larkspurId);
    expect((await failure(spoof.workspaces.get(harbor.id))).status).toBe(404);
    // Naming their own organization doesn't reach another organization's workspace.
    expect((await failure(owner.workspaces.get(harbor.id))).status).toBe(404);
    // Larkspur's owner naming Northwind is refused, even for a workspace that exists.
    const rowanAsNorthwind = clientFor(
      must(db().users.find((u) => u.email === "rowan@larkspur.example")).id,
      northwindId,
    );
    expect((await failure(rowanAsNorthwind.workspaces.get(acme.id))).status).toBe(404);
    // And each organization still reaches its own.
    expect((await rowan().workspaces.get(harbor.id)).id).toBe(harbor.id);
    expect((await owner.workspaces.get(acme.id)).id).toBe(acme.id);
  });

  it("requires a Console session to name its organization on workspace routes", async () => {
    const unnamed = clientFor(DEMO_USER_ID, () => undefined);
    const error = await failure(unnamed.workspaces.get(ws("acme").id));
    expect(error.status).toBe(400);
    expect(error.code).toBe("organization_required");
    // Organization routes name it in the path.
    expect((await unnamed.members.list(must(northwindId()))).length).toBeGreaterThan(0);
  });

  it("never lists another organization's rows", async () => {
    const larkspur = must(larkspurId());
    const harbor = ws("harbor");
    const me = await owner.me.get();
    expect(me.organizations.map((o) => o.id)).not.toContain(larkspur);
    const org = must(northwindId());
    const [workspaces, runs, audit] = await Promise.all([
      owner.workspaces.list(org, { limit: 100, q: "harbor" }),
      owner.organizationRuns.list(org, { environment: "production", limit: 100 }),
      owner.audit.list(org, { limit: 100 }),
    ]);
    const listed = JSON.stringify([workspaces, runs, audit]);
    expect(listed).not.toContain(harbor.id);
    expect(listed).not.toContain(larkspur);
  });
});
