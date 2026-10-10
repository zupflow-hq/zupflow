/**
 * Tests for the mock API. The shared contract suite (`@zupflow/contract-tests`)
 * runs here first, with the black-box cases the backend must pass too. The
 * rest reach into the mock's database to set up and inspect what the API
 * can't: isolation across organizations and workspaces, mislabelled rows,
 * simulated consent and the seed. They move into the shared suite as the API
 * grows the means to express them.
 *
 * Every response these tests receive is also checked against the contract.
 */

import { routes } from "@zupflow/contract";
import {
  contractSuite,
  createResponseChecker,
  expectSealed,
  loadTenant,
  type ResponseChecker,
} from "@zupflow/contract-tests";
import {
  type ConnectResult,
  createClient,
  createHttpTransport,
  ID_PREFIXES,
  type InstallationId,
  type InstallationTool,
  type OrganizationId,
  type WorkspaceId,
  ZupflowApiError,
} from "@zupflow/sdk";
import { setupServer } from "msw/node";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { db, resetDb } from "./db";
import { createHandlers, NO_SIGN_IN_HOST, UNREACHABLE_HOST } from "./handlers";
import { setLatency } from "./http";
import { hashReference, isId, newId, newReference, uuidOf } from "./ids";
import type { IdempotencyRow } from "./model";
import { mulberry32 } from "./random";
import { runEventsKey, timelineOf } from "./scope";
import { DEMO_USER_ID, seed } from "./seed";
import { listedFrom, MAX_GONE, recordListing } from "./tools";

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

/** Mock-only routes: stand-ins for things a provider does, never part of the API. */
const MOCK_ONLY = ["post /v1/connect/sessions/{reference}/simulate-consent", "post /__mock/reset"];
const mockOnly = (method: string, path: string) =>
  MOCK_ONLY.some((entry) => {
    const [m, p] = entry.split(" ") as [string, string];
    return m === method.toLowerCase() && new RegExp(`^${p.replace(/\{[^}]+\}/g, "[^/]+")}$`).test(path);
  });

let checker: ResponseChecker;
const unchecked = globalThis.fetch;

beforeAll(() => {
  setLatency([0, 0]);
  server.listen({ onUnhandledFrame: "error" });
  // Every response these tests receive is checked against the contract.
  checker = createResponseChecker({ baseUrl: API, ignore: mockOnly, fetch: globalThis.fetch });
  globalThis.fetch = checker.fetch;
});
afterAll(() => {
  globalThis.fetch = unchecked;
  server.close();
});
afterEach(() => {
  expect(checker.takeViolations(), "responses that break the contract").toEqual([]);
});
beforeEach(() => resetDb());

/** The shared contract suite, against the mock: the same cases the backend must pass. */
const userByEmail = (email: string) => must(db().users.find((u) => u.email === email)?.id, email);
contractSuite({
  baseUrl: API,
  token: async (persona) =>
    `mock_${{
      owner: () => DEMO_USER_ID,
      developer: () => userByEmail("sam@northwind.example"),
      viewer: () => userByEmail("alex@northwind.example"),
      outsider: () => userByEmail("rowan@larkspur.example"),
    }[persona]()}`,
  reset: async () => resetDb(),
});

describe("the contract", () => {
  it("serves exactly the contract's routes, and nothing else outside the mock-only ones", () => {
    const served = createHandlers(API).map((handler) => {
      const { method, path } = handler.info as { method: string; path: string };
      return `${method.toLowerCase()} ${path.replace(API, "").replace(/:(\w+)/g, "{$1}")}`;
    });
    const contract = routes.map((route) => `${route.method} ${route.path}`);
    expect(served.filter((entry) => !MOCK_ONLY.includes(entry)).sort()).toEqual([...contract].sort());
  });
});

describe("idempotency store", () => {
  // Black-box idempotency is in the shared suite. These plant the states a request can't reach on its own.
  const qa = () => ws("qa-playground").id;
  const input = { name: "Stored", description: "", instructions: "", model: "claude-haiku-4-5-20251001" };
  const rowFor = (key: string) =>
    must(
      db().idempotency_keys.find((r) => r.key === key),
      "idempotency row",
    );

  it("answers 409 while the same key is in flight, and takes over a key whose lock lapsed", async () => {
    await owner.agents.create(qa(), input, { idempotencyKey: "store-lock" });
    const row = rowFor("store-lock");
    Object.assign(row, { state: "running", status: null, body: null });
    row.locked_until = new Date(Date.now() + 30_000).toISOString();
    const busy = await failure(owner.agents.create(qa(), input, { idempotencyKey: "store-lock" }));
    expect([busy.status, busy.code]).toEqual([409, "idempotency_request_in_progress"]);
    // A crashed request's lock lapses after a minute; the next one runs and completes the key.
    row.locked_until = new Date(Date.now() - 1).toISOString();
    await owner.agents.create(qa(), input, { idempotencyKey: "store-lock" });
    expect(rowFor("store-lock").state).toBe("completed");
    expect(db().agents.filter((a) => a.workspace_id === qa() && a.name === "Stored")).toHaveLength(2);
  });

  it("forgets a key after 24 hours", async () => {
    await owner.agents.create(qa(), input, { idempotencyKey: "store-ttl" });
    rowFor("store-ttl").created_at = new Date(Date.now() - 24 * 3600_000 - 1).toISOString();
    await owner.agents.create(qa(), input, { idempotencyKey: "store-ttl" });
    expect(db().agents.filter((a) => a.workspace_id === qa() && a.name === "Stored")).toHaveLength(2);
  });

  it("saves nothing for a server error, so the retry runs again", async () => {
    const agents = db().agents;
    // A store that refuses writes makes the handler fail with a 500.
    db().agents = Object.freeze([...agents]) as typeof agents;
    const broken = await failure(owner.agents.create(qa(), input, { idempotencyKey: "store-5xx" }));
    expect(broken.status).toBe(500);
    expect(db().idempotency_keys.some((r) => r.key === "store-5xx")).toBe(false);
    db().agents = [...agents];
    const agent = await owner.agents.create(qa(), input, { idempotencyKey: "store-5xx" });
    expect(agent.workspace_id).toBe(qa());
    expect(rowFor("store-5xx").state).toBe("completed");
  });

  it("stores no Connect reference and no plain body digest", async () => {
    const session = await owner.connectSessions.create(ws("acme-sandbox").id, {
      app_id: app("google-workspace").id,
      intent: "create",
      label: "Stored OAuth",
      installation_id: null,
      return_url: null,
    });
    const reference = referenceOf(session);
    await owner.connect.authorize(reference);
    await owner.connect.cancel(reference);
    const stored = JSON.stringify(db().idempotency_keys);
    expect(stored).not.toContain(reference);
    // The fingerprint is keyed: the body's plain SHA-256 appears nowhere.
    const digest = Array.from(
      new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(JSON.stringify(input)))),
      (b) => b.toString(16).padStart(2, "0"),
    ).join("");
    await owner.agents.create(qa(), input, { idempotencyKey: "store-digest" });
    expect(JSON.stringify(db().idempotency_keys)).not.toContain(digest);
  });

  const larkspur = () => must(db().organizations.find((o) => o.slug === "larkspur")?.id, "Larkspur");
  /** A row of another organization, otherwise like `template`, as a corrupted or colliding store might hold. */
  const foreign = (template: IdempotencyRow, key: string): IdempotencyRow => ({
    ...structuredClone(template),
    organization_id: larkspur(),
    key,
  });

  it("deletes a workspace's saved responses with the workspace, and no other organization's", async () => {
    await owner.agents.create(qa(), input, { idempotencyKey: "store-purge" });
    expect(rowFor("store-purge").workspace_id).toBe(qa());
    // Another organization's row naming the same workspace ID must survive.
    db().idempotency_keys.push(foreign(rowFor("store-purge"), "store-purge-foreign"));
    await owner.workspaces.delete(qa());
    expect(db().idempotency_keys.some((r) => r.key === "store-purge")).toBe(false);
    expect(db().idempotency_keys.some((r) => r.key === "store-purge-foreign")).toBe(true);
  });

  it("sheds only its own organization's expired keys", async () => {
    await owner.agents.create(qa(), input, { idempotencyKey: "store-sweep" });
    const old = foreign(rowFor("store-sweep"), "store-sweep-foreign");
    old.created_at = new Date(Date.now() - 25 * 3600_000).toISOString();
    db().idempotency_keys.push(old);
    await owner.agents.create(qa(), input, { idempotencyKey: "store-sweep-2" });
    expect(db().idempotency_keys.some((r) => r.key === "store-sweep-foreign")).toBe(true);
  });

  it("keeps a member's key to the organization it was used in", async () => {
    // Jordan joins Larkspur too, and sends the same key and request to each organization's own route.
    db().memberships.push({
      id: newId("member"),
      organization_id: larkspur(),
      user_id: DEMO_USER_ID,
      role: "owner",
      status: "active",
      invited_at: new Date().toISOString(),
      joined_at: new Date().toISOString(),
      last_active_at: null,
    });
    const app = { kind: "mcp", name: "Both", slug: "both-orgs", description: "", auth_type: "token" } as const;
    const inNorthwind = await owner.organizationApps.create(must(northwindId()), app, { idempotencyKey: "store-orgs" });
    const asLarkspur = clientFor(DEMO_USER_ID, larkspur);
    const inLarkspur = await asLarkspur.organizationApps.create(larkspur(), app, { idempotencyKey: "store-orgs" });
    expect(inLarkspur.organization_id).toBe(larkspur());
    expect(inLarkspur.id).not.toBe(inNorthwind.id);
  });

  it("keeps each key in its organization's scope and saves no secret", async () => {
    const created = await owner.apiKeys.create(
      must(northwindId()),
      { name: "Stored key", environment: "sandbox" },
      {
        idempotencyKey: "store-secret",
      },
    );
    const row = rowFor("store-secret");
    expect(row.organization_id).toBe(northwindId());
    expect(row.caller).toBe(`member:${DEMO_USER_ID}|-`);
    expect(row.body).toBeNull();
    expect(JSON.stringify(db().idempotency_keys)).not.toContain(created.secret);
    await owner.agents.create(qa(), input, { idempotencyKey: "store-env" });
    expect(rowFor("store-env").caller).toBe(`member:${DEMO_USER_ID}|sandbox`);
  });
});

describe("Connect (simulated consent)", () => {
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

  it("signs in through an MCP server: anonymous without an identity endpoint, kept on reconnect", async () => {
    const sandbox = ws("acme-sandbox");
    const org = must(northwindId());
    const created = await owner.organizationApps.create(org, {
      kind: "mcp",
      name: "Ledger",
      slug: "ledger",
      description: "",
      auth_type: "mcp_oauth",
    });
    expect(created.auth).toEqual({ type: "mcp_oauth", scopes: [], identity_url: null });
    await owner.organizationApps.update(org, created.id, {
      auth: { type: "mcp_oauth", scopes: [{ name: "read", description: "Read entries" }], identity_url: null },
    });
    const configuration = await owner.organizationApps.updateConfiguration(org, created.id, "sandbox", {
      mcp_server_url: "https://ledger.example/mcp",
    });
    expect([configuration.status, configuration.oauth, configuration.missing]).toEqual(["ready", null, []]);

    const transport = createHttpTransport({ baseUrl: API, getToken: async () => null });
    const consent = async (reference: string, email: string) =>
      (
        await transport.request<ConnectResult>({
          method: "POST",
          path: `/v1/connect/sessions/${reference}/simulate-consent`,
          body: { decision: "approved", email },
        })
      ).data;

    const session = await owner.connectSessions.create(sandbox.id, {
      app_id: created.id,
      intent: "create",
      label: "Books",
      installation_id: null,
      return_url: null,
    });
    expect(session.requested_scopes).toEqual(["read"]);
    const pasted = await failure(owner.connect.submitCredentials(referenceOf(session), { values: { token: "t" } }));
    expect(pasted.code).toBe("wrong_auth_type");
    const { authorization_url } = await owner.connect.authorize(referenceOf(session));
    expect(authorization_url).toContain("/simulated-consent");
    const first = await consent(referenceOf(session), "pat@acme.example");
    const account = first.installation.connection?.account;
    expect(account).toMatchObject({ display_name: "Ledger account", email: null });

    // Nothing identifies the account, so a reconnect can't refuse another one: it keeps the account it had.
    const reconnect = await owner.connectSessions.create(sandbox.id, {
      app_id: created.id,
      intent: "reconnect",
      label: "Books",
      installation_id: first.installation.id,
      return_url: null,
    });
    const again = await consent(referenceOf(reconnect), "someone-else@acme.example");
    expect(again.installation.connection?.account).toEqual(account);
  });

  it("fails a sign-in the server can't offer, and one whose identity URL left the server", async () => {
    const sandbox = ws("acme-sandbox");
    const org = must(northwindId());
    const created = await owner.organizationApps.create(org, {
      kind: "mcp",
      name: "Ledger",
      slug: "ledger",
      description: "",
      auth_type: "mcp_oauth",
    });
    const serve = (url: string) =>
      owner.organizationApps.updateConfiguration(org, created.id, "sandbox", { mcp_server_url: url });
    const start = async () =>
      referenceOf(
        await owner.connectSessions.create(sandbox.id, {
          app_id: created.id,
          intent: "create",
          label: "Books",
          installation_id: null,
          return_url: null,
        }),
      );

    // A server that offers no sign-in Zupflow can use.
    await serve(`https://${NO_SIGN_IN_HOST}/mcp`);
    const unusable = await start();
    const discovery = await failure(owner.connect.authorize(unusable));
    expect([discovery.status, discovery.code]).toEqual([502, "discovery_failed"]);
    expect(await owner.connect.get(unusable)).toMatchObject({ status: "failed", failure_code: "discovery_failed" });

    // A server that can't be reached fails the session too: Connect's page can't start it again.
    await serve(`https://${UNREACHABLE_HOST}/mcp`);
    const down = await start();
    const unreachable = await failure(owner.connect.authorize(down));
    expect([unreachable.status, unreachable.code]).toEqual([502, "provider_unavailable"]);
    expect(await owner.connect.get(down)).toMatchObject({ status: "failed", failure_code: "provider_unavailable" });
    const again = await failure(owner.connect.authorize(down));
    expect(again.status).toBe(409);

    // An identity URL on the server is accepted; the configuration then moves the server away from it.
    await serve("https://ledger.example/mcp");
    await owner.organizationApps.update(org, created.id, {
      auth: { type: "mcp_oauth", scopes: [], identity_url: "https://ledger.example/me" },
    });
    await serve("https://moved.example/mcp");
    const moved = await start();
    const off = await failure(owner.connect.authorize(moved));
    expect([off.status, off.code]).toEqual([422, "identity_off_server"]);
    expect(await owner.connect.get(moved)).toMatchObject({ status: "failed", failure_code: "identity_off_server" });
  });

  it("persists a session a refused start failed, so the Console's window sees it", async () => {
    const sandbox = ws("acme-sandbox");
    const org = must(northwindId());
    const fieldnote = app("fieldnote");
    await owner.organizationApps.updateConfiguration(org, fieldnote.id, "sandbox", {
      mcp_server_url: `https://${NO_SIGN_IN_HOST}/mcp`,
    });
    const session = await owner.connectSessions.create(sandbox.id, {
      app_id: fieldnote.id,
      intent: "create",
      label: "Field team 2",
      installation_id: null,
      return_url: null,
    });
    await failure(owner.connect.authorize(referenceOf(session)));
    // What another tab reads: the stored copy, not this one's memory.
    const stored = JSON.parse(must(localStorage.getItem("zupflow.mock.db"), "stored database")) as ReturnType<
      typeof db
    >;
    expect(stored.connect_sessions.find((s) => s.id === session.id)).toMatchObject({
      status: "failed",
      failure_code: "discovery_failed",
    });
  });

  it("persists a session that consent failed, though the request answered an error", async () => {
    const sandbox = ws("acme-sandbox");
    const transport = createHttpTransport({ baseUrl: API, getToken: async () => null });
    // Mock-only and sent without an Idempotency-Key, so nothing but the route itself stores what it changed.
    const consent = (reference: string, body: { decision: "approved" | "denied"; email: string | null }) =>
      failure(transport.request({ method: "POST", path: `/v1/connect/sessions/${reference}/simulate-consent`, body }));
    const stored = (id: string) =>
      (
        JSON.parse(must(localStorage.getItem("zupflow.mock.db"), "stored database")) as ReturnType<typeof db>
      ).connect_sessions.find((s) => s.id === id);

    const denied = await owner.connectSessions.create(sandbox.id, {
      app_id: app("google-workspace").id,
      intent: "create",
      label: "Denied",
      installation_id: null,
      return_url: null,
    });
    expect((await consent(referenceOf(denied), { decision: "denied", email: null })).code).toBe("consent_denied");
    expect(stored(denied.id)).toMatchObject({ status: "failed", failure_code: "consent_denied" });

    const installation = must(
      db().installations.find((i) => i.workspace_id === sandbox.id && i.label === "Marketing (test)"),
      "Marketing (test)",
    );
    const other = await owner.connectSessions.create(sandbox.id, {
      app_id: app("google-workspace").id,
      intent: "reconnect",
      label: "ignored",
      installation_id: installation.id,
      return_url: null,
    });
    const mismatch = await consent(referenceOf(other), { decision: "approved", email: "someone-else@acme.example" });
    expect(mismatch.code).toBe("account_mismatch");
    expect(stored(other.id)).toMatchObject({ status: "failed", failure_code: "account_mismatch" });
  });

  it("seeds an app that signs in through its MCP server, whose account a reconnect can't check", async () => {
    const sandbox = ws("acme-sandbox");
    const fieldnote = app("fieldnote");
    expect(fieldnote.auth).toMatchObject({ type: "mcp_oauth", identity_url: null });
    expect(fieldnote.auth.type === "mcp_oauth" && fieldnote.auth.scopes.length).toBeGreaterThan(0);
    const installation = must(
      db().installations.find((i) => i.workspace_id === sandbox.id && i.app_id === fieldnote.id),
      "Fieldnote installation",
    );
    expect(installation.connection?.account.email).toBeNull();

    // The hosted page reads the template, so it can say the account won't be checked.
    const session = await owner.connectSessions.create(sandbox.id, {
      app_id: fieldnote.id,
      intent: "reconnect",
      label: installation.label,
      installation_id: installation.id,
      return_url: null,
    });
    const hosted = await owner.connect.get(referenceOf(session));
    expect(hosted.auth).toMatchObject({ type: "mcp_oauth", identity_url: null });
    expect(hosted.expected_account).toEqual(installation.connection?.account);
  });

  it("configures nothing but the server for an app that signs in through it", async () => {
    const org = must(northwindId());
    const created = await owner.organizationApps.create(org, {
      kind: "mcp",
      name: "Ledger",
      slug: "ledger",
      description: "",
      auth_type: "mcp_oauth",
    });
    for (const input of [
      { oauth_client_id: "client" },
      { oauth_client_secret: "a-long-secret" },
      { oauth_scopes: ["read"] },
    ]) {
      const error = await failure(owner.organizationApps.updateConfiguration(org, created.id, "sandbox", input));
      expect([error.status, error.code]).toEqual([422, "oauth_not_supported"]);
    }
    const unconfigured = await owner.organizationApps.getConfiguration(org, created.id, "sandbox");
    expect([unconfigured.status, unconfigured.missing]).toEqual(["unconfigured", ["mcp_server_url"]]);
  });

  it("starts no consent for an app without the provider's endpoints", async () => {
    const sandbox = ws("acme-sandbox");
    const google = app("google-workspace");
    if (google.auth.type !== "oauth2") throw new Error("an OAuth app");
    google.auth.token_url = null;
    const session = await owner.connectSessions.create(sandbox.id, {
      app_id: google.id,
      intent: "create",
      label: "Main",
      installation_id: null,
      return_url: null,
    });
    const error = await failure(owner.connect.authorize(referenceOf(session)));
    expect(error.code).toBe("configuration_not_ready");
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
});

describe("two organizations", () => {
  const larkspurId = () => db().organizations.find((o) => o.slug === "larkspur")?.id;
  const rowan = () => clientFor(must(db().users.find((u) => u.email === "rowan@larkspur.example")).id, larkspurId);

  type Client = ReturnType<typeof clientFor>;

  /** Every row an organization owns, to prove a refused request changed nothing the API can't show. */
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
      d.idempotency_keys.filter((r) => r.organization_id === org),
    ]);
  }

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
    const northwind = must(northwindId());
    const before = stateOf(larkspur);
    await expectSealed(
      asNorthwind,
      await loadTenant(asNorthwind, northwind, "acme", "acme-sandbox"),
      await loadTenant(rowan(), larkspur, "harbor", "harbor-sandbox"),
      rowan(),
      { victimOrgPaths: false },
    );
    expect(stateOf(larkspur), "every Larkspur row").toBe(before);
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
});

describe("installation tools", () => {
  /** Every tool of the installation, read a page of two at a time. */
  const allTools = async (workspaceId: WorkspaceId, installationId: InstallationId) => {
    const tools: InstallationTool[] = [];
    let cursor: string | undefined;
    do {
      const page = await owner.installations.tools(workspaceId, installationId, { cursor, limit: 2 });
      tools.push(...page.items);
      cursor = page.pagination.next_cursor ?? undefined;
    } while (cursor);
    return tools;
  };
  const larkspur = () => must(db().organizations.find((o) => o.slug === "larkspur")?.id);
  /** A fresh installation of the echo server in qa-playground, connected through Connect. */
  const connectEcho = async () => {
    const qa = ws("qa-playground");
    const session = await owner.connectSessions.create(qa.id, {
      app_id: app("echo-server").id,
      intent: "create",
      label: "Tools",
      installation_id: null,
      return_url: null,
    });
    const { installation } = await owner.connect.submitCredentials(referenceOf(session), {
      values: { token: "abc123" },
    });
    return { qa, installation };
  };

  it("lists a new installation's tools from its app's definitions, pinned and current", async () => {
    const { qa, installation } = await connectEcho();
    const tools = await allTools(qa.id, installation.id);
    const definitions = [...app("echo-server").tools].sort((a, b) => (a.name < b.name ? -1 : 1));
    expect(tools.map((t) => [t.name, t.effect, t.status, t.pinned.title, t.pinned.description])).toEqual(
      definitions.map((d) => [d.name, d.effect, "current", d.title, d.description]),
    );
    expect(tools.every((t) => t.latest === null && t.first_seen_at === t.seen_at)).toBe(true);
  });

  it("keeps the pinned definition on refresh: a changed one waits beside it, a missing one is gone", async () => {
    const { qa, installation } = await connectEcho();
    const before = await allTools(qa.id, installation.id);
    const echoApp = app("echo-server");
    const [first, ...rest] = echoApp.tools;
    const original = structuredClone(must(first));
    // The server now describes the first tool differently, as read-only, drops the rest and adds one.
    echoApp.tools = [
      { ...original, description: "Repeat the message back, twice.", effect: "read" },
      { name: "aaa_new", title: "New", description: "Added later.", effect: "write" },
    ];
    await owner.installations.refreshTools(qa.id, installation.id);
    const after = await allTools(qa.id, installation.id);
    const byName = new Map(after.map((t) => [t.name, t]));
    const changed = must(byName.get(original.name));
    expect(changed.status).toBe("changed");
    expect(changed.pinned.description).toBe(original.description);
    expect(changed.latest?.description).toBe("Repeat the message back, twice.");
    expect(changed.effect, "a listing never changes an effect").toBe(original.effect);
    expect(changed.first_seen_at).toBe(must(before.find((t) => t.name === original.name)).first_seen_at);
    expect(changed.changed_at).not.toBeNull();
    for (const gone of rest) {
      const tool = must(byName.get(gone.name));
      expect([tool.status, tool.gone_at !== null]).toEqual(["gone", true]);
    }
    expect(must(byName.get("aaa_new")).status).toBe("current");
    expect(after.map((t) => t.name)).toEqual([...after.map((t) => t.name)].sort());

    // Listed as pinned again, the change is dropped and the gone tools return.
    const changedAt = changed.changed_at;
    echoApp.tools = [original, ...rest];
    // A minute later, as far as pacing goes.
    db().tool_refreshes = [];
    await owner.installations.refreshTools(qa.id, installation.id);
    const restored = await allTools(qa.id, installation.id);
    for (const tool of restored.filter((t) => t.name !== "aaa_new")) {
      expect([tool.name, tool.status, tool.latest, tool.changed_at, tool.gone_at]).toEqual([
        tool.name,
        "current",
        null,
        null,
        null,
      ]);
    }
    expect(must(restored.find((t) => t.name === "aaa_new")).status).toBe("gone");
    expect(changedAt).not.toBeNull();
  });

  it("leaves a grant alone when its tool goes, and refuses to grant the gone tool again", async () => {
    const { qa, installation } = await connectEcho();
    const models = await owner.models.list();
    const agent = await owner.agents.create(qa.id, {
      name: "Echo probe",
      description: "",
      model: must(models[0]).id,
      instructions: "",
    });
    const grant = await owner.grants.create(qa.id, agent.id, {
      installation_id: installation.id,
      tools: ["echo", "echo_write"],
      approval_required: ["echo_write"],
    });
    // The server stops listing echo_write.
    const echoApp = app("echo-server");
    echoApp.tools = echoApp.tools.filter((t) => t.name !== "echo_write");
    await owner.installations.refreshTools(qa.id, installation.id);
    const tools = await allTools(qa.id, installation.id);
    expect(must(tools.find((t) => t.name === "echo_write")).status).toBe("gone");
    // The grant isn't rewritten: the run engine refuses the gone tool when it's called.
    expect(await owner.grants.list(qa.id, agent.id)).toEqual([grant]);
    const kept = await failure(
      owner.grants.update(qa.id, agent.id, grant.id, {
        tools: ["echo", "echo_write"],
        approval_required: ["echo_write"],
      }),
    );
    expect([kept.status, kept.code, kept.fields]).toEqual([422, "tool_gone", { tools: expect.any(String) }]);
    const trimmed = await owner.grants.update(qa.id, agent.id, grant.id, { tools: ["echo"], approval_required: [] });
    expect(trimmed.tools).toEqual(["echo"]);
  });

  it("checks grants against the installation's own registry, not its app's definitions", async () => {
    const { qa, installation } = await connectEcho();
    const agent = await owner.agents.create(qa.id, {
      name: "Registry only",
      description: "",
      model: must((await owner.models.list())[0]).id,
      instructions: "",
    });
    // The app defines a tool its server hasn't listed to this account yet: it isn't grantable.
    app("echo-server").tools.push({
      name: "echo_later",
      title: "Later",
      description: "Not listed yet.",
      effect: "read",
    });
    const unlisted = await failure(
      owner.grants.create(qa.id, agent.id, {
        installation_id: installation.id,
        tools: ["echo_later"],
        approval_required: [],
      }),
    );
    expect(unlisted.code).toBe("unknown_tool");
    // Another installation's registry doesn't count either: a row labelled with this installation's
    // ID in another workspace is ignored.
    const mine = must(db().installation_tools.find((t) => t.installation_id === installation.id));
    db().installation_tools.push({ ...structuredClone(mine), name: "stray", workspace_id: ws("acme-sandbox").id });
    const stray = await failure(
      owner.grants.create(qa.id, agent.id, {
        installation_id: installation.id,
        tools: ["stray"],
        approval_required: [],
      }),
    );
    expect(stray.code).toBe("unknown_tool");
    await owner.installations.refreshTools(qa.id, installation.id);
    const granted = await owner.grants.create(qa.id, agent.id, {
      installation_id: installation.id,
      tools: ["echo_later"],
      approval_required: [],
    });
    expect(granted.tools).toEqual(["echo_later"]);
  });

  it("seeds only grants of tools their installation's registry holds, none gone", () => {
    const d = db();
    expect(d.grants.length).toBeGreaterThan(0);
    for (const grant of d.grants) {
      const registry = d.installation_tools.filter(
        (t) =>
          t.organization_id === grant.organization_id &&
          t.workspace_id === grant.workspace_id &&
          t.installation_id === grant.installation_id,
      );
      for (const name of grant.tools) {
        const tool = registry.find((t) => t.name === name);
        expect(tool, `${grant.id} grants ${name}`).toBeDefined();
        expect(tool?.gone_at, `${grant.id} grants ${name}, which is gone`).toBeNull();
      }
      expect(grant.approval_required.every((name) => grant.tools.includes(name))).toBe(true);
    }
  });

  it("seeds a Sandbox installation with a changed tool and a gone one", async () => {
    const sandbox = ws("acme-sandbox");
    const installation = must(
      db().installations.find((i) => i.workspace_id === sandbox.id && i.label === "Product"),
      "Product installation",
    );
    const tools = await allTools(sandbox.id, installation.id);
    const statuses = Object.fromEntries(tools.map((t) => [t.name, t.status]));
    expect(statuses).toMatchObject({ create_issue: "changed", export_roadmap: "gone", search_issues: "current" });
    const changed = must(tools.find((t) => t.name === "create_issue"));
    expect(changed.latest?.description).not.toBe(changed.pinned.description);
    // Refreshing lists what the app says now: the change stays unreviewed, the gone tool stays gone.
    await owner.installations.refreshTools(sandbox.id, installation.id);
    const again = Object.fromEntries((await allTools(sandbox.id, installation.id)).map((t) => [t.name, t.status]));
    expect(again).toEqual(statuses);
  });

  it("stores each tool under its installation's full scope, and lists only rows that match it", async () => {
    const { qa, installation } = await connectEcho();
    for (const row of db().installation_tools) {
      expect(Object.keys(row).slice(0, 3)).toEqual(["organization_id", "workspace_id", "installation_id"]);
    }
    const mine = must(db().installation_tools.find((t) => t.installation_id === installation.id));
    // The same installation and workspace IDs, labelled with another organization; and another workspace's.
    db().installation_tools.push(
      { ...structuredClone(mine), name: "stray_org", organization_id: larkspur() },
      { ...structuredClone(mine), name: "stray_ws", workspace_id: ws("acme-sandbox").id },
    );
    const names = (await allTools(qa.id, installation.id)).map((t) => t.name);
    expect(names).not.toContain("stray_org");
    expect(names).not.toContain("stray_ws");
  });

  it("removes an installation's tools with it, and a workspace's with the workspace, and no one else's", async () => {
    const { qa, installation } = await connectEcho();
    const mine = must(db().installation_tools.find((t) => t.installation_id === installation.id));
    const stray = { ...structuredClone(mine), name: "stray_org", organization_id: larkspur() };
    db().installation_tools.push(stray);
    // Pacing rows: one labelled with another organization, one of a sibling installation.
    const strayPacing = {
      organization_id: larkspur(),
      workspace_id: qa.id,
      installation_id: installation.id,
      until: "2999-01-01T00:00:00.000Z",
    };
    const siblingPacing = {
      ...strayPacing,
      organization_id: mine.organization_id,
      installation_id: "inst_sibling" as typeof installation.id,
    };
    db().tool_refreshes.push({ ...strayPacing, organization_id: mine.organization_id }, strayPacing, siblingPacing);
    await owner.installations.delete(qa.id, installation.id);
    expect(db().installation_tools.filter((t) => t.installation_id === installation.id)).toEqual([stray]);
    expect(db().tool_refreshes.filter((r) => r.workspace_id === qa.id)).toEqual([strayPacing, siblingPacing]);
    const others = db().installation_tools.filter((t) => t.workspace_id === qa.id && t.organization_id !== larkspur());
    expect(others.length).toBeGreaterThan(0);
    await owner.workspaces.delete(qa.id);
    expect(db().installation_tools.filter((t) => t.workspace_id === qa.id)).toEqual([stray]);
    expect(db().tool_refreshes.filter((r) => r.workspace_id === qa.id)).toEqual([strayPacing]);
  });

  it("keeps at most 500 gone tools per installation, dropping those gone longest", async () => {
    const { qa, installation } = await connectEcho();
    const row = must(db().installations.find((i) => i.id === installation.id));
    const batch = (prefix: string) =>
      Array.from({ length: MAX_GONE }, (_, i) =>
        listedFrom({
          name: `${prefix}${String(i).padStart(4, "0")}`,
          title: "T",
          description: "D",
          effect: "read",
        }),
      );
    recordListing(db(), row, batch("a"), "2026-01-01T00:00:00.000Z");
    recordListing(db(), row, batch("b"), "2026-01-02T00:00:00.000Z");
    recordListing(db(), row, [], "2026-01-03T00:00:00.000Z");
    const page = await owner.installations.tools(qa.id, installation.id, { limit: 100 });
    expect(page.pagination.total).toBe(MAX_GONE);
    const gone = db().installation_tools.filter((t) => t.installation_id === installation.id);
    expect(gone.every((t) => t.name.startsWith("b") && t.gone_at === "2026-01-03T00:00:00.000Z")).toBe(true);
  });

  it("shows everything a definition hashes: its schemas and hints", async () => {
    const sandbox = ws("acme-sandbox");
    const installation = must(db().installations.find((i) => i.workspace_id === sandbox.id && i.label === "Product"));
    const comment = must((await allTools(sandbox.id, installation.id)).find((t) => t.name === "add_comment"));
    expect(comment.status).toBe("changed");
    expect([comment.pinned.description, comment.pinned.input_schema]).toEqual([
      comment.latest?.description,
      comment.latest?.input_schema,
    ]);
    expect(comment.pinned.annotations).toEqual({ readOnlyHint: false });
    expect(comment.latest?.annotations).toEqual({ readOnlyHint: false, destructiveHint: false });
    expect(comment.latest?.output_schema).toBeNull();
  });

  it("refuses a second refresh of one installation within a minute, and keeps the pacing to it", async () => {
    const { qa, installation } = await connectEcho();
    await owner.installations.refreshTools(qa.id, installation.id);
    const error = await failure(owner.installations.refreshTools(qa.id, installation.id));
    expect([error.status, error.code]).toEqual([429, "refresh_too_soon"]);
    expect(error.message).toContain("minute");
    const pacing = must(db().tool_refreshes.find((r) => r.installation_id === installation.id));
    expect(Object.keys(pacing).slice(0, 3)).toEqual(["organization_id", "workspace_id", "installation_id"]);
    // Another installation of the workspace isn't held back.
    const echo = must(
      db().installations.find((i) => i.workspace_id === qa.id && i.id !== installation.id && i.status === "connected"),
    );
    await owner.installations.refreshTools(qa.id, echo.id);
    // A pacing row labelled with another organization holds nothing back here.
    db().tool_refreshes = db().tool_refreshes.map((r) =>
      r.installation_id === installation.id ? { ...r, organization_id: larkspur() } : r,
    );
    await owner.installations.refreshTools(qa.id, installation.id);
    // Once the minute has passed, it refreshes again.
    for (const r of db().tool_refreshes) r.until = new Date(Date.now() - 1).toISOString();
    await owner.installations.refreshTools(qa.id, installation.id);
  });

  it("refuses a tool cursor it didn't issue", async () => {
    const { qa, installation } = await connectEcho();
    const error = await failure(owner.installations.tools(qa.id, installation.id, { cursor: "not base64!" }));
    expect([error.status, error.code, error.fields]).toEqual([
      400,
      "validation_failed",
      { cursor: expect.any(String) },
    ]);
  });

  it("lists nothing for an installation that never connected, and refuses to refresh it", async () => {
    const qa = ws("qa-playground");
    const old = must(db().installations.find((i) => i.workspace_id === qa.id && i.status === "disconnected"));
    expect(await allTools(qa.id, old.id)).toEqual([]);
    const error = await failure(owner.installations.refreshTools(qa.id, old.id));
    expect([error.status, error.code]).toEqual([409, "installation_not_connected"]);
    // Reconnect required counts as not connected too.
    const sandbox = ws("acme-sandbox");
    const live = must(db().installations.find((i) => i.workspace_id === sandbox.id && i.status === "connected"));
    live.status = "reconnect_required";
    const refused = await failure(owner.installations.refreshTools(sandbox.id, live.id));
    expect(refused.code).toBe("installation_not_connected");
  });
});
