import type { OrganizationId } from "@zupflow/contract";
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
});
