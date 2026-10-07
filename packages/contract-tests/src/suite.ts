/**
 * The contract suite: black-box cases any server must pass, reaching it only
 * over HTTP. Every response is also checked against the contract, and one
 * case drives every operation to a checked success.
 *
 * Call `contractSuite(target)` from a test file; see `Target` for what a
 * server provides and the reference fixture it loads.
 */
import { matchRoute, routes, WEBHOOK_EVENT_TYPES } from "@zupflow/contract";
import {
  type AgentId,
  type ApiKeyId,
  type AppId,
  type AuthTemplate,
  type CreateConnectSessionInput,
  createClient,
  createHttpTransport,
  type OrganizationId,
  type WorkspaceId,
  ZupflowApiError,
  type ZupflowClient,
} from "@zupflow/sdk";
import { afterEach, beforeEach, describe, expect, it as vitestIt } from "vitest";
import { createResponseChecker } from "./checker";
import { type Fixture, loadFixture, must } from "./fixture";
import { expectSealed, loadTenant, snapshotOf, type Tenant } from "./isolation";
import { NotServed, notServedIn } from "./served";
import type { Persona, Target } from "./target";

async function failure(promise: Promise<unknown>) {
  try {
    await promise;
  } catch (error) {
    if (error instanceof ZupflowApiError) return error;
    throw error;
  }
  throw new Error("Expected the request to fail");
}

/** The hosted Connect reference: the last segment of the session's link. */
const referenceOf = (session: { url: string }) => must(session.url.split("/").pop(), "Connect reference");

/** The canonical UUID inside an ID, decoded independently of any server. */
function uuidOf(id: string): string {
  const alphabet = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
  let n = 0n;
  for (const c of id.slice(id.indexOf("_") + 1)) n = n * 62n + BigInt(alphabet.indexOf(c));
  const hex = n.toString(16).padStart(32, "0");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/**
 * Operations the suite can't drive to a success yet, with the reason. A target
 * that fails one of these is not excused: only the coverage check skips them.
 */
export const NOT_YET_COVERED: Record<string, string> = {
  resolveApproval: "Approvals come only from an API key, and the suite has no API-key credential yet.",
};

/**
 * A case of the suite. One that needs an operation the target doesn't serve
 * is skipped, naming the operation; it never fails or passes for it.
 */
function it(name: string, test: () => Promise<void>) {
  vitestIt(name, async (context) => {
    try {
      await test();
    } catch (error) {
      const notServed = notServedIn(error);
      if (notServed) context.skip(`needs ${notServed.operationId}, which this server doesn't serve yet`);
      throw error;
    }
  });
}

export function contractSuite(target: Target) {
  describe("the contract suite", () => {
    const checker = createResponseChecker({ baseUrl: target.baseUrl, fetch: target.fetch });
    const served = target.operations ? new Set(target.operations) : null;
    const serves = (operationId: string) => served === null || served.has(operationId);
    const base = new URL(target.baseUrl).pathname.replace(/\/$/, "");
    /** The checked fetch, refusing to send a request for an operation the target doesn't serve. */
    const servedFetch: typeof fetch = async (input, init) => {
      const request = new Request(input, init);
      const route = matchRoute(request.method, new URL(request.url).pathname.slice(base.length));
      if (route && !serves(route.operationId)) throw new NotServed(route.operationId);
      return checker.fetch(input, init);
    };
    let fx: Fixture;

    const transportFor = (persona: Persona | null, organization: () => OrganizationId | null) =>
      createHttpTransport({
        baseUrl: target.baseUrl,
        getToken: async () => (persona ? target.token(persona) : null),
        getOrganization: organization,
        fetch: servedFetch,
        // An operation the target doesn't serve is refused at once, not retried as a network error.
        retries: 0,
      });
    /** A Console session acting in the persona's own organization: the outsider's is `larkspur`. */
    const as = (persona: Persona | null): ZupflowClient =>
      createClient(
        transportFor(persona, () => (persona === "outsider" ? fx?.otherOrganization : fx?.organization) ?? null),
      );
    /** A Console session naming `organization` in every request, whatever the persona belongs to. */
    const naming = (persona: Persona, organization: () => OrganizationId | null): ZupflowClient =>
      createClient(transportFor(persona, organization));
    /** A model the server offers, rather than one the suite assumes. */
    const anyModel = async () => must((await owner.models.list())[0], "model").id;
    /** A workspace of the reference organization, found by slug. */
    const workspaceBySlug = async (slug: string) =>
      must(
        (await owner.workspaces.list(fx.organization, { q: slug, limit: 100 })).items.find((w) => w.slug === slug),
        `workspace ${slug}`,
      );
    const owner = as("owner");
    const northwind = () => loadTenant(owner, fx.organization, "acme", "acme-sandbox");
    /** `beta-labs`, given a workspace app of its own first: the fixture has none there. */
    const betaLabs = async () => {
      const beta = await workspaceBySlug("beta-labs");
      if (!(await owner.workspaceApps.list(beta.id)).some((a) => a.owner_workspace_id === beta.id)) {
        await owner.workspaceApps.create(beta.id, {
          kind: "mcp",
          name: "Beta tools",
          slug: "beta-tools",
          description: "",
          auth_type: "token",
        });
      }
      return loadTenant(owner, fx.organization, "beta-labs", "qa-playground");
    };
    const larkspur = () => loadTenant(as("outsider"), fx.otherOrganization, "harbor", "harbor-sandbox");

    beforeEach(async () => {
      await target.reset();
      fx = await loadFixture(
        (org) => createClient(transportFor("owner", () => org)),
        createClient(transportFor("outsider", () => null)),
      );
    });
    afterEach(() => {
      expect(checker.takeViolations(), "responses that break the contract").toEqual([]);
    });

    it("answers every operation with a success that matches its schema", async () => {
      checker.succeeded.clear();
      const org = fx.organization;
      const { acme, acmeSandbox: sandbox, qaPlayground: qa } = fx.workspaces;

      // A server that serves part of the API stops at the first operation it
      // doesn't, so what every server has comes first, in the order the
      // backend builds it: organizations, members, workspaces, API keys, apps,
      // installations, Connect.
      try {
        await everyOperation();
      } catch (error) {
        if (!notServedIn(error)) throw error;
      }
      const missed = routes
        .map((r) => r.operationId)
        .filter((id) => serves(id) && !checker.succeeded.has(id) && !(id in NOT_YET_COVERED));
      expect(missed, "served operations with no checked success, or served after one that isn't").toEqual([]);

      async function everyOperation() {
        await owner.me.get();
        await owner.organizations.create({ name: "Contract Co", slug: "contract-co" });
        await owner.organizations.get(org);
        await owner.organizations.update(org, { name: "Northwind" });
        await owner.members.list(org);
        const invited = await owner.members.invite(org, { email: "contract@northwind.example", role: "developer" });
        await owner.members.update(org, invited.id, { role: "admin" });
        await owner.members.remove(org, invited.id);
        await owner.workspaces.list(org, { environment: "production" });
        const workspace = await owner.workspaces.create(org, {
          name: "Contract",
          slug: "contract",
          environment: "sandbox",
          external_id: null,
        });
        await owner.workspaces.get(workspace.id);
        await owner.workspaces.update(workspace.id, { name: "Contract 2" });
        const spare = await owner.workspaces.create(org, {
          name: "Spare",
          slug: "spare",
          environment: "sandbox",
          external_id: null,
        });
        await owner.workspaces.delete(spare.id);
        await owner.apiKeys.list(org);
        const key = await owner.apiKeys.create(org, { name: "Contract", environment: "sandbox" });
        await owner.apiKeys.revoke(org, key.key.id);

        // Apps, at both levels, including OAuth apps that have no endpoints yet.
        const oauthApp = { kind: "mcp", description: "", auth_type: "oauth2" } as const;
        await owner.organizationApps.list(org);
        const orgApp = await owner.organizationApps.create(org, { ...oauthApp, name: "Org OAuth", slug: "org-oauth" });
        await owner.organizationApps.get(org, orgApp.id);
        await owner.organizationApps.update(org, orgApp.id, { description: "Changed" });
        await owner.workspaceApps.list(workspace.id);
        const wsApp = await owner.workspaceApps.create(workspace.id, {
          ...oauthApp,
          name: "WS OAuth",
          slug: "ws-oauth",
        });
        await owner.workspaceApps.get(workspace.id, wsApp.id);
        await owner.workspaceApps.update(workspace.id, wsApp.id, { description: "Changed" });

        // Their client configurations, then where they're offered and installed.
        await owner.organizationApps.getConfiguration(org, orgApp.id, "sandbox");
        await owner.organizationApps.updateConfiguration(org, orgApp.id, "sandbox", { oauth_client_id: "client" });
        await owner.workspaceApps.getConfiguration(workspace.id, wsApp.id);
        await owner.workspaceApps.updateConfiguration(workspace.id, wsApp.id, { oauth_client_id: "client" });
        await owner.workspaceApps.available(workspace.id);
        await owner.organizationApps.installations(org, orgApp.id);

        // Installations: read, renamed, and one disconnected and deleted.
        const installation = must((await owner.installations.list(sandbox.id))[0], "installation in acme-sandbox");
        await owner.installations.get(sandbox.id, installation.id);
        await owner.installations.update(sandbox.id, installation.id, { label: installation.label });
        const echo = must((await owner.installations.list(qa.id))[0], "installation in qa-playground");
        await owner.installations.disconnect(qa.id, echo.id);
        await owner.installations.delete(qa.id, echo.id);

        // Connect, token and OAuth.
        await owner.connectSessions.list(sandbox.id);
        const session = (appId: AppId, workspaceId: WorkspaceId = qa.id) =>
          owner.connectSessions.create(workspaceId, {
            app_id: appId,
            intent: "create",
            label: "Contract",
            installation_id: null,
            return_url: null,
          });
        const tokenSession = await session(fx.apps.echoServer);
        await owner.connectSessions.get(qa.id, tokenSession.id);
        await owner.connect.get(referenceOf(tokenSession));
        await owner.connect.submitCredentials(referenceOf(tokenSession), { values: { token: "abc" } });
        await owner.connect.authorize(referenceOf(await session(fx.apps.googleWorkspace, sandbox.id)));
        await owner.connect.cancel(referenceOf(await session(fx.apps.echoServer)));

        // The rest of the API.
        await owner.models.list();
        await owner.organizations.usage(org, "production");

        // Agents, grants and runs.
        const agent = must(
          (await owner.agents.list(acme.id)).find((a) => a.status === "active"),
          "active agent in acme",
        );
        const newAgent = await owner.agents.create(acme.id, {
          name: "Contract",
          description: "",
          instructions: "Be brief.",
          model: agent.model,
        });
        await owner.agents.get(acme.id, newAgent.id);
        await owner.agents.update(acme.id, newAgent.id, { name: "Contract 2" });
        const grant = must((await owner.grants.list(acme.id, agent.id))[0], "grant in acme");
        await owner.grants.update(acme.id, agent.id, grant.id, {
          tools: grant.tools,
          approval_required: grant.approval_required,
        });
        // The new agent has no grants yet, so the existing grant's installation is free for it.
        const newGrant = await owner.grants.create(acme.id, newAgent.id, {
          installation_id: grant.installation_id,
          tools: grant.tools.slice(0, 1),
          approval_required: [],
        });
        await owner.grants.delete(acme.id, newAgent.id, newGrant.id);
        await owner.agents.delete(acme.id, newAgent.id);
        await owner.organizationRuns.list(org, { environment: "production" });
        const run = must((await owner.runs.list(acme.id)).items[0], "run in acme");
        await owner.runs.get(acme.id, run.id);
        await owner.runs.events(acme.id, run.id);
        const sandboxAgent = must(
          (await owner.agents.list(sandbox.id)).find((a) => a.status === "active"),
          "active agent in acme-sandbox",
        );
        const started = await owner.runs.create(sandbox.id, sandboxAgent.id, { input: "Hello" });
        await owner.runs.cancel(sandbox.id, started.id);

        // Organization settings.
        await owner.connectSettings.get(org, "sandbox");
        await owner.connectSettings.update(org, "sandbox", { return_urls: [], allowed_origins: [] });
        await owner.webhooks.list(org);
        const hook = await owner.webhooks.create(org, {
          environment: "sandbox",
          url: "https://hooks.northwind.example/zupflow",
          description: "Contract",
          events: ["run.completed"],
        });
        await owner.webhooks.update(org, hook.endpoint.id, { enabled: false });
        await owner.webhooks.sendTest(org, hook.endpoint.id);
        await owner.webhooks.delete(org, hook.endpoint.id);
        await owner.audit.list(org);
        await owner.workspaces.delete(workspace.id);
      }
    });

    describe("authentication and membership", () => {
      it("answers 401 without a credential", async () => {
        const error = await failure(as(null).me.get());
        expect(error.status).toBe(401);
        expect(error.code).toBe("unauthenticated");
      });

      it("answers 401 for a token it didn't issue", async () => {
        const forged = createClient(
          createHttpTransport({
            baseUrl: target.baseUrl,
            getToken: async () => "not-a-token-this-server-issued",
            getOrganization: () => fx.organization,
            fetch: servedFetch,
            retries: 0,
          }),
        );
        for (const call of [() => forged.me.get(), () => forged.workspaces.get(fx.workspaces.acme.id)]) {
          const error = await failure(call());
          expect(error.status).toBe(401);
          expect(error.code).toBe("unauthenticated");
        }
      });

      it("checks the credential before parsing an ID, so a signed-out caller always gets 401", async () => {
        const anonymous = as(null);
        const acme = fx.workspaces.acme.id;
        for (const call of [
          () => anonymous.workspaces.get("ws_short"),
          () => anonymous.workspaces.get(acme),
          () => anonymous.members.list("org_short"),
          () => anonymous.members.list(fx.organization),
          () => anonymous.runs.get(acme, "run_short"),
        ]) {
          expect((await failure(call())).status).toBe(401);
        }
      });

      it("hides another organization's workspace as 404", async () => {
        const error = await failure(as("outsider").workspaces.get(fx.workspaces.acme.id));
        expect(error.status).toBe(404);
      });

      it("requires a Console session to name its organization on workspace routes", async () => {
        const unnamed = naming("owner", () => null);
        const error = await failure(unnamed.workspaces.get(fx.workspaces.acme.id));
        expect(error.status).toBe(400);
        expect(error.code).toBe("organization_required");
        // Organization routes name it in the path.
        expect((await unnamed.members.list(fx.organization)).length).toBeGreaterThan(0);
      });

      it("checks the named organization against both the membership and the workspace", async () => {
        const harbor = (await larkspur()).workspace.id;
        const acme = fx.workspaces.acme.id;
        // A Northwind member naming Larkspur isn't a member there.
        const spoof = naming("owner", () => fx.otherOrganization);
        expect((await failure(spoof.workspaces.get(harbor))).status).toBe(404);
        // Naming their own organization doesn't reach another organization's workspace.
        expect((await failure(owner.workspaces.get(harbor))).status).toBe(404);
        // Larkspur's owner naming Northwind is refused, even for a workspace that exists.
        expect((await failure(naming("outsider", () => fx.organization).workspaces.get(acme))).status).toBe(404);
        // And each organization still reaches its own.
        expect((await as("outsider").workspaces.get(harbor)).id).toBe(harbor);
        expect((await owner.workspaces.get(acme)).id).toBe(acme);
      });

      it("never lists another organization's rows", async () => {
        const harbor = (await larkspur()).workspace.id;
        expect((await owner.me.get()).organizations.map((o) => o.id)).not.toContain(fx.otherOrganization);
        const listed = JSON.stringify(
          await Promise.all([
            owner.workspaces.list(fx.organization, { limit: 100, q: "harbor" }),
            owner.organizationRuns.list(fx.organization, { environment: "production", limit: 100 }),
            owner.audit.list(fx.organization, { limit: 100 }),
          ]),
        );
        expect(listed).not.toContain(harbor);
        expect(listed).not.toContain(fx.otherOrganization);
        expect((await failure(as("outsider").organizationRuns.list(fx.organization))).status).toBe(404);
      });

      it("refuses Production keys to developers but allows Sandbox keys", async () => {
        const developer = as("developer");
        const denied = await failure(
          developer.apiKeys.create(fx.organization, { name: "x", environment: "production" }),
        );
        expect(denied.status).toBe(403);
        const created = await developer.apiKeys.create(fx.organization, { name: "x", environment: "sandbox" });
        expect(created.secret.startsWith("zf_test_")).toBe(true);
        expect(created.key).not.toHaveProperty("secret");
      });

      it("answers 404 for a malformed ID, a bare UUID or another object's ID", async () => {
        const acme = fx.workspaces.acme.id;
        const agent = must((await owner.agents.list(acme))[0], "agent in acme");
        for (const bad of [uuidOf(acme), `ws_${"z".repeat(22)}`, "ws_short", agent.id]) {
          const error = await failure(owner.workspaces.get(bad as WorkspaceId));
          expect(error.status).toBe(404);
          expect(error.code).toBe("not_found");
        }
        const wrongType = await failure(owner.agents.get(acme, acme as unknown as AgentId));
        expect(wrongType.status).toBe(404);
      });
    });

    describe("apps and environments", () => {
      it("rejects an unsupported app kind instead of coercing it", async () => {
        const error = await failure(
          owner.organizationApps.create(fx.organization, {
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

      it("refuses an unknown environment filter on an app's installations", async () => {
        const error = await failure(
          owner.organizationApps.installations(fx.organization, fx.apps.linear, {
            // @ts-expect-error: the contract only accepts sandbox or production
            environment: "staging",
          }),
        );
        expect([error.status, error.code]).toEqual([400, "validation_failed"]);
      });

      it("never lets a workspace change environment", async () => {
        const error = await failure(
          transportFor("owner", () => fx.organization).request({
            method: "PATCH",
            path: `/v1/workspaces/${fx.workspaces.acme.id}`,
            body: { environment: "sandbox" },
          }),
        );
        expect(error.code).toBe("environment_immutable");
      });

      it("offers only apps ready in the workspace's own environment, with no fallback", async () => {
        const prod = await owner.workspaceApps.available(fx.workspaces.acme.id);
        const sandbox = await owner.workspaceApps.available(fx.workspaces.acmeSandbox.id);
        expect(prod.some((a) => a.app.id === fx.apps.linear)).toBe(false);
        expect(sandbox.some((a) => a.app.id === fx.apps.linear)).toBe(true);

        const error = await failure(
          owner.connectSessions.create(fx.workspaces.acmeSandbox.id, {
            app_id: fx.apps.notion,
            intent: "create",
            label: "Docs",
            installation_id: null,
            return_url: null,
          }),
        );
        expect(error.code).toBe("configuration_not_ready");
      });

      it("keeps a workspace app invisible to every other workspace", async () => {
        const other = fx.workspaces.acmeSandbox.id;
        const echo = fx.apps.echoServer;
        const available = await owner.workspaceApps.available(other);
        expect(available.some((a) => a.app.id === echo)).toBe(false);
        expect((await failure(owner.workspaceApps.get(other, echo))).status).toBe(404);
        const error = await failure(
          owner.connectSessions.create(other, {
            app_id: echo,
            intent: "create",
            label: "Echo",
            installation_id: null,
            return_url: null,
          }),
        );
        expect(error.status).toBe(404);
      });

      it("refuses private and plain-HTTP MCP endpoints", async () => {
        const local = await failure(
          owner.organizationApps.updateConfiguration(fx.organization, fx.apps.notion, "sandbox", {
            mcp_server_url: "https://169.254.169.254/mcp",
          }),
        );
        expect(local.code).toBe("destination_not_allowed");
        const http = await failure(
          owner.organizationApps.updateConfiguration(fx.organization, fx.apps.notion, "sandbox", {
            mcp_server_url: "http://mcp.notion.example/mcp",
          }),
        );
        expect(http.code).toBe("insecure_url");
      });

      describe("authentication templates", () => {
        const oauthTemplate: Extract<AuthTemplate, { type: "oauth2" }> = {
          type: "oauth2",
          authorize_url: " https://auth.provider.example/authorize ",
          token_url: "https://auth.provider.example/token",
          identity_url: null,
          available_scopes: [{ name: "read", description: "Read everything" }],
          pkce: true,
        };
        const newOAuthApp = (slug: string) =>
          owner.organizationApps.create(fx.organization, {
            kind: "mcp",
            name: "Provider",
            slug,
            description: "",
            auth_type: "oauth2",
          });

        it("sets an OAuth app's endpoints and raises its version", async () => {
          const app = await newOAuthApp("templated");
          const updated = await owner.organizationApps.update(fx.organization, app.id, { auth: oauthTemplate });
          expect(updated.auth).toEqual({ ...oauthTemplate, authorize_url: "https://auth.provider.example/authorize" });
          expect(updated.version).toBe(app.version + 1);
          expect((await owner.organizationApps.get(fx.organization, app.id)).auth).toEqual(updated.auth);
          // Renaming alone leaves the template and the version as they are.
          const renamed = await owner.organizationApps.update(fx.organization, app.id, { name: "Renamed" });
          expect([renamed.auth, renamed.version]).toEqual([updated.auth, updated.version]);
        });

        it("sets a workspace app's token template", async () => {
          const ws = fx.workspaces.qaPlayground.id;
          const app = await owner.workspaceApps.create(ws, {
            kind: "mcp",
            name: "Tools",
            slug: "templated-tools",
            description: "",
            auth_type: "token",
          });
          const auth: AuthTemplate = {
            type: "token",
            inputs: [
              { key: "api_key", label: "API key", secret: true, help: null, placeholder: "tk_…", pattern: null },
            ],
            placement: { location: "header", name: "X-Api-Key", prefix: "" },
            verify_url: "https://tools.provider.example/me",
            instructions: "Paste a key from Settings.",
            docs_url: null,
          };
          const updated = await owner.workspaceApps.update(ws, app.id, { auth });
          expect(updated.auth).toEqual(auth);
          expect(updated.version).toBe(app.version + 1);
        });

        it("keeps an app's authentication type", async () => {
          const app = await newOAuthApp("typed");
          const error = await failure(
            owner.organizationApps.update(fx.organization, app.id, {
              auth: {
                type: "token",
                inputs: [{ key: "token", label: "Token", secret: true, help: null, placeholder: null, pattern: null }],
                placement: { location: "header", name: "Authorization", prefix: "Bearer " },
                verify_url: null,
                instructions: "",
                docs_url: null,
              },
            }),
          );
          expect([error.status, error.code]).toEqual([422, "auth_type_immutable"]);
        });

        it("refuses private and plain-HTTP provider endpoints, and changes nothing", async () => {
          const app = await newOAuthApp("endpoints");
          const insecure = await failure(
            owner.organizationApps.update(fx.organization, app.id, {
              auth: { ...oauthTemplate, authorize_url: "http://auth.provider.example/authorize" },
            }),
          );
          expect([insecure.code, Object.keys(insecure.fields ?? {})]).toEqual(["insecure_url", ["auth.authorize_url"]]);
          const local = await failure(
            owner.organizationApps.update(fx.organization, app.id, {
              name: "Changed",
              auth: { ...oauthTemplate, token_url: "https://169.254.169.254/token" },
            }),
          );
          expect([local.code, Object.keys(local.fields ?? {})]).toEqual([
            "destination_not_allowed",
            ["auth.token_url"],
          ]);
          const after = await owner.organizationApps.get(fx.organization, app.id);
          expect([after.name, after.version, after.auth]).toEqual([app.name, app.version, app.auth]);
        });

        it("refuses a token placement in a header Zupflow sets itself", async () => {
          const ws = fx.workspaces.qaPlayground.id;
          const app = await owner.workspaceApps.create(ws, {
            kind: "mcp",
            name: "Hosty",
            slug: "hosty",
            description: "",
            auth_type: "token",
          });
          const error = await failure(
            owner.workspaceApps.update(ws, app.id, {
              auth: {
                type: "token",
                inputs: [{ key: "token", label: "Token", secret: true, help: null, placeholder: null, pattern: null }],
                placement: { location: "header", name: "Host", prefix: "" },
                verify_url: null,
                instructions: "",
                docs_url: null,
              },
            }),
          );
          expect([error.status, error.code, Object.keys(error.fields ?? {})]).toEqual([
            400,
            "validation_failed",
            ["auth.placement.name"],
          ]);
        });

        it("accepts only input patterns both the browser and the server understand", async () => {
          const ws = fx.workspaces.qaPlayground.id;
          const app = await owner.workspaceApps.create(ws, {
            kind: "mcp",
            name: "Patterned",
            slug: "patterned",
            description: "",
            auth_type: "token",
          });
          const withPattern = (pattern: string): AuthTemplate => ({
            type: "token",
            inputs: [{ key: "token", label: "Token", secret: true, help: null, placeholder: null, pattern }],
            placement: { location: "header", name: "Authorization", prefix: "Bearer " },
            verify_url: null,
            instructions: "",
            docs_url: null,
          });
          for (const pattern of [
            "(",
            "(?=tk)tk",
            "(?i)tk",
            "(?<name>tk)",
            "(tk)\\1",
            "a**",
            "a{,3}",
            "[]a]",
            "\\x{41}",
          ]) {
            const error = await failure(owner.workspaceApps.update(ws, app.id, { auth: withPattern(pattern) }));
            expect([error.status, Object.keys(error.fields ?? {})], pattern).toEqual([400, ["auth.inputs"]]);
          }
          const ok = await owner.workspaceApps.update(ws, app.id, { auth: withPattern("^(?:sk|rk)_[a-z0-9]{8,}$") });
          expect(ok.auth.type === "token" && ok.auth.inputs[0]?.pattern).toBe("^(?:sk|rk)_[a-z0-9]{8,}$");
        });

        it("won't remove a scope a configuration still requests", async () => {
          const app = await newOAuthApp("scoped");
          await owner.organizationApps.update(fx.organization, app.id, { auth: oauthTemplate });
          await owner.organizationApps.updateConfiguration(fx.organization, app.id, "sandbox", {
            oauth_scopes: ["read"],
          });
          const error = await failure(
            owner.organizationApps.update(fx.organization, app.id, {
              auth: { ...oauthTemplate, available_scopes: [] },
            }),
          );
          expect([error.status, error.code]).toEqual([409, "scope_in_use"]);
          // Once the configuration stops asking for it, it can go.
          await owner.organizationApps.updateConfiguration(fx.organization, app.id, "sandbox", { oauth_scopes: [] });
          const updated = await owner.organizationApps.update(fx.organization, app.id, {
            auth: { ...oauthTemplate, available_scopes: [] },
          });
          expect(updated.auth.type === "oauth2" && updated.auth.available_scopes).toEqual([]);
        });
      });

      it("never returns a stored client secret", async () => {
        const config = await owner.organizationApps.updateConfiguration(fx.organization, fx.apps.notion, "production", {
          mcp_server_url: "https://mcp.notion.example/mcp",
          oauth_client_id: "client",
          oauth_client_secret: "super-secret-value-1234",
        });
        expect(JSON.stringify(config)).not.toContain("super-secret-value");
        expect(config.oauth?.client_secret).toEqual(expect.objectContaining({ set: true, last4: "1234" }));
        expect(config.status).toBe("ready");
      });
    });

    describe("Connect", () => {
      const start = (return_url: string | null = null) =>
        owner.connectSessions.create(fx.workspaces.qaPlayground.id, {
          app_id: fx.apps.echoServer,
          intent: "create",
          label: "Echo 2",
          installation_id: null,
          return_url,
        });

      it("only returns to allowlisted URLs", async () => {
        const error = await failure(start("https://evil.example/callback"));
        expect(error.code).toBe("return_url_not_allowed");
        const ok = await start("http://localhost:3000/settings/integrations");
        expect(ok.status).toBe("pending");
      });

      it("completes once and refuses replay", async () => {
        const session = await start();
        const result = await owner.connect.submitCredentials(referenceOf(session), { values: { token: "abc123" } });
        expect(result.installation.workspace_id).toBe(fx.workspaces.qaPlayground.id);
        expect(result.session.status).toBe("completed");
        const replay = await failure(
          owner.connect.submitCredentials(referenceOf(session), { values: { token: "abc123" } }),
        );
        expect(replay.code).toBe("session_not_pending");
      });

      it("keeps the session open when the provider rejects a credential", async () => {
        const session = await start();
        const error = await failure(
          owner.connect.submitCredentials(referenceOf(session), { values: { token: "invalid" } }),
        );
        expect(error.code).toBe("credential_rejected");
        expect((await owner.connect.get(referenceOf(session))).status).toBe("pending");
      });

      it("gives every Connect session its hosted URL, never carrying its ID", async () => {
        const session = await start();
        expect(session.url).toMatch(/\/connect\/[0-9A-Za-z]{32}$/);
        expect(session.url).not.toContain(session.id);
      });
    });

    describe("isolation", () => {
      it("answers 404 and changes nothing when Larkspur uses Northwind's real IDs", async () => {
        await expectSealed(as("outsider"), await larkspur(), await northwind(), owner);
      });

      it("answers 404 and changes nothing when Northwind uses Larkspur's real IDs", async () => {
        await expectSealed(owner, await northwind(), await larkspur(), as("outsider"));
      });

      it("keeps one workspace's IDs out of another workspace of the same organization", async () => {
        const acme = await northwind();
        const beta = await betaLabs();
        await expectSealed(owner, acme, beta, owner, { sameOrganization: true });
        await expectSealed(owner, beta, acme, owner, { sameOrganization: true });
      });

      it("deletes one workspace and leaves the rest of its organization intact", async () => {
        const acme = await northwind();
        const rest = async () => {
          const w = acme.workspace.id;
          const agents = await owner.agents.list(w);
          return JSON.stringify([
            (await owner.workspaces.get(w)).stats,
            agents,
            await Promise.all(agents.map((a) => owner.grants.list(w, a.id))),
            (await owner.runs.list(w, { limit: 100 })).items.map((r) => r.id),
            await owner.installations.list(w),
            await owner.connectSessions.list(w),
            await owner.workspaceApps.list(w),
            // Install counts span workspaces, so the deleted one's installations rightly leave them.
            (await owner.organizationApps.list(fx.organization)).map(({ installation_count: _n, ...a }) => a),
            await owner.apiKeys.list(fx.organization),
            await owner.webhooks.list(fx.organization),
          ]);
        };
        const count = async () => (await owner.workspaces.list(fx.organization, { limit: 1 })).pagination.total;
        const [before, workspaces] = [await rest(), await count()];
        await owner.workspaces.delete(fx.workspaces.qaPlayground.id);
        expect(await rest()).toBe(before);
        expect(await count()).toBe((workspaces ?? 0) - 1);
        expect((await failure(owner.workspaces.get(fx.workspaces.qaPlayground.id))).status).toBe(404);
      });

      it("counts a workspace's own objects only", async () => {
        for (const slug of ["acme", "acme-sandbox", "beta-labs"]) {
          const workspace = await workspaceBySlug(slug);
          expect(workspace.stats.agents, slug).toBe((await owner.agents.list(workspace.id)).length);
          expect(workspace.stats.installations, slug).toBe((await owner.installations.list(workspace.id)).length);
          const runs = await owner.runs.list(workspace.id, { limit: 100 });
          expect(workspace.stats.runs_last_24h, slug).toBeLessThanOrEqual(runs.pagination.total ?? runs.items.length);
        }
      });

      it("keeps workspace apps off the organization's catalog routes", async () => {
        const acme = await northwind();
        const own = acme.workspaceApp.id;
        expect((await owner.organizationApps.list(fx.organization)).some((a) => a.id === own)).toBe(false);
        expect((await failure(owner.organizationApps.get(fx.organization, own))).status).toBe(404);
        expect(
          (await failure(owner.organizationApps.getConfiguration(fx.organization, own, "production"))).status,
        ).toBe(404);
      });

      it("keeps app slugs unique within their owner: the catalog or one workspace", async () => {
        const app = { kind: "mcp", name: "Tools", slug: "shared-tools", description: "", auth_type: "token" } as const;
        await owner.workspaceApps.create(fx.workspaces.qaPlayground.id, app);
        // Another workspace, and the catalog, may use the same slug.
        await owner.workspaceApps.create(fx.workspaces.acmeSandbox.id, app);
        await owner.organizationApps.create(fx.organization, app);
        const taken = await failure(owner.workspaceApps.create(fx.workspaces.qaPlayground.id, app));
        expect(taken.status).toBe(409);
        expect(taken.code).toBe("slug_taken");
      });

      it("treats an invited member as not yet a member", async () => {
        const outsider = as("outsider");
        const email = (await outsider.me.get()).user.email;
        const invitation = await owner.members.invite(fx.organization, { email, role: "owner" });
        expect([invitation.status, invitation.user.email]).toEqual(["invited", email]);
        const asInvitee = naming("outsider", () => fx.organization);
        expect((await failure(asInvitee.workspaces.get(fx.workspaces.acme.id))).status).toBe(404);
        expect((await failure(asInvitee.members.list(fx.organization))).status).toBe(404);
        expect((await failure(asInvitee.organizations.get(fx.organization))).status).toBe(404);
      });

      it("keeps membership rules inside the organization", async () => {
        // Someone who belongs to another organization can still be invited here.
        const email = (await as("outsider").me.get()).user.email;
        const invited = await owner.members.invite(fx.organization, { email, role: "developer" });
        expect(invited.organization_id).toBe(fx.organization);
        // Another organization's owners don't count towards this one's last owner.
        const me = (await owner.me.get()).user.id;
        const self = must(
          (await owner.members.list(fx.organization)).find((m) => m.user.id === me),
          "the owner's membership",
        );
        expect((await failure(owner.members.update(fx.organization, self.id, { role: "admin" }))).code).toBe(
          "last_owner",
        );
        expect((await failure(owner.members.remove(fx.organization, self.id))).code).toBe("last_owner");
      });

      it("keeps uniqueness, settings and Connect rules inside each organization", async () => {
        const other = fx.otherOrganization;
        const r = as("outsider");
        // Slugs another organization already uses are free here.
        const appSlug = await r.organizationApps.create(other, {
          kind: "mcp",
          name: "Google Workspace",
          slug: "google-workspace",
          description: "",
          auth_type: "token",
        });
        expect(appSlug.slug).toBe("google-workspace");
        const workspaceSlug = await r.workspaces.create(other, {
          name: "Acme",
          slug: "acme",
          environment: "sandbox",
          external_id: null,
        });
        expect(workspaceSlug.slug).toBe("acme");
        // Another organization's return-URL allowlist doesn't apply here.
        const allowedElsewhere = must(
          (await owner.connectSettings.get(fx.organization, "sandbox")).return_urls[0],
          "Northwind Sandbox return URL",
        );
        const harborSandbox = (await larkspur()).sandbox.id;
        const linear = must(
          (await r.organizationApps.list(other)).find((a) => a.slug === "linear"),
          "Larkspur's linear app",
        );
        const refused = await failure(
          r.connectSessions.create(harborSandbox, {
            app_id: linear.id,
            intent: "create",
            label: "x",
            installation_id: null,
            return_url: allowedElsewhere,
          }),
        );
        expect(refused.code).toBe("return_url_not_allowed");
        // Writing one organization's settings never touches another's.
        const before = JSON.stringify(await owner.connectSettings.get(fx.organization, "sandbox"));
        await r.connectSettings.update(other, "sandbox", {
          return_urls: ["https://app.larkspur.example/back"],
          allowed_origins: [],
        });
        expect(JSON.stringify(await owner.connectSettings.get(fx.organization, "sandbox"))).toBe(before);
        // And each organization's settings name only its own.
        const environments = ["sandbox", "production"] as const;
        const theirs = await Promise.all(environments.map((env) => r.connectSettings.get(other, env)));
        expect(theirs[0]?.return_urls).toEqual(["https://app.larkspur.example/back"]);
        // None of Northwind's own values appear in Larkspur's settings.
        const northwindValues = (
          await Promise.all(environments.map((env) => owner.connectSettings.get(fx.organization, env)))
        ).flatMap((settings) => [...settings.return_urls, ...settings.allowed_origins]);
        expect(northwindValues.length).toBeGreaterThan(0);
        for (const value of northwindValues) expect(JSON.stringify(theirs)).not.toContain(value);
      });

      it("counts usage within its own organization", async () => {
        const usage = () => as("outsider").organizations.usage(fx.otherOrganization, "sandbox");
        const own = () => owner.organizations.usage(fx.organization, "sandbox");
        const before = JSON.stringify(await usage());
        const ownBefore = (await own()).totals.runs;
        const sandbox = fx.workspaces.acmeSandbox.id;
        const agent = must(
          (await owner.agents.list(sandbox)).find((a) => a.status === "active"),
          "active agent in acme-sandbox",
        );
        await owner.runs.create(sandbox, agent.id, { input: "Hello" });
        // The run counts where it belongs, and nowhere else.
        expect((await own()).totals.runs).toBe(ownBefore + 1);
        expect(JSON.stringify(await usage())).toBe(before);
      });

      it("lists runs across workspaces of one environment only", async () => {
        const runs = await owner.organizationRuns.list(fx.organization, { environment: "sandbox", limit: 100 });
        expect(runs.items.length).toBeGreaterThan(0);
        expect(runs.items.every((r) => r.workspace.environment === "sandbox")).toBe(true);
      });
    });

    describe("grants, runs and approvals", () => {
      it("cannot grant an installation from another workspace", async () => {
        const acme = await northwind();
        const beta = must(
          (await owner.installations.list((await workspaceBySlug("beta-labs")).id))[0],
          "installation in beta-labs",
        );
        const error = await failure(
          owner.grants.create(acme.workspace.id, acme.agent.id, {
            installation_id: beta.id,
            tools: acme.grant.tools.slice(0, 1),
            approval_required: [],
          }),
        );
        expect(error.status).toBe(404);
      });

      it("rejects tools the app doesn't have and approvals outside the grant", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const agent = await owner.agents.create(qa, {
          name: "Probe",
          description: "",
          model: await anyModel(),
          instructions: "",
        });
        const installation = must(
          (await owner.installations.list(qa)).find((i) => i.status === "connected"),
          "connected installation in qa-playground",
        );
        const unknown = await failure(
          owner.grants.create(qa, agent.id, {
            installation_id: installation.id,
            tools: ["rm_rf"],
            approval_required: [],
          }),
        );
        expect(unknown.code).toBe("unknown_tool");
        const outside = await failure(
          owner.grants.create(qa, agent.id, {
            installation_id: installation.id,
            tools: ["echo"],
            approval_required: ["echo_write"],
          }),
        );
        expect(outside.code).toBe("approval_outside_grant");
      });

      it("does not read another workspace's run through this workspace's path", async () => {
        const error = await failure(owner.runs.get(fx.workspaces.acme.id, (await betaLabs()).run.id));
        expect(error.status).toBe(404);
      });

      it("refuses runs for a paused agent", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const agent = await owner.agents.create(qa, {
          name: "Paused",
          description: "",
          model: await anyModel(),
          instructions: "",
        });
        await owner.agents.update(qa, agent.id, { status: "paused" });
        const error = await failure(owner.runs.create(qa, agent.id, { input: "hi" }));
        expect(error.code).toBe("agent_paused");
      });

      it("never lets the Console act on Production accounts", async () => {
        const acme = fx.workspaces.acme.id;
        const agent = must(
          (await owner.agents.list(acme)).find((a) => a.status === "active"),
          "active agent in acme",
        );
        const run = await failure(owner.runs.create(acme, agent.id, { input: "hi" }));
        expect(run.status).toBe(403);
        expect(run.code).toBe("console_read_only_in_production");
        const connect = await failure(
          owner.connectSessions.create(acme, {
            app_id: fx.apps.googleWorkspace,
            intent: "create",
            label: "Sales",
            installation_id: null,
            return_url: null,
          }),
        );
        expect(connect.code).toBe("console_read_only_in_production");
      });

      it("refuses approvals from a Console session", async () => {
        const pages = await Promise.all(
          (["production", "sandbox"] as const).map((environment) =>
            owner.organizationRuns.list(fx.organization, { environment, status: "waiting_for_approval" }),
          ),
        );
        const waiting = must(pages.flatMap((page) => page.items)[0], "run waiting for approval");
        const events = await owner.runs.events(waiting.workspace_id, waiting.id);
        const requested = [...events].reverse().find((e) => e.type === "approval.requested");
        const callId = must(requested?.type === "approval.requested" ? requested.data.call_id : null, "pending call");
        const error = await failure(
          owner.runs.resolveApproval(waiting.workspace_id, waiting.id, { call_id: callId, decision: "approved" }),
        );
        expect(error.status).toBe(403);
        expect(error.code).toBe("approval_requires_api_key");
        expect((await owner.runs.get(waiting.workspace_id, waiting.id)).status).toBe("waiting_for_approval");
      });
    });

    describe("Connect sessions", () => {
      const qa = () => fx.workspaces.qaPlayground.id;
      const create = (extra: Partial<CreateConnectSessionInput> = {}) =>
        owner.connectSessions.create(qa(), {
          app_id: fx.apps.echoServer,
          intent: "create",
          label: "Echo X",
          installation_id: null,
          return_url: null,
          ...extra,
        });

      it("refuses an installation on a create session, including another workspace's", async () => {
        const beta = await betaLabs();
        for (const installation_id of [beta.installation.id, "inst_garbage" as const]) {
          const error = await failure(create({ installation_id }));
          expect(error.status).toBe(400);
        }
      });

      it("keeps Connect sessions inside their workspace", async () => {
        const beta = await betaLabs();
        // A reconnect can't name another workspace's installation.
        const reconnect = await failure(create({ intent: "reconnect", installation_id: beta.installation.id }));
        expect(reconnect.status).toBe(404);
        // Another workspace's session isn't readable through this workspace's path.
        expect((await failure(owner.connectSessions.get(qa(), beta.session.id))).status).toBe(404);
      });

      it("authorizes hosted Connect by the reference, never by the session ID", async () => {
        const session = await create();
        const reference = referenceOf(session);
        expect((await owner.connect.get(reference)).id).toBe(session.id);
        // The workspace's own view reads the session by ID, as the Console does while it waits.
        expect((await owner.connectSessions.get(qa(), session.id)).status).toBe("pending");
        await owner.connect.submitCredentials(reference, { values: { token: "abc123" } });
        expect((await owner.connectSessions.get(qa(), session.id)).status).toBe("completed");
        const listed = await owner.connectSessions.list(qa());
        expect(listed.some((s) => s.id === session.id && s.status === "completed")).toBe(true);
        // Only the creating response carries the link; reads never hand the reference out again.
        expect(JSON.stringify(listed)).not.toContain(reference);
        expect(JSON.stringify(await owner.connectSessions.get(qa(), session.id))).not.toContain(reference);
        expect(JSON.stringify(await owner.connect.get(reference))).not.toMatch(/"url"/);
        expect((await failure(owner.connect.get(session.id))).status).toBe(404);
        expect(referenceOf(await create())).not.toBe(reference);
      });
    });

    describe("idempotency", () => {
      const agentInput = async (name: string) => ({
        name,
        description: "",
        instructions: "Be brief.",
        model: await anyModel(),
      });

      it("replays a repeated POST instead of running it twice", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const input = await agentInput("Once");
        const first = await owner.agents.create(qa, input, { idempotencyKey: "suite-replay-1" });
        const again = await owner.agents.create(qa, input, { idempotencyKey: "suite-replay-1" });
        expect(again).toEqual(first);
        expect((await owner.agents.list(qa)).filter((a) => a.name === "Once")).toHaveLength(1);
        // A new key is a new request.
        const other = await owner.agents.create(qa, input, { idempotencyKey: "suite-replay-2" });
        expect(other.id).not.toBe(first.id);
      });

      it("replays a saved error too", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const bad = { ...(await agentInput("")), name: "" };
        const first = await failure(owner.agents.create(qa, bad, { idempotencyKey: "suite-error-1" }));
        const again = await failure(owner.agents.create(qa, bad, { idempotencyKey: "suite-error-1" }));
        expect(again.status).toBe(first.status);
        expect([again.code, again.fields]).toEqual([first.code, first.fields]);
      });

      it("refuses a key reused for a different request", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        await owner.agents.create(qa, await agentInput("First"), { idempotencyKey: "suite-reused" });
        const error = await failure(
          owner.agents.create(qa, await agentInput("Second"), { idempotencyKey: "suite-reused" }),
        );
        expect([error.status, error.code]).toEqual([422, "idempotency_key_reused"]);
        expect((await owner.agents.list(qa)).some((a) => a.name === "Second")).toBe(false);
      });

      it("refuses a malformed key, after the credential", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const long = "k".repeat(256);
        const error = await failure(owner.agents.create(qa, await agentInput("Long"), { idempotencyKey: long }));
        expect([error.status, error.code]).toEqual([400, "invalid_idempotency_key"]);
        const anonymous = await failure(as(null).agents.create(qa, await agentInput("Long"), { idempotencyKey: long }));
        expect(anonymous.status).toBe(401);
      });

      it("never shows a secret twice: a retry answers 409 and the resource is created once", async () => {
        const name = "Suite once";
        const input = { name, environment: "sandbox" } as const;
        const created = await owner.apiKeys.create(fx.organization, input, { idempotencyKey: "suite-secret" });
        const error = await failure(owner.apiKeys.create(fx.organization, input, { idempotencyKey: "suite-secret" }));
        expect([error.status, error.code]).toEqual([409, "idempotency_key_completed"]);
        expect(JSON.stringify(error)).not.toContain(created.secret);
        expect((await owner.apiKeys.list(fx.organization)).filter((k) => k.name === name)).toHaveLength(1);
      });

      it("replays an error from a secret-once route, rather than calling it completed", async () => {
        const developer = as("developer");
        const input = { name: "Not allowed", environment: "production" } as const;
        const first = await failure(
          developer.apiKeys.create(fx.organization, input, { idempotencyKey: "suite-secret-error" }),
        );
        const again = await failure(
          developer.apiKeys.create(fx.organization, input, { idempotencyKey: "suite-secret-error" }),
        );
        expect(first.status).toBe(403);
        expect([again.status, again.code]).toEqual([first.status, first.code]);
      });

      it("keeps a key to the caller that sent it", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const input = await agentInput("Shared key");
        const mine = await owner.agents.create(qa, input, { idempotencyKey: "suite-shared" });
        // Another member sending the same key and request gets their own answer, never the owner's.
        const theirs = await as("developer").agents.create(qa, input, { idempotencyKey: "suite-shared" });
        expect(theirs.id).not.toBe(mine.id);
        // And a key someone used elsewhere replays nothing to an outsider: their request is refused on its own merits.
        const outsider = await failure(as("outsider").agents.create(qa, input, { idempotencyKey: "suite-shared" }));
        expect([outsider.status, outsider.code]).toEqual([404, "not_found"]);
      });

      it("keeps a key to its workspace: the same key elsewhere is a new request", async () => {
        const [a, b] = [fx.workspaces.qaPlayground.id, fx.workspaces.acmeSandbox.id];
        const input = await agentInput("Per workspace");
        const inA = await owner.agents.create(a, input, { idempotencyKey: "suite-per-workspace" });
        const inB = await owner.agents.create(b, input, { idempotencyKey: "suite-per-workspace" });
        // Never workspace A's answer replayed into workspace B.
        expect(inB.workspace_id).toBe(b);
        expect(inB.id).not.toBe(inA.id);
        expect((await owner.agents.list(b)).filter((x) => x.name === "Per workspace")).toHaveLength(1);
        expect((await owner.agents.list(a)).filter((x) => x.name === "Per workspace")).toHaveLength(1);
      });

      it("never shows a Connect authorization twice", async () => {
        const session = await owner.connectSessions.create(fx.workspaces.acmeSandbox.id, {
          app_id: fx.apps.googleWorkspace,
          intent: "create",
          label: "OAuth retry",
          installation_id: null,
          return_url: null,
        });
        await owner.connect.authorize(referenceOf(session), { idempotencyKey: "suite-authorize" });
        const again = await failure(
          owner.connect.authorize(referenceOf(session), { idempotencyKey: "suite-authorize" }),
        );
        expect([again.status, again.code]).toEqual([409, "idempotency_key_completed"]);
        // A fresh key asks again, as a retry should.
        expect((await owner.connect.authorize(referenceOf(session))).authorization_url).toBeTruthy();
      });

      it("replays hosted Connect to the same session", async () => {
        const session = await owner.connectSessions.create(fx.workspaces.qaPlayground.id, {
          app_id: fx.apps.echoServer,
          intent: "create",
          label: "Echo retry",
          installation_id: null,
          return_url: null,
        });
        const values = { values: { token: "abc123" } };
        const first = await owner.connect.submitCredentials(referenceOf(session), values, {
          idempotencyKey: "suite-c",
        });
        // Without the key this would be session_not_pending; with it, the retry sees the same result.
        const again = await owner.connect.submitCredentials(referenceOf(session), values, {
          idempotencyKey: "suite-c",
        });
        expect(again).toEqual(first);
        const installs = await owner.installations.list(fx.workspaces.qaPlayground.id);
        expect(installs.filter((i) => i.id === first.installation.id)).toHaveLength(1);
      });
    });

    describe("the viewer role", () => {
      it("reads what a developer reads", async () => {
        const reads = (c: ZupflowClient) => async () => {
          const acme = fx.workspaces.acme.id;
          const agents = await c.agents.list(acme);
          // An API error is an answer to compare; anything else (an operation the
          // server doesn't serve, a network failure) ends the case instead.
          const settled = async (p: Promise<unknown>) =>
            p.then(
              (v) => v,
              (e: unknown) => {
                if (e instanceof ZupflowApiError) return e.code;
                throw e;
              },
            );
          return JSON.stringify(
            await Promise.all([
              settled(c.workspaces.list(fx.organization, { limit: 100 })),
              settled(c.workspaces.get(acme)),
              agents,
              settled(Promise.all(agents.map((a) => c.grants.list(acme, a.id)))),
              settled(c.runs.list(acme, { limit: 100 })),
              settled(c.installations.list(acme)),
              settled(c.connectSessions.list(acme)),
              settled(c.organizationApps.list(fx.organization)),
              settled(c.workspaceApps.list(acme)),
              settled(c.workspaceApps.available(fx.workspaces.acmeSandbox.id)),
              settled(c.apiKeys.list(fx.organization)),
              settled(c.webhooks.list(fx.organization)),
              settled(c.connectSettings.get(fx.organization, "sandbox")),
              settled(c.organizations.usage(fx.organization, "production")),
              settled(c.organizationRuns.list(fx.organization, { environment: "production", limit: 100 })),
              settled(c.audit.list(fx.organization)),
            ]),
          );
        };
        expect(await reads(as("viewer"))()).toBe(await reads(as("developer"))());
        expect((await as("viewer").members.list(fx.organization)).length).toBeGreaterThan(0);
        const me = await as("viewer").me.get();
        expect(me.organizations.find((o) => o.id === fx.organization)?.role).toBe("viewer");
      });

      /** Every change a Console member can make, by operation, aimed at real objects of the tenant. */
      const changesBy = (c: ZupflowClient, t: Tenant, sandboxKey: ApiKeyId): [string, () => Promise<unknown>][] => {
        const w = t.workspace.id;
        const sandbox = t.sandbox.id;
        const agentInput = { name: "x", description: "", instructions: "", model: t.agent.model };
        const app = { kind: "mcp", name: "x", slug: "viewer-x", description: "", auth_type: "token" } as const;
        return [
          ["updateOrganization", () => c.organizations.update(t.org, { name: "Viewed" })],
          ["inviteMember", () => c.members.invite(t.org, { email: "v@northwind.example", role: "developer" })],
          ["updateMember", () => c.members.update(t.org, t.member.id, { role: "viewer" })],
          ["removeMember", () => c.members.remove(t.org, t.member.id)],
          [
            "createWorkspace",
            () =>
              c.workspaces.create(t.org, {
                name: "x",
                slug: "viewer-x",
                environment: "sandbox",
                external_id: null,
              }),
          ],
          ["updateWorkspace", () => c.workspaces.update(w, { name: "Viewed" })],
          ["deleteWorkspace", () => c.workspaces.delete(sandbox)],
          ["createOrganizationApp", () => c.organizationApps.create(t.org, app)],
          ["updateOrganizationApp", () => c.organizationApps.update(t.org, t.orgApp.id, { name: "Viewed" })],
          [
            "updateOrganizationAppConfiguration",
            () => c.organizationApps.updateConfiguration(t.org, t.orgApp.id, "sandbox", { oauth_client_id: "x" }),
          ],
          ["createWorkspaceApp", () => c.workspaceApps.create(w, app)],
          ["updateWorkspaceApp", () => c.workspaceApps.update(w, t.workspaceApp.id, { name: "Viewed" })],
          [
            "updateWorkspaceAppConfiguration",
            () => c.workspaceApps.updateConfiguration(w, t.workspaceApp.id, { mcp_server_url: null }),
          ],
          ["updateInstallation", () => c.installations.update(w, t.installation.id, { label: "Viewed" })],
          ["disconnectInstallation", () => c.installations.disconnect(w, t.installation.id)],
          ["deleteInstallation", () => c.installations.delete(w, t.installation.id)],
          [
            "createConnectSession",
            () =>
              c.connectSessions.create(sandbox, {
                app_id: t.orgApp.id,
                intent: "create",
                label: "x",
                installation_id: null,
                return_url: null,
              }),
          ],
          // Changes first and removals last, so each change still finds what it changes.
          ["createAgent", () => c.agents.create(w, agentInput)],
          ["updateAgent", () => c.agents.update(w, t.agent.id, { name: "Changed" })],
          [
            "createGrant",
            () =>
              c.grants.create(w, t.agent.id, {
                installation_id: t.installation.id,
                tools: t.grant.tools.slice(0, 1),
                approval_required: [],
              }),
          ],
          [
            "updateGrant",
            () => c.grants.update(w, t.agent.id, t.grant.id, { tools: t.grant.tools, approval_required: [] }),
          ],
          ["startRun", () => c.runs.create(w, t.agent.id, { input: "hi" })],
          ["cancelRun", () => c.runs.cancel(w, t.run.id)],
          ["deleteGrant", () => c.grants.delete(w, t.agent.id, t.grant.id)],
          ["deleteAgent", () => c.agents.delete(w, t.agent.id)],
          [
            "updateConnectSettings",
            () => c.connectSettings.update(t.org, "sandbox", { return_urls: [], allowed_origins: [] }),
          ],
          ["createApiKey", () => c.apiKeys.create(t.org, { name: "x", environment: "sandbox" })],
          ["revokeApiKey", () => c.apiKeys.revoke(t.org, sandboxKey)],
          [
            "createWebhook",
            () =>
              c.webhooks.create(t.org, {
                environment: "sandbox",
                url: "https://hooks.northwind.example/viewer",
                description: "x",
                events: ["run.completed"],
              }),
          ],
          ["updateWebhook", () => c.webhooks.update(t.org, t.hook.id, { enabled: false })],
          ["deleteWebhook", () => c.webhooks.delete(t.org, t.hook.id)],
          ["testWebhook", () => c.webhooks.sendTest(t.org, t.hook.id)],
        ];
      };

      /** Every change a Console member could make: the route table's, minus creating one's own organization. */
      const MUTATING = routes
        .filter((r) => r.method !== "get" && r.callers.includes("console") && r.operationId !== "createOrganization")
        .map((r) => r.operationId);
      /** A Sandbox key, so a developer's revoke isn't refused for being Production's. */
      const sandboxKey = async () =>
        (await owner.apiKeys.create(fx.organization, { name: "Revocable", environment: "sandbox" })).key.id;

      it("changes nothing: every change answers 403 and leaves the organization as it was", async () => {
        const t = await northwind();
        const changes = changesBy(as("viewer"), t, await sandboxKey());
        // Every change a Console member could make is tried; only creating one's own organization is open to all.
        expect(changes.map(([id]) => id).sort()).toEqual([...MUTATING].sort());

        const before = await snapshotOf(owner, t);
        for (const [what, change] of changes) {
          const error = await failure(change());
          expect(`${error.status} ${error.code}`, what).toBe("403 forbidden");
        }
        expect(await snapshotOf(owner, t), "the organization after the viewer's attempts").toBe(before);
      });

      it("leaves developers every change but the admins' and owners'", async () => {
        // The admins' and owners' operations, named so a check made stricter than the role allows fails here.
        const ADMIN_ONLY = new Set([
          "updateOrganization",
          "inviteMember",
          "updateMember",
          "removeMember",
          "createWorkspace",
          "updateWorkspace",
          "deleteWorkspace",
          "createOrganizationApp",
          "updateOrganizationApp",
          "updateOrganizationAppConfiguration",
          "createWorkspaceApp",
          "updateWorkspaceApp",
          "updateWorkspaceAppConfiguration",
          "deleteInstallation",
          "updateConnectSettings",
          "createWebhook",
          "updateWebhook",
          "deleteWebhook",
          "testWebhook",
        ]);
        const t = await northwind();
        const changes = changesBy(as("developer"), t, await sandboxKey());
        expect(changes.map(([id]) => id).sort()).toEqual([...MUTATING].sort());
        for (const [what, change] of changes) {
          let code = "succeeded";
          try {
            await change();
          } catch (error) {
            if (!(error instanceof ZupflowApiError)) throw error;
            code = error.code;
          }
          // A developer's own change may be refused by a product rule (Production's read-only one, a grant that
          // already exists), never by the role, and never by failing to find what the fixture holds.
          if (ADMIN_ONLY.has(what)) expect(code, what).toBe("forbidden");
          else expect(["forbidden", "not_found"], what).not.toContain(code);
        }
      });

      it("can be given: an owner invites a viewer and makes a developer one", async () => {
        const invited = await owner.members.invite(fx.organization, {
          email: "viewer@northwind.example",
          role: "viewer",
        });
        expect([invited.role, invited.status]).toEqual(["viewer", "invited"]);
        const developerId = (await as("developer").me.get()).user.id;
        const developer = must(
          (await owner.members.list(fx.organization)).find((m) => m.user.id === developerId),
          "the developer's membership",
        );
        expect((await owner.members.update(fx.organization, developer.id, { role: "viewer" })).role).toBe("viewer");
        const qa = fx.workspaces.qaPlayground.id;
        const error = await failure(
          as("developer").agents.create(qa, { name: "x", description: "", instructions: "", model: await anyModel() }),
        );
        expect(`${error.status} ${error.code}`).toBe("403 forbidden");
      });
    });

    describe("webhooks", () => {
      it("subscribes to every event type, and never to the test button's", async () => {
        const input = {
          environment: "sandbox" as const,
          url: "https://hooks.northwind.example/all",
          description: "Everything",
          events: [...WEBHOOK_EVENT_TYPES],
        };
        const created = await owner.webhooks.create(fx.organization, input);
        expect([...created.endpoint.events].sort()).toEqual([...WEBHOOK_EVENT_TYPES].sort());
        expect(created.signing_secret.length).toBeGreaterThan(0);
        const test = await failure(
          owner.webhooks.create(fx.organization, {
            ...input,
            // @ts-expect-error: webhook.test is sent by the test button only
            events: ["webhook.test"],
          }),
        );
        expect([test.status, test.code]).toEqual([400, "validation_failed"]);
        const update = await failure(
          // @ts-expect-error: nor can an update add it
          owner.webhooks.update(fx.organization, created.endpoint.id, { events: ["webhook.test"] }),
        );
        expect([update.status, update.code]).toEqual([400, "validation_failed"]);
      });
    });
  });
}
