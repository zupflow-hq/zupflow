import type { AuthTemplate, OrganizationId } from "@zupflow/contract";
import { IDEMPOTENCY_HEADER } from "@zupflow/contract/idempotency";
import { describe, expect, it } from "vitest";
import { createClient } from "./client";
import { createHttpTransport, ORGANIZATION_HEADER, ZupflowApiError, ZupflowNetworkError } from "./transport";

function fakeFetch(status: number, body: unknown, seen: Request[] = []): typeof fetch {
  return async (input, init) => {
    seen.push(new Request(input as string, init));
    return new Response(body === undefined ? null : JSON.stringify(body), { status });
  };
}

describe("createHttpTransport", () => {
  it("resends the named delivery without a replacement event and preserves the caller's idempotency key", async () => {
    const seen: Request[] = [];
    const queued = { id: "whd_new", state: "pending", attempts: 0, event_id: "evt_original" };
    const client = createClient(
      createHttpTransport({
        baseUrl: "https://api.zupflow.example",
        getToken: async () => null,
        fetch: fakeFetch(201, { data: queued, meta: null, error: null }, seen),
      }),
    );
    expect(
      await client.webhooks.resendDelivery("org_a b", "we_c/d", "whd_e f", { idempotencyKey: "resend-once" }),
    ).toEqual(queued);
    const request = seen[0];
    expect(request?.url).toBe(
      "https://api.zupflow.example/v1/organizations/org_a%20b/webhooks/we_c%2Fd/deliveries/whd_e%20f/resend",
    );
    expect(request?.method).toBe("POST");
    expect(request?.headers.get(IDEMPOTENCY_HEADER)).toBe("resend-once");
    expect(await request?.text()).toBe("");
  });

  it("scopes delivery history and attempts to the organization and endpoint, preserving filters and pages", async () => {
    const seen: Request[] = [];
    const client = createClient(
      createHttpTransport({
        baseUrl: "https://api.zupflow.example",
        getToken: async () => null,
        fetch: fakeFetch(
          200,
          { data: [], meta: { pagination: { next_cursor: "page+2", total: 7 } }, error: null },
          seen,
        ),
      }),
    );
    const page = await client.webhooks.deliveries("org_a b", "we_c/d", {
      workspace_id: "ws_other",
      state: "pending",
      event_type: "run.completed",
      cursor: "page+1",
      limit: 2,
    });
    await client.webhooks.delivery("org_a b", "we_c/d", "whd_e f");
    const attempts = await client.webhooks.deliveryAttempts("org_a b", "we_c/d", "whd_e f", {
      cursor: "page+1",
      limit: 2,
    });
    const first = new URL(seen[0]?.url ?? "");
    expect(first.pathname).toBe("/v1/organizations/org_a%20b/webhooks/we_c%2Fd/deliveries");
    expect(Object.fromEntries(first.searchParams)).toEqual({
      workspace_id: "ws_other",
      state: "pending",
      event_type: "run.completed",
      cursor: "page+1",
      limit: "2",
    });
    expect(seen[1]?.url).toBe(
      "https://api.zupflow.example/v1/organizations/org_a%20b/webhooks/we_c%2Fd/deliveries/whd_e%20f",
    );
    expect(new URL(seen[2]?.url ?? "").pathname).toBe(
      "/v1/organizations/org_a%20b/webhooks/we_c%2Fd/deliveries/whd_e%20f/attempts",
    );
    expect(page.pagination).toEqual({ next_cursor: "page+2", total: 7 });
    expect(attempts.pagination).toEqual(page.pagination);
  });

  it("preserves an OAuth issuer exactly when updating either app scope", async () => {
    const seen: Request[] = [];
    const auth: AuthTemplate = {
      type: "oauth2",
      authorize_url: "https://auth.provider.example/authorize",
      token_url: "https://auth.provider.example/token",
      issuer: "https://AUTH.provider.example/Tenant/",
      identity_url: null,
      available_scopes: [],
      pkce: true,
    };
    const client = createClient(
      createHttpTransport({
        baseUrl: "https://api.zupflow.example",
        getToken: async () => null,
        fetch: fakeFetch(200, { data: { auth }, meta: null, error: null }, seen),
      }),
    );
    for (const result of [
      await client.organizationApps.update("org_1", "app_1", { auth }),
      await client.workspaceApps.update("ws_1", "app_1", { auth }),
    ])
      expect(result.auth).toEqual(auth);
    for (const request of seen) expect(await request.json()).toEqual({ auth });
  });

  it("unwraps the envelope and sends a fresh bearer token", async () => {
    const seen: Request[] = [];
    let calls = 0;
    const client = createClient(
      createHttpTransport({
        baseUrl: "https://api.zupflow.test",
        getToken: async () => `token-${++calls}`,
        fetch: fakeFetch(200, { data: [{ id: "w1" }], meta: null, error: null }, seen),
      }),
    );
    await client.workspaces.list("org_a b");
    await client.workspaces.list("org_a b");
    expect(seen[0]?.url).toBe("https://api.zupflow.test/v1/organizations/org_a%20b/workspaces");
    expect(seen[0]?.headers.get("authorization")).toBe("Bearer token-1");
    expect(seen[1]?.headers.get("authorization")).toBe("Bearer token-2");
  });

  it("puts the workspace in the path for workspace-scoped calls", async () => {
    const seen: Request[] = [];
    const client = createClient(
      createHttpTransport({
        baseUrl: "https://api.zupflow.test/",
        getToken: async () => null,
        fetch: fakeFetch(200, { data: [], meta: { pagination: { next_cursor: "c2", total: 40 } }, error: null }, seen),
      }),
    );
    const page = await client.runs.list("ws_1", { status: "failed", agent_id: undefined });
    expect(seen[0]?.url).toBe("https://api.zupflow.test/v1/workspaces/ws_1/runs?status=failed");
    expect(page.pagination.next_cursor).toBe("c2");
  });

  it("raises the API's error code and field errors", async () => {
    const client = createClient(
      createHttpTransport({
        baseUrl: "https://api.zupflow.test",
        getToken: async () => null,
        fetch: fakeFetch(422, {
          data: null,
          meta: null,
          error: { code: "return_url_not_allowed", message: "Not allowed.", fields: { return_url: "Nope" } },
        }),
      }),
    );
    const error = await client.connectSessions
      .create("ws_0000000000000000000001", {
        app_id: "app_0000000000000000000001",
        intent: "create",
        label: "x",
        installation_id: null,
        return_url: "https://x",
      })
      .catch((e: unknown) => e);
    expect(error).toBeInstanceOf(ZupflowApiError);
    expect(error).toMatchObject({ status: 422, code: "return_url_not_allowed", fields: { return_url: "Nope" } });
  });

  it("treats a non-envelope body as an error rather than data", async () => {
    const client = createClient(
      createHttpTransport({
        baseUrl: "https://api.zupflow.test",
        getToken: async () => null,
        fetch: fakeFetch(200, { id: 1 }),
      }),
    );
    await expect(client.me.get()).rejects.toMatchObject({ code: "invalid_response" });
  });

  it("reports network failures distinctly", async () => {
    const client = createClient(
      createHttpTransport({
        baseUrl: "https://api.zupflow.test",
        getToken: async () => null,
        fetch: async () => {
          throw new TypeError("Failed to fetch");
        },
      }),
    );
    await expect(client.me.get()).rejects.toBeInstanceOf(ZupflowNetworkError);
  });

  it("names the organization when one is given, and omits the header otherwise", async () => {
    const seen: Request[] = [];
    const fetch = fakeFetch(200, { data: {}, meta: null, error: null }, seen);
    let organization: OrganizationId | null = "org_031DCoVdPC4Z4DgUS9oR60";
    const named = createClient(
      createHttpTransport({
        baseUrl: "https://api.zupflow.test",
        getToken: async () => "t",
        getOrganization: () => organization,
        fetch,
      }),
    );
    await named.workspaces.get("ws_031DCp7XSYxLgDmAT3Dc7r");
    organization = null;
    await named.workspaces.get("ws_031DCp7XSYxLgDmAT3Dc7r");
    const plain = createClient(
      createHttpTransport({ baseUrl: "https://api.zupflow.test", getToken: async () => "t", fetch }),
    );
    await plain.workspaces.get("ws_031DCp7XSYxLgDmAT3Dc7r");
    expect(seen[0]?.headers.get(ORGANIZATION_HEADER)).toBe("org_031DCoVdPC4Z4DgUS9oR60");
    expect(seen[1]?.headers.has(ORGANIZATION_HEADER)).toBe(false);
    expect(seen[2]?.headers.has(ORGANIZATION_HEADER)).toBe(false);
  });

  describe("idempotency", () => {
    const created = { data: { id: "org_1" }, meta: null, error: null };
    const input = { name: "Acme", slug: "acme" };

    it("sends a fresh key on every POST, and none on other methods", async () => {
      const seen: Request[] = [];
      const client = createClient(
        createHttpTransport({
          baseUrl: "https://api.zupflow.test",
          getToken: async () => "t",
          fetch: fakeFetch(201, created, seen),
        }),
      );
      await client.organizations.create(input);
      await client.organizations.create(input);
      await client.organizations.update("org_031DCoVdPC4Z4DgUS9oR60", { name: "B" });
      const [first, second, patch] = seen.map((r) => r.headers.get(IDEMPOTENCY_HEADER));
      expect(first).toMatch(/^[\x20-\x7E]{1,255}$/);
      expect(second).toMatch(/^[\x20-\x7E]{1,255}$/);
      expect(first).not.toBe(second);
      expect(patch).toBeNull();
    });

    it("sends the caller's own key when given one", async () => {
      const seen: Request[] = [];
      const client = createClient(
        createHttpTransport({
          baseUrl: "https://api.zupflow.test",
          getToken: async () => "t",
          fetch: fakeFetch(201, created, seen),
        }),
      );
      await client.organizations.create(input, { idempotencyKey: "signup-42" });
      expect(seen[0]?.headers.get(IDEMPOTENCY_HEADER)).toBe("signup-42");
    });

    it("retries a request that got no response with the same key, and gives up after its retries", async () => {
      const keys: (string | null)[] = [];
      let failures = 2;
      const flaky: typeof fetch = async (input, init) => {
        keys.push(new Request(input as string, init).headers.get(IDEMPOTENCY_HEADER));
        if (failures-- > 0) throw new TypeError("connection reset");
        return new Response(JSON.stringify(created), { status: 201 });
      };
      const client = createClient(
        createHttpTransport({ baseUrl: "https://api.zupflow.test", getToken: async () => "t", fetch: flaky }),
      );
      await client.organizations.create(input);
      expect(keys).toHaveLength(3);
      expect(new Set(keys).size).toBe(1);

      let calls = 0;
      const down: typeof fetch = async () => {
        calls++;
        throw new TypeError("offline");
      };
      const offline = createClient(
        createHttpTransport({
          baseUrl: "https://api.zupflow.test",
          getToken: async () => "t",
          fetch: down,
          retries: 1,
        }),
      );
      await expect(offline.organizations.create(input)).rejects.toBeInstanceOf(ZupflowNetworkError);
      expect(calls).toBe(2);
    });

    it("retries twice by default", async () => {
      let calls = 0;
      const client = createClient(
        createHttpTransport({
          baseUrl: "https://api.zupflow.test",
          getToken: async () => "t",
          fetch: async () => {
            calls++;
            throw new TypeError("offline");
          },
        }),
      );
      await expect(client.organizations.create(input)).rejects.toBeInstanceOf(ZupflowNetworkError);
      expect(calls).toBe(3);
    });

    it("never retries a cancelled request, and rethrows the caller's reason", async () => {
      let calls = 0;
      const controller = new AbortController();
      const reason = new Error("navigated away");
      const transport = createHttpTransport({
        baseUrl: "https://api.zupflow.test",
        getToken: async () => "t",
        fetch: async () => {
          calls++;
          controller.abort(reason);
          throw reason;
        },
      });
      await expect(
        transport.request({ method: "POST", path: "/v1/organizations", body: input, signal: controller.signal }),
      ).rejects.toBe(reason);
      expect(calls).toBe(1);
    });

    it("never retries a request the server answered", async () => {
      const seen: Request[] = [];
      const error = { data: null, meta: null, error: { code: "internal_error", message: "x", fields: null } };
      const client = createClient(
        createHttpTransport({
          baseUrl: "https://api.zupflow.test",
          getToken: async () => "t",
          fetch: fakeFetch(500, error, seen),
        }),
      );
      await expect(client.organizations.create(input)).rejects.toBeInstanceOf(ZupflowApiError);
      expect(seen).toHaveLength(1);
    });
  });
});
