/**
 * Contract tests for the mock API. They pin the isolation and Connect rules
 * the Console relies on, so the real backend can be held to the same cases.
 */
import { createClient, createHttpTransport, ZupflowApiError } from "@zupflow/sdk";
import { setupServer } from "msw/node";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { db, resetDb } from "./db";
import { createHandlers } from "./handlers";
import { setLatency } from "./http";
import { DEMO_USER_ID } from "./seed";

const API = "http://zupflow.test/api";
const server = setupServer(...createHandlers(API));

function clientFor(userId: string | null) {
  return createClient(createHttpTransport({ baseUrl: API, getToken: async () => (userId ? `mock_${userId}` : null) }));
}

async function failure(promise: Promise<unknown>) {
  try {
    await promise;
  } catch (error) {
    if (error instanceof ZupflowApiError) return error;
    throw error;
  }
  throw new Error("Expected the request to fail");
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

  it("hides another organization's workspace as 404", async () => {
    const outsider = db().users.push({ id: "outsider", name: "Out Sider", email: "o@x.test", avatar_url: null });
    expect(outsider).toBeGreaterThan(0);
    const error = await failure(clientFor("outsider").workspaces.get(ws("acme").id));
    expect(error.status).toBe(404);
  });

  it("refuses Production keys to developers but allows Sandbox keys", async () => {
    const sam = db().users.find((u) => u.email === "sam@northwind.example");
    const orgId = db().organizations[0]?.id ?? "";
    const dev = clientFor(sam?.id ?? "");
    const denied = await failure(dev.apiKeys.create(orgId, { name: "x", environment: "production" }));
    expect(denied.status).toBe(403);
    const created = await dev.apiKeys.create(orgId, { name: "x", environment: "sandbox" });
    expect(created.secret.startsWith("zf_test_")).toBe(true);
    expect(created.key).not.toHaveProperty("secret");
  });
});

describe("apps and environments", () => {
  it("rejects an unsupported app kind instead of coercing it", async () => {
    const orgId = db().organizations[0]?.id ?? "";
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
    const transport = createHttpTransport({ baseUrl: API, getToken: async () => `mock_${DEMO_USER_ID}` });
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
        subject: "user_1",
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
        subject: "user_1",
        return_url: null,
      }),
    );
    expect(error.status).toBe(404);
  });

  it("refuses private and plain-HTTP MCP endpoints", async () => {
    const orgId = db().organizations[0]?.id ?? "";
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
    const orgId = db().organizations[0]?.id ?? "";
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
    workspaceId: string,
    appId: string,
    extra: Partial<Parameters<typeof owner.connectSessions.create>[1]> = {},
  ) =>
    owner.connectSessions.create(workspaceId, {
      app_id: appId,
      intent: "create",
      label: "Echo 2",
      installation_id: null,
      subject: "user_1",
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
    const result = await owner.connect.submitCredentials(session.id, { values: { token: "abc123" } });
    expect(result.installation.workspace_id).toBe(ws("qa-playground").id);
    expect(result.session.status).toBe("completed");
    const replay = await failure(owner.connect.submitCredentials(session.id, { values: { token: "abc123" } }));
    expect(replay.code).toBe("session_not_pending");
  });

  it("keeps the session open when the provider rejects a credential", async () => {
    const session = await start(ws("qa-playground").id, app("echo-server").id);
    const error = await failure(owner.connect.submitCredentials(session.id, { values: { token: "invalid" } }));
    expect(error.code).toBe("credential_rejected");
    expect((await owner.connect.get(session.id)).status).toBe("pending");
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
      subject: "user_1",
      return_url: null,
    });
    const transport = createHttpTransport({ baseUrl: API, getToken: async () => null });
    const error = await failure(
      transport.request({
        method: "POST",
        path: `/v1/connect/sessions/${session.id}/simulate-consent`,
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
      owner.grants.create(acme.id, agent?.id ?? "", {
        installation_id: betaInstallation?.id ?? "",
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
        installation_id: installation?.id ?? "",
        tools: ["rm_rf"],
        approval_required: [],
      }),
    );
    expect(unknown.code).toBe("unknown_tool");
    const outside = await failure(
      owner.grants.create(qa.id, agent.id, {
        installation_id: installation?.id ?? "",
        tools: ["echo"],
        approval_required: ["echo_write"],
      }),
    );
    expect(outside.code).toBe("approval_outside_grant");
  });

  it("does not read another workspace's run through this workspace's path", async () => {
    const betaRun = db().runs.find((r) => r.workspace_id === ws("beta-labs").id);
    const error = await failure(owner.runs.get(ws("acme").id, betaRun?.id ?? ""));
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
    const error = await failure(owner.runs.create(qa.id, agent.id, { input: "hi", subject: null }));
    expect(error.code).toBe("agent_paused");
  });

  it("never lets the Console act on Production accounts", async () => {
    const acme = ws("acme");
    const agent = db().agents.find((a) => a.workspace_id === acme.id && a.status === "active");
    const run = await failure(owner.runs.create(acme.id, agent?.id ?? "", { input: "hi", subject: null }));
    expect(run.status).toBe(403);
    expect(run.code).toBe("console_read_only_in_production");
    const connect = await failure(
      owner.connectSessions.create(acme.id, {
        app_id: app("google-workspace").id,
        intent: "create",
        label: "Sales",
        installation_id: null,
        subject: "user_1",
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
    const pending = run.plan.find((s) => s.kind === "tool_call");
    const callId = pending?.kind === "tool_call" ? pending.call.call_id : "";
    const tool = pending?.kind === "tool_call" ? pending.call.tool : "";
    resolveApproval(db(), run, callId, "denied", "Tester", new Date().toISOString());
    for (let i = 0; i < 10 && run.status !== "completed"; i++) {
      advanceRun(db(), run, new Date().toISOString(), () => 0.5);
    }
    expect(run.status).toBe("completed");
    const events = db().run_events[run.id] ?? [];
    expect(events.some((e) => e.type === "tool_call.started" && e.data.tool === tool)).toBe(false);
    expect(run.output).not.toMatch(/published the post/);
  });
});

describe("scale and environments", () => {
  it("pages and searches workspaces on the server, one environment at a time", async () => {
    const orgId = db().organizations[0]?.id ?? "";
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
    const orgId = db().organizations[0]?.id ?? "";
    const runs = await owner.organizationRuns.list(orgId, { environment: "sandbox", limit: 100 });
    expect(runs.items.length).toBeGreaterThan(0);
    expect(runs.items.every((r) => r.workspace.environment === "sandbox")).toBe(true);
    const outsider = db().users.push({ id: "outsider-2", name: "O", email: "o2@x.test", avatar_url: null });
    expect(outsider).toBeGreaterThan(0);
    expect((await failure(clientFor("outsider-2").organizationRuns.list(orgId))).status).toBe(404);
  });

  it("refuses approvals from a Console session", async () => {
    const run = db().runs.find((r) => r.status === "waiting_for_approval");
    const pending = run?.plan.find((s) => s.kind === "tool_call");
    const error = await failure(
      owner.runs.resolveApproval(run?.workspace_id ?? "", run?.id ?? "", {
        call_id: pending?.kind === "tool_call" ? pending.call.call_id : "",
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
      subject: "user_1",
      return_url: null,
    });
    expect(session.url).toMatch(new RegExp(`/connect/${session.id}$`));
  });
});
