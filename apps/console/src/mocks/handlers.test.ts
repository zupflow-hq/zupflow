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
  type WorkspaceId,
  ZupflowApiError,
} from "@zupflow/sdk";
import { setupServer } from "msw/node";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { db, resetDb } from "./db";
import { createHandlers } from "./handlers";
import { setLatency } from "./http";
import { hashReference, isId, newId, newReference, uuidOf } from "./ids";
import { DEMO_USER_ID, seed } from "./seed";

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
    const requested = [...(db().run_events[run.id] ?? [])].reverse().find((e) => e.type === "approval.requested");
    const callId = must(requested?.type === "approval.requested" ? requested.data.call_id : null, "pending call");
    const pending = run.plan.find((s) => s.kind === "tool_call" && s.call.call_id === callId);
    const tool = must(pending?.kind === "tool_call" ? pending.call.tool : null, "pending tool");
    resolveApproval(db(), run, callId, "denied", approvalKeyFor(run.workspace_id), new Date().toISOString());
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
