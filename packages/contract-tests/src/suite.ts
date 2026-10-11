/**
 * The contract suite: black-box cases any server must pass, reaching it only
 * over HTTP. Every response is also checked against the contract, and one
 * case drives every operation to a checked success.
 *
 * Call `contractSuite(target)` from a test file; see `Target` for what a
 * server provides and the reference fixture it loads.
 */
import { INVITATION_TTL_DAYS, matchRoute, routes, WEBHOOK_EVENT_TYPES } from "@zupflow/contract";
import {
  type AgentId,
  type ApiKeyId,
  type AppId,
  type AuthTemplate,
  type CreateConnectSessionInput,
  createClient,
  createHttpTransport,
  type MemberId,
  type OrganizationId,
  type WebhookDeliveryId,
  type WebhookDeliverySummary,
  type WebhookEndpoint,
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
    /** A client acting with an API key's secret, in the reference organization. */
    const withKey = (secret: string): ZupflowClient => {
      if (target.apiKeys === false) throw new NotServed("API-key credentials");
      return createClient(
        createHttpTransport({
          baseUrl: target.baseUrl,
          getToken: async () => secret,
          getOrganization: () => fx?.organization ?? null,
          fetch: servedFetch,
          retries: 0,
        }),
      );
    };
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
        await as(null).signIn.settings();
        await owner.me.get();
        await owner.organizations.create({ name: "Contract Co", slug: "contract-co" });
        await owner.organizations.get(org);
        await owner.organizations.update(org, { name: "Northwind" });
        await owner.members.list(org);
        const invited = await owner.members.invite(org, { email: "contract@northwind.example", role: "developer" });
        await owner.members.update(org, invited.id, { role: "admin" });
        await owner.members.remove(org, invited.id);
        // Invitations are answered by the person invited: the outsider declines one, then accepts another.
        const outsider = as("outsider");
        const outsiderEmail = (await outsider.me.get()).user.email;
        const declined = await owner.members.invite(org, { email: outsiderEmail, role: "viewer" });
        await outsider.invitations.list();
        await outsider.invitations.decline(declined.id);
        const accepted = await owner.members.invite(org, { email: outsiderEmail, role: "viewer" });
        await outsider.invitations.accept(accepted.id);
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
        const connected = must(
          (await owner.installations.list(sandbox.id)).find((i) => i.status === "connected"),
          "connected installation in acme-sandbox",
        );
        await owner.installations.tools(sandbox.id, connected.id);
        await owner.installations.refreshTools(sandbox.id, connected.id);
        const reviewed = must(
          (await owner.installations.list(sandbox.id)).find((i) => i.app.id === fx.apps.linear),
          "installation of linear in acme-sandbox",
        );
        const changed = must(
          (await owner.installations.tools(sandbox.id, reviewed.id, { limit: 100 })).items.find(
            (t) => t.status === "changed",
          ),
          "changed tool in acme-sandbox",
        );
        await owner.installations.acceptToolChange(sandbox.id, reviewed.id, {
          name: changed.name,
          latest_hash: must(changed.latest_hash, "latest hash"),
        });
        const echo = must((await owner.installations.list(qa.id))[0], "installation in qa-playground");
        await owner.installations.disconnect(qa.id, echo.id);
        await owner.installations.delete(qa.id, echo.id);

        // Connect: settings, then sessions, then the hosted page, token and OAuth.
        await owner.connectSettings.get(org, "sandbox");
        await owner.connectSettings.update(org, "sandbox", {
          return_urls: ["http://localhost:3000/settings/integrations"],
          allowed_origins: [],
        });
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
        await owner.connect.cancel(referenceOf(await session(fx.apps.echoServer)));
        await owner.connect.submitCredentials(referenceOf(tokenSession), { values: { token: "abc" } });
        await owner.connect.authorize(referenceOf(await session(fx.apps.googleWorkspace, sandbox.id)));

        // The tool policy and a catalog app's levels.
        const policy = await owner.toolPolicy.get(org);
        await owner.toolPolicy.update(org, { mode: policy.mode, new_tool_level: policy.new_tool_level });
        await owner.toolPolicy.levels(org, fx.apps.googleWorkspace);
        await owner.toolPolicy.setLevels(org, fx.apps.googleWorkspace, {
          levels: [{ tool: "search_files", level: "allow" }],
        });

        // Models, agents and their grants.
        await owner.models.list();
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

        // Runs, then the rest of the API.
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
        await owner.organizations.usage(org, "production");

        // Organization settings.
        await owner.webhooks.list(org);
        const hook = await owner.webhooks.create(org, {
          environment: "sandbox",
          url: "https://hooks.northwind.example/zupflow",
          description: "Contract",
          events: ["run.completed"],
        });
        await owner.webhooks.update(org, hook.endpoint.id, { enabled: false });
        await owner.webhooks.sendTest(org, hook.endpoint.id);
        const delivery = must((await owner.webhooks.deliveries(org, hook.endpoint.id)).items[0], "test delivery");
        await owner.webhooks.delivery(org, hook.endpoint.id, delivery.id);
        await owner.webhooks.deliveryAttempts(org, hook.endpoint.id, delivery.id);
        await owner.webhooks.delete(org, hook.endpoint.id);
        await owner.audit.list(org);
        await owner.workspaces.delete(workspace.id);
      }
    });

    describe("authentication and membership", () => {
      it("tells anyone how the Console signs in, with public values only", async () => {
        const forged = createClient(
          createHttpTransport({
            baseUrl: target.baseUrl,
            getToken: async () => "not-a-token-this-server-issued",
            getOrganization: () => null,
            fetch: servedFetch,
            retries: 0,
          }),
        );
        const settings = await as(null).signIn.settings();
        // The same answer signed out, signed in, and with a token the server never issued:
        // it depends on the deployment, never on the caller.
        expect(await as("owner").signIn.settings()).toEqual(settings);
        expect(await as("outsider").signIn.settings()).toEqual(settings);
        expect(await forged.signIn.settings()).toEqual(settings);
        if (settings.mode === "oidc") {
          const issuer = new URL(settings.issuer);
          const loopback = ["localhost", "127.0.0.1", "[::1]"].includes(issuer.hostname);
          expect(issuer.protocol === "https:" || (loopback && issuer.protocol === "http:"), settings.issuer).toBe(true);
          expect(settings.client_id.trim()).not.toBe("");
        }
      });

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

    describe("invitations", () => {
      /** The outsider, invited to Northwind by the owner: someone who signs in and belongs to it only once they accept. */
      const inviteOutsider = async (role: "owner" | "admin" | "developer" | "viewer" = "developer") => {
        const email = (await as("outsider").me.get()).user.email;
        return owner.members.invite(fx.organization, { email, role });
      };
      const inNorthwind = async (persona: Persona) =>
        (await as(persona).me.get()).organizations.find((o) => o.id === fx.organization);

      it("shows an invitation to the invited person only, who joins nothing until they accept", async () => {
        const invited = await inviteOutsider();
        const ownerUser = (await owner.me.get()).user;
        const listed = must(
          (await as("outsider").invitations.list()).find((i) => i.id === invited.id),
          "the outsider's invitation",
        );
        expect(listed.organization).toEqual({ id: fx.organization, name: expect.any(String), slug: "northwind" });
        expect([listed.role, listed.status, listed.invited_by.email]).toEqual([
          "developer",
          "pending",
          ownerUser.email,
        ]);
        expect(Date.parse(listed.expires_at) - Date.parse(listed.invited_at)).toBe(INVITATION_TTL_DAYS * 86_400_000);
        // Nobody else sees it: not the inviter, not another member.
        for (const persona of ["owner", "developer", "viewer"] as const) {
          expect(
            (await as(persona).invitations.list()).map((i) => i.id),
            persona,
          ).not.toContain(invited.id);
        }
        // Signing in with the invited address joins nothing.
        expect(await inNorthwind("outsider")).toBeUndefined();
        const acme = fx.workspaces.acme.id;
        expect((await failure(naming("outsider", () => fx.organization).workspaces.get(acme))).status).toBe(404);
      });

      it("joins on accept, with the invited role, and answers a second accept or decline with 409", async () => {
        const invited = await inviteOutsider();
        const outsider = as("outsider");
        const joined = await outsider.invitations.accept(invited.id, { idempotencyKey: "suite-accept" });
        expect([joined.id, joined.role]).toEqual([fx.organization, "developer"]);
        // The same key replays the answer instead of running again.
        expect(await outsider.invitations.accept(invited.id, { idempotencyKey: "suite-accept" })).toEqual(joined);
        expect((await inNorthwind("outsider"))?.role).toBe("developer");
        const member = must(
          (await owner.members.list(fx.organization)).find((m) => m.id === invited.id),
          "the outsider's membership",
        );
        expect([member.status, member.expires_at]).toEqual(["active", null]);
        expect(member.joined_at).not.toBeNull();
        // A member now: Northwind's workspaces open to them.
        const acme = fx.workspaces.acme.id;
        expect((await naming("outsider", () => fx.organization).workspaces.get(acme)).id).toBe(acme);
        expect((await outsider.invitations.list()).map((i) => i.id)).not.toContain(invited.id);
        for (const again of [
          () => outsider.invitations.accept(invited.id),
          () => outsider.invitations.decline(invited.id),
        ]) {
          const error = await failure(again());
          expect(`${error.status} ${error.code}`).toBe("409 invitation_accepted");
        }
      });

      it("accepts the role the invitation holds when accepted", async () => {
        const invited = await inviteOutsider("viewer");
        await owner.members.update(fx.organization, invited.id, { role: "admin" });
        expect((await as("outsider").invitations.accept(invited.id)).role).toBe("admin");
      });

      it("answers anyone but the invited person with the same 404 as a missing invitation", async () => {
        const invited = await inviteOutsider();
        const developerId = (await as("developer").me.get()).user.id;
        const developerMembership = must(
          (await owner.members.list(fx.organization)).find((m) => m.user.id === developerId),
          "the developer's membership",
        );
        for (const persona of ["owner", "developer", "viewer"] as const) {
          for (const call of [
            () => as(persona).invitations.accept(invited.id),
            () => as(persona).invitations.decline(invited.id),
          ]) {
            const error = await failure(call());
            expect(`${error.status} ${error.code}`, persona).toBe("404 not_found");
          }
        }
        // Someone else's membership, a malformed ID and a bare UUID are no different.
        for (const id of [developerMembership.id, "mem_short", uuidOf(invited.id)]) {
          const error = await failure(as("outsider").invitations.accept(id as typeof invited.id));
          expect(`${error.status} ${error.code}`, id).toBe("404 not_found");
        }
        expect((await failure(as(null).invitations.list())).status).toBe(401);
        expect((await failure(as(null).invitations.accept(invited.id))).status).toBe(401);
        // Nothing changed: the invitation still waits for the outsider.
        const still = must(
          (await owner.members.list(fx.organization)).find((m) => m.id === invited.id),
          "the invitation",
        );
        expect(still.status).toBe("invited");
        expect((await as("outsider").invitations.list()).map((i) => i.id)).toContain(invited.id);
      });

      it("withdraws an invitation on decline, which can then be sent again", async () => {
        const invited = await inviteOutsider();
        const outsider = as("outsider");
        await outsider.invitations.decline(invited.id);
        expect((await outsider.invitations.list()).map((i) => i.id)).not.toContain(invited.id);
        expect((await owner.members.list(fx.organization)).map((m) => m.id)).not.toContain(invited.id);
        expect(await inNorthwind("outsider")).toBeUndefined();
        for (const call of [
          () => outsider.invitations.accept(invited.id),
          () => outsider.invitations.decline(invited.id),
        ]) {
          expect(`${(await failure(call())).status}`).toBe("404");
        }
        expect((await inviteOutsider()).status).toBe("invited");
      });

      it("refuses a withdrawn invitation as missing", async () => {
        const invited = await inviteOutsider();
        await owner.members.remove(fx.organization, invited.id);
        expect((await as("outsider").invitations.list()).map((i) => i.id)).not.toContain(invited.id);
        const error = await failure(as("outsider").invitations.accept(invited.id));
        expect(`${error.status} ${error.code}`).toBe("404 not_found");
        expect(await inNorthwind("outsider")).toBeUndefined();
      });

      it("refuses an expired invitation, which can still be declined", async () => {
        const viewer = as("viewer");
        const expired = must(
          (await viewer.invitations.list()).find((i) => i.organization.id === fx.otherOrganization),
          "the viewer's expired invitation to larkspur",
        );
        expect(expired.status).toBe("expired");
        expect(Date.parse(expired.expires_at)).toBeLessThanOrEqual(Date.now());
        const error = await failure(viewer.invitations.accept(expired.id));
        expect(`${error.status} ${error.code}`).toBe("422 invitation_expired");
        expect((await viewer.me.get()).organizations.map((o) => o.id)).not.toContain(fx.otherOrganization);
        await viewer.invitations.decline(expired.id);
        expect((await viewer.invitations.list()).map((i) => i.id)).not.toContain(expired.id);
      });

      it("renews an expired invitation when the address is invited again", async () => {
        const viewer = as("viewer");
        const expired = must(
          (await viewer.invitations.list()).find((i) => i.organization.id === fx.otherOrganization),
          "the viewer's expired invitation to larkspur",
        );
        const email = (await viewer.me.get()).user.email;
        const outsider = await as("outsider").me.get();
        const renewed = await as("outsider").members.invite(fx.otherOrganization, { email, role: "viewer" });
        expect([renewed.id, renewed.role, renewed.status]).toEqual([expired.id, "viewer", "invited"]);
        expect(Date.parse(renewed.invited_at)).toBeGreaterThan(Date.parse(expired.invited_at));
        const listed = must(
          (await viewer.invitations.list()).find((i) => i.id === expired.id),
          "the renewed one",
        );
        expect([listed.status, listed.role, listed.invited_by.email]).toEqual([
          "pending",
          "viewer",
          outsider.user.email,
        ]);
        expect((await viewer.invitations.accept(expired.id)).role).toBe("viewer");
        // Pending, an invitation isn't sent twice.
        const pending = await inviteOutsider();
        const again = await failure(inviteOutsider("viewer"));
        expect(`${again.status} ${again.code}`).toBe("409 already_member");
        expect((await as("outsider").invitations.list()).find((i) => i.id === pending.id)?.role).toBe("developer");
      });

      it("cancels only an invitation still pending, never a member who accepted it", async () => {
        const invited = await inviteOutsider();
        await as("outsider").invitations.accept(invited.id);
        const error = await failure(owner.members.remove(fx.organization, invited.id, { status: "invited" }));
        expect(`${error.status} ${error.code}`).toBe("409 invitation_accepted");
        expect((await inNorthwind("outsider"))?.role).toBe("developer");
        const invalid = await failure(
          owner.members.remove(fx.organization, invited.id, { status: "active" as "invited" }),
        );
        expect(`${invalid.status} ${invalid.code}`).toBe("400 validation_failed");
        expect((await inNorthwind("outsider"))?.role).toBe("developer");
        const pending = await owner.members.invite(fx.organization, {
          email: "cancel@northwind.example",
          role: "viewer",
        });
        await owner.members.remove(fx.organization, pending.id, { status: "invited" });
        expect((await owner.members.list(fx.organization)).map((m) => m.id)).not.toContain(pending.id);
      });

      describe("grants only what its grantor can still grant", () => {
        /** The developer's own membership in Northwind, promoted to `role` by the owner. */
        const promoteDeveloper = async (role: "owner" | "admin") => {
          const developerId = (await as("developer").me.get()).user.id;
          const membership = must(
            (await owner.members.list(fx.organization)).find((m) => m.user.id === developerId),
            "the developer's membership",
          );
          await owner.members.update(fx.organization, membership.id, { role });
          return membership;
        };
        const outsiderEmail = async () => (await as("outsider").me.get()).user.email;
        const pendingIds = async () =>
          (await owner.members.list(fx.organization)).filter((m) => m.status === "invited").map((m) => m.id);
        const expectWithdrawn = async (id: MemberId) => {
          expect(await pendingIds()).not.toContain(id);
          expect((await as("outsider").invitations.list()).map((i) => i.id)).not.toContain(id);
          const error = await failure(as("outsider").invitations.accept(id));
          expect(`${error.status} ${error.code}`).toBe("404 not_found");
          expect(await inNorthwind("outsider")).toBeUndefined();
        };

        it("withdraws a removed member's invitations, and nobody else's", async () => {
          const grantor = await promoteDeveloper("admin");
          const theirs = await as("developer").members.invite(fx.organization, {
            email: await outsiderEmail(),
            role: "developer",
          });
          const owners = await owner.members.invite(fx.organization, {
            email: "kept@northwind.example",
            role: "admin",
          });
          await owner.members.remove(fx.organization, grantor.id);
          await expectWithdrawn(theirs.id);
          expect(await pendingIds()).toContain(owners.id);
        });

        it("withdraws, on a demotion, the invitations above what the grantor can grant now", async () => {
          const grantor = await promoteDeveloper("owner");
          const developer = as("developer");
          const ownerInvite = await developer.members.invite(fx.organization, {
            email: await outsiderEmail(),
            role: "owner",
          });
          const adminInvite = await developer.members.invite(fx.organization, {
            email: "admin-invite@northwind.example",
            role: "admin",
          });
          // Admin now: owner invitations go, the rest stand.
          await owner.members.update(fx.organization, grantor.id, { role: "admin" });
          await expectWithdrawn(ownerInvite.id);
          expect(await pendingIds()).toContain(adminInvite.id);
          // Developer now: they grant nothing.
          await owner.members.update(fx.organization, grantor.id, { role: "developer" });
          expect(await pendingIds()).not.toContain(adminInvite.id);
        });

        it("makes whoever changes an invitation's role its grantor", async () => {
          const grantor = await promoteDeveloper("admin");
          const invited = await as("developer").members.invite(fx.organization, {
            email: await outsiderEmail(),
            role: "viewer",
          });
          await owner.members.update(fx.organization, invited.id, { role: "developer" });
          await owner.members.update(fx.organization, grantor.id, { role: "viewer" });
          const listed = must(
            (await as("outsider").invitations.list()).find((i) => i.id === invited.id),
            "the invitation, granted by the owner now",
          );
          expect(listed.invited_by.email).toBe((await owner.me.get()).user.email);
          expect((await as("outsider").invitations.accept(invited.id)).role).toBe("developer");
        });

        it("never lets an accept outrun the grantor's demotion", async () => {
          const grantor = await promoteDeveloper("owner");
          const invited = await as("developer").members.invite(fx.organization, {
            email: await outsiderEmail(),
            role: "owner",
          });
          const [accepted, demoted] = await Promise.allSettled([
            as("outsider").invitations.accept(invited.id),
            owner.members.update(fx.organization, grantor.id, { role: "admin" }),
          ]);
          expect(demoted.status).toBe("fulfilled");
          if (accepted.status === "fulfilled") {
            // Accepted first: they joined while it was still granted.
            expect((await inNorthwind("outsider"))?.role).toBe("owner");
          } else {
            await expectWithdrawn(invited.id);
          }
        });
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
          expect(updated.auth).toEqual({
            ...oauthTemplate,
            authorize_url: "https://auth.provider.example/authorize",
            issuer: null,
          });
          expect(updated.version).toBe(app.version + 1);
          expect((await owner.organizationApps.get(fx.organization, app.id)).auth).toEqual(updated.auth);
          // Renaming alone leaves the template and the version as they are.
          const renamed = await owner.organizationApps.update(fx.organization, app.id, { name: "Renamed" });
          expect([renamed.auth, renamed.version]).toEqual([updated.auth, updated.version]);
        });

        it("persists an exact configured issuer and clears it with null or an old template", async () => {
          const app = await newOAuthApp("issuer");
          for (const issuer of ["https://AUTH.provider.example/Tenant/", "https://auth.provider.example", null]) {
            const updated = await owner.organizationApps.update(fx.organization, app.id, {
              auth: { ...oauthTemplate, issuer },
            });
            expect(updated.auth.type === "oauth2" && updated.auth.issuer).toBe(issuer);
            expect((await owner.organizationApps.get(fx.organization, app.id)).auth).toEqual(updated.auth);
          }
          const legacy = await owner.organizationApps.update(fx.organization, app.id, { auth: oauthTemplate });
          expect(legacy.auth.type === "oauth2" && legacy.auth.issuer).toBeNull();
        });

        it("refuses unsafe or ambiguous configured issuers without changing the app", async () => {
          const app = await newOAuthApp("issuer-validation");
          for (const [issuer, status, code] of [
            ["", 400, "validation_failed"],
            ["not a URL", 400, "validation_failed"],
            [" https://auth.provider.example", 400, "validation_failed"],
            ["https://auth.provider.example ", 400, "validation_failed"],
            ["https://auth.provider.example/\u0085tenant", 400, "validation_failed"],
            ["https://auth.provider.example/\ufefftenant", 400, "validation_failed"],
            ["https://auth.provider.example/\u0000tenant", 400, "validation_failed"],
            ["https:////auth.provider.example", 400, "validation_failed"],
            ["https://auth.provider.example/tenant?", 400, "validation_failed"],
            ["https://auth.provider.example/tenant#", 400, "validation_failed"],
            ["https://auth.provider.example?tenant=one", 400, "validation_failed"],
            ["https://auth.provider.example#one", 400, "validation_failed"],
            ["https://auth.provider.example\\tenant", 400, "validation_failed"],
            ["https:/auth.provider.example", 400, "validation_failed"],
            ["http://auth.provider.example", 422, "insecure_url"],
            ["https://user:password@auth.provider.example", 400, "validation_failed"],
            ["https://@auth.provider.example", 400, "validation_failed"],
            ["https://localhost", 422, "destination_not_allowed"],
            ["https://127.0.0.1", 422, "destination_not_allowed"],
            ["https://[::1]", 422, "destination_not_allowed"],
          ] as const) {
            const error = await failure(
              owner.organizationApps.update(fx.organization, app.id, {
                name: "Must not change",
                auth: { ...oauthTemplate, issuer },
              }),
            );
            expect([error.status, error.code, Object.keys(error.fields ?? {})], issuer).toEqual([
              status,
              code,
              ["auth.issuer"],
            ]);
          }
          expect(await owner.organizationApps.get(fx.organization, app.id)).toEqual(app);
        });

        it("validates an issuer's shape before scope conflicts and URL policy", async () => {
          const app = await newOAuthApp("issuer-validation-order");
          await owner.organizationApps.update(fx.organization, app.id, { auth: oauthTemplate });
          await owner.organizationApps.updateConfiguration(fx.organization, app.id, "sandbox", {
            oauth_scopes: ["read"],
          });
          const before = await owner.organizationApps.get(fx.organization, app.id);
          for (const issuer of [
            "https://auth.provider.example?tenant=one",
            "https://auth.provider.example#one",
            "https://user:password@auth.provider.example",
            "https://@auth.provider.example",
            "https://[invalid",
          ]) {
            for (const available_scopes of [[], oauthTemplate.available_scopes]) {
              const error = await failure(
                owner.organizationApps.update(fx.organization, app.id, {
                  auth: {
                    ...oauthTemplate,
                    issuer,
                    available_scopes,
                    authorize_url: "http://auth.provider.example/authorize",
                  },
                }),
              );
              expect([error.status, error.code, Object.keys(error.fields ?? {})], issuer).toEqual([
                400,
                "validation_failed",
                ["auth.issuer"],
              ]);
            }
          }
          // HTTPS/public-host policy remains after conflicts in the decision order.
          for (const issuer of ["http://auth.provider.example", "https://localhost"]) {
            const error = await failure(
              owner.organizationApps.update(fx.organization, app.id, {
                auth: { ...oauthTemplate, issuer, available_scopes: [] },
              }),
            );
            expect([error.status, error.code]).toEqual([409, "scope_in_use"]);
          }
          expect(await owner.organizationApps.get(fx.organization, app.id)).toEqual(before);
        });

        it("keeps configured issuers inside their organization and workspace", async () => {
          const ws = fx.workspaces.qaPlayground.id;
          const sibling = fx.workspaces.acmeSandbox.id;
          const catalog = await newOAuthApp("issuer-catalog");
          const own = await owner.workspaceApps.create(ws, {
            kind: "mcp",
            name: "Provider",
            slug: "issuer-workspace",
            description: "",
            auth_type: "oauth2",
          });
          const auth = { ...oauthTemplate, issuer: "https://auth.provider.example/tenant" };
          await owner.workspaceApps.update(ws, own.id, { auth });
          const saved = await owner.workspaceApps.get(ws, own.id);
          expect(saved.auth.type === "oauth2" && saved.auth.issuer).toBe(auth.issuer);
          for (const request of [
            () => as("outsider").organizationApps.update(fx.organization, catalog.id, { auth }),
            () => naming("outsider", () => fx.organization).workspaceApps.update(ws, own.id, { auth }),
            () => owner.workspaceApps.update(sibling, own.id, { auth: { ...auth, issuer: null } }),
          ])
            expect((await failure(request())).status).toBe(404);
          for (const persona of ["viewer", "developer"] as const) {
            expect(
              (await failure(as(persona).organizationApps.update(fx.organization, catalog.id, { auth }))).status,
            ).toBe(403);
            expect((await failure(as(persona).workspaceApps.update(ws, own.id, { auth }))).status).toBe(403);
          }
          expect(await owner.workspaceApps.get(ws, own.id)).toEqual(saved);
          expect(await owner.organizationApps.get(fx.organization, catalog.id)).toEqual(catalog);
          expect((await owner.workspaceApps.list(sibling)).some((app) => app.id === own.id)).toBe(false);
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

      describe("sign-in through the MCP server", () => {
        const signIn = { kind: "mcp", description: "", auth_type: "mcp_oauth" } as const;

        it("starts blank, at both levels, and keeps its type", async () => {
          const ws = fx.workspaces.qaPlayground.id;
          const catalog = await owner.organizationApps.create(fx.organization, {
            ...signIn,
            name: "Ledger",
            slug: "ledger",
          });
          const own = await owner.workspaceApps.create(ws, { ...signIn, name: "Ledger", slug: "ledger" });
          for (const app of [catalog, own]) {
            expect(app.auth).toEqual({ type: "mcp_oauth", scopes: [], identity_url: null });
          }
          const auth: AuthTemplate = {
            type: "mcp_oauth",
            scopes: [{ name: "read", description: "Read entries" }],
            identity_url: " https://ledger.provider.example/me ",
          };
          const updated = await owner.organizationApps.update(fx.organization, catalog.id, { auth });
          expect(updated.auth).toEqual({ ...auth, identity_url: "https://ledger.provider.example/me" });
          expect(updated.version).toBe(catalog.version + 1);
          const insecure = await failure(
            owner.workspaceApps.update(ws, own.id, {
              auth: { ...auth, identity_url: "http://ledger.provider.example/me" },
            }),
          );
          expect([insecure.code, Object.keys(insecure.fields ?? {})]).toEqual(["insecure_url", ["auth.identity_url"]]);
          const retyped = await failure(
            owner.organizationApps.update(fx.organization, catalog.id, {
              auth: {
                type: "oauth2",
                authorize_url: null,
                token_url: null,
                identity_url: null,
                available_scopes: [],
                pkce: true,
              },
            }),
          );
          expect([retyped.status, retyped.code]).toEqual([422, "auth_type_immutable"]);
        });

        it("is ready with only its server, and has no OAuth client to configure", async () => {
          const app = await owner.organizationApps.create(fx.organization, {
            ...signIn,
            name: "Ledger",
            slug: "ledger-ready",
          });
          const blank = await owner.organizationApps.getConfiguration(fx.organization, app.id, "sandbox");
          expect([blank.status, blank.missing, blank.oauth]).toEqual(["unconfigured", ["mcp_server_url"], null]);
          for (const input of [
            { oauth_client_id: "client" },
            { oauth_client_secret: "a-long-client-secret" },
            { oauth_scopes: ["read"] },
          ]) {
            const error = await failure(
              owner.organizationApps.updateConfiguration(fx.organization, app.id, "sandbox", input),
            );
            expect([error.status, error.code]).toEqual([422, "oauth_not_supported"]);
          }
          const ready = await owner.organizationApps.updateConfiguration(fx.organization, app.id, "sandbox", {
            mcp_server_url: "https://ledger.provider.example/mcp",
          });
          expect([ready.status, ready.missing, ready.oauth]).toEqual(["ready", [], null]);
          const offered = await owner.workspaceApps.available(fx.workspaces.qaPlayground.id);
          expect(offered.find((a) => a.app.id === app.id)?.configuration.status).toBe("ready");
        });

        it("keeps its identity URL on the MCP server", async () => {
          const ws = fx.workspaces.qaPlayground.id;
          const catalog = await owner.organizationApps.create(fx.organization, {
            ...signIn,
            name: "Ledger",
            slug: "ledger-identity",
          });
          const own = await owner.workspaceApps.create(ws, { ...signIn, name: "Ledger", slug: "ledger-identity" });
          await owner.organizationApps.updateConfiguration(fx.organization, catalog.id, "sandbox", {
            mcp_server_url: "https://ledger.provider.example/mcp",
          });
          await owner.workspaceApps.updateConfiguration(ws, own.id, {
            mcp_server_url: "https://ledger.provider.example/mcp",
          });
          const naming = (identity_url: string): AuthTemplate => ({ type: "mcp_oauth", scopes: [], identity_url });
          for (const elsewhere of ["https://accounts.provider.example/me", "https://ledger.provider.example:8443/me"]) {
            const refused = await failure(
              owner.organizationApps.update(fx.organization, catalog.id, { auth: naming(elsewhere) }),
            );
            expect([refused.status, refused.code, refused.fields]).toEqual([
              422,
              "identity_off_server",
              {
                "auth.identity_url":
                  "Use an address on https://ledger.provider.example, the Sandbox configuration's MCP server.",
              },
            ]);
            const ownRefused = await failure(owner.workspaceApps.update(ws, own.id, { auth: naming(elsewhere) }));
            expect([ownRefused.status, ownRefused.code]).toEqual([422, "identity_off_server"]);
          }
          for (const updated of [
            await owner.organizationApps.update(fx.organization, catalog.id, {
              auth: naming("https://LEDGER.provider.example:443/v2/me"),
            }),
            await owner.workspaceApps.update(ws, own.id, { auth: naming("https://ledger.provider.example/me") }),
          ]) {
            expect(updated.auth.type === "mcp_oauth" && updated.auth.identity_url).toMatch(
              /^https:\/\/ledger\.provider\.example/i,
            );
          }
        });

        it("asks for its template's scopes, and takes no pasted credential", async () => {
          const app = await owner.organizationApps.create(fx.organization, {
            ...signIn,
            name: "Ledger",
            slug: "ledger-scopes",
          });
          await owner.organizationApps.update(fx.organization, app.id, {
            auth: {
              type: "mcp_oauth",
              scopes: [
                { name: "read", description: "" },
                { name: "write", description: "" },
              ],
              identity_url: null,
            },
          });
          await owner.organizationApps.updateConfiguration(fx.organization, app.id, "sandbox", {
            mcp_server_url: "https://ledger.provider.example/mcp",
          });
          const session = await owner.connectSessions.create(fx.workspaces.qaPlayground.id, {
            app_id: app.id,
            intent: "create",
            label: "Books",
            installation_id: null,
            return_url: null,
          });
          expect(session.requested_scopes).toEqual(["read", "write"]);
          expect(session.app.auth_type).toBe("mcp_oauth");
          const pasted = await failure(
            owner.connect.submitCredentials(referenceOf(session), { values: { token: "t" } }),
          );
          expect([pasted.status, pasted.code]).toEqual([422, "wrong_auth_type"]);
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
        // Connect records the installation, once, as Zupflow itself.
        const events = (await owner.audit.list(fx.organization, { workspace_id: fx.workspaces.qaPlayground.id })).items;
        expect(events[0]).toMatchObject({
          action: "installation.connected",
          actor: { type: "system", id: null, name: "Connect" },
          target: {
            type: "installation",
            id: result.installation.id,
            name: expect.stringMatching(new RegExp(` · ${result.installation.label}$`)),
          },
          environment: "sandbox",
        });
        expect(events[1]?.action).not.toBe("installation.connected");
      });

      it("checks submitted values against the app's inputs", async () => {
        const session = await start();
        for (const [values, fields] of [
          [{}, { token: "Required." }],
          [{ token: "   " }, { token: "Required." }],
          [{ token: "x".repeat(4097) }, { token: "Use 4,096 characters or fewer." }],
        ] as const) {
          const error = await failure(owner.connect.submitCredentials(referenceOf(session), { values }));
          expect([error.status, error.code, error.fields]).toEqual([400, "validation_failed", fields]);
        }
        expect((await owner.connect.get(referenceOf(session))).status).toBe("pending");
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

      it("reads days as a whole number, clamped to 1 through 90", async () => {
        const days = async (value: string | number) =>
          (await owner.organizations.usage(fx.organization, "sandbox", value as number)).days.length;
        expect(await days(7)).toBe(7);
        // Whole numbers however they're written; anything else means the default.
        expect(await days("5.0")).toBe(5);
        expect(await days("1e20")).toBe(90);
        expect(await days(0)).toBe(1);
        expect(await days(-3)).toBe(1);
        expect(await days("1.5")).toBe(14);
        expect(await days("seven")).toBe(14);
      });

      it("lists runs across workspaces of one environment only", async () => {
        const runs = await owner.organizationRuns.list(fx.organization, { environment: "sandbox", limit: 100 });
        expect(runs.items.length).toBeGreaterThan(0);
        expect(runs.items.every((r) => r.workspace.environment === "sandbox")).toBe(true);
      });

      it("refuses an environment that isn't one", async () => {
        const error = await failure(
          owner.organizationRuns.list(fx.organization, { environment: "staging" as unknown as "sandbox" }),
        );
        expect([error.status, error.code, Object.keys(error.fields ?? {})]).toEqual([
          400,
          "validation_failed",
          ["environment"],
        ]);
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

      it("rejects tools the installation doesn't list and approvals outside the grant", async () => {
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
        expect([unknown.status, unknown.code, Object.keys(unknown.fields ?? {})]).toEqual([
          422,
          "unknown_tool",
          ["tools"],
        ]);
        const outside = await failure(
          owner.grants.create(qa, agent.id, {
            installation_id: installation.id,
            tools: ["echo"],
            approval_required: ["echo_write"],
          }),
        );
        expect([outside.status, outside.code, Object.keys(outside.fields ?? {})]).toEqual([
          422,
          "approval_outside_grant",
          ["approval_required"],
        ]);
        expect(await owner.grants.list(qa, agent.id)).toEqual([]);
      });

      it("grants what the installation's registry holds: a changed tool, never a gone one", async () => {
        const sandbox = fx.workspaces.acmeSandbox.id;
        const installation = must(
          (await owner.installations.list(sandbox)).find(
            (i) => i.status === "connected" && i.app.id === fx.apps.linear,
          ),
          "connected installation of linear in acme-sandbox",
        );
        const tools = (await owner.installations.tools(sandbox, installation.id, { limit: 100 })).items;
        const current = must(
          tools.find((t) => t.status === "current"),
          "current tool",
        ).name;
        const changed = must(
          tools.find((t) => t.status === "changed"),
          "changed tool",
        ).name;
        const gone = must(
          tools.find((t) => t.status === "gone"),
          "gone tool",
        ).name;
        const developer = as("developer");
        const agent = await developer.agents.create(sandbox, {
          name: "Registry probe",
          description: "",
          model: await anyModel(),
          instructions: "",
        });
        const goneGrant = await failure(
          developer.grants.create(sandbox, agent.id, {
            installation_id: installation.id,
            tools: [current, gone],
            approval_required: [],
          }),
        );
        expect([goneGrant.status, goneGrant.code, Object.keys(goneGrant.fields ?? {})]).toEqual([
          422,
          "tool_gone",
          ["tools"],
        ]);

        const grant = await developer.grants.create(sandbox, agent.id, {
          installation_id: installation.id,
          tools: [current, changed],
          approval_required: [changed],
        });
        expect([grant.agent_id, grant.installation_id, grant.tools, grant.approval_required]).toEqual([
          agent.id,
          installation.id,
          [current, changed],
          [changed],
        ]);
        // One grant per agent and installation.
        const twice = await failure(
          developer.grants.create(sandbox, agent.id, {
            installation_id: installation.id,
            tools: [current],
            approval_required: [],
          }),
        );
        expect([twice.status, twice.code]).toEqual([409, "grant_exists"]);
        // Changing it is held to the same rules, and a refused change changes nothing.
        const refused = [
          await failure(
            developer.grants.update(sandbox, agent.id, grant.id, { tools: [current, gone], approval_required: [] }),
          ),
          await failure(
            developer.grants.update(sandbox, agent.id, grant.id, { tools: ["not_listed"], approval_required: [] }),
          ),
          await failure(
            developer.grants.update(sandbox, agent.id, grant.id, { tools: [current], approval_required: [changed] }),
          ),
        ];
        expect(refused.map((e) => `${e.status} ${e.code}`)).toEqual([
          "422 tool_gone",
          "422 unknown_tool",
          "422 approval_outside_grant",
        ]);
        expect(await developer.grants.list(sandbox, agent.id)).toEqual([grant]);
        const updated = await developer.grants.update(sandbox, agent.id, grant.id, {
          tools: [changed],
          approval_required: [],
        });
        expect([updated.id, updated.tools, updated.approval_required]).toEqual([grant.id, [changed], []]);
        expect((await developer.agents.get(sandbox, agent.id)).grant_count).toBe(1);
      });

      it("takes an agent's grants with the agent, and an installation's with the installation", async () => {
        const sandbox = fx.workspaces.acmeSandbox.id;
        const linear = must(
          (await owner.installations.list(sandbox)).find(
            (i) => i.status === "connected" && i.app.id === fx.apps.linear,
          ),
          "connected installation of linear in acme-sandbox",
        );
        const current = must(
          (await owner.installations.tools(sandbox, linear.id, { limit: 100 })).items.find(
            (t) => t.status === "current",
          ),
          "current tool",
        ).name;
        const model = await anyModel();
        const agent = await owner.agents.create(sandbox, {
          name: "Short-lived",
          description: "",
          model,
          instructions: "",
        });
        await owner.grants.create(sandbox, agent.id, {
          installation_id: linear.id,
          tools: [current],
          approval_required: [],
        });
        expect((await owner.installations.get(sandbox, linear.id)).grant_count).toBe(linear.grant_count + 1);
        await owner.agents.delete(sandbox, agent.id);
        expect((await owner.installations.get(sandbox, linear.id)).grant_count).toBe(linear.grant_count);
        const deleted = await failure(owner.grants.list(sandbox, agent.id));
        expect(`${deleted.status} ${deleted.code}`).toBe("404 not_found");

        const qa = fx.workspaces.qaPlayground.id;
        const echo = must(
          (await owner.installations.list(qa)).find((i) => i.status === "connected" && i.app.id === fx.apps.echoServer),
          "connected installation of echo-server in qa-playground",
        );
        const kept = await owner.agents.create(qa, { name: "Kept", description: "", model, instructions: "" });
        await owner.grants.create(qa, kept.id, { installation_id: echo.id, tools: ["echo"], approval_required: [] });
        expect((await owner.agents.get(qa, kept.id)).grant_count).toBe(1);
        await owner.installations.delete(qa, echo.id);
        expect(await owner.grants.list(qa, kept.id)).toEqual([]);
        expect((await owner.agents.get(qa, kept.id)).grant_count).toBe(0);
      });

      it("keeps agents and grants to their own workspace and organization", async () => {
        const acme = fx.workspaces.acme.id;
        const beta = (await workspaceBySlug("beta-labs")).id;
        const agent = must(
          (await owner.agents.list(acme)).find((a) => a.grant_count > 0),
          "agent with a grant in acme",
        );
        const grant = must((await owner.grants.list(acme, agent.id))[0], "grant in acme");
        const outsider = as("outsider");
        const posing = naming("outsider", () => fx.organization);
        const change = { tools: grant.tools, approval_required: [] };
        const attempts: [string, () => Promise<unknown>][] = [
          ["sibling workspace", () => owner.agents.get(beta, agent.id)],
          ["sibling workspace", () => owner.agents.update(beta, agent.id, { status: "paused" })],
          ["sibling workspace", () => owner.agents.delete(beta, agent.id)],
          ["sibling workspace", () => owner.grants.list(beta, agent.id)],
          ["sibling workspace", () => owner.grants.update(beta, agent.id, grant.id, change)],
          ["sibling workspace", () => owner.grants.delete(beta, agent.id, grant.id)],
          ["other organization", () => outsider.agents.get(acme, agent.id)],
          ["other organization", () => outsider.agents.list(acme)],
          ["other organization", () => outsider.grants.list(acme, agent.id)],
          ["other organization", () => outsider.grants.delete(acme, agent.id, grant.id)],
          ["non-member naming it", () => posing.agents.get(acme, agent.id)],
          ["non-member naming it", () => posing.grants.update(acme, agent.id, grant.id, change)],
          ["non-member naming it", () => posing.agents.delete(acme, agent.id)],
        ];
        for (const [what, attempt] of attempts) {
          const error = await failure(attempt());
          expect(`${error.status} ${error.code}`, what).toBe("404 not_found");
        }
        expect(await owner.agents.get(acme, agent.id)).toEqual(agent);
        expect(await owner.grants.list(acme, agent.id)).toContainEqual(grant);
      });

      it("creates and changes agents only with a model the server offers", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const models = await owner.models.list();
        expect(models.length).toBeGreaterThan(0);
        const input = { name: "  Modelled  ", description: "", model: "not-a-model", instructions: "" };
        const refused = await failure(owner.agents.create(qa, input));
        expect([refused.status, refused.code, Object.keys(refused.fields ?? {})]).toEqual([
          400,
          "validation_failed",
          ["model"],
        ]);
        const agent = await owner.agents.create(qa, { ...input, model: must(models[0], "model").id });
        expect([agent.name, agent.status, agent.grant_count, agent.runs_last_24h, agent.last_run_at]).toEqual([
          "Modelled",
          "active",
          0,
          0,
          null,
        ]);
        const change = await failure(owner.agents.update(qa, agent.id, { model: "not-a-model" }));
        expect([change.status, change.code, Object.keys(change.fields ?? {})]).toEqual([
          400,
          "validation_failed",
          ["model"],
        ]);
        const paused = await owner.agents.update(qa, agent.id, { status: "paused" });
        expect([paused.status, paused.model, paused.name]).toEqual(["paused", agent.model, "Modelled"]);
        expect((await owner.agents.list(qa)).find((a) => a.id === agent.id)).toEqual(paused);
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

    describe("installation tools", () => {
      /** acme-sandbox's connected installation: the reference fixture has discovered its tools. */
      const sandboxInstallation = async () =>
        must(
          (await owner.installations.list(fx.workspaces.acmeSandbox.id)).find(
            (i) => i.status === "connected" && i.app.id === fx.apps.linear,
          ),
          "connected installation of linear in acme-sandbox",
        );

      it("lists an installation's tools by name, to every member", async () => {
        const sandbox = fx.workspaces.acmeSandbox.id;
        const installation = await sandboxInstallation();
        const page = await owner.installations.tools(sandbox, installation.id, { limit: 100 });
        const tools = page.items;
        expect(tools.length).toBeGreaterThan(0);
        expect(page.pagination).toEqual({ next_cursor: null, total: tools.length });
        const names = tools.map((t) => t.name);
        expect(names).toEqual([...names].sort());
        // What a tool's status says matches what it holds.
        for (const tool of tools) {
          const held = { latest: tool.latest !== null, changed: tool.changed_at !== null, gone: tool.gone_at !== null };
          if (tool.status === "current")
            expect(held, tool.name).toEqual({ latest: false, changed: false, gone: false });
          if (tool.status === "changed") expect(held, tool.name).toEqual({ latest: true, changed: true, gone: false });
          if (tool.status === "gone") expect(held.gone, tool.name).toBe(true);
        }
        expect(await as("viewer").installations.tools(sandbox, installation.id, { limit: 100 })).toEqual(page);
      });

      it("pages an installation's tools by name, and refuses a cursor it didn't issue", async () => {
        const sandbox = fx.workspaces.acmeSandbox.id;
        const installation = await sandboxInstallation();
        const whole = await owner.installations.tools(sandbox, installation.id, { limit: 100 });
        expect(whole.items.length, "the fixture's tools span pages").toBeGreaterThan(1);
        const paged: string[] = [];
        let cursor: string | undefined;
        do {
          const page = await owner.installations.tools(sandbox, installation.id, { cursor, limit: 1 });
          expect(page.items.length).toBe(1);
          expect(page.pagination.total).toBe(whole.items.length);
          paged.push(...page.items.map((t) => t.name));
          cursor = page.pagination.next_cursor ?? undefined;
        } while (cursor);
        expect(paged).toEqual(whole.items.map((t) => t.name));
        // Out-of-range limits are clamped.
        expect((await owner.installations.tools(sandbox, installation.id, { limit: 0 })).items.length).toBe(1);
        const error = await failure(owner.installations.tools(sandbox, installation.id, { cursor: "%%%" }));
        expect([error.status, error.code]).toEqual([400, "validation_failed"]);
      });

      it("keeps another workspace's and another organization's installation out of both routes", async () => {
        const outsider = as("outsider");
        const harborSlug = async (slug: string) =>
          must(
            (await outsider.workspaces.list(fx.otherOrganization, { q: slug, limit: 100 })).items.find(
              (w) => w.slug === slug,
            ),
            `workspace ${slug}`,
          );
        const [harbor, harborSandbox, beta] = await Promise.all([
          harborSlug("harbor"),
          harborSlug("harbor-sandbox"),
          workspaceBySlug("beta-labs"),
        ]);
        const { acme, acmeSandbox } = fx.workspaces;
        const first = async (client: ZupflowClient, workspace: WorkspaceId, what: string) =>
          must((await client.installations.list(workspace))[0], `installation in ${what}`).id;
        const harborInstallation = await first(outsider, harbor.id, "harbor");
        const acmeInstallation = await first(owner, acme.id, "acme");
        const betaInstallation = await first(owner, beta.id, "beta-labs");
        // Refreshes go through a Sandbox, so Production's read-only refusal can't answer first.
        const attempts: [string, () => Promise<unknown>][] = [
          ["other organization", () => owner.installations.tools(acme.id, harborInstallation)],
          ["other organization", () => owner.installations.refreshTools(acmeSandbox.id, harborInstallation)],
          ["other organization", () => outsider.installations.tools(harbor.id, acmeInstallation)],
          ["other organization", () => outsider.installations.refreshTools(harborSandbox.id, acmeInstallation)],
          ["other organization's path", () => outsider.installations.tools(acme.id, acmeInstallation)],
          ["sibling workspace", () => owner.installations.tools(acme.id, betaInstallation)],
          ["sibling workspace", () => owner.installations.refreshTools(acmeSandbox.id, betaInstallation)],
        ];
        for (const [what, attempt] of attempts) {
          const error = await failure(attempt());
          expect(`${error.status} ${error.code}`, what).toBe("404 not_found");
        }
      });

      it("never lets the Console refresh tools in Production", async () => {
        const acme = fx.workspaces.acme.id;
        const installation = must(
          (await owner.installations.list(acme)).find((i) => i.status === "connected"),
          "connected installation in acme",
        );
        const error = await failure(owner.installations.refreshTools(acme, installation.id));
        expect(`${error.status} ${error.code}`).toBe("403 console_read_only_in_production");
        // A developer is refused the same way; a viewer by role first.
        const developer = await failure(as("developer").installations.refreshTools(acme, installation.id));
        expect(developer.code).toBe("console_read_only_in_production");
        const viewer = await failure(as("viewer").installations.refreshTools(acme, installation.id));
        expect(`${viewer.status} ${viewer.code}`).toBe("403 forbidden");
      });

      it("paces refreshes: a second within a minute answers 429", async () => {
        const sandbox = fx.workspaces.acmeSandbox.id;
        const installation = await sandboxInstallation();
        expect((await owner.installations.refreshTools(sandbox, installation.id)).id).toBe(installation.id);
        const error = await failure(as("developer").installations.refreshTools(sandbox, installation.id));
        expect(`${error.status} ${error.code}`).toBe("429 refresh_too_soon");
      });

      it("refreshes a connected installation and refuses one that isn't connected", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const installation = must(
          (await owner.installations.list(qa)).find((i) => i.status === "connected"),
          "connected installation in qa-playground",
        );
        const refreshed = await as("developer").installations.refreshTools(qa, installation.id);
        expect([refreshed.id, refreshed.status]).toEqual([installation.id, "connected"]);
        await owner.installations.disconnect(qa, installation.id);
        const error = await failure(owner.installations.refreshTools(qa, installation.id));
        expect(`${error.status} ${error.code}`).toBe("409 installation_not_connected");
      });
    });

    describe("tool permissions", () => {
      const google = async () =>
        must(
          (await owner.installations.list(fx.workspaces.acme.id)).find((i) => i.app.id === fx.apps.googleWorkspace),
          "installation of google-workspace in acme",
        );
      const probe = async (workspace: WorkspaceId) =>
        owner.agents.create(workspace, {
          name: "Policy probe",
          description: "",
          model: await anyModel(),
          instructions: "",
        });
      const refusal = async (promise: Promise<unknown>) => {
        const error = await failure(promise);
        return [error.status, error.code, Object.keys(error.fields ?? {})];
      };

      it("reads the policy to every member, and lets only owners and admins set it", async () => {
        const policy = await owner.toolPolicy.get(fx.organization);
        expect([policy.organization_id, policy.mode, policy.new_tool_level, policy.updated_at]).toEqual([
          fx.organization,
          "manual",
          "approve",
          null,
        ]);
        expect(await as("viewer").toolPolicy.get(fx.organization)).toEqual(policy);
        const refused = await failure(
          as("developer").toolPolicy.update(fx.organization, { mode: "manual", new_tool_level: "deny" }),
        );
        expect(`${refused.status} ${refused.code}`).toBe("403 forbidden");
        const invalid = await failure(
          // @ts-expect-error: a tool nobody set a level for is never allowed
          owner.toolPolicy.update(fx.organization, { mode: "manual", new_tool_level: "allow" }),
        );
        expect([invalid.status, invalid.code, Object.keys(invalid.fields ?? {})]).toEqual([
          400,
          "validation_failed",
          ["new_tool_level"],
        ]);
        expect(await owner.toolPolicy.get(fx.organization)).toEqual(policy);
        const saved = await owner.toolPolicy.update(fx.organization, { mode: "manual", new_tool_level: "deny" });
        const ownerId = (await owner.me.get()).user.id;
        expect([saved.new_tool_level, saved.updated_by?.id, saved.updated_at === null]).toEqual([
          "deny",
          ownerId,
          false,
        ]);
        expect(await as("viewer").toolPolicy.get(fx.organization)).toEqual(saved);
      });

      it("lists a catalog app's levels by name, suggested and confirmed, and only a catalog app's", async () => {
        const page = await owner.toolPolicy.levels(fx.organization, fx.apps.googleWorkspace, { limit: 100 });
        const names = page.items.map((l) => l.tool);
        expect(names).toEqual([...names].sort());
        expect(page.pagination.total).toBe(page.items.length);
        const level = (tool: string) => {
          const found = must(
            page.items.find((l) => l.tool === tool),
            `level of ${tool}`,
          );
          return [found.app_id, found.level, found.confirmed];
        };
        const app = fx.apps.googleWorkspace;
        expect(level("send_email")).toEqual([app, "approve", true]);
        expect(level("delete_file")).toEqual([app, "deny", true]);
        expect(level("list_events")).toEqual([app, "allow", false]);
        const first = await owner.toolPolicy.levels(fx.organization, app, { limit: 2 });
        const second = await owner.toolPolicy.levels(fx.organization, app, {
          limit: 2,
          cursor: must(first.pagination.next_cursor, "next cursor"),
        });
        expect([...first.items, ...second.items]).toEqual(page.items.slice(0, 4));
        const workspaceApp = await failure(owner.toolPolicy.levels(fx.organization, fx.apps.echoServer));
        expect(`${workspaceApp.status} ${workspaceApp.code}`).toBe("404 not_found");
      });

      it("sets levels by name, confirmed, and refuses a malformed list", async () => {
        const app = fx.apps.googleWorkspace;
        const refused = await failure(
          as("developer").toolPolicy.setLevels(fx.organization, app, {
            levels: [{ tool: "list_events", level: "allow" }],
          }),
        );
        expect(`${refused.status} ${refused.code}`).toBe("403 forbidden");
        for (const levels of [
          [],
          [
            { tool: "list_events", level: "allow" as const },
            { tool: "list_events", level: "deny" as const },
          ],
          [{ tool: "", level: "allow" as const }],
          [{ tool: "x".repeat(129), level: "allow" as const }],
        ]) {
          const invalid = await failure(owner.toolPolicy.setLevels(fx.organization, app, { levels }));
          expect(
            [invalid.status, invalid.code, Object.keys(invalid.fields ?? {})],
            JSON.stringify(levels).slice(0, 60),
          ).toEqual([400, "validation_failed", ["levels"]]);
        }
        const set = await owner.toolPolicy.setLevels(fx.organization, app, {
          levels: [
            { tool: "list_events", level: "allow" },
            { tool: "archive_everything", level: "deny" },
          ],
        });
        const ownerId = (await owner.me.get()).user.id;
        expect(set.map((l) => [l.tool, l.level, l.confirmed, l.updated_by?.id])).toEqual([
          ["archive_everything", "deny", true, ownerId],
          ["list_events", "allow", true, ownerId],
        ]);
        const after = (await owner.toolPolicy.levels(fx.organization, app, { limit: 100 })).items;
        expect(after).toEqual(expect.arrayContaining(set));
      });

      it("holds grants inside the organization's levels", async () => {
        const acme = fx.workspaces.acme.id;
        const installation = (await google()).id;
        const agent = await probe(acme);
        const grant = (tools: string[], approval_required: string[] = []) =>
          owner.grants.create(acme, agent.id, { installation_id: installation, tools, approval_required });
        expect(await refusal(grant(["search_files", "delete_file"]))).toEqual([422, "tool_denied", ["tools"]]);
        expect(await refusal(grant(["search_files", "send_email"]))).toEqual([
          422,
          "approval_required_by_policy",
          ["approval_required"],
        ]);
        // Only suggested, so the policy's level for new tools applies: approve, then deny.
        expect(await refusal(grant(["list_events"]))).toEqual([
          422,
          "approval_required_by_policy",
          ["approval_required"],
        ]);
        await owner.toolPolicy.update(fx.organization, { mode: "manual", new_tool_level: "deny" });
        expect(await refusal(grant(["list_events"], ["list_events"]))).toEqual([422, "tool_denied", ["tools"]]);
        // The registry's own rules come first.
        expect(await refusal(grant(["not_listed", "delete_file"]))).toEqual([422, "unknown_tool", ["tools"]]);
        expect(await owner.grants.list(acme, agent.id)).toEqual([]);

        await owner.toolPolicy.setLevels(fx.organization, fx.apps.googleWorkspace, {
          levels: [{ tool: "list_events", level: "allow" }],
        });
        const made = await grant(["list_events", "send_email"], ["send_email"]);
        expect([made.tools, made.approval_required]).toEqual([["list_events", "send_email"], ["send_email"]]);
        // Tightening a level rewrites no grant; a change to the grant is held to it.
        await owner.toolPolicy.setLevels(fx.organization, fx.apps.googleWorkspace, {
          levels: [{ tool: "list_events", level: "deny" }],
        });
        expect(await owner.grants.list(acme, agent.id)).toEqual([made]);
        expect(
          await refusal(
            owner.grants.update(acme, agent.id, made.id, {
              tools: made.tools,
              approval_required: made.approval_required,
            }),
          ),
        ).toEqual([422, "tool_denied", ["tools"]]);
        expect(await owner.grants.list(acme, agent.id)).toEqual([made]);
      });

      it("leaves a workspace app's tools to the grant alone", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const echo = must(
          (await owner.installations.list(qa)).find((i) => i.app.id === fx.apps.echoServer),
          "installation of echo-server in qa-playground",
        );
        await owner.toolPolicy.update(fx.organization, { mode: "manual", new_tool_level: "deny" });
        const agent = await probe(qa);
        const grant = await owner.grants.create(qa, agent.id, {
          installation_id: echo.id,
          tools: ["echo", "echo_write"],
          approval_required: [],
        });
        expect(grant.approval_required).toEqual([]);
      });

      it("accepts a reviewed change exactly as reviewed, for owners and admins", async () => {
        const sandbox = fx.workspaces.acmeSandbox.id;
        const installation = must(
          (await owner.installations.list(sandbox)).find(
            (i) => i.status === "connected" && i.app.id === fx.apps.linear,
          ),
          "connected installation of linear in acme-sandbox",
        );
        const tools = (await owner.installations.tools(sandbox, installation.id, { limit: 100 })).items;
        const changed = must(
          tools.find((t) => t.status === "changed"),
          "changed tool",
        );
        const current = must(
          tools.find((t) => t.status === "current"),
          "current tool",
        );
        const latest = must(changed.latest_hash, "latest hash");
        expect(changed.pinned_hash).not.toBe(latest);
        const accept = (name: string, latest_hash: string, c = owner) =>
          c.installations.acceptToolChange(sandbox, installation.id, { name, latest_hash });
        expect(await refusal(accept(changed.name, latest, as("developer")))).toEqual([403, "forbidden", []]);
        expect(await refusal(accept(changed.name, changed.pinned_hash))).toEqual([409, "stale_definition", []]);
        expect(await refusal(accept(current.name, current.pinned_hash))).toEqual([409, "tool_not_changed", []]);
        expect(await refusal(accept("not_listed", latest))).toEqual([404, "not_found", []]);
        expect(await owner.installations.tools(sandbox, installation.id, { limit: 100 })).toEqual(
          expect.objectContaining({ items: tools }),
        );

        const accepted = await accept(changed.name, latest);
        expect(accepted).toEqual({
          ...changed,
          status: "current",
          pinned: changed.latest,
          pinned_hash: latest,
          latest: null,
          latest_hash: null,
          changed_at: null,
        });
        const after = (await owner.installations.tools(sandbox, installation.id, { limit: 100 })).items;
        expect(after.find((t) => t.name === changed.name)).toEqual(accepted);
        expect(await refusal(accept(changed.name, latest))).toEqual([409, "tool_not_changed", []]);
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

      it("reconnects a stopped installation with a new credential version", async () => {
        const session = await create();
        const first = await owner.connect.submitCredentials(referenceOf(session), { values: { token: "abc123" } });
        await owner.installations.disconnect(qa(), first.installation.id);
        const reconnect = await create({ intent: "reconnect", installation_id: first.installation.id });
        const result = await owner.connect.submitCredentials(referenceOf(reconnect), {
          values: { token: "renewed123" },
        });
        expect(result.installation.id).toBe(first.installation.id);
        expect(result.installation.status).toBe("connected");
        expect(result.installation.connection?.credential_version).toBe(
          (first.installation.connection?.credential_version ?? 0) + 1,
        );
        expect(result.installation.connection?.account).toEqual(first.installation.connection?.account);
      });

      it("shows a used link its outcome, which the OAuth callback sends the browser back to read", async () => {
        // Until five minutes past the link's expiry; after that, and for a
        // link that expired unused, the same 404 as no session (the mock's
        // and the backend's own tests move the clock).
        const completed = await create();
        await owner.connect.submitCredentials(referenceOf(completed), { values: { token: "abc123" } });
        expect((await owner.connect.get(referenceOf(completed))).status).toBe("completed");
        const cancelled = await create();
        await owner.connect.cancel(referenceOf(cancelled));
        const read = await owner.connect.get(referenceOf(cancelled));
        expect([read.id, read.status]).toEqual([cancelled.id, "cancelled"]);
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
              settled(
                c.installations
                  .list(acme)
                  .then((installations) => Promise.all(installations.map((i) => c.installations.tools(acme, i.id)))),
              ),
              settled(c.connectSessions.list(acme)),
              settled(c.organizationApps.list(fx.organization)),
              settled(c.workspaceApps.list(acme)),
              settled(c.workspaceApps.available(fx.workspaces.acmeSandbox.id)),
              settled(c.apiKeys.list(fx.organization)),
              settled(c.webhooks.list(fx.organization)),
              settled(c.connectSettings.get(fx.organization, "sandbox")),
              settled(c.toolPolicy.get(fx.organization)),
              settled(c.toolPolicy.levels(fx.organization, fx.apps.googleWorkspace, { limit: 100 })),
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
          ["refreshInstallationTools", () => c.installations.refreshTools(w, t.installation.id)],
          [
            "acceptToolChange",
            () =>
              c.installations.acceptToolChange(w, t.installation.id, {
                name: "search_files",
                latest_hash: "0".repeat(64),
              }),
          ],
          ["updateToolPolicy", () => c.toolPolicy.update(t.org, { mode: "manual", new_tool_level: "deny" })],
          [
            "updateAppToolLevels",
            () => c.toolPolicy.setLevels(t.org, t.orgApp.id, { levels: [{ tool: "viewer_x", level: "deny" }] }),
          ],
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

      /**
       * Changes that are the person's own, with no member role to check: creating their own organization, and
       * answering an invitation to them (they aren't a member until they accept; anyone else gets 404).
       */
      const OWN_CHANGES = new Set(["createOrganization", "acceptInvitation", "declineInvitation"]);
      /** Every change a Console member could make: the route table's, minus the person's own. */
      const MUTATING = routes
        .filter((r) => r.method !== "get" && r.callers.includes("console") && !OWN_CHANGES.has(r.operationId))
        .map((r) => r.operationId);
      /** A Sandbox key, so a developer's revoke isn't refused for being Production's. */
      const sandboxKey = async () =>
        (await owner.apiKeys.create(fx.organization, { name: "Revocable", environment: "sandbox" })).key.id;

      it("changes nothing: every change answers 403 and leaves the organization as it was", async () => {
        const t = await northwind();
        const changes = changesBy(as("viewer"), t, await sandboxKey());
        // Every change a Console member could make is tried; only the person's own changes are open to all.
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
          "acceptToolChange",
          "updateToolPolicy",
          "updateAppToolLevels",
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

    describe("the audit log", () => {
      /** The reference organization's events, newest first: the whole log, or one workspace's. */
      const log = async (workspace_id?: string) =>
        await owner.audit.list(fx.organization, {
          limit: 100,
          // Malformed IDs are sent as they are: the server must find nothing for them.
          ...(workspace_id ? { workspace_id: workspace_id as WorkspaceId } : {}),
        });
      const sandboxWorkspace = async (slug: string) =>
        await owner.workspaces.create(fx.organization, {
          name: slug,
          slug,
          environment: "sandbox",
          external_id: null,
        });
      /** A run waiting for approval, and the call it waits on. */
      const waitingRun = async () => {
        const pages = await Promise.all(
          (["production", "sandbox"] as const).map((environment) =>
            owner.organizationRuns.list(fx.organization, { environment, status: "waiting_for_approval" }),
          ),
        );
        const run = must(pages.flatMap((page) => page.items)[0], "run waiting for approval");
        const events = await owner.runs.events(run.workspace_id, run.id);
        const requested = [...events].reverse().find((e) => e.type === "approval.requested");
        const callId = must(requested?.type === "approval.requested" ? requested.data.call_id : null, "pending call");
        return { run, callId };
      };

      it("records each change with who made it, what it changed and where", async () => {
        const me = (await owner.me.get()).user;
        const sandbox = fx.workspaces.acmeSandbox;
        const workspace = await sandboxWorkspace("audit-trail");
        await owner.workspaces.update(workspace.id, { name: "Audit trail" });
        const hook = (
          await owner.webhooks.create(fx.organization, {
            environment: "sandbox",
            url: "https://hooks.northwind.example/audit?token=hook-secret#top",
            description: "Audit",
            events: ["run.completed"],
          })
        ).endpoint;
        await owner.webhooks.update(fx.organization, hook.id, { enabled: false });
        await owner.webhooks.update(fx.organization, hook.id, {
          url: "https://hooks.northwind.example/moved?signature=moved-secret",
        });
        const installation = must((await owner.installations.list(sandbox.id))[0], "installation in acme-sandbox");
        await owner.installations.update(sandbox.id, installation.id, { label: "Audited" });
        const agent = must(
          (await owner.agents.list(sandbox.id)).find((a) => a.status === "active"),
          "active agent in acme-sandbox",
        );
        const run = await owner.runs.create(sandbox.id, agent.id, { input: "Hello" });
        await owner.runs.cancel(sandbox.id, run.id);

        const page = await log();
        const recorded = page.items.slice(0, 7).map((e) => ({
          action: e.action,
          target: [e.target.type, e.target.id, e.target.name],
          where: [e.workspace_id, e.environment],
        }));
        expect(recorded).toEqual([
          { action: "run.cancelled", target: ["run", run.id, agent.name], where: [sandbox.id, "sandbox"] },
          {
            action: "installation.updated",
            target: ["installation", installation.id, expect.stringMatching(/ · Audited$/)],
            where: [sandbox.id, "sandbox"],
          },
          // A webhook is named by its URL's scheme, host and path: never its query or fragment.
          {
            action: "webhook.updated",
            target: ["webhook", hook.id, "https://hooks.northwind.example/moved"],
            where: [null, "sandbox"],
          },
          {
            action: "webhook.updated",
            target: ["webhook", hook.id, "https://hooks.northwind.example/audit"],
            where: [null, "sandbox"],
          },
          {
            action: "webhook.created",
            target: ["webhook", hook.id, "https://hooks.northwind.example/audit"],
            where: [null, "sandbox"],
          },
          {
            action: "workspace.updated",
            target: ["workspace", workspace.id, "Audit trail"],
            where: [workspace.id, "sandbox"],
          },
          {
            action: "workspace.created",
            target: ["workspace", workspace.id, "audit-trail"],
            where: [workspace.id, "sandbox"],
          },
        ]);
        for (const event of page.items.slice(0, 7)) {
          expect(event.organization_id).toBe(fx.organization);
          expect(event.actor).toEqual({ type: "user", id: me.id, name: me.name });
        }
        // The workspace reference reads as the workspace is now.
        expect(page.items[6]?.workspace).toEqual({
          id: workspace.id,
          name: "Audit trail",
          slug: "audit-trail",
          environment: "sandbox",
        });
        const text = JSON.stringify(page);
        for (const secret of ["hook-secret", "moved-secret", "#top"]) expect(text).not.toContain(secret);
      });

      it("records members and invitations, each answer by the person invited", async () => {
        const outsider = as("outsider");
        const them = (await outsider.me.get()).user;
        const me = (await owner.me.get()).user;
        const declined = await owner.members.invite(fx.organization, { email: them.email, role: "viewer" });
        await outsider.invitations.decline(declined.id);
        const joined = await owner.members.invite(fx.organization, { email: them.email, role: "viewer" });
        await outsider.invitations.accept(joined.id);
        await owner.members.update(fx.organization, joined.id, { role: "developer" });
        await owner.members.remove(fx.organization, joined.id);
        const events = (await log()).items.slice(0, 6);
        expect(events.map((e) => [e.action, e.actor.id, e.target.type, e.target.id, e.target.name])).toEqual([
          ["member.removed", me.id, "member", joined.id, them.email],
          ["member.role_changed", me.id, "member", joined.id, them.email],
          ["member.joined", them.id, "member", joined.id, them.email],
          ["member.invited", me.id, "member", joined.id, them.email],
          ["member.declined", them.id, "member", declined.id, them.email],
          ["member.invited", me.id, "member", declined.id, them.email],
        ]);
        expect(events.every((e) => e.workspace_id === null && e.environment === null)).toBe(true);
      });

      it("records nothing for a refused change", async () => {
        const before = (await log()).pagination.total;
        const refusals = await Promise.all([
          failure(
            as("viewer").workspaces.create(fx.organization, {
              name: "x",
              slug: "x",
              environment: "sandbox",
              external_id: null,
            }),
          ),
          failure(
            as("developer").webhooks.create(fx.organization, {
              environment: "sandbox",
              url: "https://hooks.northwind.example/refused",
              description: "",
              events: ["run.completed"],
            }),
          ),
          failure(as("developer").apiKeys.create(fx.organization, { name: "x", environment: "production" })),
          failure(as("outsider").workspaces.update(fx.workspaces.acme.id, { name: "Taken" })),
        ]);
        expect(refusals.map((e) => e.status)).toEqual([403, 403, 403, 404]);
        const taken = await failure(
          owner.workspaces.create(fx.organization, {
            name: "Acme",
            slug: fx.workspaces.acme.slug,
            environment: "production",
            external_id: null,
          }),
        );
        expect(taken.status).toBe(409);
        expect((await log()).pagination.total).toBe(before);
      });

      it("filters by workspace, and finds nothing for one the organization doesn't hold", async () => {
        const acme = fx.workspaces.acme;
        const agent = must((await owner.agents.list(acme.id))[0], "agent in acme");
        await owner.agents.update(acme.id, agent.id, { name: `${agent.name} (audited)` });
        const page = await log(acme.id);
        expect(page.items[0]).toMatchObject({
          action: "agent.updated",
          target: { type: "agent", id: agent.id, name: `${agent.name} (audited)` },
          workspace_id: acme.id,
          workspace: { id: acme.id, name: acme.name, slug: acme.slug, environment: acme.environment },
          environment: acme.environment,
        });
        expect(page.items.every((e) => e.workspace_id === acme.id)).toBe(true);
        expect(page.pagination.total).toBe(page.items.length);
        const harbor = (await larkspur()).workspace.id;
        for (const other of [harbor, "ws_short", uuidOf(acme.id)]) {
          const none = await log(other);
          expect([none.items, none.pagination.total]).toEqual([[], 0]);
        }
      });

      it("keeps a deleted workspace's history", async () => {
        const doomed = await sandboxWorkspace("doomed");
        await owner.workspaces.update(doomed.id, { name: "Doomed" });
        await owner.workspaces.delete(doomed.id);
        const history = await log(doomed.id);
        expect(history.items.map((e) => e.action)).toEqual([
          "workspace.deleted",
          "workspace.updated",
          "workspace.created",
        ]);
        for (const event of history.items) {
          expect([event.workspace_id, event.workspace, event.environment]).toEqual([doomed.id, null, "sandbox"]);
        }
        expect(history.items[0]?.target).toEqual({ type: "workspace", id: doomed.id, name: "Doomed" });
      });

      it("pages newest first, without repeating or skipping an event", async () => {
        const workspace = await sandboxWorkspace("paged");
        for (const n of [1, 2, 3, 4]) await owner.workspaces.update(workspace.id, { name: `Paged ${n}` });
        const whole = await log(workspace.id);
        expect(whole.items.map((e) => e.target.name)).toEqual(["Paged 4", "Paged 3", "Paged 2", "Paged 1", "paged"]);
        const seen: string[] = [];
        let cursor: string | undefined;
        let pages = 0;
        do {
          const page = await owner.audit.list(fx.organization, { workspace_id: workspace.id, limit: 2, cursor });
          expect(page.pagination.total).toBe(5);
          expect(page.items.length).toBeLessThanOrEqual(2);
          seen.push(...page.items.map((e) => e.id));
          cursor = page.pagination.next_cursor ?? undefined;
          pages += 1;
        } while (cursor);
        expect(pages).toBe(3);
        expect(seen).toEqual(whole.items.map((e) => e.id));
        const times = whole.items.map((e) => Date.parse(e.created_at));
        expect(times).toEqual([...times].sort((a, b) => b - a));
        // Limits are clamped to 1–100, and a non-integer one means the default.
        for (const [limit, count] of [
          [0, 1],
          [1000, 5],
          [2.5, 5],
        ] as const) {
          const page = await owner.audit.list(fx.organization, { workspace_id: workspace.id, limit });
          expect(page.items.length).toBe(count);
        }
      });

      it("is read by owners and admins only", async () => {
        for (const persona of ["developer", "viewer"] as const) {
          const refused = await failure(as(persona).audit.list(fx.organization));
          expect([refused.status, refused.code]).toEqual([403, "forbidden"]);
        }
        expect((await failure(as("outsider").audit.list(fx.organization))).status).toBe(404);
      });

      it("is read from the Console only, and names the API key that made a change", async () => {
        const sandbox = fx.workspaces.acmeSandbox;
        const key = await owner.apiKeys.create(fx.organization, { name: "Backend", environment: "sandbox" });
        const refused = await failure(withKey(key.secret).audit.list(fx.organization));
        expect([refused.status, refused.code]).toEqual([403, "console_only"]);
        const { run, callId } = await waitingRun();
        const environment = (await owner.workspaces.get(run.workspace_id)).environment;
        const approver = await owner.apiKeys.create(fx.organization, { name: "Approver", environment });
        const installation = must((await owner.installations.list(sandbox.id))[0], "installation in acme-sandbox");
        await withKey(key.secret).installations.update(sandbox.id, installation.id, { label: "By key" });
        await withKey(approver.secret).runs.resolveApproval(run.workspace_id, run.id, {
          call_id: callId,
          decision: "denied",
        });
        const [resolved, renamed] = (await log()).items;
        expect(renamed).toMatchObject({
          action: "installation.updated",
          actor: { type: "api_key", id: key.key.id, name: "Backend" },
          workspace_id: sandbox.id,
        });
        expect(resolved).toMatchObject({
          action: "approval.resolved",
          actor: { type: "api_key", id: approver.key.id, name: "Approver" },
          target: { type: "run", id: run.id },
          workspace_id: run.workspace_id,
          environment,
        });
      });
    });

    describe("webhooks", () => {
      const endpointIn = async (environment: "sandbox" | "production") =>
        must(
          (await owner.webhooks.list(fx.organization)).find((endpoint) => endpoint.environment === environment),
          `${environment} webhook`,
        );

      it("lists every delivery outcome and returns its lean event and recorded attempts", async () => {
        const endpoint = await endpointIn("production");
        const list = await owner.webhooks.deliveries(fx.organization, endpoint.id);
        expect(new Set(list.items.map((row) => row.state))).toEqual(
          new Set(["pending", "succeeded", "failed", "skipped"]),
        );
        expect(list.items.length).toBeGreaterThanOrEqual(5);
        for (const row of list.items) {
          expect(row.organization_id).toBe(fx.organization);
          expect(row.endpoint_id).toBe(endpoint.id);
          expect(row.environment).toBe("production");
          const detail = await owner.webhooks.delivery(fx.organization, endpoint.id, row.id);
          expect(detail).toMatchObject(row);
          expect(detail.event).toMatchObject({
            id: row.event_id,
            organization_id: fx.organization,
            environment: row.environment,
            type: row.event_type,
            created_at: row.event_created_at,
            workspace: row.workspace,
          });
          expect(detail.attempt_history_complete).toBe(true);
          const attempts = await owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, row.id);
          expect(attempts.items).toHaveLength(row.attempts);
          expect(attempts.items.map((attempt) => attempt.number)).toEqual(
            Array.from({ length: row.attempts }, (_, index) => index + 1),
          );
          const last = attempts.items.at(-1);
          expect(last?.status_code ?? null).toBe(row.last_status_code);
          expect(last?.error_class ?? null).toBe(row.last_error);
          expect(last?.attempted_at ?? null).toBe(row.last_attempt_at);
          for (const attempt of attempts.items) expect(attempt.duration_ms).toBeGreaterThanOrEqual(0);
          expect(row.next_attempt_at === null).toBe(row.state !== "pending");
        }
        const test = must(
          list.items.find((row) => row.event_type === "webhook.test"),
          "test event",
        );
        expect(test.workspace).toBeNull();
      });

      it("pages deliveries newest first and attempts oldest first without overlap", async () => {
        const endpoint = await endpointIn("production");
        const whole = await owner.webhooks.deliveries(fx.organization, endpoint.id, { limit: 100 });
        const seen: WebhookDeliverySummary[] = [];
        let cursor: string | undefined;
        for (let pages = 0; pages < 20; pages++) {
          const page = await owner.webhooks.deliveries(fx.organization, endpoint.id, { cursor, limit: 2 });
          expect(page.items.length).toBeLessThanOrEqual(2);
          expect(page.pagination.total).toBe(whole.items.length);
          seen.push(...page.items);
          cursor = page.pagination.next_cursor ?? undefined;
          if (!cursor) break;
        }
        expect(cursor).toBeUndefined();
        expect(seen).toEqual(whole.items);
        const keys = whole.items.map((row) => `${row.created_at}/${row.id}`);
        expect(keys).toEqual([...keys].sort().reverse());
        const failed = must(
          whole.items.find((row) => row.state === "failed"),
          "failed delivery",
        );
        const numbers: number[] = [];
        for (let pages = 0; pages < 20; pages++) {
          const page = await owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, failed.id, {
            cursor,
            limit: 3,
          });
          expect(page.items.length).toBeLessThanOrEqual(3);
          expect(page.pagination.total).toBe(failed.attempts);
          numbers.push(...page.items.map((row) => row.number));
          cursor = page.pagination.next_cursor ?? undefined;
          if (!cursor) break;
        }
        expect(cursor).toBeUndefined();
        expect(numbers).toEqual(Array.from({ length: failed.attempts }, (_, index) => index + 1));
        for (const [limit, count] of [
          [0, 1],
          [1000, whole.items.length],
          [2.5, whole.items.length],
        ] as const) {
          expect((await owner.webhooks.deliveries(fx.organization, endpoint.id, { limit })).items).toHaveLength(count);
        }
      });

      it("filters exactly by workspace, environment, state and event type", async () => {
        const endpoint = await endpointIn("production");
        const beta = await workspaceBySlug("beta-labs");
        const all = await owner.webhooks.deliveries(fx.organization, endpoint.id);
        for (const workspace of [fx.workspaces.acme, beta]) {
          const page = await owner.webhooks.deliveries(fx.organization, endpoint.id, { workspace_id: workspace.id });
          expect(page.items.length).toBeGreaterThan(0);
          expect(
            page.items.every(
              (row) => row.workspace?.id === workspace.id && row.workspace.external_id === workspace.external_id,
            ),
          ).toBe(true);
          expect(page.items).toEqual(all.items.filter((row) => row.workspace?.id === workspace.id));
        }
        const outsiderWorkspace = must(
          (await as("outsider").workspaces.list(fx.otherOrganization)).items[0],
          "outsider workspace",
        );
        for (const workspace_id of [fx.workspaces.acmeSandbox.id, outsiderWorkspace.id, "ws_missing" as const]) {
          expect((await owner.webhooks.deliveries(fx.organization, endpoint.id, { workspace_id })).items).toEqual([]);
        }
        for (const state of ["pending", "succeeded", "failed", "skipped"] as const) {
          const page = await owner.webhooks.deliveries(fx.organization, endpoint.id, { state });
          expect(page.items).toEqual(all.items.filter((row) => row.state === state));
        }
        expect(
          (await owner.webhooks.deliveries(fx.organization, endpoint.id, { event_type: "webhook.test" })).items,
        ).toEqual(all.items.filter((row) => row.event_type === "webhook.test"));
        expect(
          (
            await owner.webhooks.deliveries(fx.organization, endpoint.id, {
              workspace_id: beta.id,
              state: "succeeded",
              event_type: "run.completed",
            })
          ).items,
        ).toEqual(
          all.items.filter(
            (row) => row.workspace?.id === beta.id && row.state === "succeeded" && row.event_type === "run.completed",
          ),
        );
        // @ts-expect-error: unknown filters are not a request for every event.
        expect((await owner.webhooks.deliveries(fx.organization, endpoint.id, { state: "unknown" })).items).toEqual([]);
        expect(
          // @ts-expect-error: nor an unknown event type.
          (await owner.webhooks.deliveries(fx.organization, endpoint.id, { event_type: "unknown" })).items,
        ).toEqual([]);
      });

      it("binds cursors to their organization, endpoint, filters and delivery", async () => {
        const endpoint = await endpointIn("production");
        const sandbox = await endpointIn("sandbox");
        const first = await owner.webhooks.deliveries(fx.organization, endpoint.id, { limit: 1 });
        const cursor = must(first.pagination.next_cursor, "delivery cursor");
        const invalid = async (call: Promise<unknown>) => {
          expect(await failure(call)).toMatchObject({
            status: 400,
            code: "validation_failed",
            fields: { cursor: expect.any(String) },
          });
        };
        for (const bad of ["!invalid", "e30"])
          await invalid(owner.webhooks.deliveries(fx.organization, endpoint.id, { cursor: bad }));
        await invalid(owner.webhooks.deliveries(fx.organization, sandbox.id, { cursor }));
        await invalid(
          owner.webhooks.deliveries(fx.organization, endpoint.id, { cursor, workspace_id: fx.workspaces.acme.id }),
        );
        await invalid(owner.webhooks.deliveries(fx.organization, endpoint.id, { cursor, state: "pending" }));
        await invalid(owner.webhooks.deliveries(fx.organization, endpoint.id, { cursor, event_type: "run.completed" }));
        const outsider = as("outsider");
        const other = must((await outsider.webhooks.list(fx.otherOrganization))[0], "other webhook");
        await invalid(outsider.webhooks.deliveries(fx.otherOrganization, other.id, { cursor }));
        const all = (await owner.webhooks.deliveries(fx.organization, endpoint.id)).items;
        const delivery = must(
          all.find((row) => row.attempts > 1),
          "retried delivery",
        );
        const sibling = must(
          all.find((row) => row.id !== delivery.id),
          "other delivery",
        );
        const attempts = await owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, delivery.id, { limit: 1 });
        const attemptCursor = must(attempts.pagination.next_cursor, "attempt cursor");
        await invalid(
          owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, sibling.id, { cursor: attemptCursor }),
        );
        await invalid(owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, delivery.id, { cursor }));
        await invalid(owner.webhooks.deliveries(fx.organization, endpoint.id, { cursor: attemptCursor }));
      });

      it("lets viewers and developers read the same history as owners", async () => {
        const endpoint = await endpointIn("sandbox");
        const expected = await owner.webhooks.deliveries(fx.organization, endpoint.id);
        const delivery = must(expected.items[0], "delivery");
        const detail = await owner.webhooks.delivery(fx.organization, endpoint.id, delivery.id);
        const attempts = await owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, delivery.id);
        for (const persona of ["developer", "viewer"] as const) {
          const reader = as(persona);
          expect(await reader.webhooks.deliveries(fx.organization, endpoint.id)).toEqual(expected);
          expect(await reader.webhooks.delivery(fx.organization, endpoint.id, delivery.id)).toEqual(detail);
          expect(await reader.webhooks.deliveryAttempts(fx.organization, endpoint.id, delivery.id)).toEqual(attempts);
        }
      });

      it("hides foreign and wrong-parent delivery IDs before checking their queries", async () => {
        const mine = await endpointIn("production");
        const sibling = await endpointIn("sandbox");
        const outsider = as("outsider");
        const other = must((await outsider.webhooks.list(fx.otherOrganization))[0], "foreign webhook");
        const ownDelivery = must((await owner.webhooks.deliveries(fx.organization, mine.id)).items[0], "own delivery");
        const otherList = await outsider.webhooks.deliveries(fx.otherOrganization, other.id);
        const otherDelivery = must(otherList.items[0], "foreign delivery");
        const checks: [ZupflowClient, OrganizationId, WebhookEndpoint, WebhookDeliveryId][] = [
          [owner, fx.otherOrganization, other, otherDelivery.id],
          [owner, fx.organization, other, otherDelivery.id],
          [owner, fx.organization, mine, otherDelivery.id],
          [owner, fx.organization, sibling, ownDelivery.id],
          [outsider, fx.organization, mine, ownDelivery.id],
          [outsider, fx.otherOrganization, mine, ownDelivery.id],
          [outsider, fx.otherOrganization, other, ownDelivery.id],
        ];
        for (const [client, organization, endpoint, delivery] of checks) {
          expect(await failure(client.webhooks.delivery(organization, endpoint.id, delivery))).toMatchObject({
            status: 404,
            code: "not_found",
          });
          expect(
            await failure(client.webhooks.deliveryAttempts(organization, endpoint.id, delivery, { cursor: "invalid" })),
          ).toMatchObject({ status: 404, code: "not_found" });
          if (
            endpoint.organization_id !== organization ||
            (client === owner && organization === fx.otherOrganization) ||
            (client === outsider && organization === fx.organization)
          ) {
            expect(
              await failure(client.webhooks.deliveries(organization, endpoint.id, { cursor: "invalid" })),
            ).toMatchObject({ status: 404, code: "not_found" });
          }
        }
        expect(await outsider.webhooks.deliveries(fx.otherOrganization, other.id)).toEqual(otherList);
        expect(
          await failure(as(null).webhooks.deliveries(fx.organization, mine.id, { cursor: "invalid" })),
        ).toMatchObject({ status: 401, code: "unauthenticated" });
      });

      it("keeps an API key's delivery history inside its organization and environment", async () => {
        const production = await endpointIn("production");
        const sandbox = await endpointIn("sandbox");
        const endpoints = { production, sandbox };
        for (const environment of ["sandbox", "production"] as const) {
          const key = await owner.apiKeys.create(fx.organization, { name: "Webhook history", environment });
          const client = withKey(key.secret);
          const endpoint = endpoints[environment];
          const other = endpoints[environment === "sandbox" ? "production" : "sandbox"];
          const list = await owner.webhooks.deliveries(fx.organization, endpoint.id);
          const row = must(list.items[0], "delivery in key environment");
          const foreign = must(
            (await owner.webhooks.deliveries(fx.organization, other.id)).items[0],
            "other environment delivery",
          );
          expect(await client.webhooks.deliveries(fx.organization, endpoint.id)).toEqual(list);
          expect(await client.webhooks.delivery(fx.organization, endpoint.id, row.id)).toEqual(
            await owner.webhooks.delivery(fx.organization, endpoint.id, row.id),
          );
          expect(await client.webhooks.deliveryAttempts(fx.organization, endpoint.id, row.id)).toEqual(
            await owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, row.id),
          );
          expect(await failure(client.webhooks.deliveries(fx.organization, other.id))).toMatchObject({
            status: 404,
            code: "not_found",
          });
          expect(await failure(client.webhooks.delivery(fx.organization, other.id, foreign.id))).toMatchObject({
            status: 404,
            code: "not_found",
          });
          expect(await failure(client.webhooks.deliveryAttempts(fx.organization, other.id, foreign.id))).toMatchObject({
            status: 404,
            code: "not_found",
          });
          expect(await failure(client.webhooks.deliveries(fx.otherOrganization, endpoint.id))).toMatchObject({
            status: 404,
            code: "not_found",
          });
        }
      });

      it("records a test event once under idempotency and removes its endpoint's history on deletion", async () => {
        const created = await owner.webhooks.create(fx.organization, {
          environment: "sandbox",
          url: "https://hooks.northwind.example/history",
          description: "History",
          events: ["run.completed"],
        });
        const endpoint = created.endpoint;
        expect(await owner.webhooks.deliveries(fx.organization, endpoint.id)).toEqual({
          items: [],
          pagination: { next_cursor: null, total: 0 },
        });
        const first = await owner.webhooks.sendTest(fx.organization, endpoint.id, {
          idempotencyKey: "delivery-history-test",
        });
        const replay = await owner.webhooks.sendTest(fx.organization, endpoint.id, {
          idempotencyKey: "delivery-history-test",
        });
        expect(replay).toEqual(first);
        const history = await owner.webhooks.deliveries(fx.organization, endpoint.id);
        expect(history.items).toHaveLength(1);
        const row = must(history.items[0], "test delivery");
        expect(row).toMatchObject({
          event_type: "webhook.test",
          workspace: null,
          attempts: 1,
          last_status_code: first.status_code || null,
          last_attempt_at: first.delivered_at,
        });
        const detail = await owner.webhooks.delivery(fx.organization, endpoint.id, row.id);
        expect(detail.event.data).toEqual({ object: "webhook_endpoint", id: endpoint.id });
        expect(JSON.stringify(detail)).not.toContain(created.signing_secret);
        await owner.webhooks.delete(fx.organization, endpoint.id);
        expect(await failure(owner.webhooks.deliveries(fx.organization, endpoint.id))).toMatchObject({
          status: 404,
          code: "not_found",
        });
        expect(await failure(owner.webhooks.delivery(fx.organization, endpoint.id, row.id))).toMatchObject({
          status: 404,
          code: "not_found",
        });
        expect(await failure(owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, row.id))).toMatchObject({
          status: 404,
          code: "not_found",
        });
      });

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

      it("keeps each event type once", async () => {
        const created = await owner.webhooks.create(fx.organization, {
          environment: "sandbox",
          url: "https://hooks.northwind.example/twice",
          description: "",
          events: ["run.completed", "run.failed", "run.completed"],
        });
        expect(created.endpoint.events).toEqual(["run.completed", "run.failed"]);
        const updated = await owner.webhooks.update(fx.organization, created.endpoint.id, {
          events: ["run.failed", "run.failed"],
        });
        expect(updated.events).toEqual(["run.failed"]);
      });

      it("takes public HTTPS URLs only, and never saves an empty one", async () => {
        const insecure = await failure(
          owner.webhooks.create(fx.organization, {
            environment: "sandbox",
            url: "http://hooks.northwind.example/plain",
            description: "",
            events: ["run.completed"],
          }),
        );
        expect([insecure.status, insecure.code, Object.keys(insecure.fields ?? {})]).toEqual([
          422,
          "insecure_url",
          ["url"],
        ]);
        const hook = must((await owner.webhooks.list(fx.organization))[0], "webhook");
        const empty = await failure(owner.webhooks.update(fx.organization, hook.id, { url: "" }));
        expect([empty.status, empty.code, Object.keys(empty.fields ?? {})]).toEqual([
          400,
          "validation_failed",
          ["url"],
        ]);
        const after = (await owner.webhooks.list(fx.organization)).find((h) => h.id === hook.id);
        expect(after?.url).toBe(hook.url);
      });

      it("keeps the test button's answer as the endpoint's last delivery", async () => {
        const hook = must((await owner.webhooks.list(fx.organization))[0], "webhook");
        const delivery = await owner.webhooks.sendTest(fx.organization, hook.id);
        const after = (await owner.webhooks.list(fx.organization)).find((h) => h.id === hook.id);
        expect(after?.last_delivery).toEqual(delivery);
        expect(after?.signing_secret_last4).toBe(hook.signing_secret_last4);
      });
    });
  });
}
