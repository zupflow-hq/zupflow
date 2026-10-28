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
  agentChannelDefaults,
  type ChannelConnectionConnectResult,
  createClient,
  createHttpTransport,
  EXPIRED_INVITATION_SHOWN_DAYS,
  ID_PREFIXES,
  INVITATION_LIST_LIMIT,
  INVITATION_TTL_DAYS,
  type InstallationConnectResult,
  type InstallationId,
  type InstallationTool,
  type OrganizationId,
  type WorkspaceId,
  ZupflowApiError,
} from "@zupflow/sdk";
import { setupServer } from "msw/node";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { signUp } from "./accounts";
import { slackSignature } from "./channels";
import * as credentials from "./credentials";
import { db, resetDb } from "./db";
import { createHandlers, NO_SIGN_IN_HOST, OUTCOME_GRACE_MS, UNREACHABLE_HOST } from "./handlers";
import { announceLifecycle, grantorAllows, setLatency, withdrawUngrantable, withExpiry } from "./http";
import { hashReference, isId, newId, newReference, uuidOf } from "./ids";
import type { IdempotencyRow, MembershipRow } from "./model";
import { mulberry32 } from "./random";
import { inWorkspace, issueShortId, runEventsKey, SHORT_ID_DRAWS, timelineOf } from "./scope";
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
const app = (key: string) => {
  const a = db().apps.find((x) => x.key === key);
  if (!a) throw new Error(`No app ${key}`);
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

describe("Connect lifecycle snapshots", () => {
  it("refuses a foreign workspace and keeps lifecycle snapshots and deliveries in scope", () => {
    const workspace = ws("qa-playground");
    const foreign = ws("harbor-sandbox");
    const event = {
      type: "connect_session.failed" as const,
      data: {
        object: "connect_session" as const,
        id: newId("connect_session"),
        status: "cancelled" as const,
        installation_id: null,
        target: null,
        failure_code: null,
      },
    };
    const at = new Date().toISOString();
    const foreignEndpoint = {
      ...structuredClone(must(db().webhooks.find((endpoint) => endpoint.organization_id === foreign.organization_id))),
      id: newId("webhook_endpoint"),
      environment: workspace.environment,
      events: [event.type],
      enabled: true,
    };
    db().webhooks.push(foreignEndpoint);
    const before = structuredClone({ events: db().webhook_events, deliveries: db().webhook_deliveries });
    expect(() => announceLifecycle(workspace.organization_id, foreign.id, event, at)).toThrow();
    expect({ events: db().webhook_events, deliveries: db().webhook_deliveries }).toEqual(before);
    db().workspaces.unshift({
      ...structuredClone(workspace),
      organization_id: foreign.organization_id,
      environment: "production",
      external_id: "foreign-snapshot",
    });
    announceLifecycle(workspace.organization_id, workspace.id, event, at);
    expect(db().webhook_events.at(-1)?.body).toMatchObject({
      organization_id: workspace.organization_id,
      environment: workspace.environment,
      workspace: { id: workspace.id, external_id: workspace.external_id },
      created_at: at,
      ...event,
    });
    expect(db().webhook_deliveries.filter((delivery) => delivery.endpoint_id === foreignEndpoint.id)).toEqual(
      before.deliveries.filter((delivery) => delivery.endpoint_id === foreignEndpoint.id),
    );
  });

  it.each(["organization", "workspace"] as const)(
    "refuses a poisoned %s before expiring or emitting",
    async (scope) => {
      const workspace = ws("qa-playground");
      const session = await owner.connectSessions.create(workspace.id, {
        app_id: app("echo_server").id,
        intent: "create",
        label: "Poisoned expiry",
        installation_id: null,
        return_url: null,
      });
      const row = must(db().connect_sessions.find((item) => item.id === session.id));
      const poisoned = {
        ...structuredClone(row),
        expires_at: new Date(Date.now() - 1).toISOString(),
        ...(scope === "organization"
          ? { organization_id: must(db().organizations.find((item) => item.slug === "larkspur")).id }
          : { workspace_id: ws("acme-sandbox").id }),
      };
      db().connect_sessions.unshift(poisoned);
      const snapshot = () => ({
        sessions: db().connect_sessions,
        events: db().webhook_events,
        deliveries: db().webhook_deliveries,
      });
      const before = structuredClone(snapshot());
      expect(() => withExpiry(workspace.organization_id, workspace.id, poisoned)).toThrow();
      expect(snapshot()).toEqual(before);
      expect(poisoned.status).toBe("pending");
    },
  );

  it("expires at the exact boundary once with no installation result", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    try {
      const session = await owner.connectSessions.create(ws("qa-playground").id, {
        app_id: app("echo_server").id,
        intent: "create",
        label: "Expired",
        installation_id: null,
        return_url: null,
      });
      const row = must(db().connect_sessions.find((item) => item.id === session.id));
      vi.setSystemTime(Date.parse(row.expires_at) - 1);
      expect(withExpiry(row.organization_id, row.workspace_id, row).status).toBe("pending");
      vi.setSystemTime(Date.parse(row.expires_at));
      expect(withExpiry(row.organization_id, row.workspace_id, row).status).toBe("expired");
      withExpiry(row.organization_id, row.workspace_id, row);
      const events = db().webhook_events.filter((event) => event.body.data.id === session.id);
      expect(events).toHaveLength(1);
      expect(events[0]?.body).toMatchObject({
        type: "connect_session.failed",
        created_at: row.expires_at,
        data: { status: "expired", installation_id: null, failure_code: null },
      });
      expect((await failure(owner.connect.cancel(referenceOf(session)))).code).toBe("session_not_pending");
      expect(db().webhook_events.filter((event) => event.body.data.id === session.id)).toEqual(events);
    } finally {
      vi.useRealTimers();
    }
  });

  it("records only one terminal result when credentials race cancellation", async () => {
    const session = await owner.connectSessions.create(ws("qa-playground").id, {
      app_id: app("echo_server").id,
      intent: "create",
      label: "Race",
      installation_id: null,
      return_url: null,
    });
    await Promise.allSettled([
      owner.connect.submitCredentials(referenceOf(session), { values: { token: "race-private-token" } }),
      owner.connect.cancel(referenceOf(session)),
    ]);
    const events = db().webhook_events.filter((event) => event.body.data.id === session.id);
    expect(events).toHaveLength(1);
    const row = must(db().connect_sessions.find((item) => item.id === session.id));
    expect(events[0]?.body.data).toMatchObject({ status: row.status });
  });
});

describe("webhook secret rotation", () => {
  it.each(["sandbox", "production"] as const)(
    "rolls only the named organization's %s endpoint despite a poisoned row ahead of it",
    async (environment) => {
      const organization = must(northwindId());
      const endpoint = must(
        db().webhooks.find((row) => row.organization_id === organization && row.environment === environment),
      );
      const otherOrganization = must(db().organizations.find((row) => row.slug === "larkspur")).id;
      const poisoned = {
        ...structuredClone(endpoint),
        organization_id: otherOrganization,
        previous_signing_secret_expires_at: new Date(Date.now() + 48 * 60 * 60_000).toISOString(),
      };
      db().webhooks.unshift(poisoned);
      const unaffected = () => ({
        endpoints: db().webhooks.filter((row) => row !== endpoint),
        deliveries: db().webhook_deliveries,
        events: db().webhook_events,
        attempts: db().webhook_delivery_attempts,
      });
      const before = structuredClone(unaffected());
      const original = structuredClone(endpoint);
      const result = await owner.webhooks.rollSecret(organization, endpoint.id);
      expect(result.endpoint).toEqual({
        ...original,
        signing_secret_last4: result.signing_secret.slice(-4),
        previous_signing_secret_expires_at: expect.any(String),
      });
      expect(unaffected()).toEqual(before);
      const event = must(db().audit.find((row) => row.action === "webhook.secret_rolled"));
      expect(event).toMatchObject({
        organization_id: organization,
        workspace_id: null,
        environment,
        target: { type: "webhook", id: endpoint.id, name: endpoint.url },
      });
      expect(Object.keys(event).slice(0, 3)).toEqual(["id", "organization_id", "workspace_id"]);
    },
  );

  it("stores neither secret, including in successful idempotency records, and cannot extend overlap with new keys", async () => {
    const organization = must(northwindId());
    const created = await owner.webhooks.create(organization, {
      environment: "sandbox",
      url: "https://hooks.northwind.example/rotation",
      description: "Secret rotation",
      events: ["run.completed"],
    });
    const options = { idempotencyKey: "rotate-once" };
    const result = await owner.webhooks.rollSecret(organization, created.endpoint.id, options);
    const row = must(db().idempotency_keys.find((item) => item.key === options.idempotencyKey));
    expect(row).toMatchObject({
      organization_id: organization,
      workspace_id: null,
      state: "completed",
      status: 200,
      body: null,
    });
    expect(Object.keys(must(db().webhooks.find((item) => item.id === created.endpoint.id))).slice(0, 2)).toEqual([
      "id",
      "organization_id",
    ]);
    expect(await failure(owner.webhooks.rollSecret(organization, created.endpoint.id, options))).toMatchObject({
      status: 409,
      code: "idempotency_key_completed",
    });
    for (const idempotencyKey of ["rotate-again", "rotate-yet-again"]) {
      expect(
        await failure(owner.webhooks.rollSecret(organization, created.endpoint.id, { idempotencyKey })),
      ).toMatchObject({
        status: 409,
        code: "rotation_in_progress",
      });
    }
    const updated = await owner.webhooks.update(organization, created.endpoint.id, { description: "Updated" });
    expect(updated).toEqual({ ...result.endpoint, description: "Updated" });
    expect(db().audit.filter((item) => item.action === "webhook.secret_rolled")).toHaveLength(1);
    const exposed = JSON.stringify({
      endpoints: await owner.webhooks.list(organization),
      updated,
      audit: await owner.audit.list(organization),
      database: db(),
      persisted: localStorage.getItem("zupflow.mock.db"),
    });
    for (const secret of [created.signing_secret, result.signing_secret]) expect(exposed).not.toContain(secret);
  });

  it("accepts only one concurrent rotation and records one audit event", async () => {
    const organization = must(northwindId());
    const endpoint = must(db().webhooks.find((row) => row.organization_id === organization));
    const results = await Promise.allSettled([
      owner.webhooks.rollSecret(organization, endpoint.id, { idempotencyKey: "rotate-a" }),
      owner.webhooks.rollSecret(organization, endpoint.id, { idempotencyKey: "rotate-b" }),
    ]);
    expect(results.filter((result) => result.status === "fulfilled")).toHaveLength(1);
    const rejected = must(results.find((result) => result.status === "rejected"));
    expect(rejected.reason).toMatchObject({ status: 409, code: "rotation_in_progress" });
    expect(db().audit.filter((row) => row.action === "webhook.secret_rolled")).toHaveLength(1);
  });

  it("ends overlap at the exact expiry boundary and permits a fresh rotation then", async () => {
    const organization = must(northwindId());
    const endpoint = must(db().webhooks.find((row) => row.organization_id === organization));
    const start = Date.parse("2030-01-02T03:04:05.000Z");
    const expires = start + 24 * 60 * 60_000;
    vi.useFakeTimers({ toFake: ["Date"] });
    try {
      vi.setSystemTime(start);
      const rolled = await owner.webhooks.rollSecret(organization, endpoint.id);
      expect(rolled.endpoint.previous_signing_secret_expires_at).toBe(new Date(expires).toISOString());
      vi.setSystemTime(expires - 1);
      expect((await owner.webhooks.list(organization)).find((row) => row.id === endpoint.id)).toEqual(rolled.endpoint);
      expect((await owner.webhooks.update(organization, endpoint.id, {})).previous_signing_secret_expires_at).toBe(
        rolled.endpoint.previous_signing_secret_expires_at,
      );
      expect(await failure(owner.webhooks.rollSecret(organization, endpoint.id))).toMatchObject({
        status: 409,
        code: "rotation_in_progress",
      });
      vi.setSystemTime(expires);
      expect((await owner.webhooks.list(organization)).find((row) => row.id === endpoint.id)).toEqual({
        ...rolled.endpoint,
        previous_signing_secret_expires_at: null,
      });
      expect(
        (await owner.webhooks.update(organization, endpoint.id, {})).previous_signing_secret_expires_at,
      ).toBeNull();
      const again = await owner.webhooks.rollSecret(organization, endpoint.id);
      expect(again.endpoint.previous_signing_secret_expires_at).toBe(
        new Date(expires + 24 * 60 * 60_000).toISOString(),
      );
      expect(again.endpoint.signing_secret_last4).toBe(again.signing_secret.slice(-4));
      expect(again.signing_secret).not.toBe(rolled.signing_secret);
      expect(await failure(owner.webhooks.rollSecret(organization, endpoint.id))).toMatchObject({
        status: 409,
        code: "rotation_in_progress",
      });
      expect(db().audit.filter((row) => row.action === "webhook.secret_rolled")).toHaveLength(2);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("webhook delivery history isolation", () => {
  const fixture = () => {
    const organization = must(northwindId());
    const endpoint = must(
      db().webhooks.find((row) => row.organization_id === organization && row.environment === "production"),
    );
    const delivery = must(
      db().webhook_deliveries.find(
        (row) => row.organization_id === organization && row.endpoint_id === endpoint.id && row.state === "pending",
      ),
    );
    const event = must(
      db().webhook_events.find((row) => row.organization_id === organization && row.id === delivery.event_id),
    );
    const otherOrganization = must(db().organizations.find((row) => row.slug === "larkspur")).id;
    return { organization, endpoint, delivery, event, otherOrganization };
  };

  it("resend creates only its own delivery, leaving poisoned rows and the original event and attempts intact", async () => {
    const { organization, endpoint, delivery, event, otherOrganization } = fixture();
    delivery.state = "failed";
    delivery.next_attempt_at = null;
    db().webhooks.unshift({ ...structuredClone(endpoint), organization_id: otherOrganization, enabled: false });
    db().webhook_deliveries.unshift({ ...delivery, organization_id: otherOrganization, state: "pending" });
    db().webhook_events.unshift({ ...structuredClone(event), organization_id: otherOrganization });
    const attempt = must(
      db().webhook_delivery_attempts.find(
        (row) =>
          row.organization_id === organization && row.endpoint_id === endpoint.id && row.delivery_id === delivery.id,
      ),
    );
    db().webhook_delivery_attempts.unshift({ ...attempt, organization_id: otherOrganization, status_code: 418 });
    const before = structuredClone({
      endpoints: db().webhooks,
      deliveries: db().webhook_deliveries,
      events: db().webhook_events,
      attempts: db().webhook_delivery_attempts,
    });
    const resent = await owner.webhooks.resendDelivery(organization, endpoint.id, delivery.id);
    expect(resent).toMatchObject({
      organization_id: organization,
      endpoint_id: endpoint.id,
      event_id: event.id,
      environment: endpoint.environment,
      workspace: event.body.workspace,
      state: "pending",
      attempts: 0,
      last_status_code: null,
      last_error: null,
      last_attempt_at: null,
    });
    expect(resent.id).not.toBe(delivery.id);
    expect({
      endpoints: db().webhooks,
      deliveries: db().webhook_deliveries.filter((row) => row.id !== resent.id),
      events: db().webhook_events,
      attempts: db().webhook_delivery_attempts,
    }).toEqual(before);
    expect((await owner.webhooks.delivery(organization, endpoint.id, resent.id)).event).toEqual(event.body);
    expect((await owner.webhooks.deliveryAttempts(organization, endpoint.id, resent.id)).items).toEqual([]);
    const stored = must(db().webhook_deliveries.find((row) => row.id === resent.id));
    expect(Object.keys(stored).slice(0, 2)).toEqual(["id", "organization_id"]);
    const auditEvent = must(db().audit.find((row) => row.target.id === resent.id));
    expect(auditEvent).toMatchObject({
      organization_id: organization,
      workspace_id: event.workspace_id,
      environment: endpoint.environment,
      action: "webhook.delivery_resent",
      target: { type: "webhook_delivery", id: resent.id, name: event.body.type },
    });
    expect(Object.keys(auditEvent).slice(0, 3)).toEqual(["id", "organization_id", "workspace_id"]);

    // Sending a test creates an attempt; queuing the resend above correctly created none.
    await owner.webhooks.sendTest(organization, endpoint.id);
    const newAttempt = must(db().webhook_delivery_attempts.at(-1));
    expect(newAttempt).toMatchObject({ organization_id: organization, endpoint_id: endpoint.id, number: 1 });
    expect(Object.keys(newAttempt).slice(0, 3)).toEqual(["organization_id", "endpoint_id", "delivery_id"]);
    const testDelivery = must(db().webhook_deliveries.find((row) => row.id === newAttempt.delivery_id));
    expect(Object.keys(testDelivery).slice(0, 2)).toEqual(["id", "organization_id"]);
  });

  it.each(["body organization", "body environment", "workspace organization", "workspace environment"])(
    "resend refuses an event with a mismatched %s without inserting or auditing a delivery",
    async (mismatch) => {
      const { organization, endpoint, delivery, event, otherOrganization } = fixture();
      delivery.state = "failed";
      delivery.next_attempt_at = null;
      if (mismatch === "body organization") event.body.organization_id = otherOrganization;
      else if (mismatch === "body environment") event.body.environment = "sandbox";
      else {
        event.workspace_id = ws(mismatch === "workspace organization" ? "harbor" : "acme-sandbox").id;
        if (event.body.workspace) event.body.workspace.id = event.workspace_id;
      }
      const snapshot = () => ({
        deliveries: db().webhook_deliveries,
        events: db().webhook_events,
        attempts: db().webhook_delivery_attempts,
        audit: db().audit,
      });
      const before = structuredClone(snapshot());
      expect(await failure(owner.webhooks.resendDelivery(organization, endpoint.id, delivery.id))).toMatchObject({
        status: 404,
        code: "not_found",
      });
      expect(snapshot()).toEqual(before);
    },
  );

  it("ignores poisoned delivery, event and attempt rows carrying another organization", async () => {
    const { organization, endpoint, delivery, event, otherOrganization } = fixture();
    const before = await owner.webhooks.delivery(organization, endpoint.id, delivery.id);
    const attempts = await owner.webhooks.deliveryAttempts(organization, endpoint.id, delivery.id);
    db().webhook_deliveries.unshift({ ...delivery, organization_id: otherOrganization, attempts: 900 });
    db().webhook_events.unshift({
      ...structuredClone(event),
      organization_id: otherOrganization,
      body: { ...event.body, created_at: "2000-01-01T00:00:00.000Z" },
    });
    const first = must(
      db().webhook_delivery_attempts.find(
        (row) => row.organization_id === organization && row.delivery_id === delivery.id,
      ),
    );
    db().webhook_delivery_attempts.unshift({ ...first, organization_id: otherOrganization, status_code: 418 });
    expect(await owner.webhooks.delivery(organization, endpoint.id, delivery.id)).toEqual(before);
    expect(await owner.webhooks.deliveryAttempts(organization, endpoint.id, delivery.id)).toEqual(attempts);
    expect(
      (await owner.webhooks.deliveries(organization, endpoint.id)).items.filter((row) => row.id === delivery.id),
    ).toHaveLength(1);
  });

  it("reads attempts only from the named endpoint and delivery", async () => {
    const { organization, endpoint, delivery } = fixture();
    const before = await owner.webhooks.deliveryAttempts(organization, endpoint.id, delivery.id);
    const first = must(
      db().webhook_delivery_attempts.find(
        (row) => row.organization_id === organization && row.delivery_id === delivery.id,
      ),
    );
    db().webhook_delivery_attempts.push({ ...first, endpoint_id: newId("webhook_endpoint"), status_code: 418 });
    db().webhook_delivery_attempts.push({ ...first, delivery_id: newId("webhook_delivery"), status_code: 418 });
    expect(await owner.webhooks.deliveryAttempts(organization, endpoint.id, delivery.id)).toEqual(before);
  });

  it.each(["organization", "environment", "body workspace", "body organization", "body environment"])(
    "hides an event whose %s does not match its trusted scope",
    async (mismatch) => {
      const { organization, endpoint, delivery, event, otherOrganization } = fixture();
      const foreign = ws("harbor");
      const sibling = ws("beta-labs");
      if (mismatch === "organization") {
        event.workspace_id = foreign.id;
        if (event.body.workspace) event.body.workspace.id = foreign.id;
      } else if (mismatch === "environment") {
        event.workspace_id = ws("acme-sandbox").id;
        if (event.body.workspace) event.body.workspace.id = event.workspace_id;
      } else if (mismatch === "body workspace") {
        if (event.body.workspace) event.body.workspace.id = sibling.id;
      } else if (mismatch === "body organization") event.body.organization_id = otherOrganization;
      else event.body.environment = "sandbox";
      const list = await owner.webhooks.deliveries(organization, endpoint.id);
      expect(list.items.some((row) => row.id === delivery.id)).toBe(false);
      expect(await failure(owner.webhooks.delivery(organization, endpoint.id, delivery.id))).toMatchObject({
        status: 404,
        code: "not_found",
      });
      expect(await failure(owner.webhooks.deliveryAttempts(organization, endpoint.id, delivery.id))).toMatchObject({
        status: 404,
        code: "not_found",
      });
    },
  );

  it("reports incomplete legacy attempt history without inventing records", async () => {
    const { organization, endpoint, delivery } = fixture();
    db().webhook_delivery_attempts = db().webhook_delivery_attempts.filter((row) => row.delivery_id !== delivery.id);
    const detail = await owner.webhooks.delivery(organization, endpoint.id, delivery.id);
    expect(detail.attempts).toBe(2);
    expect(detail.attempt_history_complete).toBe(false);
    expect((await owner.webhooks.deliveryAttempts(organization, endpoint.id, delivery.id)).items).toEqual([]);
  });

  it("never returns response text or unknown error strings from history", async () => {
    const { organization, endpoint, delivery } = fixture();
    Object.assign(delivery, {
      last_error: "secret-from-an-internal-error",
      response_body: "private response",
      request_headers: { authorization: "private bearer" },
    });
    const attempt = must(
      db().webhook_delivery_attempts.find(
        (row) => row.organization_id === organization && row.delivery_id === delivery.id,
      ),
    );
    Object.assign(attempt, { error_class: "secret-from-an-internal-error", response_body: "private response" });
    const detail = await owner.webhooks.delivery(organization, endpoint.id, delivery.id);
    const attempts = await owner.webhooks.deliveryAttempts(organization, endpoint.id, delivery.id);
    expect(detail.last_error).toBe("failed");
    expect(attempts.items[0]?.error_class).toBe("failed");
    const serialized = JSON.stringify([detail, attempts]);
    expect(serialized).not.toContain("private");
    expect(serialized).not.toContain("secret-from");
  });

  it("uses the ASCII delivery ID as the stable tie-breaker and does not repeat rows after new arrivals", async () => {
    const { organization, endpoint, delivery } = fixture();
    const at = delivery.created_at;
    const rows = db().webhook_deliveries.filter(
      (row) => row.organization_id === organization && row.endpoint_id === endpoint.id,
    );
    for (const row of rows) row.created_at = at;
    const expected = rows
      .map((row) => row.id)
      .sort()
      .reverse();
    const first = await owner.webhooks.deliveries(organization, endpoint.id, { limit: 2 });
    expect(first.items.map((row) => row.id)).toEqual(expected.slice(0, 2));
    await owner.webhooks.sendTest(organization, endpoint.id);
    const second = await owner.webhooks.deliveries(organization, endpoint.id, {
      limit: 100,
      cursor: must(first.pagination.next_cursor),
    });
    expect(second.items.map((row) => row.id)).toEqual(expected.slice(2));
  });

  it("deleting an endpoint or workspace removes only its own delivery history", async () => {
    const { organization, endpoint, otherOrganization } = fixture();
    const productionDelivery = must(
      db().webhook_deliveries.find((row) => row.organization_id === organization && row.endpoint_id === endpoint.id),
    );
    const productionAttempt = must(
      db().webhook_delivery_attempts.find(
        (row) => row.organization_id === organization && row.delivery_id === productionDelivery.id,
      ),
    );
    db().webhook_deliveries.push({ ...productionDelivery, organization_id: otherOrganization });
    db().webhook_delivery_attempts.push({ ...productionAttempt, organization_id: otherOrganization });
    const deletedWorkspace = ws("acme-sandbox").id;
    const sandboxEvent = must(
      db().webhook_events.find((row) => row.organization_id === organization && row.workspace_id === deletedWorkspace),
    );
    const sandboxDelivery = must(
      db().webhook_deliveries.find((row) => row.organization_id === organization && row.event_id === sandboxEvent.id),
    );
    const sandboxAttempt = must(
      db().webhook_delivery_attempts.find(
        (row) => row.organization_id === organization && row.delivery_id === sandboxDelivery.id,
      ),
    );
    db().webhook_events.push({ ...structuredClone(sandboxEvent), organization_id: otherOrganization });
    db().webhook_deliveries.push({ ...sandboxDelivery, organization_id: otherOrganization });
    db().webhook_delivery_attempts.push({ ...sandboxAttempt, organization_id: otherOrganization });
    const siblingEvent = must(
      db().webhook_events.find(
        (row) => row.organization_id === organization && row.workspace_id === ws("qa-playground").id,
      ),
    );
    const siblingDelivery = must(
      db().webhook_deliveries.find((row) => row.organization_id === organization && row.event_id === siblingEvent.id),
    );
    // Foreign rows must not contribute IDs to a local deletion cascade.
    db().webhook_events.push({
      ...structuredClone(siblingEvent),
      organization_id: otherOrganization,
      workspace_id: deletedWorkspace,
    });
    db().webhook_deliveries.push({
      ...siblingDelivery,
      organization_id: otherOrganization,
      event_id: sandboxEvent.id,
    });
    const otherBefore = structuredClone({
      deliveries: db().webhook_deliveries.filter((row) => row.organization_id === otherOrganization),
      attempts: db().webhook_delivery_attempts.filter((row) => row.organization_id === otherOrganization),
      events: db().webhook_events.filter((row) => row.organization_id === otherOrganization),
    });
    const sandbox = must(
      db().webhooks.find((row) => row.organization_id === organization && row.environment === "sandbox"),
    );
    const sandboxBefore = await owner.webhooks.deliveries(organization, sandbox.id);
    const siblingBefore = await owner.webhooks.delivery(organization, sandbox.id, siblingDelivery.id);
    const siblingAttemptsBefore = await owner.webhooks.deliveryAttempts(organization, sandbox.id, siblingDelivery.id);
    expect(siblingAttemptsBefore.items.length).toBeGreaterThan(0);
    await owner.webhooks.delete(organization, endpoint.id);
    expect(
      db().webhook_deliveries.some((row) => row.organization_id === organization && row.endpoint_id === endpoint.id),
    ).toBe(false);
    expect(
      db().webhook_delivery_attempts.some(
        (row) => row.organization_id === organization && row.endpoint_id === endpoint.id,
      ),
    ).toBe(false);
    expect(await owner.webhooks.deliveries(organization, sandbox.id)).toEqual(sandboxBefore);
    await owner.workspaces.delete(deletedWorkspace);
    const after = await owner.webhooks.deliveries(organization, sandbox.id);
    expect(after.items).toEqual(sandboxBefore.items.filter((row) => row.workspace?.id !== deletedWorkspace));
    expect(await owner.webhooks.delivery(organization, sandbox.id, siblingDelivery.id)).toEqual(siblingBefore);
    expect(await owner.webhooks.deliveryAttempts(organization, sandbox.id, siblingDelivery.id)).toEqual(
      siblingAttemptsBefore,
    );
    expect(
      db().webhook_events.some((row) => row.organization_id === organization && row.workspace_id === deletedWorkspace),
    ).toBe(false);
    expect({
      deliveries: db().webhook_deliveries.filter((row) => row.organization_id === otherOrganization),
      attempts: db().webhook_delivery_attempts.filter((row) => row.organization_id === otherOrganization),
      events: db().webhook_events.filter((row) => row.organization_id === otherOrganization),
    }).toEqual(otherBefore);
  });
});

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
  // The fake provider's own record of an app: what Slack would hold, read from the mock's database.
  channelAppSecrets: async (app) => {
    const row = must(
      db().channel_apps.find((a) => a.id === app),
      app,
    );
    return { inbound_ref: row.inbound_ref, signing_secret: row.signing_secret };
  },
});

describe("API-key creation authority changes", () => {
  function pauseApiKeyHash() {
    let resume = () => {};
    let notify = () => {};
    const released = new Promise<void>((resolve) => {
      resume = resolve;
    });
    const entered = new Promise<void>((resolve) => {
      notify = resolve;
    });
    const digest = credentials.keyedDigest;
    let paused = false;
    const spy = vi.spyOn(credentials, "keyedDigest").mockImplementation(async (domain, value) => {
      if (domain === "api_key" && !paused) {
        paused = true;
        notify();
        await released;
      }
      return digest(domain, value);
    });
    return { entered, release: () => resume(), restore: () => spy.mockRestore() };
  }

  it.each([
    { label: "Sandbox developer demotion", environment: "sandbox", initial: "developer", next: "viewer", status: 403 },
    { label: "Production admin demotion", environment: "production", initial: "admin", next: "developer", status: 403 },
    { label: "Sandbox member removal", environment: "sandbox", initial: "developer", next: null, status: 404 },
    { label: "Production member removal", environment: "production", initial: "admin", next: null, status: 404 },
    {
      label: "Sandbox demotion retaining permission",
      environment: "sandbox",
      initial: "admin",
      next: "developer",
      status: 201,
    },
  ] as const)("rechecks $label after hashing", async ({ environment, initial, next, status }) => {
    const organization = must(northwindId());
    const userId = userByEmail("sam@northwind.example");
    const member = must(db().memberships.find((m) => m.organization_id === organization && m.user_id === userId));
    await owner.members.update(organization, member.id, { role: initial });
    const workspace = must(
      db().workspaces.find((w) => w.organization_id === organization && w.environment === environment),
    );
    const keysBefore = structuredClone(db().api_keys);
    const auditBefore = db().audit.filter((event) => event.action === "api_key.created").length;
    const barrier = pauseApiKeyHash();
    const pending = clientFor(userId)
      .apiKeys.create(organization, { name: "Authority check", environment })
      .then(
        (created) => ({ status: 201, code: null, created }),
        (error: unknown) => {
          if (!(error instanceof ZupflowApiError)) throw error;
          return { status: error.status, code: error.code, created: null };
        },
      );
    let result: Awaited<typeof pending>;
    try {
      await Promise.race([
        barrier.entered,
        pending.then(() => {
          throw new Error("Key creation finished before the hashing barrier");
        }),
      ]);
      if (next === null) await owner.members.remove(organization, member.id);
      else await owner.members.update(organization, member.id, { role: next });
    } finally {
      barrier.release();
      result = await pending.finally(barrier.restore);
    }
    let usable = false;
    if (result.created) {
      const { secret } = result.created;
      const keyClient = createClient(createHttpTransport({ baseUrl: API, getToken: async () => secret }));
      usable = (await keyClient.workspaces.get(workspace.id)).id === workspace.id;
    }
    expect({ status: result.status, code: result.code, usable }).toEqual({
      status,
      code: status === 201 ? null : status === 403 ? "forbidden" : "not_found",
      usable: status === 201,
    });
    expect(db().api_keys.filter((key) => key.id !== result.created?.key.id)).toEqual(keysBefore);
    expect(db().api_keys).toHaveLength(keysBefore.length + (status === 201 ? 1 : 0));
    expect(db().audit.filter((event) => event.action === "api_key.created")).toHaveLength(
      auditBefore + (status === 201 ? 1 : 0),
    );
  });
});

describe("API-key storage", () => {
  it("stores only a domain-separated hash and never persists a created secret", async () => {
    const owner = clientFor(DEMO_USER_ID);
    const organization = must(northwindId(), "Northwind");
    const created = await owner.apiKeys.create(
      organization,
      { name: "Storage check", environment: "sandbox" },
      { idempotencyKey: "key-storage" },
    );
    const row = must(db().api_keys.find((key) => key.organization_id === organization && key.id === created.key.id));
    expect(row.secret_hash === (await credentials.keyedDigest("api_key", created.secret))).toBe(true);
    expect(row.secret_hash === (await credentials.keyedDigest("idempotency", created.secret))).toBe(false);
    expect(JSON.stringify(db()).includes(created.secret)).toBe(false);
    expect(localStorage.getItem("zupflow.mock.db")?.includes(created.secret)).toBe(false);
    const saved = must(
      db().idempotency_keys.find((entry) => entry.organization_id === organization && entry.key === "key-storage"),
    );
    expect(saved.state).toBe("completed");
    expect(saved.body === null).toBe(true);
    const keyed = createClient(createHttpTransport({ baseUrl: API, getToken: async () => created.secret }));
    await keyed.models.list(organization);
    const used = row.last_used_at;
    expect(used).not.toBeNull();
    await keyed.models.list(organization);
    expect(row.last_used_at).toBe(used);
    row.last_used_at = new Date(Date.now() - 60_000).toISOString();
    const previous = row.last_used_at;
    await keyed.models.list(organization);
    expect(Date.parse(must(row.last_used_at))).toBeGreaterThan(Date.parse(previous));
    // Revocation remains authoritative even though the stored hash still matches.
    await owner.apiKeys.revoke(organization, row.id);
    expect(await failure(keyed.models.list(organization))).toMatchObject({ status: 401, code: "unauthenticated" });
  });

  it("never treats historical key display metadata as a usable credential", async () => {
    const organization = must(northwindId());
    const historical = must(
      db().api_keys.find((key) => key.organization_id === organization && key.revoked_at === null),
    );
    expect(historical.secret_hash).toBeNull();
    const guessed = `${historical.prefix}${"0".repeat(36)}${historical.last4}`;
    const client = createClient(createHttpTransport({ baseUrl: API, getToken: async () => guessed }));
    expect(await failure(client.models.list(organization))).toMatchObject({ status: 401, code: "unauthenticated" });
  });
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

describe("configured OAuth issuer isolation", () => {
  it.each(["organization", "workspace"] as const)(
    "ignores foreign %s configurations when saving a configured issuer",
    async (scope) => {
      const workspace = ws("qa-playground");
      const sibling = ws("acme-sandbox");
      expect(sibling.environment).toBe(workspace.environment);
      const input = {
        kind: "mcp" as const,
        name: "Issuer isolation",
        key: "issuer_isolation",
        description: "",
        auth_type: "oauth2" as const,
      };
      const created =
        scope === "organization"
          ? await owner.organizationApps.create(workspace.organization_id, input)
          : await owner.workspaceApps.create(workspace.id, input);
      if (created.auth.type !== "oauth2") throw new Error("Expected an OAuth app");
      const own = must(
        db().configurations.find(
          (c) =>
            c.organization_id === workspace.organization_id &&
            c.workspace_id === (scope === "organization" ? null : workspace.id) &&
            c.environment === workspace.environment &&
            c.app_id === created.id,
        ),
        "the app's configuration",
      );
      // An app ID alone must not let a mislabelled row constrain its template.
      const foreign = {
        ...structuredClone(own),
        organization_id:
          scope === "organization"
            ? must(db().organizations.find((o) => o.slug === "larkspur")?.id)
            : own.organization_id,
        workspace_id: scope === "workspace" ? sibling.id : own.workspace_id,
        oauth_scopes: ["foreign:read"],
      };
      db().configurations.push(foreign);
      const before = structuredClone(foreign);
      const auth = { ...created.auth, issuer: "https://auth.provider.example/Tenant/" };
      const updated =
        scope === "organization"
          ? await owner.organizationApps.update(workspace.organization_id, created.id, { auth })
          : await owner.workspaceApps.update(workspace.id, created.id, { auth });
      expect(updated.auth).toEqual(auth);
      expect(foreign).toEqual(before);
    },
  );
});

describe("idempotency store", () => {
  // Black-box idempotency is in the shared suite. These plant the states a request can't reach on its own.
  const qa = () => ws("qa-playground").id;
  const input = { name: "Stored", description: "", instructions: "" };
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
      app_id: app("google_workspace").id,
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
      invited_by: null,
      joined_at: new Date().toISOString(),
      last_active_at: null,
    });
    const app = { kind: "mcp", name: "Both", key: "both_orgs", description: "", auth_type: "token" } as const;
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
    expect(JSON.stringify(db().idempotency_keys).includes(created.secret)).toBe(false);
    await owner.agents.create(qa(), input, { idempotencyKey: "store-env" });
    expect(rowFor("store-env").caller).toBe(`member:${DEMO_USER_ID}|sandbox`);
  });
});

describe("Connect (simulated consent)", () => {
  it("clears reconnect required with the same account and the next credential version", async () => {
    const sandbox = ws("acme-sandbox");
    const transport = createHttpTransport({ baseUrl: API, getToken: async () => null });
    const consent = async (session: { url: string }) =>
      transport.request<InstallationConnectResult>({
        method: "POST",
        path: `/v1/connect/sessions/${referenceOf(session)}/simulate-consent`,
        body: { decision: "approved", email: "reconnect@acme.example" },
      });
    const first = await owner.connectSessions.create(sandbox.id, {
      app_id: app("google_workspace").id,
      intent: "create",
      label: "Reconnect test",
      installation_id: null,
      return_url: null,
    });
    const { data } = await consent(first);
    const installation = must(db().installations.find((i) => i.id === data.installation.id));
    installation.status = "reconnect_required";
    const connection = must(installation.connection);
    const version = connection.credential_version;
    const session = await owner.connectSessions.create(sandbox.id, {
      app_id: installation.app_id,
      intent: "reconnect",
      label: "ignored",
      installation_id: installation.id,
      return_url: null,
    });
    await consent(session);
    const renewed = await owner.installations.get(sandbox.id, installation.id);
    expect(renewed.status).toBe("connected");
    expect(renewed.connection?.credential_version).toBe(version + 1);
    expect(renewed.connection?.account).toEqual(connection.account);
  });

  it("refuses to reconnect a different account", async () => {
    const sandbox = ws("acme-sandbox");
    const installation = db().installations.find(
      (i) => i.workspace_id === sandbox.id && i.label === "Marketing (test)",
    );
    const session = await owner.connectSessions.create(sandbox.id, {
      app_id: app("google_workspace").id,
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
      key: "ledger",
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
        await transport.request<InstallationConnectResult>({
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
      key: "ledger",
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
      app_id: app("google_workspace").id,
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
      app_id: app("google_workspace").id,
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
      key: "ledger",
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
    const google = app("google_workspace");
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

  it("gives the model a denial's reason", async () => {
    const run = db().runs.find((r) => r.status === "waiting_for_approval");
    expect(run).toBeDefined();
    if (!run) return;
    const { resolveApproval, advanceRun } = await import("./runner");
    const requested = [...timelineOf(db(), run)].reverse().find((e) => e.type === "approval.requested");
    const callId = must(requested?.type === "approval.requested" ? requested.data.call_id : null, "pending call");
    resolveApproval(
      db(),
      run,
      callId,
      "denied",
      approvalKeyFor(run.workspace_id),
      new Date().toISOString(),
      "  Not this week.  ",
    );
    advanceRun(db(), run, new Date().toISOString(), () => 0.5);
    const said = [...timelineOf(db(), run)].reverse().find((e) => e.type === "message.assistant");
    expect(said?.type === "message.assistant" ? said.data.text : "").toMatch(
      /the request was denied: Not this week\.$/,
    );
  });
});

describe("calls waiting on an installation that goes", () => {
  /** A waiting run, the call it waits on and that call's installation. */
  function waiting() {
    const run = must(
      db().runs.find((r) => r.status === "waiting_for_approval"),
      "waiting run",
    );
    const requested = [...timelineOf(db(), run)].reverse().find((e) => e.type === "approval.requested");
    if (requested?.type !== "approval.requested" || !("installation_id" in requested.data))
      throw new Error("Missing pending call");
    return { run, callId: requested.data.call_id, installationId: requested.data.installation_id };
  }

  for (const [how, code] of [
    ["removed", "installation_removed"],
    ["disconnected", "installation_disconnected"],
  ] as const) {
    it(`refuses the call a run waits on once its installation is ${how}, and only in its workspace`, async () => {
      const { run, callId, installationId } = waiting();
      // A run of another workspace, and one of another organization, whose
      // pending call names the same installation ID: neither is touched.
      const others = db().runs.filter(
        (r) =>
          r.status === "waiting_for_approval" &&
          !(r.organization_id === run.organization_id && r.workspace_id === run.workspace_id),
      );
      const sibling = must(
        others.find((r) => r.organization_id === run.organization_id),
        "a waiting run in a sibling workspace",
      );
      // Another organization's run, under this workspace's own ID.
      const larkspur = must(
        db().organizations.find((o) => o.slug === "larkspur"),
        "Larkspur",
      ).id;
      const foreign = { ...structuredClone(run), id: newId("run"), organization_id: larkspur };
      db().runs.push(foreign);
      db().run_events[runEventsKey(foreign)] = timelineOf(db(), run).map((e) => ({
        ...structuredClone(e),
        id: newId("run_event"),
        organization_id: larkspur,
        run_id: foreign.id,
      }));
      others.push(foreign);
      for (const other of [sibling, foreign]) {
        const pending = [...timelineOf(db(), other)].reverse().find((e) => e.type === "approval.requested");
        if (pending?.type !== "approval.requested" || !("installation_id" in pending.data))
          throw new Error("Missing pending call");
        pending.data.installation_id = installationId;
      }
      const before = others.map((r) => [r.id, r.status, timelineOf(db(), r).length]);

      const workspace = must(
        db().workspaces.find((w) => w.organization_id === run.organization_id && w.id === run.workspace_id),
        "workspace",
      );
      if (how === "removed") await owner.installations.delete(workspace.id, installationId);
      else await owner.installations.disconnect(workspace.id, installationId);

      expect(run.status).toBe("running");
      const events = timelineOf(db(), run);
      expect(events.some((e) => e.type === "approval.resolved" && e.data.call_id === callId)).toBe(false);
      const failed = must(
        events.find((e) => e.type === "tool_call.failed" && e.data.call_id === callId),
        "the refusal",
      );
      expect(failed.type === "tool_call.failed" ? failed.data.error.code : null).toBe(code);
      expect(events.some((e) => e.type === "tool_call.started" && e.data.call_id === callId)).toBe(false);
      expect(others.map((r) => [r.id, r.status, timelineOf(db(), r).length])).toEqual(before);

      // A decision that comes later: too late.
      const created = await owner.apiKeys.create(run.organization_id, {
        name: "Late approver",
        environment: workspace.environment,
      });
      const keyed = createClient(createHttpTransport({ baseUrl: API, getToken: async () => created.secret }));
      expect(
        await failure(keyed.runs.resolveApproval(workspace.id, run.id, { call_id: callId, decision: "approved" })),
      ).toMatchObject({ status: 409, code: "not_waiting" });
    });
  }

  it("never asks about a call whose installation went before the run got to it", async () => {
    const { advanceRun } = await import("./runner");
    const { run, callId, installationId } = waiting();
    const step = must(
      run.plan.find((s) => s.kind === "tool_call" && s.call.call_id === callId),
      "the pending call's step",
    );
    if (step.kind !== "tool_call") throw new Error("Not a call");
    run.status = "running";
    run.plan.unshift({ kind: "approval", call: step.call });
    const d = db();
    const gone = must(
      d.installations.find(
        (i) =>
          i.organization_id === run.organization_id && i.workspace_id === run.workspace_id && i.id === installationId,
      ),
      "the installation",
    );
    // The same ID, connected, in a sibling workspace and in another
    // organization's workspace of the same ID: neither is the run's.
    const sibling = must(
      d.workspaces.find((w) => w.organization_id === run.organization_id && w.id !== run.workspace_id),
      "sibling workspace",
    );
    const larkspur = must(
      d.organizations.find((o) => o.slug === "larkspur"),
      "Larkspur",
    ).id;
    d.installations.push(
      { ...structuredClone(gone), workspace_id: sibling.id, status: "connected" },
      { ...structuredClone(gone), organization_id: larkspur, status: "connected" },
    );
    d.installations = d.installations.filter(
      (i) =>
        !(i.organization_id === run.organization_id && i.workspace_id === run.workspace_id && i.id === installationId),
    );
    const asked = timelineOf(d, run).filter((e) => e.type === "approval.requested").length;
    expect(advanceRun(d, run, new Date().toISOString(), () => 0.5)).toBe("continue");
    expect(run.status).toBe("running");
    expect(timelineOf(d, run).filter((e) => e.type === "approval.requested")).toHaveLength(asked);
    const last = must(timelineOf(d, run).at(-1), "event");
    expect(last.type === "tool_call.failed" ? last.data.error.code : null).toBe("installation_removed");
  });
});

describe("approvals in the audit log", () => {
  it("names the API key that recorded the decision", async () => {
    const run = must(
      db().runs.find((r) => r.status === "waiting_for_approval"),
      "waiting run",
    );
    const { resolveApproval } = await import("./runner");
    const requested = [...timelineOf(db(), run)].reverse().find((e) => e.type === "approval.requested");
    const callId = must(requested?.type === "approval.requested" ? requested.data.call_id : null, "pending call");
    const keyId = approvalKeyFor(run.workspace_id);
    const key = must(
      db().api_keys.find((k) => k.id === keyId),
      "key",
    );
    resolveApproval(db(), run, callId, "approved", keyId, new Date().toISOString());
    const agent = must(
      db().agents.find((a) => a.organization_id === run.organization_id && a.id === run.agent_id),
      "agent",
    );
    expect(must(db().audit[0], "event")).toMatchObject({
      organization_id: run.organization_id,
      workspace_id: run.workspace_id,
      actor: { type: "api_key", id: key.id, name: key.name },
      action: "approval.resolved",
      target: { type: "run", id: run.id, name: agent.name },
    });
  });

  it("does not borrow another organization's workspace environment", async () => {
    const run = must(
      db().runs.find((r) => r.status === "waiting_for_approval"),
      "waiting run",
    );
    const foreign = must(
      db().workspaces.find((w) => w.organization_id !== run.organization_id),
      "foreign workspace",
    );
    run.workspace_id = foreign.id;
    const { resolveApproval } = await import("./runner");
    resolveApproval(db(), run, newId("tool_call"), "approved", newId("api_key"), new Date().toISOString());
    expect(must(db().audit[0], "event")).toMatchObject({
      organization_id: run.organization_id,
      workspace_id: foreign.id,
      environment: null,
    });
  });

  it("does not name an API key from another organization", async () => {
    const run = must(
      db().runs.find((r) => r.status === "waiting_for_approval"),
      "waiting run",
    );
    const foreign = must(
      db().api_keys.find((k) => k.organization_id !== run.organization_id),
      "foreign key",
    );
    expect(foreign.name).not.toBe("API key");
    const { resolveApproval } = await import("./runner");
    resolveApproval(db(), run, newId("tool_call"), "approved", foreign.id, new Date().toISOString());
    expect(must(db().audit[0], "event").actor).toEqual({ type: "api_key", id: foreign.id, name: "API key" });
  });
});

describe("run audit targets", () => {
  it("does not name an agent from a sibling workspace", async () => {
    const run = must(db().runs[0], "run");
    const sibling = must(
      db().agents.find((a) => a.organization_id === run.organization_id && a.workspace_id !== run.workspace_id),
      "sibling agent",
    );
    expect(sibling.name).not.toBe("Agent");
    run.agent_id = sibling.id;
    const { runTarget } = await import("./runner");
    expect(runTarget(db(), run)).toEqual({ type: "run", id: run.id, name: "Agent" });
  });

  it("does not name an agent from another organization when its workspace matches", async () => {
    const run = must(db().runs[0], "run");
    const foreign = must(
      db().agents.find((a) => a.organization_id !== run.organization_id),
      "foreign agent",
    );
    expect(foreign.name).not.toBe("Agent");
    run.agent_id = foreign.id;
    run.workspace_id = foreign.workspace_id;
    const { runTarget } = await import("./runner");
    expect(runTarget(db(), run)).toEqual({ type: "run", id: run.id, name: "Agent" });
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

  it("keeps each pending fixture call distinct from every other call", () => {
    const database = db();
    const pending = database.runs.filter((run) => run.status === "waiting_for_approval");
    const environments = pending.map(
      (run) =>
        must(
          database.workspaces.find(
            (workspace) => workspace.organization_id === run.organization_id && workspace.id === run.workspace_id,
          ),
        ).environment,
    );
    expect(new Set(environments)).toEqual(new Set(["sandbox", "production"]));
    const callIds = database.runs.flatMap((run) => [
      ...timelineOf(database, run).flatMap((event) => (event.type === "tool_call.started" ? [event.data.call_id] : [])),
      ...run.plan.flatMap((step) => (step.kind === "tool_call" ? [step.call.call_id] : [])),
    ]);
    expect(new Set(callIds).size).toBe(callIds.length);
    for (const run of pending) {
      const requested = must(timelineOf(database, run).find((event) => event.type === "approval.requested"));
      if (requested.type !== "approval.requested") throw new Error("Expected a pending approval");
      expect(
        run.plan.filter((step) => step.kind === "tool_call" && step.call.call_id === requested.data.call_id),
      ).toHaveLength(1);
    }
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
      ...structuredClone(app("echo_server")),
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
      app_id: app("echo_server").id,
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

  it("shows a link's session only while pending, or ended within its grace", async () => {
    const start = (label: string) =>
      owner.connectSessions.create(ws("qa-playground").id, {
        app_id: app("echo_server").id,
        intent: "create",
        label,
        installation_id: null,
        return_url: null,
      });
    const unknown = await failure(owner.connect.get("A".repeat(32)));
    expect(unknown.status).toBe(404);
    const ago = (ms: number) => new Date(Date.now() - ms).toISOString();

    // Expired unused: nothing to show, exactly as no session.
    const expired = await start("Echo 8");
    must(db().connect_sessions.find((x) => x.id === expired.id)).expires_at = ago(1000);
    const expiredError = await failure(owner.connect.get(referenceOf(expired)));
    expect([expiredError.status, expiredError.code, expiredError.message]).toEqual([
      unknown.status,
      unknown.code,
      unknown.message,
    ]);

    // Ended: its outcome until the grace past its expiry is over.
    const ended = await start("Echo 9");
    await owner.connect.cancel(referenceOf(ended));
    const row = must(db().connect_sessions.find((x) => x.id === ended.id));
    row.expires_at = ago(OUTCOME_GRACE_MS - 60_000);
    expect((await owner.connect.get(referenceOf(ended))).status).toBe("cancelled");
    row.expires_at = ago(OUTCOME_GRACE_MS + 1000);
    expect((await failure(owner.connect.get(referenceOf(ended)))).code).toBe(unknown.code);
  });

  it("stores only a hash of the Connect reference", async () => {
    const session = await owner.connectSessions.create(ws("qa-playground").id, {
      app_id: app("echo_server").id,
      intent: "create",
      label: "Echo 6",
      installation_id: null,
      return_url: null,
    });
    expect(JSON.stringify(db())).not.toContain(referenceOf(session));
  });
});

describe("installation short IDs", () => {
  /** Draws exactly these characters from the short ID alphabet, in order. */
  const drawing = (...shortIds: string[]) => {
    const alphabet = "abcdefghijklmnopqrstuvwxyz234567";
    const draws = shortIds.join("").split("");
    return () => (alphabet.indexOf(must(draws.shift(), "a draw")) + 0.5) / alphabet.length;
  };

  it("never issues one twice in a workspace, even after its installation is deleted", async () => {
    const qa = ws("qa-playground");
    const session = await owner.connectSessions.create(qa.id, {
      app_id: app("echo_server").id,
      intent: "create",
      label: "Short",
      installation_id: null,
      return_url: null,
    });
    const { installation } = await owner.connect.submitCredentials(referenceOf(session), {
      values: { token: "short" },
    });
    await owner.installations.delete(qa.id, installation.id);
    expect(db().installations.some((i) => i.id === installation.id)).toBe(false);
    const scope = { organization_id: qa.organization_id, workspace_id: qa.id };
    expect(db().installation_short_ids).toContainEqual({ ...scope, short_id: installation.short_id });

    const next = installation.short_id === "zzzzzz" ? "yyyyyy" : "zzzzzz";
    expect(issueShortId(db().installation_short_ids, scope, drawing(installation.short_id, next))).toBe(next);
    // Drawing only taken ones gives up rather than reuse one.
    const before = db().installation_short_ids.length;
    expect(() =>
      issueShortId(db().installation_short_ids, scope, drawing(...Array(SHORT_ID_DRAWS).fill(installation.short_id))),
    ).toThrow();
    expect(db().installation_short_ids).toHaveLength(before);
    // Another workspace, of this organization or another, has its own short IDs.
    const harbor = ws("harbor-sandbox");
    for (const other of [ws("acme-sandbox"), harbor]) {
      const otherScope = { organization_id: other.organization_id, workspace_id: other.id };
      expect(issueShortId(db().installation_short_ids, otherScope, drawing(installation.short_id))).toBe(
        installation.short_id,
      );
    }
  });
});

describe("installation short IDs across scopes", () => {
  const drawing = (...shortIds: string[]) => {
    const alphabet = "abcdefghijklmnopqrstuvwxyz234567";
    const draws = shortIds.join("").split("");
    return () => (alphabet.indexOf(must(draws.shift(), "a draw")) + 0.5) / alphabet.length;
  };

  it("compares both the organization and the workspace", () => {
    const qa = ws("qa-playground");
    const larkspur = must(db().organizations.find((o) => o.slug === "larkspur")).id;
    // A row naming this workspace's ID under another organization is not this workspace's.
    db().installation_short_ids.push({ organization_id: larkspur, workspace_id: qa.id, short_id: "stray2" });
    const scope = { organization_id: qa.organization_id, workspace_id: qa.id };
    expect(issueShortId(db().installation_short_ids, scope, drawing("stray2"))).toBe("stray2");
    // A row naming this organization under another workspace is not this workspace's either.
    const sibling = ws("acme-sandbox");
    db().installation_short_ids.push({
      organization_id: qa.organization_id,
      workspace_id: sibling.id,
      short_id: "other2",
    });
    expect(issueShortId(db().installation_short_ids, scope, drawing("other2"))).toBe("other2");
  });

  it("deletes a workspace's short IDs with it, and only its own", async () => {
    const doomed = ws("acme-sandbox");
    const larkspur = must(db().organizations.find((o) => o.slug === "larkspur")).id;
    const stray = { organization_id: larkspur, workspace_id: doomed.id, short_id: "stray2" };
    db().installation_short_ids.push(stray);
    const own = db().installation_short_ids.filter((r) => inWorkspace(r, doomed));
    expect(own.length).toBeGreaterThan(0);
    const others = db().installation_short_ids.filter((r) => !inWorkspace(r, doomed));
    await owner.workspaces.delete(doomed.id);
    expect(db().installation_short_ids.filter((r) => inWorkspace(r, doomed))).toEqual([]);
    expect(db().installation_short_ids).toEqual(others);
    expect(db().installation_short_ids).toContainEqual(stray);
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
      mine(d.webhook_events),
      mine(d.webhook_deliveries),
      mine(d.webhook_delivery_attempts),
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
      invited_by: null,
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
    const google = must(db().apps.find((a) => a.organization_id === org && a.key === "google_workspace"));
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
      app_id: app("echo_server").id,
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
      instructions: "",
    });
    const tool = must(app("echo_server").tools[0]).name;
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
      key: "order_check",
      description: "",
      auth_type: "token",
    });
    expect(db().configurations.some((c) => c.app_id === catalogApp.id)).toBe(true);
    const endpoint = must(
      db().webhooks.find((row) => row.organization_id === qa.organization_id && row.environment === qa.environment),
    );
    await owner.webhooks.sendTest(qa.organization_id, endpoint.id);
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
    for (const [name, rows] of Object.entries({
      audit: d.audit,
      webhook_events: d.webhook_events,
      webhook_deliveries: d.webhook_deliveries,
    })) {
      for (const row of rows) expect(Object.keys(row).slice(0, 2), name).toEqual(["id", "organization_id"]);
    }
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
    // The run's model was fixed when it was created, never looked up through the mislabelled agent.
    expect(started?.type === "run.started" ? started.data.model_id : null).toBe(run.model.model_id);
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
      db().apps.find((a) => a.organization_id === sandbox.organization_id && a.key === "google_workspace"),
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
    const google = must(db().apps.find((a) => a.organization_id === org && a.key === "google_workspace"));
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
    const echo = app("echo_server");
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
    expect((await owner.connect.get(second)).app?.name).not.toBe(echo.name);
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
    // One key space for the organization (decision 0047): another workspace
    // and the catalog can't reuse a workspace app's key.
    for (const attempt of [
      () =>
        owner.workspaceApps.create(ws("beta-labs").id, {
          kind: "mcp",
          name: "Publishing",
          key: publishing.key,
          description: "",
          auth_type: "token",
        }),
      () =>
        owner.organizationApps.create(org, {
          kind: "mcp",
          name: "Publishing",
          key: publishing.key,
          description: "",
          auth_type: "token",
        }),
    ]) {
      expect((await failure(attempt())).code).toBe("app_key_taken");
    }
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

  it("records a workspace app's changes in its workspace", async () => {
    const acme = ws("acme");
    const created = await owner.workspaceApps.create(acme.id, {
      kind: "mcp",
      name: "Ledger",
      key: "ledger",
      description: "",
      auth_type: "token",
    });
    await owner.workspaceApps.update(acme.id, created.id, { name: "Ledger 2" });
    const events = db()
      .audit.filter((e) => e.organization_id === acme.organization_id)
      .slice(0, 2)
      .map((e) => [e.action, e.workspace_id, e.target.name]);
    expect(events).toEqual([
      ["app.updated", acme.id, "Ledger 2"],
      ["app.created", acme.id, "Ledger"],
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

  it("expires an invitation exactly INVITATION_TTL_DAYS after it was sent", async () => {
    const org = must(northwindId());
    const invited = must(db().memberships.find((m) => m.organization_id === org && m.status === "invited"));
    const invitee = clientFor(invited.user_id, northwindId);
    const ttl = INVITATION_TTL_DAYS * 86_400_000;
    invited.invited_at = new Date(Date.now() - ttl + 60_000).toISOString();
    expect((await invitee.invitations.list()).find((i) => i.id === invited.id)?.status).toBe("pending");
    invited.invited_at = new Date(Date.now() - ttl).toISOString();
    expect((await invitee.invitations.list()).find((i) => i.id === invited.id)?.status).toBe("expired");
    expect((await owner.members.list(org)).find((m) => m.id === invited.id)?.expires_at).toBe(
      new Date(Date.parse(invited.invited_at) + ttl).toISOString(),
    );
    expect((await failure(invitee.invitations.accept(invited.id))).code).toBe("invitation_expired");
    expect(must(db().memberships.find((m) => m.id === invited.id)).status).toBe("invited");
  });

  it("names the inviter on an invitation, and withdraws it once its grantor is unknown", async () => {
    const fernhill = must(db().organizations.find((o) => o.slug === "fernhill"));
    const [invitation] = (await owner.invitations.list()).filter((i) => i.organization.id === fernhill.id);
    expect(invitation?.invited_by).toEqual({ name: "Casey Morgan", email: "casey@fernhill.example" });
    const row = must(db().memberships.find((m) => m.id === invitation?.id));
    // A grantor no longer known, or none recorded, grants nothing: the invitation is gone.
    for (const grantor of [newId("user"), null]) {
      row.invited_by = grantor;
      expect((await owner.invitations.list()).some((i) => i.id === row.id)).toBe(false);
      expect((await failure(owner.invitations.accept(row.id))).status).toBe(404);
      expect((await failure(owner.invitations.decline(row.id))).status).toBe(404);
    }
    // Not even for an owner invitation.
    row.role = "owner";
    expect((await failure(owner.invitations.accept(row.id))).status).toBe(404);
    expect(must(db().memberships.find((m) => m.id === row.id)).status).toBe("invited");
    // An invitation sent through the API records who sent it.
    const sent = await owner.members.invite(must(northwindId()), { email: "new@northwind.example", role: "viewer" });
    expect(must(db().memberships.find((m) => m.id === sent.id)).invited_by).toBe(DEMO_USER_ID);
  });

  it("checks an invitation's grantor in the invitation's own organization", () => {
    const org = must(northwindId());
    const other = must(db().organizations.find((o) => o.slug === "larkspur"));
    const grantor = must(db().memberships.find((m) => m.organization_id === org && m.user_id === DEMO_USER_ID));
    expect([grantor.role, grantor.status]).toEqual(["owner", "active"]);
    const invited = must(db().memberships.find((m) => m.organization_id === org && m.status === "invited"));
    const local = {
      ...structuredClone(grantor),
      id: newId("member"),
      organization_id: other.id,
      role: "viewer" as const,
    };
    const invitation = {
      ...structuredClone(invited),
      id: newId("member"),
      organization_id: other.id,
      role: "developer" as const,
      invited_by: grantor.user_id,
    };
    // The same person is an owner elsewhere, but cannot grant this invitation.
    db().memberships = [grantor, local, invitation];
    expect(grantorAllows(invitation)).toBe(false);
  });

  it.each(["viewer", null] as const)(
    "withdraws invitations only in the affected organization when the grantor's role becomes %s",
    (role) => {
      const org = must(northwindId());
      const other = must(db().organizations.find((o) => o.slug === "larkspur"));
      const invited = must(db().memberships.find((m) => m.organization_id === org && m.status === "invited"));
      const local: MembershipRow = { ...structuredClone(invited), id: newId("member"), invited_by: DEMO_USER_ID };
      const foreign = { ...structuredClone(local), id: newId("member"), organization_id: other.id };
      db().memberships = [local, foreign];
      withdrawUngrantable(org, DEMO_USER_ID, role);
      expect(db().memberships).toEqual([foreign]);
    },
  );

  it("withdraws what a member granted beyond what they can still grant", async () => {
    const org = must(northwindId());
    // A second owner, so there is one to demote and remove.
    const second = signUp({ name: "Second Owner", email: "second.owner@northwind.example" });
    db().memberships.push({
      id: newId("member"),
      organization_id: org,
      user_id: second,
      role: "owner",
      status: "active",
      invited_at: new Date().toISOString(),
      invited_by: DEMO_USER_ID,
      joined_at: new Date().toISOString(),
      last_active_at: null,
    });
    const secondRow = must(db().memberships.find((m) => m.organization_id === org && m.user_id === second));
    const asSecond = clientFor(second, northwindId);
    const ownerInvite = await asSecond.members.invite(org, { email: "o@northwind.example", role: "owner" });
    const adminInvite = await asSecond.members.invite(org, { email: "a@northwind.example", role: "admin" });
    const viewerInvite = await asSecond.members.invite(org, { email: "v@northwind.example", role: "viewer" });
    const byJordan = await owner.members.invite(org, { email: "j@northwind.example", role: "owner" });
    const pending = async () => (await owner.members.list(org)).filter((m) => m.status === "invited").map((m) => m.id);

    // Demoted to admin: they can't grant owner any more, everything else stands.
    await owner.members.update(org, secondRow.id, { role: "admin" });
    expect(await pending()).not.toContain(ownerInvite.id);
    expect(await pending()).toEqual(expect.arrayContaining([adminInvite.id, viewerInvite.id, byJordan.id]));
    // Changing an invitation's role makes whoever changed it its grantor.
    await owner.members.update(org, viewerInvite.id, { role: "developer" });
    expect(must(db().memberships.find((m) => m.id === viewerInvite.id)).invited_by).toBe(DEMO_USER_ID);
    // Removed: what they still granted goes with them; others' invitations stay.
    await owner.members.remove(org, secondRow.id);
    expect(await pending()).not.toContain(adminInvite.id);
    expect(await pending()).toEqual(expect.arrayContaining([viewerInvite.id, byJordan.id]));

    // Demoted to developer, an admin can grant nothing.
    const admin = must(
      db().memberships.find((m) => m.organization_id === org && m.role === "admin" && m.status === "active"),
    );
    const fromAdmin = await clientFor(admin.user_id, northwindId).members.invite(org, {
      email: "d@northwind.example",
      role: "developer",
    });
    await owner.members.update(org, admin.id, { role: "developer" });
    expect(await pending()).not.toContain(fromAdmin.id);
  });

  it("withdraws only an invitation still pending when asked to cancel one", async () => {
    const org = must(northwindId());
    const active = must(
      db().memberships.find((m) => m.organization_id === org && m.role === "developer" && m.status === "active"),
    );
    expect((await failure(owner.members.remove(org, active.id, { status: "invited" }))).code).toBe(
      "invitation_accepted",
    );
    expect(db().memberships.some((m) => m.id === active.id)).toBe(true);
    expect((await failure(owner.members.remove(org, active.id, { status: "member" as "invited" }))).code).toBe(
      "validation_failed",
    );
    const invited = must(db().memberships.find((m) => m.organization_id === org && m.status === "invited"));
    await owner.members.remove(org, invited.id, { status: "invited" });
    expect(db().memberships.some((m) => m.id === invited.id)).toBe(false);
  });

  it("renews an expired invitation when the address is invited again", async () => {
    const org = must(northwindId());
    const invited = must(db().memberships.find((m) => m.organization_id === org && m.status === "invited"));
    const email = must(db().users.find((u) => u.id === invited.user_id)).email;
    // Pending: inviting again is refused.
    expect((await failure(owner.members.invite(org, { email, role: "viewer" }))).code).toBe("already_member");
    invited.invited_at = new Date(Date.now() - (INVITATION_TTL_DAYS + 1) * 86_400_000).toISOString();
    invited.invited_by = newId("user");
    const renewed = await owner.members.invite(org, { email, role: "viewer" });
    expect(renewed).toMatchObject({ id: invited.id, role: "viewer", status: "invited" });
    expect(Date.parse(renewed.invited_at)).toBeGreaterThan(Date.now() - 60_000);
    expect(invited.invited_by).toBe(DEMO_USER_ID);
    const invitee = clientFor(invited.user_id, northwindId);
    expect((await invitee.invitations.list()).find((i) => i.id === invited.id)?.status).toBe("pending");
  });

  it("lists at most INVITATION_LIST_LIMIT invitations, none long expired", async () => {
    const casey = must(db().users.find((u) => u.email === "casey@fernhill.example"));
    const day = 86_400_000;
    const now = Date.now();
    const invite = (sentAgo: number) => {
      const template = must(db().organizations.find((o) => o.slug === "fernhill"));
      const org = { ...structuredClone(template), id: newId("organization"), slug: `org-${db().organizations.length}` };
      db().organizations.push(org);
      const sentAt = new Date(now - sentAgo).toISOString();
      const grantor = { ...structuredClone(must(db().memberships[0])), id: newId("member") };
      db().memberships.push(
        { ...grantor, organization_id: org.id, user_id: casey.id, role: "owner", status: "active", invited_by: null },
        {
          ...grantor,
          id: newId("member"),
          organization_id: org.id,
          user_id: DEMO_USER_ID,
          role: "developer",
          status: "invited",
          invited_at: sentAt,
          invited_by: casey.id,
          joined_at: null,
        },
      );
      return must(db().memberships.at(-1)).id;
    };
    const shownFor = (INVITATION_TTL_DAYS + EXPIRED_INVITATION_SHOWN_DAYS) * day;
    const stillShown = invite(shownFor - 60_000);
    const longGone = invite(shownFor + 60_000);
    let listed = (await owner.invitations.list()).map((i) => i.id);
    expect(listed).toContain(stillShown);
    expect(listed).not.toContain(longGone);
    const newest = Array.from({ length: INVITATION_LIST_LIMIT }, (_, i) => invite(i * 1000));
    listed = (await owner.invitations.list()).map((i) => i.id);
    expect(listed).toEqual(newest);
  });

  it("joins once when two accepts race, and audits the answer", async () => {
    const fernhill = must(db().organizations.find((o) => o.slug === "fernhill"));
    const invitation = must((await owner.invitations.list()).find((i) => i.organization.id === fernhill.id));
    const outcomes = await Promise.allSettled([
      owner.invitations.accept(invitation.id, { idempotencyKey: "race-1" }),
      owner.invitations.accept(invitation.id, { idempotencyKey: "race-2" }),
    ]);
    expect(outcomes.filter((o) => o.status === "fulfilled")).toHaveLength(1);
    const refused = outcomes.find((o) => o.status === "rejected");
    expect((refused as PromiseRejectedResult | undefined)?.reason).toMatchObject({ code: "invitation_accepted" });
    expect(
      db().memberships.filter((m) => m.organization_id === fernhill.id && m.user_id === DEMO_USER_ID),
    ).toMatchObject([{ status: "active", role: "developer" }]);
    expect((await owner.me.get()).organizations.map((o) => o.slug)).toContain("fernhill");
    expect(db().audit.find((e) => e.organization_id === fernhill.id)).toMatchObject({
      action: "member.joined",
      actor: { id: DEMO_USER_ID },
    });
  });

  it("joins nobody by signing up with an invited address", async () => {
    const pending = db().memberships.filter((m) => m.status === "invited");
    const invitee = must(db().users.find((u) => u.id === pending[0]?.user_id));
    // The same person, whatever the address's case: their invitations wait for them.
    expect(signUp({ name: "Someone", email: invitee.email.toUpperCase() })).toBe(invitee.id);
    expect(db().memberships.filter((m) => m.status === "invited")).toEqual(pending);
    const client = clientFor(invitee.id);
    expect((await client.me.get()).organizations).toEqual([]);
    expect((await client.invitations.list()).length).toBeGreaterThan(0);
    // A new address is a new person, with no invitations.
    const fresh = signUp({ name: "New Person", email: "new.person@example.com" });
    expect(await clientFor(fresh).invitations.list()).toEqual([]);
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
      app_id: app("echo_server").id,
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
    const definitions = [...app("echo_server").tools].sort((a, b) => (a.name < b.name ? -1 : 1));
    expect(tools.map((t) => [t.name, t.effect, t.status, t.pinned.title, t.pinned.description])).toEqual(
      definitions.map((d) => [d.name, d.effect, "current", d.title, d.description]),
    );
    expect(tools.every((t) => t.latest === null && t.first_seen_at === t.seen_at)).toBe(true);
  });

  it("keeps the pinned definition on refresh: a changed one waits beside it, a missing one is gone", async () => {
    const { qa, installation } = await connectEcho();
    const before = await allTools(qa.id, installation.id);
    const echoApp = app("echo_server");
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
    const agent = await owner.agents.create(qa.id, {
      name: "Echo probe",
      description: "",
      instructions: "",
    });
    const grant = await owner.grants.create(qa.id, agent.id, {
      installation_id: installation.id,
      tools: ["echo", "echo_write"],
      approval_required: ["echo_write"],
    });
    // The server stops listing echo_write.
    const echoApp = app("echo_server");
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
      instructions: "",
    });
    // The app defines a tool its server hasn't listed to this account yet: it isn't grantable.
    app("echo_server").tools.push({
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
    expect(statuses).toEqual({
      create_task: "changed",
      delete_task: "changed",
      export_tasks: "gone",
      list_tasks: "current",
    });
    const changed = must(tools.find((t) => t.name === "create_task"));
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
    const deletion = must((await allTools(sandbox.id, installation.id)).find((t) => t.name === "delete_task"));
    expect(deletion.status).toBe("changed");
    expect(deletion.effect).toBe("destructive");
    expect([deletion.pinned.description, deletion.pinned.input_schema]).toEqual([
      deletion.latest?.description,
      deletion.latest?.input_schema,
    ]);
    expect(deletion.pinned.annotations).toEqual({ readOnlyHint: false });
    expect(deletion.latest?.annotations).toEqual({ readOnlyHint: false, destructiveHint: true });
    expect(deletion.latest?.output_schema).toBeNull();
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

describe("tool policy", () => {
  const northwind = () => must(northwindId());
  const larkspur = () => must(db().organizations.find((o) => o.slug === "larkspur")?.id);
  const sandboxTaskboard = () =>
    must(
      db().installations.find(
        (i) => i.workspace_id === ws("acme-sandbox").id && i.app_id === app("taskboard").id && i.status === "connected",
      ),
    );
  const probe = async (workspaceId: WorkspaceId) =>
    owner.agents.create(workspaceId, {
      name: "Policy probe",
      description: "",
      instructions: "",
    });

  it("suggests a level for each tool a catalog app's installation lists first, and never changes one", async () => {
    const taskboard = app("taskboard");
    const sandbox = ws("acme-sandbox");
    taskboard.tools.push(
      { name: "list_boards", title: "List boards", description: "List the available boards.", effect: "read" },
      {
        name: "clear_board",
        title: "Clear board",
        description: "Delete every task from a board.",
        effect: "destructive",
      },
    );
    // A person already set create_task; the listing describes it as destructive now.
    const createTask = must(taskboard.tools.find((t) => t.name === "create_task"));
    createTask.effect = "destructive";
    await owner.installations.refreshTools(sandbox.id, sandboxTaskboard().id);
    const levels = (await owner.toolPolicy.levels(northwind(), taskboard.id, { limit: 100 })).items;
    const of = (tool: string) => {
      const level = must(levels.find((l) => l.tool === tool));
      return [level.level, level.confirmed, level.destructive, level.updated_by];
    };
    expect(of("list_boards")).toEqual(["allow", false, false, null]);
    expect(of("clear_board")).toEqual(["approve", false, true, null]);
    expect(of("create_task").slice(0, 2)).toEqual(["allow", true]);
    // Larkspur's levels for its own Taskboard are its own.
    expect(db().app_tool_levels.some((l) => l.organization_id === larkspur() && l.tool === "list_boards")).toBe(false);
  });

  it("suggests no level for a workspace app's tools", async () => {
    const before = db().app_tool_levels.length;
    const qa = ws("qa-playground");
    const echo = must(db().installations.find((i) => i.workspace_id === qa.id && i.status === "connected"));
    app("echo_server").tools.push({ name: "echo_twice", title: "Twice", description: "", effect: "read" });
    await owner.installations.refreshTools(qa.id, echo.id);
    expect(db().app_tool_levels.length).toBe(before);
    expect(db().app_tool_levels.some((l) => l.app_id === app("echo_server").id)).toBe(false);
  });

  it("seeds levels that hold every grant in acme", () => {
    const d = db();
    const acme = ws("acme");
    for (const grant of d.grants.filter((g) => g.workspace_id === acme.id)) {
      const installation = must(d.installations.find((i) => i.id === grant.installation_id));
      const own = must(d.apps.find((a) => a.id === installation.app_id));
      if (own.owner_workspace_id !== null) continue;
      for (const tool of grant.tools) {
        const level = d.app_tool_levels.find(
          (l) => l.organization_id === own.organization_id && l.app_id === own.id && l.tool === tool,
        );
        expect(level?.confirmed, `${tool} is confirmed`).toBe(true);
        expect(level?.level, `${tool} isn't denied`).not.toBe("deny");
        if (level?.level === "approve") expect(grant.approval_required, tool).toContain(tool);
      }
    }
  });

  it("ignores a level labelled with another organization, in reads and grants", async () => {
    const taskboard = app("taskboard");
    // Larkspur's label on Northwind's app: it must decide nothing for Northwind.
    db().app_tool_levels.push({
      organization_id: larkspur(),
      app_id: taskboard.id,
      tool: "list_tasks",
      level: "deny",
      confirmed: true,
      destructive: false,
      updated_at: new Date().toISOString(),
      updated_by_user_id: null,
    });
    db().tool_policies.push({
      organization_id: larkspur(),
      mode: "manual",
      new_tool_level: "deny",
      updated_at: new Date().toISOString(),
      updated_by_user_id: null,
    });
    const levels = (await owner.toolPolicy.levels(northwind(), taskboard.id, { limit: 100 })).items;
    expect(levels.filter((l) => l.tool === "list_tasks").map((l) => l.level)).toEqual(["allow"]);
    expect((await owner.toolPolicy.get(northwind())).new_tool_level).toBe("approve");
    const sandbox = ws("acme-sandbox");
    const agent = await probe(sandbox.id);
    const grant = await owner.grants.create(sandbox.id, agent.id, {
      installation_id: sandboxTaskboard().id,
      tools: ["list_tasks"],
      approval_required: [],
    });
    expect(grant.tools).toEqual(["list_tasks"]);
  });

  it("accepts a change only of a tool in the installation's own scope", async () => {
    const sandbox = ws("acme-sandbox");
    const installation = sandboxTaskboard();
    const changed = must(
      db().installation_tools.find((t) => t.installation_id === installation.id && t.latest !== null),
    );
    // The same name labelled with another workspace isn't this installation's.
    changed.workspace_id = ws("acme").id;
    const tools = await owner.installations.tools(sandbox.id, installation.id, { limit: 100 });
    expect(tools.items.some((t) => t.name === changed.name)).toBe(false);
    const error = await failure(
      owner.installations.acceptToolChange(sandbox.id, installation.id, {
        name: changed.name,
        latest_hash: "0".repeat(64),
      }),
    );
    expect(`${error.status} ${error.code}`).toBe("404 not_found");
  });

  it("records who changed the policy and the levels in the audit log", async () => {
    await owner.toolPolicy.update(northwind(), { mode: "manual", new_tool_level: "deny" });
    await owner.toolPolicy.setLevels(northwind(), app("taskboard").id, {
      levels: [{ tool: "delete_task", level: "approve" }],
    });
    const actions = db()
      .audit.filter((e) => e.organization_id === northwind())
      .slice(0, 2)
      .map((e) => e.action);
    expect(actions).toEqual(["app.tool_levels_updated", "tool_policy.updated"]);
  });
});

describe("model providers", () => {
  const northwind = () => must(northwindId(), "Northwind");
  const larkspur = () => must(db().organizations.find((o) => o.slug === "larkspur")?.id, "Larkspur");
  const sandboxDefault = () =>
    must(db().default_models.find((d) => d.organization_id === northwind() && d.environment === "sandbox"));

  it("seeds every agent's model and default within its own organization and environment", () => {
    const d = db();
    for (const agent of d.agents) {
      if (!agent.model_id) continue;
      const workspace = must(
        d.workspaces.find((w) => w.organization_id === agent.organization_id && w.id === agent.workspace_id),
      );
      const model = must(
        d.models.find((m) => m.id === agent.model_id),
        `model of ${agent.name}`,
      );
      expect([model.organization_id, model.environment], agent.name).toEqual([
        agent.organization_id,
        workspace.environment,
      ]);
    }
    for (const row of d.default_models) {
      if (!row.model_id) continue;
      const model = must(d.models.find((m) => m.id === row.model_id));
      expect([model.organization_id, model.environment]).toEqual([row.organization_id, row.environment]);
    }
    for (const model of d.models) {
      const connection = must(d.model_connections.find((c) => c.id === model.connection_id));
      expect([connection.organization_id, connection.environment]).toEqual([model.organization_id, model.environment]);
    }
    // Both environments of the demo organization have a default; its runs started on one.
    expect(d.default_models.filter((r) => r.organization_id === northwind() && r.model_id)).toHaveLength(2);
  });

  it("refuses a key replacement another one overtook, keeping the newer key", async () => {
    const connection = await owner.modelConnections.create(
      northwind(),
      { environment: "sandbox", kind: "openai", label: "Raced", api_key: "sk-mock-raced-original-0001" },
      { idempotencyKey: "raced-connection" },
    );
    // The first replacement's body arrives only after a second one saved.
    let send = (_: string) => {};
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        send = (text) => {
          controller.enqueue(new TextEncoder().encode(text));
          controller.close();
        };
      },
    });
    // Straight to the handlers: a request with a streamed body, sent through
    // fetch, isn't intercepted.
    const request = new Request(`${API}/v1/organizations/${northwind()}/model-connections/${connection.id}/api-key`, {
      method: "PUT",
      headers: { Authorization: `Bearer mock_${DEMO_USER_ID}`, "Content-Type": "application/json" },
      body: stream,
      duplex: "half",
    } as RequestInit);
    const slow = (async () => {
      for (const handler of createHandlers(API)) {
        const result = await handler.run({ request, requestId: "raced" });
        if (result?.response) return result.response;
      }
      throw new Error("no handler answered");
    })();
    await new Promise((resolve) => setTimeout(resolve, 20));
    const fast = await owner.modelConnections.replaceKey(northwind(), connection.id, {
      api_key: "sk-mock-raced-faster-0002",
    });
    expect(fast.key_version).toBe(2);
    send(JSON.stringify({ api_key: "sk-mock-raced-slower-0003" }));
    const answer = await slow;
    const refused = await answer.json();
    expect([answer.status, refused.error.code]).toEqual([409, "key_changed"]);
    const row = must(db().model_connections.find((c) => c.organization_id === northwind() && c.id === connection.id));
    expect([row.key_version, row.key_last4]).toEqual([2, "0002"]);
  });

  it("never keeps a provider key: not in the database, storage or idempotency records", async () => {
    const secret = "sk-mock-never-kept-Zq81";
    const replacement = "sk-mock-never-kept-Yw22";
    const connection = await owner.modelConnections.create(
      northwind(),
      { environment: "sandbox", kind: "openai", label: "Kept nowhere", api_key: secret },
      { idempotencyKey: "model-key-storage" },
    );
    await owner.modelConnections.replaceKey(northwind(), connection.id, { api_key: replacement });
    for (const value of [secret, replacement]) {
      expect(JSON.stringify(db()).includes(value)).toBe(false);
      expect(localStorage.getItem("zupflow.mock.db")?.includes(value)).toBe(false);
    }
    const row = must(db().model_connections.find((c) => c.organization_id === northwind() && c.id === connection.id));
    expect([row.key_last4, row.key_version]).toEqual(["Yw22", 2]);
  });

  it("never falls back to the other environment's default", async () => {
    const qa = ws("qa-playground");
    const agent = await owner.agents.create(qa.id, { name: "No fallback", description: "", instructions: "" });
    // Production has a default; Sandbox loses its own.
    sandboxDefault().model_id = null;
    expect(
      db().default_models.some(
        (d) => d.organization_id === northwind() && d.environment === "production" && d.model_id,
      ),
    ).toBe(true);
    const refused = await failure(owner.runs.create(qa.id, agent.id, { input: "hi" }));
    expect([refused.status, refused.code]).toEqual([422, "model_not_configured"]);
  });

  it("finds a model only under its own organization, whatever rows share its ID", async () => {
    const qa = ws("qa-playground");
    const model = must(db().models.find((m) => m.organization_id === northwind() && m.environment === "sandbox"));
    // A row with the same ID, mislabelled into another organization, ahead of the real one.
    db().models.unshift({ ...structuredClone(model), organization_id: larkspur(), name: "Poisoned organization" });
    expect((await owner.models.get(northwind(), model.id)).name).toBe(model.name);
    const agent = await owner.agents.create(qa.id, {
      name: "Scoped",
      description: "",
      instructions: "",
      model_id: model.id,
    });
    expect(agent.model_id).toBe(model.id);
    const names = (await owner.models.list(northwind(), { environment: "sandbox", limit: 100 })).items.map(
      (m) => m.name,
    );
    expect(names).toContain(model.name);
    expect(names.filter((name) => name.startsWith("Poisoned"))).toEqual([]);
    expect((await owner.models.getDefault(northwind(), "sandbox")).model?.name).not.toMatch(/^Poisoned/);
  });

  it("counts what a run's provider read from and wrote to its prompt cache, and sums it into usage", async () => {
    const { runRoute } = await import("./model-scope");
    const finished = db().runs.filter((r) => r.organization_id === northwind() && r.status === "completed");
    const multiTurn = finished.filter((r) => r.usage.tool_calls > 0);
    expect(multiTurn.length).toBeGreaterThan(0);
    for (const run of finished) {
      const { input_tokens, cache_read_input_tokens, cache_write_input_tokens } = run.usage;
      expect(cache_read_input_tokens + cache_write_input_tokens).toBeLessThanOrEqual(input_tokens);
    }
    for (const run of multiTurn) {
      // Every turn after the first sends the one before it again: the provider serves that from its cache.
      expect(run.usage.cache_read_input_tokens).toBeGreaterThan(0);
      const kind = runRoute(db(), run)?.connection.kind;
      const claude =
        kind === "anthropic" || (kind === "openrouter" && run.model.provider_model_id.startsWith("anthropic/"));
      // OpenAI and OpenRouter's other models cache on their own and report no writes.
      if (!claude) expect(run.usage.cache_write_input_tokens).toBe(0);
    }
    for (const environment of ["sandbox", "production"] as const) {
      const report = await owner.organizations.usage(northwind(), environment, 90);
      const since = must(report.days[0]).date;
      const workspaces = new Set(
        db()
          .workspaces.filter((w) => w.organization_id === northwind() && w.environment === environment)
          .map((w) => w.id),
      );
      const counted = db().runs.filter(
        (r) =>
          r.organization_id === northwind() && workspaces.has(r.workspace_id) && r.created_at.slice(0, 10) >= since,
      );
      for (const field of [
        "tool_calls",
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_write_input_tokens",
      ] as const) {
        expect(report.totals[field], `${environment} ${field}`).toBe(counted.reduce((n, r) => n + r.usage[field], 0));
      }
    }
  });

  it("starts a run on its agent's model, and fails it when the provider refuses the connection's key", async () => {
    const { advanceRun } = await import("./runner");
    const qa = ws("qa-playground");
    const model = must(
      db().models.find(
        (m) =>
          m.organization_id === northwind() &&
          m.environment === "sandbox" &&
          m.provider_model_id === "claude-haiku-4-5",
      ),
    );
    const agent = await owner.agents.create(qa.id, {
      name: "Haiku",
      description: "",
      instructions: "",
      model_id: model.id,
    });
    const drive = (runId: string) => {
      const run = must(
        db().runs.find((r) => r.organization_id === northwind() && r.workspace_id === qa.id && r.id === runId),
      );
      for (let i = 0; i < 40 && !["completed", "failed", "cancelled"].includes(run.status); i++) {
        advanceRun(db(), run, new Date().toISOString(), () => 0.5);
      }
      return run;
    };
    const ran = drive((await owner.runs.create(qa.id, agent.id, { input: "hi" })).id);
    const started = timelineOf(db(), ran).find((e) => e.type === "run.started");
    expect(started?.type === "run.started" ? started.data.model : null).toBe("claude-haiku-4-5");
    const connection = must(
      db().model_connections.find((c) => c.organization_id === northwind() && c.id === model.connection_id),
    );
    connection.status = "invalid_key";
    const failed = drive((await owner.runs.create(qa.id, agent.id, { input: "hi" })).id);
    expect([failed.status, failed.error?.code]).toEqual(["failed", "model_key_invalid"]);
    // Replacing the key makes the connection usable again.
    const replaced = await owner.modelConnections.replaceKey(northwind(), connection.id, {
      api_key: "sk-mock-fresh-replacement-0000",
    });
    expect(replaced.status).toBe("active");
    const search = await failure(
      owner.modelConnections.providerModels(
        northwind(),
        must(db().model_connections.find((c) => c.organization_id === northwind() && c.status === "invalid_key")).id,
      ),
    );
    expect([search.status, search.code]).toEqual([422, "invalid_api_key"]);
  });

  it("keeps a queued run on the model it was created with, told apart from another route to the same model", async () => {
    const { advanceRun } = await import("./runner");
    const qa = ws("qa-playground");
    const first = must(
      db().models.find(
        (m) =>
          m.organization_id === northwind() && m.environment === "sandbox" && m.provider_model_id === "openai/gpt-5",
      ),
    );
    // A second OpenRouter connection serving the same provider model ID.
    const other = await owner.modelConnections.create(
      northwind(),
      { environment: "sandbox", kind: "openrouter", label: "Team OpenRouter", api_key: "sk-or-second-team-key-0001" },
      { idempotencyKey: "second-route" },
    );
    const second = await owner.models.create(
      northwind(),
      { connection_id: other.id, provider_model_id: "openai/gpt-5" },
      { idempotencyKey: "second-route-model" },
    );
    await owner.models.setDefault(northwind(), "sandbox", { model_id: first.id });
    const agent = await owner.agents.create(qa.id, { name: "Default user", description: "", instructions: "" });
    const queued = await owner.runs.create(qa.id, agent.id, { input: "hi" });
    // The default changes while the first run is still queued.
    await owner.models.setDefault(northwind(), "sandbox", { model_id: second.id });
    const later = await owner.runs.create(qa.id, agent.id, { input: "hi" });
    const drive = (runId: string) => {
      const run = must(
        db().runs.find((r) => r.organization_id === northwind() && r.workspace_id === qa.id && r.id === runId),
      );
      for (let i = 0; i < 40 && !["completed", "failed", "cancelled"].includes(run.status); i++) {
        advanceRun(db(), run, new Date().toISOString(), () => 0.5);
      }
      return run;
    };
    const startedOn = (runId: string) => {
      const run = drive(runId);
      const started = timelineOf(db(), run).find((e) => e.type === "run.started");
      return started?.type === "run.started" ? { ...started.data, key_version: run.model.key_version } : null;
    };
    expect(startedOn(queued.id)).toEqual({
      model: "openai/gpt-5",
      model_id: first.id,
      connection_id: first.connection_id,
      key_version: 1,
    });
    expect(startedOn(later.id)).toEqual({
      model: "openai/gpt-5",
      model_id: second.id,
      connection_id: other.id,
      key_version: 1,
    });
  });

  it("never starts a run as an unknown model: one whose model went before it started fails", async () => {
    const { advanceRun } = await import("./runner");
    const qa = ws("qa-playground");
    const agent = await owner.agents.create(qa.id, { name: "Orphan", description: "", instructions: "" });
    const queued = await owner.runs.create(qa.id, agent.id, { input: "hi" });
    const run = must(db().runs.find((r) => r.organization_id === northwind() && r.id === queued.id));
    // The model is removed underneath the queued run (the API refuses this while it's the default).
    db().models = db().models.filter((m) => !(m.organization_id === northwind() && m.id === run.model.model_id));
    for (let i = 0; i < 40 && !["completed", "failed", "cancelled"].includes(run.status); i++) {
      advanceRun(db(), run, new Date().toISOString(), () => 0.5);
    }
    expect([run.status, run.error?.code]).toEqual(["failed", "model_unavailable"]);
    const started = timelineOf(db(), run).find((e) => e.type === "run.started");
    expect(started?.type === "run.started" ? started.data.model : null).not.toBe("unknown");
  });

  it("refuses viewers and developers every change, and records each change in the audit log", async () => {
    const developer = clientFor(must(db().users.find((u) => u.email === "sam@northwind.example")).id);
    const viewer = clientFor(must(db().users.find((u) => u.email === "alex@northwind.example")).id);
    const input = {
      environment: "sandbox" as const,
      kind: "anthropic" as const,
      label: "Refused",
      api_key: "sk-test-long-mock-0000-xx",
    };
    for (const client of [developer, viewer]) {
      const error = await failure(client.modelConnections.create(northwind(), input));
      expect(`${error.status} ${error.code}`).toBe("403 forbidden");
    }
    const before = db().audit.length;
    const connection = await owner.modelConnections.create(northwind(), input);
    const model = await owner.models.create(northwind(), {
      connection_id: connection.id,
      provider_model_id: "claude-sonnet-4-5",
    });
    await owner.models.update(northwind(), model.id, { name: "Renamed" });
    await owner.models.delete(northwind(), model.id);
    await owner.modelConnections.delete(northwind(), connection.id);
    expect(
      db()
        .audit.slice(0, db().audit.length - before)
        .map((e) => [e.action, e.environment]),
    ).toEqual([
      ["model_connection.deleted", "sandbox"],
      ["model.deleted", "sandbox"],
      ["model.updated", "sandbox"],
      ["model.created", "sandbox"],
      ["model_connection.created", "sandbox"],
    ]);
  });
});

describe("channels", () => {
  const sandbox = () => ws("acme-sandbox");
  const consent = (session: { url: string }, decision: "approved" | "denied") =>
    createHttpTransport({ baseUrl: API, getToken: async () => null }).request<ChannelConnectionConnectResult>({
      method: "POST",
      path: `/v1/connect/sessions/${referenceOf(session)}/simulate-consent`,
      body: { decision },
    });
  /** The fixture's linked team in a workspace. */
  const managerIn = (workspace: { id: WorkspaceId; organization_id: OrganizationId }) =>
    must(
      db().channel_app_managers.find((m) => inWorkspace(m, workspace)),
      "channel app manager",
    );
  /** An active agent of the workspace with no app yet. */
  const applessAgent = (workspace: { id: WorkspaceId; organization_id: OrganizationId }) =>
    must(
      db().agents.find(
        (a) =>
          inWorkspace(a, workspace) && a.status === "active" && !db().channel_apps.some((p) => p.agent_id === a.id),
      ),
      "agent with no app",
    );
  const eventTypes = (since: number) =>
    db()
      .webhook_events.slice(since)
      .map((e) => [e.body.type, e.body.data]);

  it("offers only the providers the deployment reaches, links teams nowhere else, and keeps what is stored", async () => {
    const workspace = sandbox();
    const agent = applessAgent(workspace);
    const manager = managerIn(workspace);
    const tokens = { configuration_token: "xoxe.xoxp-1-unreached-0000", refresh_token: "xoxe-1-unreached-0000" };
    expect(await owner.channelProviders.list(workspace.id)).toEqual([
      { provider: "slack", methods: ["configuration_token"] },
    ]);

    db().channel_providers = [];
    expect(await owner.channelProviders.list(workspace.id)).toEqual([]);
    const managers = db().channel_app_managers.length;
    const refused = await failure(
      owner.channelAppManagers.create(workspace.id, { provider: "slack", method: "configuration_token", ...tokens }),
    );
    expect([refused.status, refused.code]).toEqual([501, "channel_providers_not_configured"]);
    expect(refused.message).toContain("ZUPFLOW_PUBLIC_URL");
    // Even a token the provider would refuse: nothing reaches it to ask.
    const invalid = await failure(
      owner.channelAppManagers.update(workspace.id, manager.id, { ...tokens, configuration_token: "invalid" }),
    );
    expect([invalid.status, invalid.code]).toEqual([501, "channel_providers_not_configured"]);
    expect(db().channel_app_managers).toHaveLength(managers);
    // An app asked for meanwhile waits for a deployment that reaches the provider, as the job that makes it does.
    const app = await owner.channelApps.create(workspace.id, { agent_id: agent.id, manager_id: manager.id });
    expect(await owner.channelApps.get(workspace.id, app.id)).toMatchObject({
      status: "creating",
      last_error: null,
      external_app_id: null,
    });
    // A connection keeps its token, nothing here can revoke it.
    const connection = must(
      db().channel_connections.find((c) => inWorkspace(c, workspace)),
      "connection",
    );
    const kept = await failure(owner.channelConnections.delete(workspace.id, connection.id));
    expect([kept.status, kept.code]).toEqual([501, "channel_providers_not_configured"]);
    expect(await owner.channelConnections.get(workspace.id, connection.id)).toMatchObject({ id: connection.id });
  });

  it("installs an agent's app on consent: a connection, its short ID, the agent's channel and the events", async () => {
    const workspace = sandbox();
    const agent = applessAgent(workspace);
    const manager = managerIn(workspace);
    const events = db().webhook_events.length;
    const accepted = await owner.channelApps.create(workspace.id, { agent_id: agent.id, manager_id: manager.id });
    const session = await owner.connectSessions.create(workspace.id, {
      target: { kind: "channel_app", app: accepted.id },
      return_url: null,
    });
    const { data } = await consent(session, "approved");
    const connection = data.channel_connection;
    expect(connection).toMatchObject({
      workspace_id: workspace.id,
      app_id: accepted.id,
      agent_id: agent.id,
      status: "connected",
      label: agent.name,
      team: { id: manager.team_id },
    });
    expect(data.session).toMatchObject({
      status: "completed",
      target: { kind: "channel_app", provider: "slack", app: accepted.id, connection: connection.id },
    });
    expect(await owner.channelApps.get(workspace.id, accepted.id)).toMatchObject({
      status: "installed",
      connection_id: connection.id,
    });
    // Its short ID comes from the installations' pool: no channel tool shares a name with an app tool.
    const shortIds = db()
      .installations.filter((i) => inWorkspace(i, workspace))
      .map((i) => i.short_id);
    expect(shortIds).not.toContain(connection.short_id);
    expect(
      db().installation_short_ids.some((s) => inWorkspace(s, workspace) && s.short_id === connection.short_id),
    ).toBe(true);
    const channels = await owner.agentChannels.list(workspace.id, agent.id);
    expect(channels.map(({ id: _, created_at: _c, updated_at: _u, ...rest }) => rest)).toEqual([
      { workspace_id: workspace.id, agent_id: agent.id, connection_id: connection.id, ...agentChannelDefaults() },
    ]);
    expect(eventTypes(events)).toEqual([
      [
        "channel_app.status_changed",
        { object: "channel_app", id: accepted.id, agent_id: agent.id, status: "awaiting_install" },
      ],
      [
        "channel_connection.status_changed",
        { object: "channel_connection", id: connection.id, app_id: accepted.id, status: "connected" },
      ],
      [
        "channel_app.status_changed",
        { object: "channel_app", id: accepted.id, agent_id: agent.id, status: "installed" },
      ],
      ["connect_session.completed", expect.objectContaining({ object: "connect_session", id: session.id })],
    ]);
    // A used session can't be consented again.
    expect((await failure(consent(session, "approved"))).code).toBe("session_not_pending");
  });

  it("installs nothing when the app isn't allowed", async () => {
    const workspace = sandbox();
    const agent = applessAgent(workspace);
    const accepted = await owner.channelApps.create(workspace.id, {
      agent_id: agent.id,
      manager_id: managerIn(workspace).id,
    });
    const connections = db().channel_connections.length;
    const session = await owner.connectSessions.create(workspace.id, {
      target: { kind: "channel_app", app: accepted.id },
      return_url: null,
    });
    expect((await failure(consent(session, "denied"))).code).toBe("consent_denied");
    expect(await owner.connectSessions.get(workspace.id, session.id)).toMatchObject({
      status: "failed",
      failure_code: "consent_denied",
    });
    expect(db().channel_connections).toHaveLength(connections);
    expect((await owner.channelApps.get(workspace.id, accepted.id)).status).toBe("awaiting_install");
  });

  it("refuses apps and installs while the app isn't made or the team's token has expired", async () => {
    const workspace = sandbox();
    const agent = applessAgent(workspace);
    const manager = managerIn(workspace);
    const accepted = await owner.channelApps.create(workspace.id, { agent_id: agent.id, manager_id: manager.id });
    const row = must(db().channel_apps.find((a) => a.id === accepted.id));
    row.status = "creating";
    const install = () =>
      owner.connectSessions.create(workspace.id, {
        target: { kind: "channel_app", app: accepted.id },
        return_url: null,
      });
    expect((await failure(install())).code).toBe("channel_app_not_ready");
    row.status = "awaiting_install";
    const session = await owner.connectSessions.create(workspace.id, {
      target: { kind: "channel_app", app: accepted.id },
      return_url: null,
    });
    must(db().channel_app_managers.find((m) => m.id === manager.id)).status = "token_expired";
    expect((await failure(install())).code).toBe("channel_app_manager_inactive");
    // A session made before the token expired can't install either.
    expect((await failure(consent(session, "approved"))).code).toBe("app_not_eligible");
    const other = must(
      db().agents.find((a) => inWorkspace(a, workspace) && a.id !== agent.id),
      "another agent",
    );
    expect(
      (await failure(owner.channelApps.create(workspace.id, { agent_id: other.id, manager_id: manager.id }))).code,
    ).toBe("channel_app_manager_inactive");
    // New tokens make it active again, with the next credential version.
    const version = manager.credential_version;
    const events = db().webhook_events.length;
    const renewed = await owner.channelAppManagers.update(workspace.id, manager.id, {
      configuration_token: "xoxe.xoxp-1-renewed-0042",
      refresh_token: "xoxe-1-renewed-0043",
    });
    expect(renewed.status).toBe("active");
    expect(must(db().channel_app_managers.find((m) => m.id === manager.id)).credential_version).toBe(version + 1);
    expect(eventTypes(events)).toEqual([
      ["channel_app_manager.status_changed", { object: "channel_app_manager", id: manager.id, status: "active" }],
    ]);
  });

  it("stores no token, and keeps the inbound endpoint out of idempotency", async () => {
    const workspace = sandbox();
    const tokens = { configuration_token: "xoxe.xoxp-1-never-stored-5k2m", refresh_token: "xoxe-1-never-stored-r9x1" };
    await owner.channelAppManagers.create(workspace.id, {
      provider: "slack",
      method: "configuration_token",
      ...tokens,
    });
    const stored = JSON.stringify(db());
    expect(stored).not.toContain(tokens.configuration_token);
    expect(stored).not.toContain(tokens.refresh_token);

    const app = must(db().channel_apps.find((a) => inWorkspace(a, workspace)));
    const raw = JSON.stringify({
      type: "event_callback",
      event_id: "Ev0MOCK1",
      team_id: must(db().channel_connections.find((c) => inWorkspace(c, workspace))).team_id,
      event: { type: "message", channel: "C0MOCK", user: "U0MOCK", text: "hi", ts: "1700000000.000100" },
    });
    const timestamp = String(Math.floor(Date.now() / 1000));
    const signature = await slackSignature(app.signing_secret, timestamp, raw);
    const keys = db().idempotency_keys.length;
    for (let i = 0; i < 2; i++) {
      const response = await fetch(`${API}/channels/inbound/${app.inbound_ref}`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "Idempotency-Key": "provider-retry-0001",
          "X-Slack-Request-Timestamp": timestamp,
          "X-Slack-Signature": signature,
        },
        body: raw,
      });
      expect(await response.json()).toEqual({ challenge: null });
    }
    expect(db().idempotency_keys).toHaveLength(keys);
    // Deduplicated by the provider's event ID, within the app's own scope.
    expect(db().channel_inbound_events.filter((e) => e.provider_event_id === "Ev0MOCK1")).toEqual([
      expect.objectContaining({
        organization_id: app.organization_id,
        workspace_id: app.workspace_id,
        app_id: app.id,
      }),
    ]);
    expect(db().channel_messages.filter((m) => m.external_id === "1700000000.000100")).toHaveLength(1);
  });

  it("deletes a workspace's channels with it, and no other workspace's", async () => {
    const workspace = sandbox();
    const others = () =>
      JSON.stringify(
        [
          db().channel_app_managers,
          db().channel_apps,
          db().channel_connections,
          db().channel_conversations,
          db().channel_messages,
          db().channel_people,
          db().agent_channels,
        ].map((rows) => rows.filter((r) => !inWorkspace(r, workspace))),
      );
    const before = others();
    expect(db().channel_apps.some((a) => inWorkspace(a, workspace))).toBe(true);
    await owner.workspaces.delete(workspace.id);
    for (const rows of [
      db().channel_app_managers,
      db().channel_apps,
      db().channel_connections,
      db().channel_conversations,
      db().channel_messages,
      db().channel_people,
      db().agent_channels,
      db().channel_inbound_events,
    ]) {
      expect(rows.some((r) => inWorkspace(r, workspace))).toBe(false);
    }
    expect(others()).toBe(before);
  });

  it("seeds channel runs whose messages and sends link back to them, in their own workspace", async () => {
    const workspace = sandbox();
    const runs = (await owner.runs.list(workspace.id, { limit: 100 })).items.filter((r) => r.trigger === "channel");
    expect(runs.map((r) => r.channel?.reason).sort()).toEqual(["direct_message", "mentioned", "thread_follow_up"]);
    const connection = must(runs[0]?.channel, "channel").connection_id;
    const conversations = (await owner.channelConnections.conversations(workspace.id, connection, { limit: 100 }))
      .items;
    const messages = (
      await Promise.all(
        conversations.map((c) => owner.channelConnections.messages(workspace.id, connection, c.id, { limit: 100 })),
      )
    ).flatMap((page) => page.items);
    for (const run of runs) {
      const channel = must(run.channel, "channel");
      const origin = must(
        messages.find((m) => m.id === channel.origin_message_id),
        "origin message",
      );
      expect(origin).toMatchObject({ direction: "inbound", conversation_id: channel.conversation_id });
      expect(run.usage.cache_read_input_tokens + run.usage.cache_write_input_tokens).toBeLessThanOrEqual(
        run.usage.input_tokens,
      );
      const events = await owner.runs.events(workspace.id, run.id);
      expect(events[1]).toMatchObject({
        type: "channel_messages.received",
        data: { outcome: "started", message_ids: [origin.id] },
      });
      const silent = events.find((e) => e.type === "run.stayed_silent");
      expect(silent?.type === "run.stayed_silent" ? silent.data.reason : null).toBe(channel.silent_reason);
      for (const sent of messages.filter((m) => m.run_id === run.id)) {
        expect(sent.direction).toBe("outbound");
        if (sent.tool_call_id === null) {
          expect(sent.text).toBe(run.output);
        } else {
          expect(events.some((e) => e.type === "tool_call.started" && e.data.call_id === sent.tool_call_id)).toBe(true);
        }
      }
    }
    expect(messages.filter((m) => m.direction === "outbound").every((m) => runs.some((r) => r.id === m.run_id))).toBe(
      true,
    );
    // Another workspace's runs don't name this workspace's messages.
    const acmeRuns = (await owner.runs.list(ws("acme").id, { limit: 100 })).items.filter((r) => r.channel);
    expect(acmeRuns.length).toBeGreaterThan(0);
    expect(acmeRuns.some((r) => messages.some((m) => m.id === r.channel?.origin_message_id))).toBe(false);
  });
});
