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
  type AgentChannel,
  type AgentId,
  type ApiKeyId,
  type AppId,
  type AuthTemplate,
  agentChannelDefaults,
  CHANNEL_SETTINGS_DEFAULTS,
  CHANNEL_TOOL_NAMES,
  CHANNEL_TOOLS,
  type ChannelAppId,
  type ChannelAppManagerConnectResult,
  type CreateConnectSessionInput,
  createClient,
  createHttpTransport,
  type InstallationId,
  type MemberId,
  type ModelConnectionId,
  type OrganizationId,
  type PutAgentChannelInput,
  type UpdateAppInput,
  type UpdateChannelToolLevelsInput,
  type UpdateInstallationInput,
  type WebhookDeliveryId,
  type WebhookDeliverySummary,
  type WebhookEndpoint,
  type WebhookEventType,
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

/** A Slack configuration token and its refresh token: the fixture's provider accepts any without `invalid`. */
const TOKENS = {
  configuration_token: "xoxe.xoxp-1-contract-token-0001",
  refresh_token: "xoxe-1-contract-refresh-0001",
} as const;

/** An agent channel's settings, as `put` takes them. */
const settingsOf = ({
  id: _id,
  workspace_id: _workspace,
  agent_id: _agent,
  connection_id: _connection,
  created_at: _created,
  updated_at: _updated,
  ...settings
}: AgentChannel): PutAgentChannelInput => settings;

/**
 * Slack's request signature, computed here independently of any server: `v0=`
 * and the hex HMAC-SHA256 of `v0:{timestamp}:{body}` under the signing secret.
 */
async function slackSignature(secret: string, timestamp: string, body: string): Promise<string> {
  const encoder = new TextEncoder();
  const key = await crypto.subtle.importKey("raw", encoder.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, [
    "sign",
  ]);
  const mac = new Uint8Array(await crypto.subtle.sign("HMAC", key, encoder.encode(`v0:${timestamp}:${body}`)));
  return `v0=${Array.from(mac, (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
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
    /** A workspace of the reference organization, found by slug. */
    const workspaceBySlug = async (slug: string) =>
      must(
        (await owner.workspaces.list(fx.organization, { q: slug, limit: 100 })).items.find((w) => w.slug === slug),
        `workspace ${slug}`,
      );
    /** An API key needs no organization header; a supplied one must match its organization. */
    const withKey = (secret: string, organization: OrganizationId | null = null): ZupflowClient => {
      return createClient(
        createHttpTransport({
          baseUrl: target.baseUrl,
          getToken: async () => secret,
          getOrganization: () => organization,
          fetch: servedFetch,
          retries: 0,
        }),
      );
    };
    const owner = as("owner");
    /** A fixed pending call in each environment, read only through the public API. */
    const waitingApproval = async (environment: "sandbox" | "production") => {
      const page = await owner.organizationRuns.list(fx.organization, { environment, status: "waiting_for_approval" });
      const run = must(page.items[0], `${environment} run waiting for approval`);
      const events = await owner.runs.events(run.workspace_id, run.id);
      const requested = [...events].reverse().find((event) => event.type === "approval.requested");
      const callId = must(requested?.type === "approval.requested" ? requested.data.call_id : null, "pending call");
      return { run, callId, events };
    };
    const northwind = () => loadTenant(owner, fx.organization, "acme", "acme-sandbox");
    /** `beta-labs`, given a workspace app of its own first: the fixture has none there. */
    const betaLabs = async () => {
      const beta = await workspaceBySlug("beta-labs");
      if (!(await owner.workspaceApps.list(beta.id)).some((a) => a.owner_workspace_id === beta.id)) {
        await owner.workspaceApps.create(beta.id, {
          kind: "mcp",
          name: "Beta tools",
          key: "beta_tools",
          description: "",
          auth_type: "token",
        });
      }
      return loadTenant(owner, fx.organization, "beta-labs", "qa-playground");
    };
    const larkspur = () => loadTenant(as("outsider"), fx.otherOrganization, "harbor", "harbor-sandbox");

    /** What the channel provider holds for an app: skips the case, naming the operation, when the target can't say. */
    const secretsOf = async (app: ChannelAppId) => {
      if (!target.channelAppSecrets) throw new NotServed("receiveChannelEvent");
      return target.channelAppSecrets(app);
    };
    /**
     * Delivers a provider event to an app's inbound endpoint as Slack would:
     * signed with the app's signing secret and the current time, unless the
     * case says otherwise.
     */
    const deliver = async (
      app: ChannelAppId,
      event: Record<string, unknown>,
      sent: { secret?: string; timestamp?: number; ref?: string } = {},
    ) => {
      const secrets = await secretsOf(app);
      const raw = JSON.stringify(event);
      const timestamp = String(sent.timestamp ?? Math.floor(Date.now() / 1000));
      return servedFetch(`${target.baseUrl}/channels/inbound/${sent.ref ?? secrets.inbound_ref}`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Slack-Request-Timestamp": timestamp,
          "X-Slack-Signature": await slackSignature(sent.secret ?? secrets.signing_secret, timestamp, raw),
        },
        body: raw,
      });
    };
    /** An app read until its job has made it (or failed): a server makes it after answering `202`. */
    const madeApp = async (workspaceId: WorkspaceId, app: ChannelAppId) => {
      for (let attempt = 0; attempt < 50; attempt++) {
        const read = await owner.channelApps.get(workspaceId, app);
        if (read.status !== "creating") return read;
        await new Promise((resolve) => setTimeout(resolve, 100));
      }
      throw new Error(`${app} is still being made`);
    };
    /** A workspace's fixture channel: its linked team, the installed app, its connection and the agent's channel. */
    const installedIn = async (workspaceId: WorkspaceId, client: ZupflowClient = owner) => {
      const connection = must(
        (await client.channelConnections.list(workspaceId, { limit: 100 })).items.find((c) => c.status === "connected"),
        "connected channel connection",
      );
      const app = await client.channelApps.get(workspaceId, must(connection.app_id, "the connection's app"));
      const agentId = must(connection.agent_id, "the connection's agent");
      return {
        connection,
        app,
        agentId,
        manager: await client.channelAppManagers.get(workspaceId, app.manager_id),
        agentChannel: must(
          (await client.agentChannels.list(workspaceId, agentId)).find((c) => c.connection_id === connection.id),
          "the agent's channel",
        ),
        conversations: (await client.channelConnections.conversations(workspaceId, connection.id, { limit: 100 }))
          .items,
      };
    };
    /** An active agent of the workspace with no app yet. */
    const applessAgent = async (workspaceId: WorkspaceId) => {
      const apps = (await owner.channelApps.list(workspaceId, { limit: 100 })).items;
      return must(
        (await owner.agents.list(workspaceId)).find(
          (a) => a.status === "active" && !apps.some((p) => p.agent_id === a.id),
        ),
        "active agent with no app",
      );
    };

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
      const missed = routes.map((r) => r.operationId).filter((id) => serves(id) && !checker.succeeded.has(id));
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
        const approver = await owner.apiKeys.create(org, { name: "Contract approver", environment: "production" });
        const pending = await waitingApproval("production");
        await withKey(approver.secret).runs.resolveApproval(pending.run.workspace_id, pending.run.id, {
          call_id: pending.callId,
          decision: "denied",
        });

        // Apps, at both levels, including OAuth apps that have no endpoints yet.
        const oauthApp = { kind: "mcp", description: "", auth_type: "oauth2" } as const;
        await owner.organizationApps.list(org);
        const orgApp = await owner.organizationApps.create(org, { ...oauthApp, name: "Org OAuth", key: "org_oauth" });
        await owner.organizationApps.get(org, orgApp.id);
        await owner.organizationApps.update(org, orgApp.id, { description: "Changed" });
        await owner.workspaceApps.list(workspace.id);
        const wsApp = await owner.workspaceApps.create(workspace.id, {
          ...oauthApp,
          name: "WS OAuth",
          key: "ws_oauth",
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
          (await owner.installations.list(sandbox.id)).find((i) => i.app.id === fx.apps.taskboard),
          "installation of taskboard in acme-sandbox",
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

        // Grants, of an agent found through its runs.
        const run = must((await owner.runs.list(acme.id)).items[0], "run in acme");
        const grant = must((await owner.grants.list(acme.id, run.agent_id))[0], "grant of the agent of acme's run");
        await owner.grants.update(acme.id, run.agent_id, grant.id, {
          tools: grant.tools,
          approval_required: grant.approval_required,
        });
        // Revoked, then granted again: the same tools of the same installation.
        await owner.grants.delete(acme.id, run.agent_id, grant.id);
        await owner.grants.create(acme.id, run.agent_id, {
          installation_id: grant.installation_id,
          tools: grant.tools,
          approval_required: grant.approval_required,
        });

        // Runs, then the rest of the API.
        await owner.organizationRuns.list(org, { environment: "production" });
        await owner.runs.get(acme.id, run.id);
        await owner.runs.events(acme.id, run.id);
        const sandboxRun = must((await owner.runs.list(sandbox.id)).items[0], "run in acme-sandbox");
        const started = await owner.runs.create(sandbox.id, sandboxRun.agent_id, { input: "Hello" });
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
        await owner.webhooks.update(org, hook.endpoint.id, { enabled: true });
        await owner.webhooks.sendTest(org, hook.endpoint.id);
        const delivery = must((await owner.webhooks.deliveries(org, hook.endpoint.id)).items[0], "test delivery");
        await owner.webhooks.delivery(org, hook.endpoint.id, delivery.id);
        await owner.webhooks.deliveryAttempts(org, hook.endpoint.id, delivery.id);
        await owner.webhooks.resendDelivery(org, hook.endpoint.id, delivery.id);
        await owner.webhooks.rollSecret(org, hook.endpoint.id);
        await owner.webhooks.delete(org, hook.endpoint.id);
        await owner.audit.list(org);
        await owner.workspaces.delete(workspace.id);

        // Agents, on the environment's default model.
        await owner.agents.list(acme.id);
        const agent = await owner.agents.create(sandbox.id, {
          name: "Contract",
          description: "",
          instructions: "Be brief.",
        });
        await owner.agents.get(sandbox.id, agent.id);
        await owner.agents.update(sandbox.id, agent.id, { name: "Contract 2", model_id: null });
        await owner.agents.delete(sandbox.id, agent.id);

        // Model providers last: a server may not serve them yet. Then an agent names a model of its own.
        await owner.modelConnections.list(org, { environment: "sandbox" });
        const connection = await owner.modelConnections.create(org, {
          environment: "sandbox",
          kind: "openai",
          label: "Contract",
          api_key: "sk-test-long-contract-0001-xx",
        });
        await owner.modelConnections.get(org, connection.id);
        await owner.modelConnections.update(org, connection.id, { label: "Contract 2" });
        await owner.modelConnections.replaceKey(org, connection.id, { api_key: "sk-test-long-contract-0002-xx" });
        const offered = must(
          (await owner.modelConnections.providerModels(org, connection.id, { limit: 100 })).items.find(
            (m) => m.supports_tools,
          ),
          "a model that calls tools",
        );
        const model = await owner.models.create(org, {
          connection_id: connection.id,
          provider_model_id: offered.provider_model_id,
        });
        await owner.models.list(org, { environment: "sandbox" });
        await owner.models.get(org, model.id);
        await owner.models.update(org, model.id, { name: "Contract model" });
        const previous = await owner.models.getDefault(org, "sandbox");
        await owner.models.setDefault(org, "sandbox", { model_id: model.id });
        await owner.models.setDefault(org, "sandbox", { model_id: previous.model?.id ?? null });
        const modelled = await owner.agents.create(sandbox.id, {
          name: "Contract model user",
          description: "",
          instructions: "Be brief.",
          model_id: model.id,
        });
        await owner.agents.delete(sandbox.id, modelled.id);
        await owner.models.delete(org, model.id);
        await owner.modelConnections.delete(org, connection.id);

        // Channels: a team linked and an agent's app made in the Sandbox, then the fixture's install read and
        // changed, then removals.
        await owner.channelAppManagers.list(sandbox.id);
        const manager = await owner.channelAppManagers.create(sandbox.id, {
          provider: "slack",
          method: "configuration_token",
          ...TOKENS,
        });
        await owner.channelAppManagers.get(sandbox.id, manager.id);
        await owner.channelAppManagers.update(sandbox.id, manager.id, TOKENS);
        const channelAgent = await owner.agents.create(sandbox.id, {
          name: "Contract channel",
          description: "",
          instructions: "Be brief.",
        });
        const channelApp = await owner.channelApps.create(sandbox.id, {
          agent_id: channelAgent.id,
          manager_id: manager.id,
        });
        await owner.channelApps.list(sandbox.id);
        await owner.channelApps.get(sandbox.id, channelApp.id);
        const installed = await installedIn(sandbox.id);
        await owner.channelConnections.get(sandbox.id, installed.connection.id);
        await owner.channelConnections.update(sandbox.id, installed.connection.id, {
          label: installed.connection.label,
        });
        const conversation = must(installed.conversations[0], "conversation in acme-sandbox");
        await owner.channelConnections.messages(sandbox.id, installed.connection.id, conversation.id);
        await owner.agentChannels.put(
          sandbox.id,
          installed.agentId,
          installed.connection.id,
          settingsOf(installed.agentChannel),
        );
        await owner.channelSettings.toolLevels(org);
        await owner.channelSettings.setToolLevels(org, {
          provider: "slack",
          levels: [{ tool: "react", level: "allow" }],
        });
        await owner.channelSettings.get(org);
        await owner.channelSettings.update(org, CHANNEL_SETTINGS_DEFAULTS);
        await owner.agentChannels.delete(sandbox.id, installed.agentId, installed.connection.id);
        await owner.channelConnections.delete(sandbox.id, installed.connection.id);
        await owner.channelApps.delete(sandbox.id, channelApp.id);
        // A deleted app is removed at the provider by the team's tokens: until then the team is in use.
        for (let attempt = 0; ; attempt++) {
          const refused = await owner.channelAppManagers.delete(sandbox.id, manager.id).then(
            () => null,
            (error: unknown) => {
              if (error instanceof ZupflowApiError && error.status === 409 && error.code === "in_use" && attempt < 50) {
                return error;
              }
              throw error;
            },
          );
          if (!refused) break;
          await new Promise((resolve) => setTimeout(resolve, 100));
        }
        await owner.agents.delete(sandbox.id, channelAgent.id);
        // The provider's endpoint last: it needs what the provider knows of an app, which only the target can say.
        await deliver(installed.app.id, { type: "url_verification", challenge: "contract" });
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
            key: "other",
            description: "",
            auth_type: "token",
          }),
        );
        expect(error.code).toBe("unsupported_app_kind");
      });

      it("takes a snake-case app key of 1 to 24 characters and never changes it", async () => {
        const create = (key: string, workspace = false) => {
          const input = { kind: "mcp", name: "Keyed", key, description: "", auth_type: "token" } as const;
          return workspace
            ? owner.workspaceApps.create(fx.workspaces.qaPlayground.id, input)
            : owner.organizationApps.create(fx.organization, input);
        };
        for (const key of ["", "Keyed", "keyed-app", "_keyed", "keyed_", "keyed__app", "1keyed", "a".repeat(25)]) {
          for (const workspace of [false, true]) {
            const error = await failure(create(key, workspace));
            expect([error.status, error.code, Object.keys(error.fields ?? {})], key).toEqual([
              400,
              "validation_failed",
              ["key"],
            ]);
          }
        }
        const longest = await create(`k${"7".repeat(23)}`);
        expect(longest.key).toBe(`k${"7".repeat(23)}`);
        const catalog = await create("keyed_app2");
        const own = await create("keyed_app3", true);
        // The update input has no key: one sent anyway changes nothing.
        const renamed = await owner.organizationApps.update(fx.organization, catalog.id, {
          name: "Renamed",
          key: "changed",
        } as UpdateAppInput);
        expect([renamed.name, renamed.key]).toEqual(["Renamed", "keyed_app2"]);
        expect((await owner.organizationApps.get(fx.organization, catalog.id)).key).toBe("keyed_app2");
        const ownRenamed = await owner.workspaceApps.update(fx.workspaces.qaPlayground.id, own.id, {
          name: "Renamed",
          key: "changed",
        } as UpdateAppInput);
        expect(ownRenamed.key).toBe("keyed_app3");
        // Installations name their app by its key too.
        const echo = must(
          (await owner.installations.list(fx.workspaces.qaPlayground.id)).find((i) => i.app.id === fx.apps.echoServer),
          "echo installation",
        );
        expect(echo.app.key).toBe("echo_server");
      });

      it("refuses an unknown environment filter on an app's installations", async () => {
        const error = await failure(
          owner.organizationApps.installations(fx.organization, fx.apps.taskboard, {
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
        expect(prod.some((a) => a.app.id === fx.apps.taskboard)).toBe(false);
        expect(sandbox.some((a) => a.app.id === fx.apps.taskboard)).toBe(true);

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
        const newOAuthApp = (key: string) =>
          owner.organizationApps.create(fx.organization, {
            kind: "mcp",
            name: "Provider",
            key,
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
          const app = await newOAuthApp("issuer_validation");
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
          const app = await newOAuthApp("issuer_validation_order");
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
          const catalog = await newOAuthApp("issuer_catalog");
          const own = await owner.workspaceApps.create(ws, {
            kind: "mcp",
            name: "Provider",
            key: "issuer_workspace",
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
            key: "templated_tools",
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
            key: "hosty",
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
            key: "patterned",
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
            key: "ledger",
          });
          const own = await owner.workspaceApps.create(ws, { ...signIn, name: "Ledger", key: "ledger_own" });
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
            key: "ledger_ready",
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
            key: "ledger_identity",
          });
          const own = await owner.workspaceApps.create(ws, { ...signIn, name: "Ledger", key: "ledger_identity_own" });
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
            key: "ledger_scopes",
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
          expect(session.app?.auth_type).toBe("mcp_oauth");
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

      it("keeps app keys unique across the organization, and every conflict the same", async () => {
        const acme = await northwind();
        const beta = await betaLabs();
        const harbor = await larkspur();
        const outsider = as("outsider");
        const app = (key: string) =>
          ({ kind: "mcp", name: "Scoped", key, description: "", auth_type: "token" }) as const;
        const conflicts: ZupflowApiError[] = [];
        const taken = async (promise: Promise<unknown>) => {
          const error = await failure(promise);
          conflicts.push(error);
          return [error.status, error.code, Object.keys(error.fields ?? {})];
        };
        const refused = [409, "app_key_taken", ["key"]];

        // Two workspaces: a key one workspace holds is taken in the other and in the catalog...
        expect(await taken(owner.workspaceApps.create(beta.workspace.id, app(acme.workspaceApp.key)))).toEqual(refused);
        expect(await taken(owner.workspaceApps.create(acme.workspace.id, app(acme.workspaceApp.key)))).toEqual(refused);
        expect(await taken(owner.organizationApps.create(fx.organization, app(acme.workspaceApp.key)))).toEqual(
          refused,
        );
        // ...whichever is created first.
        const onlyInBeta = await owner.workspaceApps.create(beta.workspace.id, app("only_in_beta"));
        expect(await taken(owner.organizationApps.create(fx.organization, app(onlyInBeta.key)))).toEqual(refused);
        expect(await taken(owner.workspaceApps.create(acme.workspace.id, app(onlyInBeta.key)))).toEqual(refused);
        // A catalog key is taken in the catalog and in every workspace.
        const catalogOnly = await owner.organizationApps.create(fx.organization, app("catalog_only"));
        expect(await taken(owner.organizationApps.create(fx.organization, app(catalogOnly.key)))).toEqual(refused);
        expect(await taken(owner.workspaceApps.create(acme.workspace.id, app(catalogOnly.key)))).toEqual(refused);
        expect(await taken(owner.workspaceApps.create(beta.workspace.id, app(catalogOnly.key)))).toEqual(refused);
        // A conflict never says where the key is used.
        for (const error of conflicts) {
          expect([error.message, error.fields]).toEqual([conflicts[0]?.message, conflicts[0]?.fields]);
        }
        expect((await owner.workspaceApps.list(beta.workspace.id)).map((a) => a.key).sort()).toEqual(
          [beta.workspaceApp.key, "only_in_beta"].sort(),
        );

        // Two organizations: the other's keys never count, in either direction.
        expect((await owner.workspaceApps.create(acme.workspace.id, app(harbor.workspaceApp.key))).key).toBe(
          harbor.workspaceApp.key,
        );
        expect((await outsider.workspaceApps.create(harbor.workspace.id, app(acme.workspaceApp.key))).key).toBe(
          acme.workspaceApp.key,
        );
        expect((await outsider.organizationApps.create(harbor.org, app(onlyInBeta.key))).key).toBe(onlyInBeta.key);
        expect((await outsider.organizationApps.create(harbor.org, app(catalogOnly.key))).key).toBe(catalogOnly.key);
      });

      it("keeps installation short IDs unique in their workspace and never finds an installation by one", async () => {
        const tenants = [await northwind(), await betaLabs(), await larkspur()];
        const clients = [owner, owner, as("outsider")];
        for (const [index, t] of tenants.entries()) {
          const client = must(clients[index], "client");
          const listed = await client.installations.list(t.workspace.id);
          expect(new Set(listed.map((i) => i.short_id)).size).toBe(listed.length);
          for (const i of listed) expect(i.short_id).toMatch(/^[a-z2-7]{6}$/);
        }
        // A short ID is a label, never an identifier: no path takes one, in its own workspace or any other.
        for (const t of tenants) {
          for (const [index, other] of tenants.entries()) {
            const client = must(clients[index], "client");
            const error = await failure(
              client.installations.get(other.workspace.id, t.installation.short_id as InstallationId),
            );
            expect([error.status, error.code]).toEqual([404, "not_found"]);
          }
        }
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
        // Keys and slugs another organization already uses are free here.
        const appKey = await r.organizationApps.create(other, {
          kind: "mcp",
          name: "Google Workspace",
          key: "google_workspace",
          description: "",
          auth_type: "token",
        });
        expect(appKey.key).toBe("google_workspace");
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
        const taskboard = must(
          (await r.organizationApps.list(other)).find((a) => a.key === "taskboard"),
          "Larkspur's taskboard app",
        );
        const refused = await failure(
          r.connectSessions.create(harborSandbox, {
            app_id: taskboard.id,
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

      it("sums each day into the totals, and keeps cache tokens within input tokens on days, totals and runs", async () => {
        const fields = [
          "runs",
          "failed_runs",
          "tool_calls",
          "input_tokens",
          "output_tokens",
          "cache_read_input_tokens",
          "cache_write_input_tokens",
        ] as const;
        const withinInput = (counts: {
          input_tokens: number;
          cache_read_input_tokens: number;
          cache_write_input_tokens: number;
        }) => {
          expect(counts.cache_read_input_tokens).toBeGreaterThanOrEqual(0);
          expect(counts.cache_write_input_tokens).toBeGreaterThanOrEqual(0);
          expect(counts.cache_read_input_tokens + counts.cache_write_input_tokens).toBeLessThanOrEqual(
            counts.input_tokens,
          );
        };
        for (const environment of ["sandbox", "production"] as const) {
          const report = await owner.organizations.usage(fx.organization, environment, 90);
          for (const field of fields) {
            expect(report.totals[field], field).toBe(report.days.reduce((n, day) => n + day[field], 0));
          }
          for (const counts of [...report.days, report.totals]) withinInput(counts);
          const runs = await owner.organizationRuns.list(fx.organization, { environment, limit: 100 });
          for (const run of runs.items) withinInput(run.usage);
        }
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
            (i) => i.status === "connected" && i.app.id === fx.apps.taskboard,
          ),
          "connected installation of taskboard in acme-sandbox",
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
        const taskboard = must(
          (await owner.installations.list(sandbox)).find(
            (i) => i.status === "connected" && i.app.id === fx.apps.taskboard,
          ),
          "connected installation of taskboard in acme-sandbox",
        );
        const current = must(
          (await owner.installations.tools(sandbox, taskboard.id, { limit: 100 })).items.find(
            (t) => t.status === "current",
          ),
          "current tool",
        ).name;
        const agent = await owner.agents.create(sandbox, {
          name: "Short-lived",
          description: "",
          instructions: "",
        });
        await owner.grants.create(sandbox, agent.id, {
          installation_id: taskboard.id,
          tools: [current],
          approval_required: [],
        });
        expect((await owner.installations.get(sandbox, taskboard.id)).grant_count).toBe(taskboard.grant_count + 1);
        await owner.agents.delete(sandbox, agent.id);
        expect((await owner.installations.get(sandbox, taskboard.id)).grant_count).toBe(taskboard.grant_count);
        const deleted = await failure(owner.grants.list(sandbox, agent.id));
        expect(`${deleted.status} ${deleted.code}`).toBe("404 not_found");

        const qa = fx.workspaces.qaPlayground.id;
        const echo = must(
          (await owner.installations.list(qa)).find((i) => i.status === "connected" && i.app.id === fx.apps.echoServer),
          "connected installation of echo_server in qa-playground",
        );
        const kept = await owner.agents.create(qa, { name: "Kept", description: "", instructions: "" });
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

      it("creates agents on the environment's default, and refuses a model that names nothing", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const input = { name: "  Defaulted  ", description: "", instructions: "" };
        const refused = await failure(owner.agents.create(qa, { ...input, model_id: "mdl_0000000000000000000000" }));
        expect([refused.status, refused.code, Object.keys(refused.fields ?? {})]).toEqual([
          422,
          "unknown_model",
          ["model_id"],
        ]);
        const agent = await owner.agents.create(qa, { ...input, model_id: null });
        expect([agent.name, agent.model_id, agent.status, agent.grant_count, agent.last_run_at]).toEqual([
          "Defaulted",
          null,
          "active",
          0,
          null,
        ]);
        const change = await failure(owner.agents.update(qa, agent.id, { model_id: "mdl_0000000000000000000000" }));
        expect([change.status, change.code]).toEqual([422, "unknown_model"]);
        const paused = await owner.agents.update(qa, agent.id, { status: "paused" });
        expect([paused.status, paused.model_id, paused.name]).toEqual(["paused", null, "Defaulted"]);
        expect((await owner.agents.list(qa)).find((a) => a.id === agent.id)).toEqual(paused);
      });

      it("creates and changes agents only with a model of their organization and environment", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const sandboxModel = must(
          (await owner.models.list(fx.organization, { environment: "sandbox", limit: 1 })).items[0],
          "Sandbox model",
        );
        const productionModel = must(
          (await owner.models.list(fx.organization, { environment: "production", limit: 1 })).items[0],
          "Production model",
        );
        const foreignModel = must(
          (await as("outsider").models.list(fx.otherOrganization, { limit: 1 })).items[0],
          "the other organization's model",
        );
        const input = { name: "  Modelled  ", description: "", instructions: "" };
        // The other environment's model, another organization's, and an ID that names nothing: one answer.
        for (const model_id of [productionModel.id, foreignModel.id, "mdl_0000000000000000000000" as const]) {
          const refused = await failure(owner.agents.create(qa, { ...input, model_id }));
          expect([refused.status, refused.code, Object.keys(refused.fields ?? {})], model_id).toEqual([
            422,
            "unknown_model",
            ["model_id"],
          ]);
        }
        const agent = await owner.agents.create(qa, input);
        expect([agent.name, agent.model_id, agent.status, agent.grant_count, agent.last_run_at]).toEqual([
          "Modelled",
          null,
          "active",
          0,
          null,
        ]);
        const chosen = await owner.agents.update(qa, agent.id, { model_id: sandboxModel.id });
        expect(chosen.model_id).toBe(sandboxModel.id);
        expect((await owner.models.get(fx.organization, sandboxModel.id)).agent_count).toBe(
          sandboxModel.agent_count + 1,
        );
        const change = await failure(owner.agents.update(qa, agent.id, { model_id: productionModel.id }));
        expect([change.status, change.code]).toEqual([422, "unknown_model"]);
        // Omitted keeps the model; null goes back to the default.
        const paused = await owner.agents.update(qa, agent.id, { status: "paused" });
        expect([paused.status, paused.model_id, paused.name]).toEqual(["paused", sandboxModel.id, "Modelled"]);
        expect((await owner.agents.list(qa)).find((a) => a.id === agent.id)).toEqual(paused);
        expect((await owner.agents.update(qa, agent.id, { model_id: null })).model_id).toBeNull();
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
            (i) => i.status === "connected" && i.app.id === fx.apps.taskboard,
          ),
          "connected installation of taskboard in acme-sandbox",
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
        expect(Object.fromEntries(tools.map((tool) => [tool.name, [tool.status, tool.effect]]))).toEqual({
          create_task: ["changed", "write"],
          delete_task: ["changed", "destructive"],
          export_tasks: ["gone", "read"],
          list_tasks: ["current", "read"],
        });
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
          "installation of google_workspace in acme",
        );
      const probe = async (workspace: WorkspaceId) =>
        owner.agents.create(workspace, {
          name: "Policy probe",
          description: "",
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
          "installation of echo_server in qa-playground",
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
            (i) => i.status === "connected" && i.app.id === fx.apps.taskboard,
          ),
          "connected installation of taskboard in acme-sandbox",
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

      it("gives each new installation a short ID kept across renames and reconnects", async () => {
        const input = { values: { token: "short-id-token" } };
        const first = (await owner.connect.submitCredentials(referenceOf(await create()), input)).installation;
        expect(first.short_id).toMatch(/^[a-z2-7]{6}$/);
        const renamed = await owner.installations.update(qa(), first.id, { label: "Echo renamed" });
        expect([renamed.label, renamed.short_id]).toEqual(["Echo renamed", first.short_id]);
        await owner.installations.disconnect(qa(), first.id);
        expect((await owner.installations.get(qa(), first.id)).short_id).toBe(first.short_id);
        const reconnect = await create({ intent: "reconnect", installation_id: first.id });
        const reconnected = (await owner.connect.submitCredentials(referenceOf(reconnect), input)).installation;
        expect([reconnected.id, reconnected.status, reconnected.short_id]).toEqual([
          first.id,
          "connected",
          first.short_id,
        ]);
        // The update input has no short ID: one sent anyway changes nothing.
        const kept = await owner.installations.update(qa(), first.id, {
          label: "Echo kept",
          short_id: "aaaaaa",
        } as UpdateInstallationInput);
        expect(kept.short_id).toBe(first.short_id);
        // A deleted installation's short ID isn't issued again; a new installation gets its own.
        await owner.installations.delete(qa(), first.id);
        const second = (await owner.connect.submitCredentials(referenceOf(await create()), input)).installation;
        expect(second.id).not.toBe(first.id);
        expect(second.short_id).not.toBe(first.short_id);
        const listed = await owner.installations.list(qa());
        expect(new Set(listed.map((i) => i.short_id)).size).toBe(listed.length);
      });

      it("records immutable Connect lifecycle events once, within the endpoint's scope", async () => {
        const events = [
          "installation.connected",
          "installation.disconnected",
          "connect_session.completed",
          "connect_session.failed",
        ] as const;
        const makeEndpoint = (environment: "sandbox" | "production", subscribed: WebhookEventType[] = [...events]) =>
          owner.webhooks.create(fx.organization, {
            environment,
            url: "https://hooks.northwind.example/lifecycle",
            description: "Lifecycle",
            events: subscribed,
          });
        const hook = (await makeEndpoint("sandbox")).endpoint;
        const production = (await makeEndpoint("production")).endpoint;
        const unsubscribed = (await makeEndpoint("sandbox", ["run.completed"])).endpoint;
        const foreign = (
          await as("outsider").webhooks.create(fx.otherOrganization, {
            environment: "sandbox",
            url: "https://hooks.larkspur.example/lifecycle",
            description: "Other organization lifecycle",
            events: [...events],
          })
        ).endpoint;
        const originalWorkspace = await owner.workspaces.get(qa());
        const session = await create();
        const reference = referenceOf(session);
        const input = { values: { token: "lifecycle-private-token" } };
        const options = { idempotencyKey: "lifecycle-completion" };
        const result = await owner.connect.submitCredentials(reference, input, options);
        expect(await owner.connect.submitCredentials(reference, input, options)).toEqual(result);
        expect((await failure(owner.connect.submitCredentials(reference, input))).code).toBe("session_not_pending");
        await owner.installations.disconnect(qa(), result.installation.id);
        await owner.installations.disconnect(qa(), result.installation.id);
        const reconnect = await create({ intent: "reconnect", installation_id: result.installation.id });
        await owner.connect.submitCredentials(referenceOf(reconnect), input);
        const cancelled = await create({ intent: "reconnect", installation_id: result.installation.id });
        await owner.connect.cancel(referenceOf(cancelled));
        expect((await failure(owner.connect.cancel(referenceOf(cancelled)))).code).toBe("session_not_pending");
        const before = await owner.webhooks.deliveries(fx.organization, hook.id, { limit: 100 });
        expect(before.items).toHaveLength(6);
        const snapshots = await Promise.all(
          before.items.map((row) => owner.webhooks.delivery(fx.organization, hook.id, row.id)),
        );
        const completed = snapshots.filter((row) => row.event.type === "connect_session.completed");
        expect(completed).toHaveLength(2);
        for (const row of completed)
          expect(row.event.data).toMatchObject({
            status: "completed",
            installation_id: result.installation.id,
            target: null,
            failure_code: null,
          });
        expect(snapshots.find((row) => row.event.type === "connect_session.failed")?.event.data).toEqual({
          object: "connect_session",
          id: cancelled.id,
          status: "cancelled",
          installation_id: null,
          target: null,
          failure_code: null,
        });
        await owner.workspaces.update(qa(), { external_id: "lifecycle-renamed" });
        await owner.installations.delete(qa(), result.installation.id);
        const after = await owner.webhooks.deliveries(fx.organization, hook.id, { limit: 100 });
        expect(after.items).toHaveLength(7);
        expect(new Set(after.items.map((row) => row.event_id)).size).toBe(7);
        for (const row of snapshots) {
          expect(await owner.webhooks.delivery(fx.organization, hook.id, row.id)).toEqual(row);
          expect(row.event.workspace).toEqual({ id: qa(), external_id: originalWorkspace.external_id });
          expect(row.event).toMatchObject({ organization_id: fx.organization, environment: "sandbox" });
        }
        const deletion = must(
          after.items.find((row) => !before.items.some((old) => old.id === row.id)),
          "deletion event",
        );
        expect((await owner.webhooks.delivery(fx.organization, hook.id, deletion.id)).event).toMatchObject({
          type: "installation.disconnected",
          workspace: { id: qa(), external_id: "lifecycle-renamed" },
          data: { id: result.installation.id, app_id: fx.apps.echoServer, status: "disconnected" },
        });
        expect((await owner.webhooks.deliveries(fx.organization, production.id)).items).toEqual([]);
        expect((await owner.webhooks.deliveries(fx.organization, unsubscribed.id)).items).toEqual([]);
        expect((await as("outsider").webhooks.deliveries(fx.otherOrganization, foreign.id)).items).toEqual([]);
        expect(
          (await owner.webhooks.deliveries(fx.organization, hook.id, { workspace_id: fx.workspaces.acmeSandbox.id }))
            .items,
        ).toEqual([]);
        expect((await failure(as("outsider").webhooks.deliveries(fx.organization, hook.id))).status).toBe(404);
        expect(JSON.stringify(snapshots)).not.toContain(input.values.token);
        expect(JSON.stringify(snapshots)).not.toContain(reference);
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

    describe("API-key authentication", () => {
      it("shows each environment's secret once, records use and refuses revoked or forged credentials", async () => {
        for (const environment of ["sandbox", "production"] as const) {
          const input = { name: `  Backend ${environment}  `, environment };
          const options = { idempotencyKey: `create-key-${environment}` };
          const created = await owner.apiKeys.create(fx.organization, input, options);
          const { key, secret } = created;
          const label = environment === "sandbox" ? "test" : "live";
          expect(new RegExp(`^zf_${label}_[0-9A-Za-z]{44}$`).test(secret)).toBe(true);
          expect(key).toMatchObject({
            organization_id: fx.organization,
            environment,
            name: `Backend ${environment}`,
            prefix: secret.slice(0, 12),
            last4: secret.slice(-4),
            last_used_at: null,
            revoked_at: null,
          });
          expect(await failure(owner.apiKeys.create(fx.organization, input, options))).toMatchObject({
            status: 409,
            code: "idempotency_key_completed",
          });
          const listed = (await owner.apiKeys.list(fx.organization)).filter((row) => row.id === key.id);
          expect(listed).toEqual([key]);
          expect(JSON.stringify(listed).includes(secret)).toBe(false);
          expect(JSON.stringify(listed).includes("secret_hash")).toBe(false);
          await withKey(secret).toolPolicy.get(fx.organization);
          const used = must(
            (await owner.apiKeys.list(fx.organization)).find((row) => row.id === key.id),
            "used key",
          );
          expect(used.last_used_at).not.toBeNull();
          const revoked = await owner.apiKeys.revoke(fx.organization, key.id);
          expect(revoked.revoked_at).not.toBeNull();
          expect(JSON.stringify(revoked).includes(secret)).toBe(false);
          expect(await failure(owner.apiKeys.revoke(fx.organization, key.id))).toMatchObject({
            status: 409,
            code: "already_revoked",
          });
          const changed = `${secret.slice(0, -1)}${secret.endsWith("x") ? "y" : "x"}`;
          for (const bearer of [secret, changed, `zf_${label}_`]) {
            const client = withKey(bearer);
            for (const attempt of [
              () => client.toolPolicy.get(fx.organization),
              () => client.me.get(),
              () => client.workspaces.list(fx.organization),
            ]) {
              expect(await failure(attempt())).toMatchObject({ status: 401, code: "unauthenticated" });
            }
          }
        }
      });

      it("binds keys to their organization and environment, including absent and conflicting headers", async () => {
        const outsider = as("outsider");
        for (const [console, organization, foreignOrganization] of [
          [owner, fx.organization, fx.otherOrganization],
          [outsider, fx.otherOrganization, fx.organization],
        ] as const) {
          const foreignConsole = organization === fx.organization ? outsider : owner;
          const foreign = must(
            (await foreignConsole.workspaces.list(foreignOrganization)).items[0],
            "foreign workspace",
          );
          const foreignBefore = await foreignConsole.workspaces.get(foreign.id);
          for (const environment of ["sandbox", "production"] as const) {
            const created = await console.apiKeys.create(organization, { name: "Scoped backend", environment });
            const client = withKey(created.secret);
            const otherEnvironment = environment === "sandbox" ? "production" : "sandbox";
            const ownPage = await console.workspaces.list(organization, { environment, limit: 100 });
            const own = must(ownPage.items[0], "own environment workspace");
            const other = must(
              (await console.workspaces.list(organization, { environment: otherEnvironment })).items[0],
              "other environment workspace",
            );
            const before = await console.workspaces.get(other.id);
            expect(
              (await client.workspaces.list(organization, { limit: 100 })).items.map((row) => row.id).sort(),
            ).toEqual(ownPage.items.map((row) => row.id).sort());
            expect((await client.workspaces.get(own.id)).id).toBe(own.id);
            expect((await withKey(created.secret, organization).workspaces.get(own.id)).id).toBe(own.id);
            for (const attempt of [
              () => client.workspaces.list(foreignOrganization),
              () => client.workspaces.get(foreign.id),
              () => client.workspaces.update(foreign.id, { name: "Unreachable" }),
              () => client.workspaces.delete(foreign.id),
              () => withKey(created.secret, foreignOrganization).workspaces.get(own.id),
              () => withKey(created.secret, foreignOrganization).workspaces.get(foreign.id),
              () => client.workspaces.get(other.id),
              () => client.workspaces.update(other.id, { name: "Unreachable" }),
              () => client.workspaces.delete(other.id),
            ])
              expect(await failure(attempt())).toMatchObject({ status: 404, code: "not_found" });
            for (const header of [" ", "org_invalid"]) {
              const response = await servedFetch(`${target.baseUrl}/v1/workspaces/${own.id}`, {
                headers: { Authorization: `Bearer ${created.secret}`, "Zupflow-Organization": header },
              });
              expect(response.status).toBe(404);
              expect(await response.json()).toMatchObject({ error: { code: "not_found" } });
            }
            expect(await console.workspaces.get(other.id)).toEqual(before);
            expect(await foreignConsole.workspaces.get(foreign.id)).toEqual(foreignBefore);
            for (const attempt of [
              () => client.workspaces.list(organization, { environment: other.environment }),
              () =>
                client.workspaces.create(organization, {
                  name: "Other environment",
                  slug: `other-${environment}`,
                  environment: other.environment,
                  external_id: null,
                }),
            ])
              expect(await failure(attempt())).toMatchObject({ status: 422, code: "environment_mismatch" });
            expect(
              (await console.workspaces.list(organization, { environment, limit: 100 })).items
                .map((row) => row.id)
                .sort(),
            ).toEqual(ownPage.items.map((row) => row.id).sort());
            expect((await console.workspaces.list(organization, { q: `other-${environment}` })).items).toEqual([]);
          }
        }
      });

      it("keeps child resources scoped to their workspace for API callers too", async () => {
        for (const environment of ["sandbox", "production"] as const) {
          const key = await owner.apiKeys.create(fx.organization, { name: "Workspace scope", environment });
          const client = withKey(key.secret);
          const first = environment === "sandbox" ? fx.workspaces.acmeSandbox : fx.workspaces.acme;
          const second = environment === "sandbox" ? fx.workspaces.qaPlayground : await workspaceBySlug("beta-labs");
          const agent = must((await owner.agents.list(first.id))[0], "first workspace agent");
          const agentsBefore = await owner.agents.list(first.id);
          for (const attempt of [
            () => client.agents.get(second.id, agent.id),
            () => client.agents.update(second.id, agent.id, { name: "Foreign path" }),
            () => client.agents.delete(second.id, agent.id),
            () => client.grants.list(second.id, agent.id),
          ])
            expect(await failure(attempt())).toMatchObject({ status: 404, code: "not_found" });
          expect(await owner.agents.list(first.id)).toEqual(agentsBefore);
          expect((await client.agents.get(first.id, agent.id)).id).toBe(agent.id);
        }
      });

      it("authenticates a key as the organization's backend after its creator loses membership", async () => {
        const developer = as("developer");
        const creator = (await developer.me.get()).user;
        const member = must(
          (await owner.members.list(fx.organization)).find((row) => row.user.id === creator.id),
          "key creator's membership",
        );
        const key = await developer.apiKeys.create(fx.organization, {
          name: "Independent backend",
          environment: "sandbox",
        });
        await owner.members.remove(fx.organization, member.id);
        expect(await failure(developer.workspaces.get(fx.workspaces.qaPlayground.id))).toMatchObject({
          status: 404,
          code: "not_found",
        });
        const workspace = await withKey(key.secret).workspaces.create(fx.organization, {
          name: "Backend workspace",
          slug: "independent-backend",
          environment: "sandbox",
          external_id: null,
        });
        expect(workspace.organization_id).toBe(fx.organization);
        expect((await owner.audit.list(fx.organization, { workspace_id: workspace.id })).items).toEqual(
          expect.arrayContaining([
            expect.objectContaining({
              action: "workspace.created",
              actor: { type: "api_key", id: key.key.id, name: "Independent backend" },
            }),
          ]),
        );
      });

      it("filters organization views and refuses another environment's configuration", async () => {
        for (const environment of ["sandbox", "production"] as const) {
          const key = await owner.apiKeys.create(fx.organization, { name: "Environment views", environment });
          const client = withKey(key.secret);
          const other = environment === "sandbox" ? "production" : "sandbox";
          const appId = fx.apps.googleWorkspace;
          const app = await client.organizationApps.get(fx.organization, appId);
          expect(app.configurations.map((row) => row.environment)).toEqual([environment]);
          const installations = await owner.organizationApps.installations(fx.organization, appId, {
            environment,
            limit: 100,
          });
          expect(app.installation_count).toBe(installations.pagination.total);
          expect(await client.organizationApps.installations(fx.organization, appId, { limit: 100 })).toEqual(
            installations,
          );
          for (const listed of await client.organizationApps.list(fx.organization)) {
            expect(listed.configurations.map((row) => row.environment)).toEqual([environment]);
          }
          const before = await owner.organizationApps.getConfiguration(fx.organization, appId, other);
          for (const attempt of [
            () => client.organizationApps.getConfiguration(fx.organization, appId, other),
            () =>
              client.organizationApps.updateConfiguration(fx.organization, appId, other, {
                oauth_client_id: "unreachable",
              }),
            () => client.organizationApps.installations(fx.organization, appId, { environment: other }),
            () => client.organizationRuns.list(fx.organization, { environment: other }),
            () => client.organizations.usage(fx.organization, other),
            () => client.connectSettings.get(fx.organization, other),
            () => client.connectSettings.update(fx.organization, other, { return_urls: [], allowed_origins: [] }),
            () =>
              client.webhooks.create(fx.organization, {
                environment: other,
                url: "https://hooks.northwind.example/unreachable",
                description: "",
                events: ["run.completed"],
              }),
          ])
            expect(await failure(attempt())).toMatchObject({ status: 422, code: "environment_mismatch" });
          expect(await owner.organizationApps.getConfiguration(fx.organization, appId, other)).toEqual(before);
          expect(
            (await client.organizationRuns.list(fx.organization, { limit: 100 })).items.every(
              (run) => run.workspace.environment === environment,
            ),
          ).toBe(true);
          expect(await client.organizations.usage(fx.organization, environment)).toEqual(
            await owner.organizations.usage(fx.organization, environment),
          );
          expect(await client.connectSettings.get(fx.organization, environment)).toEqual(
            await owner.connectSettings.get(fx.organization, environment),
          );
          expect(
            (await client.webhooks.list(fx.organization)).every((endpoint) => endpoint.environment === environment),
          ).toBe(true);
          const ownWorkspace = environment === "sandbox" ? fx.workspaces.acmeSandbox : fx.workspaces.acme;
          for (const available of await client.workspaceApps.available(ownWorkspace.id)) {
            expect(available.app.configurations.map((row) => row.environment)).toEqual([environment]);
          }
          expect(
            await failure(client.organizationApps.update(fx.organization, appId, { auth: app.auth })),
          ).toMatchObject({
            status: 403,
            code: "console_only",
          });
          expect(await owner.organizationApps.get(fx.organization, appId)).toMatchObject({ auth: app.auth });
        }
      });

      it("lets keys configure their workspace apps, start Connect and run in either environment", async () => {
        for (const environment of ["sandbox", "production"] as const) {
          const key = await owner.apiKeys.create(fx.organization, { name: "Backend actions", environment });
          const client = withKey(key.secret);
          const workspace = await client.workspaces.create(fx.organization, {
            name: `Backend ${environment}`,
            slug: `backend-${environment}`,
            environment,
            external_id: null,
          });
          const app = await client.workspaceApps.create(workspace.id, {
            kind: "mcp",
            name: "Backend tools",
            // Keys are unique across the organization: one per environment's workspace.
            key: `backend_tools_${environment}`,
            description: "",
            auth_type: "token",
          });
          const updated = await client.workspaceApps.update(workspace.id, app.id, { auth: app.auth });
          expect(updated.auth).toEqual(app.auth);
          const configuration = await client.workspaceApps.updateConfiguration(workspace.id, app.id, {
            mcp_server_url: "https://tools.northwind.example/mcp",
          });
          expect(configuration.updated_by).toBeNull();
          const session = await client.connectSessions.create(workspace.id, {
            app_id: app.id,
            intent: "create",
            label: "Backend",
            installation_id: null,
            return_url: null,
          });
          expect(session).toMatchObject({ workspace_id: workspace.id, environment, status: "pending" });
          await client.connect.cancel(referenceOf(session));
          const agent = await client.agents.create(workspace.id, {
            name: "Backend agent",
            description: "",
            instructions: "",
          });
          const run = await client.runs.create(workspace.id, agent.id, { input: "Hello" });
          expect(run).toMatchObject({ workspace_id: workspace.id, trigger: "api" });
          await client.runs.cancel(workspace.id, run.id);
          const cancelled = (await client.runs.events(workspace.id, run.id)).find(
            (event) => event.type === "run.cancelled",
          );
          expect(cancelled).toMatchObject({ data: { cancelled_by: "Backend actions" } });
          const events = (await owner.audit.list(fx.organization, { workspace_id: workspace.id })).items;
          expect(events.some((event) => event.action === "app_configuration.updated")).toBe(true);
          for (const event of events.filter((row) => row.actor.type !== "system")) {
            expect(event.actor).toEqual({ type: "api_key", id: key.key.id, name: "Backend actions" });
          }
        }
      });

      it("refuses valid keys on every Console-only route before IDs, bodies or idempotency", async () => {
        const key = await owner.apiKeys.create(fx.organization, { name: "Caller type", environment: "sandbox" });
        for (const route of routes.filter((route) => route.callers.length === 1 && route.callers[0] === "console")) {
          if (!serves(route.operationId)) continue;
          const path = route.path.replace(/\{[^}]+\}/g, "missing");
          const response = await servedFetch(`${target.baseUrl}${path}`, {
            method: route.method.toUpperCase(),
            headers: { Authorization: `Bearer ${key.secret}`, "Idempotency-Key": "x".repeat(256) },
            ...(route.method === "get" ? {} : { body: "not json" }),
          });
          expect(response.status, route.operationId).toBe(403);
          expect(await response.json(), route.operationId).toMatchObject({ error: { code: "console_only" } });
        }
      });

      it("scopes idempotency to a key and reauthenticates before returning saved answers", async () => {
        const first = await owner.apiKeys.create(fx.organization, { name: "First backend", environment: "sandbox" });
        const second = await owner.apiKeys.create(fx.organization, { name: "Second backend", environment: "sandbox" });
        const a = withKey(first.secret);
        const b = withKey(second.secret);
        const workspace = fx.workspaces.qaPlayground.id;
        const input = { name: "Same request", description: "", instructions: "" };
        const options = { idempotencyKey: "shared-by-credentials" };
        const own = await a.agents.create(workspace, input, options);
        expect(await a.agents.create(workspace, input, options)).toEqual(own);
        const theirs = await b.agents.create(workspace, input, options);
        expect(theirs.id).not.toBe(own.id);
        const console = await owner.agents.create(workspace, input, options);
        expect(new Set([own.id, theirs.id, console.id]).size).toBe(3);
        await owner.apiKeys.revoke(fx.organization, first.key.id);
        expect(await failure(a.agents.create(workspace, input, options))).toMatchObject({
          status: 401,
          code: "unauthenticated",
        });
        expect(await b.agents.create(workspace, input, options)).toEqual(theirs);
        expect((await owner.agents.list(workspace)).filter((agent) => agent.name === input.name)).toHaveLength(3);
      });

      it("records one winner when two keys decide the same pending call concurrently", async () => {
        const { run, callId } = await waitingApproval("sandbox");
        const first = await owner.apiKeys.create(fx.organization, { name: "First approver", environment: "sandbox" });
        const second = await owner.apiKeys.create(fx.organization, { name: "Second approver", environment: "sandbox" });
        const attempts = await Promise.allSettled([
          withKey(first.secret).runs.resolveApproval(run.workspace_id, run.id, {
            call_id: callId,
            decision: "approved",
          }),
          withKey(second.secret).runs.resolveApproval(run.workspace_id, run.id, {
            call_id: callId,
            decision: "denied",
          }),
        ]);
        expect(attempts.filter((attempt) => attempt.status === "fulfilled")).toHaveLength(1);
        const loser = must(
          attempts.find((attempt) => attempt.status === "rejected"),
          "refused second decision",
        );
        expect(loser.status === "rejected" ? loser.reason : null).toMatchObject({ status: 409, code: "not_waiting" });
        const decisions = (await owner.runs.events(run.workspace_id, run.id)).filter(
          (event) => event.type === "approval.resolved",
        );
        expect(decisions).toHaveLength(1);
        const audits = (await owner.audit.list(fx.organization, { workspace_id: run.workspace_id })).items.filter(
          (event) => event.action === "approval.resolved" && event.target.id === run.id,
        );
        expect(audits).toHaveLength(1);
      });

      for (const environment of ["sandbox", "production"] as const) {
        for (const decision of ["approved", "denied"] as const) {
          it(`records a ${decision} approval in ${environment} only for a key in that organization and environment`, async () => {
            const { run, callId, events } = await waitingApproval(environment);
            const otherEnvironment = environment === "sandbox" ? "production" : "sandbox";
            const own = await owner.apiKeys.create(fx.organization, { name: "Approver", environment });
            const wrong = await owner.apiKeys.create(fx.organization, {
              name: "Other environment",
              environment: otherEnvironment,
            });
            const foreign = await as("outsider").apiKeys.create(fx.otherOrganization, {
              name: "Other organization",
              environment,
            });
            const input = { call_id: callId, decision, reason: "  The product recorded this decision.  " };
            for (const secret of [wrong.secret, foreign.secret]) {
              expect(
                await failure(withKey(secret).runs.resolveApproval(run.workspace_id, run.id, input)),
              ).toMatchObject({
                status: 404,
                code: "not_found",
              });
            }
            expect(await owner.runs.events(run.workspace_id, run.id)).toEqual(events);
            const client = withKey(own.secret);
            const sibling = must(
              (await owner.workspaces.list(fx.organization, { environment })).items.find(
                (workspace) => workspace.id !== run.workspace_id,
              ),
              "sibling workspace",
            );
            expect(await failure(client.runs.resolveApproval(sibling.id, run.id, input))).toMatchObject({
              status: 404,
              code: "not_found",
            });
            const anotherCall = (await waitingApproval(otherEnvironment)).callId;
            expect(
              await failure(client.runs.resolveApproval(run.workspace_id, run.id, { ...input, call_id: anotherCall })),
            ).toMatchObject({
              status: 409,
              code: "not_waiting",
            });
            const options = { idempotencyKey: "approval-once" };
            const resolved = await client.runs.resolveApproval(run.workspace_id, run.id, input, options);
            expect(resolved.status).toBe("running");
            expect(await client.runs.resolveApproval(run.workspace_id, run.id, input, options)).toEqual(resolved);
            expect(await failure(client.runs.resolveApproval(run.workspace_id, run.id, input))).toMatchObject({
              status: 409,
              code: "not_waiting",
            });
            const decisions = (await owner.runs.events(run.workspace_id, run.id)).filter(
              (event) => event.type === "approval.resolved",
            );
            expect(decisions).toHaveLength(1);
            expect(decisions[0]).toMatchObject({ data: { call_id: callId, decision, api_key_id: own.key.id } });
            const audit = (await owner.audit.list(fx.organization, { workspace_id: run.workspace_id })).items.filter(
              (event) => event.action === "approval.resolved" && event.target.id === run.id,
            );
            expect(audit).toHaveLength(1);
            expect(audit[0]).toMatchObject({
              environment,
              actor: { type: "api_key", id: own.key.id, name: "Approver" },
            });
          });
        }
      }
    });

    describe("idempotency", () => {
      const agentInput = async (name: string) => ({
        name,
        description: "",
        instructions: "Be brief.",
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
        expect(JSON.stringify(error).includes(created.secret)).toBe(false);
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
        await owner.workspaces.delete(a);
        expect(await owner.agents.create(b, input, { idempotencyKey: "suite-per-workspace" })).toEqual(inB);
        expect((await owner.agents.list(b)).filter((x) => x.name === "Per workspace")).toHaveLength(1);
      });

      it("keeps an organization-scoped saved response after its workspace is deleted", async () => {
        const input = {
          name: "Saved workspace",
          slug: "saved-workspace",
          environment: "sandbox",
          external_id: null,
        } as const;
        const options = { idempotencyKey: "suite-deleted-workspace" };
        const created = await owner.workspaces.create(fx.organization, input, options);
        await owner.workspaces.delete(created.id);
        expect(await owner.workspaces.create(fx.organization, input, options)).toEqual(created);
        expect(await failure(owner.workspaces.get(created.id))).toMatchObject({ status: 404, code: "not_found" });
        expect((await owner.workspaces.list(fx.organization, { q: input.slug })).items).toEqual([]);
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

    describe("model providers", () => {
      const org = () => fx.organization;
      /** A fresh Sandbox connection, so a case never depends on the fixture's own. */
      const connect = (
        kind: "openai" | "anthropic" | "openrouter" = "openrouter",
        api_key = "sk-test-long-suite-0000-xx",
      ) => owner.modelConnections.create(org(), { environment: "sandbox", kind, label: "Suite", api_key });
      const toolModel = async (connectionId: ModelConnectionId) =>
        must(
          (await owner.modelConnections.providerModels(org(), connectionId, { limit: 100 })).items.find(
            (m) => m.supports_tools,
          ),
          "a model that calls tools",
        );
      const firstModel = async (environment: "sandbox" | "production") =>
        must((await owner.models.list(org(), { environment, limit: 1 })).items[0], `${environment} model`);

      it("connects a provider in one environment, and never shows its key", async () => {
        const secret = "sk-suite-first-key-7Qx9";
        const created = await owner.modelConnections.create(org(), {
          environment: "sandbox",
          kind: "openrouter",
          label: "  Suite  ",
          api_key: secret,
        });
        expect(created).toMatchObject({
          organization_id: org(),
          environment: "sandbox",
          kind: "openrouter",
          label: "Suite",
          status: "active",
          key_version: 1,
          model_count: 0,
          api_key: { set: true, last4: "7Qx9" },
        });
        const sandbox = await owner.modelConnections.list(org(), { environment: "sandbox" });
        expect(sandbox.map((c) => c.id)).toContain(created.id);
        expect(sandbox.every((c) => c.environment === "sandbox")).toBe(true);
        const production = await owner.modelConnections.list(org(), { environment: "production" });
        expect(production.map((c) => c.id)).not.toContain(created.id);
        expect(production.every((c) => c.environment === "production")).toBe(true);
        const second = "sk-suite-second-K2pa";
        const replaced = await owner.modelConnections.replaceKey(org(), created.id, { api_key: second });
        expect([replaced.key_version, replaced.api_key.last4, replaced.status]).toEqual([2, "K2pa", "active"]);
        const renamed = await owner.modelConnections.update(org(), created.id, { label: "Suite 2" });
        expect([renamed.label, renamed.key_version]).toEqual(["Suite 2", 2]);
        const everything = JSON.stringify([
          created,
          replaced,
          renamed,
          await owner.modelConnections.get(org(), created.id),
          await owner.modelConnections.list(org()),
          await owner.audit.list(org(), { limit: 100 }),
        ]);
        expect(everything.includes(secret)).toBe(false);
        expect(everything.includes(second)).toBe(false);
      });

      it("refuses a key the provider refuses, and keeps the key it had", async () => {
        const before = await owner.modelConnections.list(org());
        const submitted = "sk-test-long-invalid-0000";
        const refused = await failure(connect("openai", submitted));
        expect([refused.status, refused.code, Object.keys(refused.fields ?? {})]).toEqual([
          422,
          "invalid_api_key",
          ["api_key"],
        ]);
        // The refusal never repeats the key: not in its message, nor in a field's.
        expect(JSON.stringify({ ...refused, message: refused.message }).includes(submitted)).toBe(false);
        expect(await owner.modelConnections.list(org())).toEqual(before);
        const connection = await connect("openai", "sk-suite-kept-key-Ab12");
        const kept = await failure(
          owner.modelConnections.replaceKey(org(), connection.id, { api_key: "sk-test-long-invalid-1111-xx" }),
        );
        expect([kept.status, kept.code]).toEqual([422, "invalid_api_key"]);
        expect(JSON.stringify({ ...kept, message: kept.message }).includes("sk-test-long-invalid-1111-xx")).toBe(false);
        expect(await owner.modelConnections.get(org(), connection.id)).toEqual(connection);
      });

      it("searches the provider's models on the server, a page at a time", async () => {
        const connection = await connect();
        const all = (await owner.modelConnections.providerModels(org(), connection.id, { limit: 100 })).items;
        expect(all.length).toBeGreaterThan(1);
        expect(all.every((m) => m.added_model_id === null)).toBe(true);
        const first = await owner.modelConnections.providerModels(org(), connection.id, { limit: 1 });
        expect(first.items).toEqual(all.slice(0, 1));
        const next = await owner.modelConnections.providerModels(org(), connection.id, {
          limit: 1,
          cursor: must(first.pagination.next_cursor, "next page"),
        });
        expect(next.items).toEqual(all.slice(1, 2));
        const name = must(all[1], "second model").name;
        const found = (await owner.modelConnections.providerModels(org(), connection.id, { q: name, limit: 100 }))
          .items;
        expect(found.map((m) => m.provider_model_id)).toContain(must(all[1], "second model").provider_model_id);
        expect(found.length).toBeLessThan(all.length);
        const offered = await toolModel(connection.id);
        const model = await owner.models.create(org(), {
          connection_id: connection.id,
          provider_model_id: offered.provider_model_id,
        });
        const listed = (await owner.modelConnections.providerModels(org(), connection.id, { limit: 100 })).items;
        expect(listed.find((m) => m.provider_model_id === offered.provider_model_id)?.added_model_id).toBe(model.id);
      });

      it("adds a model once per connection, from what the provider lists, and only one that calls tools", async () => {
        const connection = await connect();
        const offered = await toolModel(connection.id);
        const model = await owner.models.create(org(), {
          connection_id: connection.id,
          provider_model_id: offered.provider_model_id,
        });
        expect(model).toMatchObject({
          organization_id: org(),
          environment: "sandbox",
          route: { connection_id: connection.id, kind: connection.kind, label: connection.label },
          provider_model_id: offered.provider_model_id,
          name: offered.name,
          author: offered.author,
          supports_tools: true,
          status: "available",
          is_default: false,
          agent_count: 0,
        });
        expect((await owner.modelConnections.get(org(), connection.id)).model_count).toBe(1);
        const again = await failure(
          owner.models.create(org(), { connection_id: connection.id, provider_model_id: offered.provider_model_id }),
        );
        expect([again.status, again.code]).toEqual([409, "model_exists"]);
        const unknown = await failure(
          owner.models.create(org(), { connection_id: connection.id, provider_model_id: "no-such-model" }),
        );
        expect([unknown.status, unknown.code, Object.keys(unknown.fields ?? {})]).toEqual([
          422,
          "unknown_provider_model",
          ["provider_model_id"],
        ]);
        // The reference fixture's OpenRouter lists a model that can't call tools.
        const toolless = must(
          (await owner.modelConnections.providerModels(org(), connection.id, { limit: 100 })).items.find(
            (m) => !m.supports_tools,
          ),
          "a listed model that can't call tools",
        );
        const refused = await failure(
          owner.models.create(org(), { connection_id: connection.id, provider_model_id: toolless.provider_model_id }),
        );
        expect([refused.status, refused.code]).toEqual([422, "tools_unsupported"]);
        // The route is the model's identity: the same provider model through another connection is another model.
        const other = await connect();
        const twin = await owner.models.create(org(), {
          connection_id: other.id,
          provider_model_id: offered.provider_model_id,
        });
        expect(twin.id).not.toBe(model.id);
        expect(twin.route.connection_id).toBe(other.id);
        const renamed = await owner.models.update(org(), model.id, { name: "Suite model" });
        expect(renamed.name).toBe("Suite model");
        const mine = await owner.models.list(org(), { connection_id: connection.id });
        expect(mine.items.map((m) => m.id)).toEqual([model.id]);
        expect((await owner.models.list(org(), { q: "Suite model" })).items.map((m) => m.id)).toContain(model.id);
      });

      it("keeps each environment's default to its own models", async () => {
        const sandbox = await firstModel("sandbox");
        const production = await firstModel("production");
        const foreign = must(
          (await as("outsider").models.list(fx.otherOrganization, { limit: 1 })).items[0],
          "the other organization's model",
        );
        const productionDefault = await owner.models.getDefault(org(), "production");
        for (const model_id of [production.id, foreign.id]) {
          const refused = await failure(owner.models.setDefault(org(), "sandbox", { model_id }));
          expect([refused.status, refused.code, Object.keys(refused.fields ?? {})]).toEqual([
            422,
            "unknown_model",
            ["model_id"],
          ]);
        }
        const chosen = await owner.models.setDefault(org(), "sandbox", { model_id: sandbox.id });
        expect([chosen.environment, chosen.model?.id, chosen.model?.is_default]).toEqual(["sandbox", sandbox.id, true]);
        expect(chosen.updated_at).not.toBeNull();
        expect(await owner.models.getDefault(org(), "sandbox")).toEqual(chosen);
        expect(await owner.models.getDefault(org(), "production")).toEqual(productionDefault);
        const cleared = await owner.models.setDefault(org(), "sandbox", { model_id: null });
        expect(cleared.model).toBeNull();
      });

      it("refuses a run with neither the agent's model nor a default, and never picks one", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const agent = await owner.agents.create(qa, { name: "Unmodelled", description: "", instructions: "" });
        await owner.models.setDefault(org(), "sandbox", { model_id: null });
        const refused = await failure(owner.runs.create(qa, agent.id, { input: "hi" }));
        expect([refused.status, refused.code]).toEqual([422, "model_not_configured"]);
        expect((await owner.runs.list(qa, { agent_id: agent.id })).items).toEqual([]);
        // An override is enough, and so is a default.
        const model = await firstModel("sandbox");
        await owner.agents.update(qa, agent.id, { model_id: model.id });
        await owner.runs.cancel(qa, (await owner.runs.create(qa, agent.id, { input: "hi" })).id);
        await owner.agents.update(qa, agent.id, { model_id: null });
        await owner.models.setDefault(org(), "sandbox", { model_id: model.id });
        await owner.runs.cancel(qa, (await owner.runs.create(qa, agent.id, { input: "hi" })).id);
      });

      it("refuses removing a model or a connection in use, naming what holds it", async () => {
        const qa = fx.workspaces.qaPlayground.id;
        const connection = await connect();
        const offered = await toolModel(connection.id);
        const model = await owner.models.create(org(), {
          connection_id: connection.id,
          provider_model_id: offered.provider_model_id,
        });
        const agent = await owner.agents.create(qa, {
          name: "Holds the model",
          description: "",
          instructions: "",
          model_id: model.id,
        });
        expect((await owner.models.get(org(), model.id)).agent_count).toBe(1);
        for (const remove of [
          () => owner.models.delete(org(), model.id),
          () => owner.modelConnections.delete(org(), connection.id),
        ]) {
          const held = await failure(remove());
          expect([held.status, held.code]).toEqual([409, "model_in_use"]);
          expect(held.message).toContain("Holds the model");
        }
        await owner.agents.update(qa, agent.id, { model_id: null });
        const previous = await owner.models.getDefault(org(), "sandbox");
        await owner.models.setDefault(org(), "sandbox", { model_id: model.id });
        const asDefault = await failure(owner.models.delete(org(), model.id));
        expect([asDefault.status, asDefault.code]).toEqual([409, "model_in_use"]);
        expect(asDefault.message).toContain(model.name);
        await owner.models.setDefault(org(), "sandbox", { model_id: previous.model?.id ?? null });
        // Free again: deleting the connection takes its models with it.
        await owner.modelConnections.delete(org(), connection.id);
        const gone = await failure(owner.models.get(org(), model.id));
        expect(`${gone.status} ${gone.code}`).toBe("404 not_found");
        expect((await owner.models.list(org(), { connection_id: connection.id })).items).toEqual([]);
        const missing = await failure(owner.modelConnections.get(org(), connection.id));
        expect(`${missing.status} ${missing.code}`).toBe("404 not_found");
      });

      it("keeps one organization's models out of another's agents and defaults", async () => {
        const outsider = as("outsider");
        const model = await firstModel("sandbox");
        const connection = must(
          (await owner.modelConnections.list(org(), { environment: "sandbox" }))[0],
          "connection",
        );
        for (const attempt of [
          () => outsider.models.get(fx.otherOrganization, model.id),
          () => outsider.models.update(fx.otherOrganization, model.id, { name: "Owned" }),
          () => outsider.modelConnections.get(fx.otherOrganization, connection.id),
          () =>
            outsider.models.create(fx.otherOrganization, {
              connection_id: connection.id,
              provider_model_id: model.provider_model_id,
            }),
        ]) {
          const error = await failure(attempt());
          expect(`${error.status} ${error.code}`).toBe("404 not_found");
        }
        const harbor = must(
          (await outsider.workspaces.list(fx.otherOrganization, { environment: "sandbox", limit: 100 })).items[0],
          "the other organization's Sandbox workspace",
        );
        const agent = await failure(
          outsider.agents.create(harbor.id, { name: "x", description: "", instructions: "", model_id: model.id }),
        );
        expect([agent.status, agent.code]).toEqual([422, "unknown_model"]);
        const fallback = await failure(
          outsider.models.setDefault(fx.otherOrganization, "sandbox", { model_id: model.id }),
        );
        expect([fallback.status, fallback.code]).toEqual([422, "unknown_model"]);
        expect(JSON.stringify(await outsider.models.list(fx.otherOrganization, { limit: 100 }))).not.toContain(
          model.id,
        );
      });

      it("binds an API key to its own environment's connections, models and default", async () => {
        const { secret } = await owner.apiKeys.create(org(), { name: "Models", environment: "sandbox" });
        const key = withKey(secret);
        const production = await firstModel("production");
        const productionConnection = production.route.connection_id;
        expect((await key.modelConnections.list(org())).every((c) => c.environment === "sandbox")).toBe(true);
        expect((await key.models.list(org(), { limit: 100 })).items.every((m) => m.environment === "sandbox")).toBe(
          true,
        );
        for (const attempt of [
          () => key.modelConnections.get(org(), productionConnection),
          () => key.modelConnections.providerModels(org(), productionConnection),
          () => key.models.get(org(), production.id),
          () => key.models.delete(org(), production.id),
        ]) {
          const error = await failure(attempt());
          expect(`${error.status} ${error.code}`).toBe("404 not_found");
        }
        for (const attempt of [
          () => key.modelConnections.list(org(), { environment: "production" }),
          () => key.models.getDefault(org(), "production"),
          () =>
            key.modelConnections.create(org(), {
              environment: "production",
              kind: "openai",
              label: "x",
              api_key: "sk-test-long-suite-0000-xx",
            }),
        ]) {
          const error = await failure(attempt());
          expect(`${error.status} ${error.code}`).toBe("422 environment_mismatch");
        }
        const own = await key.modelConnections.create(org(), {
          environment: "sandbox",
          kind: "anthropic",
          label: "From a key",
          api_key: "sk-test-long-suite-key-0000-xx",
        });
        expect(own.environment).toBe("sandbox");
      });
    });

    describe("channels", () => {
      const sandbox = () => fx.workspaces.acmeSandbox.id;
      const link = (workspaceId: WorkspaceId, client: ZupflowClient = owner, tokens: typeof TOKENS = TOKENS) =>
        client.channelAppManagers.create(workspaceId, { provider: "slack", method: "configuration_token", ...tokens });
      /** A refusal as `status code`, or `succeeded`. */
      const answer = async (promise: Promise<unknown>) => {
        try {
          await promise;
          return "succeeded";
        } catch (error) {
          if (!(error instanceof ZupflowApiError)) throw error;
          return `${error.status} ${error.code}`;
        }
      };
      /** An answer from the provider's endpoint, as `status code` for a refusal. */
      const delivered = async (response: Response) => {
        const body = (await response.json()) as { error?: { code: string } };
        return response.ok ? body : `${response.status} ${body.error?.code}`;
      };

      it("links a team through hosted Connect, checking its tokens with the provider first", async () => {
        // Channel sessions need a server that serves channels; one that serves only app sessions skips this.
        const managers = (await owner.channelAppManagers.list(sandbox(), { limit: 100 })).items;
        const hook = (
          await owner.webhooks.create(fx.organization, {
            environment: "sandbox",
            url: "https://hooks.northwind.example/channels",
            description: "Channels",
            events: ["channel_app_manager.status_changed", "connect_session.completed"],
          })
        ).endpoint;
        const session = await owner.connectSessions.create(sandbox(), {
          target: { kind: "channel_app_manager", provider: "slack" },
          return_url: null,
        });
        expect([session.app, session.target]).toEqual([
          null,
          { kind: "channel_app_manager", provider: "slack", manager: null },
        ]);
        const reference = referenceOf(session);
        expect((await owner.connect.get(reference)).target).toEqual(session.target);
        // A team is linked by pasting tokens, never through the provider's consent page.
        expect(await answer(owner.connect.authorize(reference))).toBe("422 wrong_auth_type");
        const refused = await failure(
          owner.connect.submitCredentials(reference, {
            values: { configuration_token: "xoxe.xoxp-1-invalid-0000", refresh_token: TOKENS.refresh_token },
          }),
        );
        expect([refused.status, refused.code, Object.keys(refused.fields ?? {})]).toEqual([
          422,
          "invalid_configuration_token",
          ["configuration_token"],
        ]);
        expect((await owner.channelAppManagers.list(sandbox(), { limit: 100 })).items).toEqual(managers);

        const result = await owner.connect.submitCredentials<ChannelAppManagerConnectResult>(reference, {
          values: { ...TOKENS },
        });
        const manager = result.channel_app_manager;
        expect(result.session.status).toBe("completed");
        expect(result.session.target).toEqual({ kind: "channel_app_manager", provider: "slack", manager: manager.id });
        expect(manager).toMatchObject({
          workspace_id: sandbox(),
          provider: "slack",
          method: "configuration_token",
          status: "active",
          app_count: 0,
          configuration_token: { set: true, last4: TOKENS.configuration_token.slice(-4) },
        });
        expect(await owner.channelAppManagers.get(sandbox(), manager.id)).toEqual(manager);
        const shown = JSON.stringify([result, await owner.channelAppManagers.list(sandbox(), { limit: 100 })]);
        expect(shown).not.toContain(TOKENS.configuration_token);
        expect(shown).not.toContain(TOKENS.refresh_token);
        expect(await answer(owner.connect.submitCredentials(reference, { values: { ...TOKENS } }))).toBe(
          "409 session_not_pending",
        );
        // A backend learns what the session linked from its events, without polling.
        const deliveries = (await owner.webhooks.deliveries(fx.organization, hook.id, { limit: 100 })).items;
        const events = await Promise.all(
          deliveries.map(async (d) => (await owner.webhooks.delivery(fx.organization, hook.id, d.id)).event),
        );
        // Both events can share a millisecond, so their delivery order isn't fixed.
        const byType = events.map((e) => [e.type, e.data]).sort(([a], [b]) => String(a).localeCompare(String(b)));
        expect(byType).toEqual([
          ["channel_app_manager.status_changed", { object: "channel_app_manager", id: manager.id, status: "active" }],
          [
            "connect_session.completed",
            {
              object: "connect_session",
              id: session.id,
              status: "completed",
              installation_id: null,
              target: { kind: "channel_app_manager", provider: "slack", manager: manager.id },
              failure_code: null,
            },
          ],
        ]);
      });

      it("makes an agent's app, then offers its install on the provider's page", async () => {
        const ws = sandbox();
        const { manager } = await installedIn(ws);
        const agent = await applessAgent(ws);
        const accepted = await owner.channelApps.create(ws, { agent_id: agent.id, manager_id: manager.id });
        expect(accepted).toMatchObject({
          workspace_id: ws,
          agent_id: agent.id,
          manager_id: manager.id,
          provider: "slack",
          status: "creating",
          connection_id: null,
        });
        const app = await madeApp(ws, accepted.id);
        expect(app.status).toBe("awaiting_install");
        expect(app.external_app_id).not.toBeNull();
        expect((await owner.channelAppManagers.get(ws, manager.id)).app_count).toBe(2);
        // One live app per agent in a team.
        expect(await answer(owner.channelApps.create(ws, { agent_id: agent.id, manager_id: manager.id }))).toBe(
          "409 channel_app_exists",
        );
        expect((await owner.channelApps.list(ws, { agent_id: agent.id })).items.map((a) => a.id)).toEqual([app.id]);
        expect((await owner.channelApps.list(ws, { manager_id: manager.id })).items.map((a) => a.id)).toContain(app.id);

        const session = await owner.connectSessions.create(ws, {
          target: { kind: "channel_app", app: app.id },
          return_url: null,
        });
        expect([session.app, session.target, session.label]).toEqual([
          null,
          { kind: "channel_app", provider: "slack", app: app.id, connection: null },
          agent.name,
        ]);
        const reference = referenceOf(session);
        // An app is installed by allowing it on the provider's page, never with pasted tokens.
        expect(await answer(owner.connect.submitCredentials(reference, { values: { ...TOKENS } }))).toBe(
          "422 wrong_auth_type",
        );
        expect((await owner.connect.authorize(reference)).authorization_url).toMatch(/^https?:\/\//);
        expect((await owner.connect.get(reference)).status).toBe("pending");
      });

      it("never links teams or makes apps from the Console in Production; the backend's key does", async () => {
        const acme = fx.workspaces.acme.id;
        const { manager, app, connection, agentId } = await installedIn(acme);
        const agent = await applessAgent(acme);
        const before = JSON.stringify([
          await owner.channelAppManagers.list(acme, { limit: 100 }),
          await owner.channelApps.list(acme, { limit: 100 }),
        ]);
        for (const attempt of [
          () => link(acme),
          () => owner.channelAppManagers.update(acme, manager.id, TOKENS),
          () => owner.channelApps.create(acme, { agent_id: agent.id, manager_id: manager.id }),
          () =>
            owner.connectSessions.create(acme, {
              target: { kind: "channel_app_manager", provider: "slack" },
              return_url: null,
            }),
          () => owner.connectSessions.create(acme, { target: { kind: "channel_app", app: app.id }, return_url: null }),
        ]) {
          expect(await answer(attempt())).toBe("403 console_read_only_in_production");
        }
        expect(
          JSON.stringify([
            await owner.channelAppManagers.list(acme, { limit: 100 }),
            await owner.channelApps.list(acme, { limit: 100 }),
          ]),
        ).toBe(before);

        const production = withKey(
          (await owner.apiKeys.create(fx.organization, { name: "Channels backend", environment: "production" })).secret,
        );
        const linked = await link(acme, production);
        expect([linked.status, linked.created_by]).toEqual(["active", null]);
        expect((await production.channelAppManagers.update(acme, manager.id, TOKENS)).status).toBe("active");
        expect((await production.channelApps.create(acme, { agent_id: agent.id, manager_id: linked.id })).status).toBe(
          "creating",
        );
        const session = await production.connectSessions.create(acme, {
          target: { kind: "channel_app_manager", provider: "slack" },
          return_url: null,
        });
        expect(session.environment).toBe("production");

        // A key reaches its own environment's channels only, and never another organization's.
        const sandboxKey = withKey(
          (await owner.apiKeys.create(fx.organization, { name: "Channels sandbox", environment: "sandbox" })).secret,
        );
        const own = await installedIn(sandbox());
        const harbor = await larkspur();
        const foreign = must(harbor.channel, "harbor's channels");
        // Channels only: the keys' own use is recorded, rightly, on the organization's key list.
        const unchanged = async () => {
          const outsider = as("outsider");
          const hw = harbor.workspace.id;
          return JSON.stringify([
            (await owner.channelAppManagers.list(acme, { limit: 100 })).items,
            (await owner.channelApps.list(acme, { limit: 100 })).items,
            (await owner.channelConnections.list(acme, { limit: 100 })).items,
            await owner.agentChannels.list(acme, agentId),
            (await owner.channelAppManagers.list(sandbox(), { limit: 100 })).items,
            (await outsider.channelAppManagers.list(hw, { limit: 100 })).items,
            (await outsider.channelConnections.messages(hw, foreign.connection.id, foreign.conversation.id)).items,
          ]);
        };
        const beforeKeys = await unchanged();
        for (const [what, attempt] of [
          ["list managers", () => sandboxKey.channelAppManagers.list(acme)],
          ["link a team", () => link(acme, sandboxKey)],
          ["get manager", () => sandboxKey.channelAppManagers.get(acme, manager.id)],
          ["replace tokens", () => sandboxKey.channelAppManagers.update(acme, manager.id, TOKENS)],
          ["unlink team", () => sandboxKey.channelAppManagers.delete(acme, manager.id)],
          ["list apps", () => sandboxKey.channelApps.list(acme)],
          ["get app", () => sandboxKey.channelApps.get(acme, app.id)],
          ["delete app", () => sandboxKey.channelApps.delete(acme, app.id)],
          ["list connections", () => sandboxKey.channelConnections.list(acme)],
          ["get connection", () => sandboxKey.channelConnections.get(acme, connection.id)],
          ["rename connection", () => sandboxKey.channelConnections.update(acme, connection.id, { label: "Moved" })],
          ["conversations", () => sandboxKey.channelConnections.conversations(acme, connection.id)],
          ["agent channels", () => sandboxKey.agentChannels.list(acme, agentId)],
          ["remove agent channel", () => sandboxKey.agentChannels.delete(acme, agentId, connection.id)],
          ["Production key, Sandbox connection", () => production.channelConnections.get(sandbox(), own.connection.id)],
          ["Production key, Sandbox team", () => production.channelAppManagers.delete(sandbox(), own.manager.id)],
          [
            "Production key, Sandbox app install",
            () =>
              production.connectSessions.create(sandbox(), {
                target: { kind: "channel_app", app: own.app.id },
                return_url: null,
              }),
          ],
          [
            "other organization's team",
            () => production.channelAppManagers.get(harbor.workspace.id, foreign.manager.id),
          ],
          [
            "other organization's messages",
            () =>
              production.channelConnections.messages(
                harbor.workspace.id,
                foreign.connection.id,
                foreign.conversation.id,
              ),
          ],
          [
            "other organization's team through own workspace",
            () => production.channelAppManagers.update(acme, foreign.manager.id, TOKENS),
          ],
        ] as const) {
          expect(await answer(attempt()), what).toBe("404 not_found");
        }
        expect(await unchanged(), "channels after the other environment's and organization's attempts").toBe(
          beforeKeys,
        );

        // Organization settings: keys read them; only the Console's admins change them.
        expect(await production.channelSettings.get(fx.organization)).toEqual(
          await owner.channelSettings.get(fx.organization),
        );
        expect(await sandboxKey.channelSettings.toolLevels(fx.organization)).toEqual(
          await owner.channelSettings.toolLevels(fx.organization),
        );
        for (const attempt of [
          () => production.channelSettings.update(fx.organization, CHANNEL_SETTINGS_DEFAULTS),
          () =>
            production.channelSettings.setToolLevels(fx.organization, {
              provider: "slack",
              levels: [{ tool: "react", level: "deny" }],
            }),
        ]) {
          expect(await answer(attempt())).toBe("403 console_only");
        }
      });

      it("keeps tokens write-only and refuses ones the provider refuses", async () => {
        const ws = sandbox();
        const first = { configuration_token: "xoxe.xoxp-1-write-only-9f3k", refresh_token: "xoxe-1-refresh-a8d2" };
        const second = { configuration_token: "xoxe.xoxp-1-replaced-q7w2", refresh_token: "xoxe-1-refresh-c4v6" };
        const manager = await link(ws, owner, first as typeof TOKENS);
        expect(manager.configuration_token).toMatchObject({ set: true, last4: "9f3k" });
        const replaced = await owner.channelAppManagers.update(ws, manager.id, second);
        expect(replaced.configuration_token).toMatchObject({ set: true, last4: "q7w2" });
        const refusedCreate = await failure(
          link(ws, owner, { ...TOKENS, configuration_token: "xoxe.xoxp-1-invalid-1111" } as unknown as typeof TOKENS),
        );
        expect([refusedCreate.status, refusedCreate.code, Object.keys(refusedCreate.fields ?? {})]).toEqual([
          422,
          "invalid_configuration_token",
          ["configuration_token"],
        ]);
        expect(
          await answer(
            owner.channelAppManagers.update(ws, manager.id, { ...second, configuration_token: "xoxe.invalid-2222" }),
          ),
        ).toBe("422 invalid_configuration_token");
        expect(await answer(owner.channelAppManagers.update(ws, manager.id, { ...second, refresh_token: "" }))).toBe(
          "400 validation_failed",
        );
        expect((await owner.channelAppManagers.get(ws, manager.id)).configuration_token.last4).toBe("q7w2");
        expect((await owner.channelAppManagers.list(ws, { limit: 100 })).items).toHaveLength(2);
        const shown = JSON.stringify([
          await owner.channelAppManagers.list(ws, { limit: 100 }),
          await owner.channelApps.list(ws, { limit: 100 }),
          await owner.channelConnections.list(ws, { limit: 100 }),
          await owner.audit.list(fx.organization, { limit: 100 }),
        ]);
        for (const value of [...Object.values(first), ...Object.values(second)]) expect(shown).not.toContain(value);
      });

      it("unlinks a team only once its apps are gone, and a removed app reads as 404", async () => {
        const ws = sandbox();
        const { manager, app, connection, agentId, conversations } = await installedIn(ws);
        expect(await answer(owner.channelAppManagers.delete(ws, manager.id))).toBe("409 in_use");
        await owner.channelApps.delete(ws, app.id);
        expect(await answer(owner.channelApps.get(ws, app.id))).toBe("404 not_found");
        expect(await answer(owner.channelApps.delete(ws, app.id))).toBe("404 not_found");
        expect((await owner.channelApps.list(ws, { limit: 100 })).items.map((a) => a.id)).not.toContain(app.id);
        // Its connection is disconnected, its history kept, and the agent's channel on it removed.
        expect((await owner.channelConnections.get(ws, connection.id)).status).toBe("disconnected");
        const conversation = must(conversations[0], "conversation");
        expect((await owner.channelConnections.messages(ws, connection.id, conversation.id)).items).not.toEqual([]);
        expect((await owner.agentChannels.list(ws, agentId)).map((c) => c.connection_id)).not.toContain(connection.id);
        expect(await answer(owner.agentChannels.put(ws, agentId, connection.id, agentChannelDefaults()))).toBe(
          "409 channel_connection_disconnected",
        );
        expect((await owner.channelAppManagers.get(ws, manager.id)).app_count).toBe(0);
        await owner.channelAppManagers.delete(ws, manager.id);
        expect(await answer(owner.channelAppManagers.get(ws, manager.id))).toBe("404 not_found");
      });

      it("removes a connection with its history, and its app waits to be installed again", async () => {
        const ws = sandbox();
        const { app, connection, agentId, conversations } = await installedIn(ws);
        await owner.channelConnections.delete(ws, connection.id);
        expect(await answer(owner.channelConnections.get(ws, connection.id))).toBe("404 not_found");
        expect(await answer(owner.channelConnections.conversations(ws, connection.id))).toBe("404 not_found");
        const conversation = must(conversations[0], "conversation");
        expect(await answer(owner.channelConnections.messages(ws, connection.id, conversation.id))).toBe(
          "404 not_found",
        );
        expect(await owner.channelApps.get(ws, app.id)).toMatchObject({
          status: "awaiting_install",
          connection_id: null,
        });
        expect((await owner.agentChannels.list(ws, agentId)).map((c) => c.connection_id)).not.toContain(connection.id);
      });

      it("deletes an agent's apps with the agent", async () => {
        const ws = sandbox();
        const { manager, app, connection, agentId } = await installedIn(ws);
        await owner.agents.delete(ws, agentId);
        expect(await answer(owner.channelApps.get(ws, app.id))).toBe("404 not_found");
        expect((await owner.channelConnections.get(ws, connection.id)).status).toBe("disconnected");
        expect((await owner.channelAppManagers.get(ws, manager.id)).app_count).toBe(0);
        expect(await answer(owner.agentChannels.list(ws, agentId))).toBe("404 not_found");
      });

      it("holds an agent's channel to the organization's tool levels and its own rules", async () => {
        const ws = sandbox();
        const { connection, agentId, agentChannel, conversations } = await installedIn(ws);
        const beta = must((await betaLabs()).channel, "beta-labs channels");
        const harbor = must((await larkspur()).channel, "harbor channels");
        const base = agentChannelDefaults();
        const put = (change: Partial<PutAgentChannelInput>, agent: AgentId = agentId) =>
          owner.agentChannels.put(ws, agent, connection.id, { ...base, ...change });
        const before = await owner.agentChannels.list(ws, agentId);
        for (const [change, refused] of [
          [{ tools: ["post_anywhere"] }, "422 unknown_tool"],
          [{ tools: ["reply"], approval_required: ["react"] }, "422 approval_outside_grant"],
          [{ tools: ["delete_message"] }, "422 tool_denied"],
          [{ tools: ["send_message"] }, "422 approval_required_by_policy"],
          [{ senders: "listed", sender_list: [] }, "422 sender_list_required"],
          [{ senders: "listed", sender_list: ["dana@acme.example", "DANA@acme.example"] }, "422 duplicate_sender"],
          // Another workspace's conversation, or another organization's, is one this connection hasn't seen.
          [{ audience: "listed", audience_conversations: [beta.conversation.id] }, "422 unknown_conversation"],
          [{ audience: "listed", audience_conversations: [harbor.conversation.id] }, "422 unknown_conversation"],
          [{ max_agent_hops: 6 }, "400 validation_failed"],
          [{ max_agent_hops: 0 }, "400 validation_failed"],
          [{ senders: "listed", sender_list: ["not an address"] }, "400 validation_failed"],
          [{ persona: { name: "Scribe", avatar_url: "http://cdn.acme.example/scribe.png" } }, "400 validation_failed"],
          [{ persona: { name: " ", avatar_url: null } }, "400 validation_failed"],
          [
            {
              senders: "listed",
              sender_list: Array.from({ length: 501 }, (_, i) => `person${i}@acme.example`),
            },
            "400 validation_failed",
          ],
        ] as [Partial<PutAgentChannelInput>, string][]) {
          expect(await answer(put(change)), JSON.stringify(change)).toBe(refused);
        }
        expect(await owner.agentChannels.list(ws, agentId)).toEqual(before);

        const support = must(
          conversations.find((c) => c.kind === "channel"),
          "channel conversation",
        );
        const input: PutAgentChannelInput = {
          ...base,
          tools: ["read_thread", "reply", "send_message"],
          approval_required: ["send_message"],
          audience: "listed",
          audience_conversations: [support.id],
          persona: { name: "Scribe", avatar_url: null },
          thread_follow_up: "always",
          agent_messages: "mentions",
          max_agent_hops: 5,
          mid_run: "collect",
          senders: "listed",
          sender_list: ["dana@acme.example"],
          reply_with_output: false,
        };
        const saved = await owner.agentChannels.put(ws, agentId, connection.id, input);
        expect(saved).toMatchObject({ id: agentChannel.id, agent_id: agentId, connection_id: connection.id, ...input });
        expect(await owner.agentChannels.list(ws, agentId)).toContainEqual(saved);

        // The organization's levels decide: allowed, a tool needs no approval; denied, it can't be given.
        await owner.channelSettings.setToolLevels(fx.organization, {
          provider: "slack",
          levels: [
            { tool: "send_message", level: "allow" },
            { tool: "reply", level: "deny" },
          ],
        });
        expect(await answer(put({ tools: ["send_message"] }))).toBe("succeeded");
        expect(await answer(put({ tools: ["reply"] }))).toBe("422 tool_denied");

        // Only the agent whose app it is has a channel on a connection.
        const other = await applessAgent(ws);
        expect(await answer(put({}, other.id))).toBe("422 not_agents_app");
        await owner.agentChannels.delete(ws, agentId, connection.id);
        expect(await answer(owner.agentChannels.delete(ws, agentId, connection.id))).toBe("404 not_found");
        expect((await owner.agentChannels.list(ws, agentId)).map((c) => c.connection_id)).not.toContain(connection.id);
        expect(await answer(put({}))).toBe("succeeded");
      });

      it("keeps channel tool levels and settings at Zupflow's defaults until an admin saves them", async () => {
        const org = fx.organization;
        const levels = await owner.channelSettings.toolLevels(org);
        expect(levels.map((l) => [l.provider, l.tool, l.level, l.effect, l.confirmed, l.updated_at])).toEqual(
          CHANNEL_TOOL_NAMES.map((tool) => [
            "slack",
            tool,
            CHANNEL_TOOLS[tool].level,
            CHANNEL_TOOLS[tool].effect,
            false,
            null,
          ]),
        );
        const settings = await owner.channelSettings.get(org);
        expect(settings).toEqual({
          organization_id: org,
          ...CHANNEL_SETTINGS_DEFAULTS,
          updated_at: null,
          updated_by: null,
        });

        // Developers read them; only admins change them.
        const developer = as("developer");
        expect(await developer.channelSettings.get(org)).toEqual(settings);
        expect(await developer.channelSettings.toolLevels(org)).toEqual(levels);
        expect(
          await answer(developer.channelSettings.update(org, { ...CHANNEL_SETTINGS_DEFAULTS, breaker_runs: 5 })),
        ).toBe("403 forbidden");
        expect(
          await answer(
            developer.channelSettings.setToolLevels(org, {
              provider: "slack",
              levels: [{ tool: "react", level: "deny" }],
            }),
          ),
        ).toBe("403 forbidden");

        for (const [input, refused] of [
          [{ provider: "slack", levels: [{ tool: "post_anywhere", level: "allow" }] }, "422 unknown_tool"],
          [{ provider: "slack", levels: [] }, "400 validation_failed"],
          [
            {
              provider: "slack",
              levels: [
                { tool: "react", level: "allow" },
                { tool: "react", level: "deny" },
              ],
            },
            "400 validation_failed",
          ],
        ] as [UpdateChannelToolLevelsInput, string][]) {
          expect(await answer(owner.channelSettings.setToolLevels(org, input)), JSON.stringify(input)).toBe(refused);
        }
        for (const change of [
          { breaker_runs: 0 },
          { breaker_runs: 201 },
          { breaker_window_minutes: 1441 },
          { breaker_pause_minutes: 0 },
          { message_retention_days: 366 },
        ]) {
          expect(
            await answer(owner.channelSettings.update(org, { ...CHANNEL_SETTINGS_DEFAULTS, ...change })),
            JSON.stringify(change),
          ).toBe("400 validation_failed");
        }
        expect(await owner.channelSettings.toolLevels(org)).toEqual(levels);
        expect(await owner.channelSettings.get(org)).toEqual(settings);

        const me = (await owner.me.get()).user;
        const saved = await owner.channelSettings.update(org, {
          ...CHANNEL_SETTINGS_DEFAULTS,
          breaker_runs: 5,
          message_retention_days: 90,
        });
        expect(saved).toMatchObject({ breaker_runs: 5, message_retention_days: 90, updated_by: { id: me.id } });
        expect(saved.updated_at).not.toBeNull();
        const set = await owner.channelSettings.setToolLevels(org, {
          provider: "slack",
          levels: [{ tool: "delete_message", level: "approve" }],
        });
        expect(set.filter((l) => l.confirmed).map((l) => [l.tool, l.level, l.updated_by?.id])).toEqual([
          ["delete_message", "approve", me.id],
        ]);
        // Another organization's stay its own.
        const outsider = as("outsider");
        expect(await outsider.channelSettings.get(fx.otherOrganization)).toMatchObject({
          ...CHANNEL_SETTINGS_DEFAULTS,
          updated_at: null,
        });
        expect((await outsider.channelSettings.toolLevels(fx.otherOrganization)).some((l) => l.confirmed)).toBe(false);
      });

      it("lists a connection's conversations and messages, searched and paged on the server", async () => {
        const ws = sandbox();
        const { connection } = await installedIn(ws);
        const id = connection.id;
        const all = (await owner.channelConnections.conversations(ws, id, { limit: 100 })).items;
        expect(all.map((c) => c.kind).sort()).toEqual(["channel", "dm"]);
        expect(all.every((c) => c.workspace_id === ws && c.connection_id === id)).toBe(true);
        const support = (await owner.channelConnections.conversations(ws, id, { q: "SUPP" })).items;
        expect(support.map((c) => c.name)).toEqual(["support"]);
        expect((await owner.channelConnections.conversations(ws, id, { kind: "dm" })).items.map((c) => c.kind)).toEqual(
          ["dm"],
        );
        expect((await owner.channelConnections.conversations(ws, id, { q: "no such conversation" })).items).toEqual([]);
        const first = await owner.channelConnections.conversations(ws, id, { limit: 1 });
        const next = must(first.pagination.next_cursor, "next page");
        const second = await owner.channelConnections.conversations(ws, id, { limit: 1, cursor: next });
        expect([...first.items, ...second.items]).toEqual(all);
        expect(second.pagination.next_cursor).toBeNull();

        const thread = must(support[0], "support");
        const messages = (await owner.channelConnections.messages(ws, id, thread.id, { limit: 100 })).items;
        expect(messages.map((m) => m.direction).sort()).toEqual(["inbound", "inbound", "outbound"]);
        expect(messages.find((m) => m.direction === "outbound")?.author).toBeNull();
        expect(messages.filter((m) => m.direction === "inbound").every((m) => m.author !== null)).toBe(true);
        const sent = messages.map((m) => m.sent_at);
        expect(sent).toEqual([...sent].sort().reverse());
        const key = must(messages[0], "message").thread_key;
        expect((await owner.channelConnections.messages(ws, id, thread.id, { thread: key })).items).toEqual(messages);
        expect(
          (await owner.channelConnections.messages(ws, id, thread.id, { thread: "1000000000.000001" })).items,
        ).toEqual([]);
        const paged = await owner.channelConnections.messages(ws, id, thread.id, { limit: 2 });
        expect(paged.items).toEqual(messages.slice(0, 2));
        // Another workspace's conversation, even of the same organization, isn't found through this connection.
        const beta = must((await betaLabs()).channel, "beta-labs channels");
        expect(await answer(owner.channelConnections.messages(ws, id, beta.conversation.id))).toBe("404 not_found");
      });

      it("reads only events signed by the app they're delivered to", async () => {
        const ws = sandbox();
        const { app, connection, conversations } = await installedIn(ws);
        const secrets = await secretsOf(app.id);
        const other = await installedIn(fx.workspaces.acme.id);
        const otherSecrets = await secretsOf(other.app.id);
        // The API never shows what the provider holds.
        const shown = JSON.stringify([
          await owner.channelApps.get(ws, app.id),
          await owner.channelApps.list(ws, { limit: 100 }),
          await owner.channelConnections.get(ws, connection.id),
          await owner.channelAppManagers.list(ws, { limit: 100 }),
          await owner.audit.list(fx.organization, { limit: 100 }),
        ]);
        expect(shown).not.toContain(secrets.signing_secret);
        expect(shown).not.toContain(secrets.inbound_ref);

        expect(await delivered(await deliver(app.id, { type: "url_verification", challenge: "c-0001" }))).toEqual({
          challenge: "c-0001",
        });
        const ts = (Date.now() / 1000).toFixed(6);
        const message = (eventId: string, extra: Record<string, unknown> = {}) => ({
          type: "event_callback",
          event_id: eventId,
          team_id: connection.team.id,
          event: {
            type: "message",
            channel: "C0CONTRACT1",
            channel_type: "channel",
            user: "U0CONTRACT1",
            text: `<@${connection.identity.external_id}> is the build green?`,
            ts,
            ...extra,
          },
        });
        const now = Math.floor(Date.now() / 1000);
        // Every refusal answers alike whether or not the reference names an app: nothing tells a prober it exists.
        const whole = async (response: Response) => `${response.status} ${JSON.stringify(await response.json())}`;
        const unknown = { ref: "a".repeat(32) };
        const stale = now - 6 * 60;
        const badSignature = await whole(
          await deliver(app.id, message("Ev0001"), { secret: "not-the-signing-secret" }),
        );
        expect(badSignature).toMatch(/^401 .*"invalid_signature"/);
        for (const [what, sent] of [
          ["another app's secret", deliver(app.id, message("Ev0001"), { secret: otherSecrets.signing_secret })],
          ["to an app that doesn't exist", deliver(app.id, message("Ev0001"), unknown)],
          [
            "to an app that doesn't exist, with a bad signature",
            deliver(app.id, message("Ev0001"), { ...unknown, secret: "not-the-signing-secret" }),
          ],
        ] as const) {
          expect(await whole(await sent), what).toBe(badSignature);
        }
        const tooOld = await whole(await deliver(app.id, message("Ev0001"), { timestamp: stale }));
        expect(tooOld).toMatch(/^401 .*"stale_request"/);
        for (const [what, sent] of [
          ["from the future", deliver(app.id, message("Ev0001"), { timestamp: now + 6 * 60 })],
          [
            "too old, to an app that doesn't exist",
            deliver(app.id, message("Ev0001"), { ...unknown, timestamp: stale }),
          ],
          [
            "too old, with a bad signature",
            deliver(app.id, message("Ev0001"), { timestamp: stale, secret: "not-the-signing-secret" }),
          ],
        ] as const) {
          expect(await whole(await sent), what).toBe(tooOld);
        }
        const found = async () =>
          (await owner.channelConnections.conversations(ws, connection.id, { limit: 100 })).items.find(
            (c) => c.external_id === "C0CONTRACT1",
          );
        expect(await found()).toBeUndefined();

        // Delivered twice, and again as a mention: one message.
        expect(await delivered(await deliver(app.id, message("Ev0001")))).toEqual({ challenge: null });
        await deliver(app.id, message("Ev0001"));
        await deliver(app.id, message("Ev0002", { type: "app_mention" }));
        // Another team's event, delivered to this app, is ignored.
        await deliver(app.id, { ...message("Ev0003", { ts: "1000000000.000003" }), team_id: "T0ELSEWHERE" });
        // So is one that names no team: the check can't be skipped by leaving it out.
        const { team_id: _team, ...teamless } = message("Ev0006", { ts: "1000000000.000006" });
        expect(await delivered(await deliver(app.id, teamless))).toEqual({ challenge: null });
        const conversation = must(await found(), "the delivered conversation");
        expect(conversation).toMatchObject({ workspace_id: ws, connection_id: connection.id, kind: "channel" });
        const received = (await owner.channelConnections.messages(ws, connection.id, conversation.id)).items;
        expect(received).toHaveLength(1);
        expect(received[0]).toMatchObject({
          direction: "inbound",
          external_id: ts,
          thread_key: ts,
          mentions: [connection.identity.external_id],
          agent_hop: 0,
          run_id: null,
        });
        // Nothing reached another workspace's connection.
        const elsewhere = (await owner.channelConnections.conversations(fx.workspaces.acme.id, other.connection.id))
          .items;
        expect(elsewhere.some((c) => c.external_id === "C0CONTRACT1")).toBe(false);
        expect(conversations.length + 1).toBe(
          (await owner.channelConnections.conversations(ws, connection.id, { limit: 100 })).items.length,
        );

        // Uninstalled in the team: the connection needs reconnecting, and reads no more messages.
        await deliver(app.id, {
          type: "event_callback",
          event_id: "Ev0004",
          team_id: connection.team.id,
          event: { type: "app_uninstalled" },
        });
        expect((await owner.channelConnections.get(ws, connection.id)).status).toBe("reconnect_required");
        await deliver(app.id, message("Ev0005", { ts: "1000000000.000005" }));
        expect((await owner.channelConnections.messages(ws, connection.id, conversation.id)).items).toHaveLength(1);
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
              // Model providers where the target serves them; left out, not the whole case, otherwise.
              ...(serves("listModelConnections") ? [settled(c.modelConnections.list(fx.organization))] : []),
              ...(serves("listModels") ? [settled(c.models.list(fx.organization, { limit: 100 }))] : []),
              ...(serves("getDefaultModel") ? [settled(c.models.getDefault(fx.organization, "sandbox"))] : []),
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
        const agentInput = { name: "x", description: "", instructions: "", model_id: t.agent.model_id };
        const app = { kind: "mcp", name: "x", key: "viewer_x", description: "", auth_type: "token" } as const;
        // Read only by the model provider changes, which are dropped when the target doesn't serve them.
        const connection = () => must(t.connection, "model connection");
        const model = () => must(t.model, "model");
        // Read only by the channel changes, which are dropped when the target doesn't serve them.
        const channel = () => must(t.channel, "channels");
        const changes: [string, () => Promise<unknown>][] = [
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
          // Channels: Sandbox for what the Console may only do there, and Production's own objects otherwise.
          [
            "createChannelAppManager",
            () => c.channelAppManagers.create(sandbox, { provider: "slack", method: "configuration_token", ...TOKENS }),
          ],
          ["updateChannelAppManager", () => c.channelAppManagers.update(w, channel().manager.id, TOKENS)],
          [
            "createChannelApp",
            () => c.channelApps.create(w, { agent_id: t.agent.id, manager_id: channel().manager.id }),
          ],
          [
            "updateChannelConnection",
            () => c.channelConnections.update(w, channel().connection.id, { label: "Viewed" }),
          ],
          [
            "putAgentChannel",
            () =>
              c.agentChannels.put(
                w,
                channel().agentChannel.agent_id,
                channel().connection.id,
                settingsOf(channel().agentChannel),
              ),
          ],
          [
            "deleteAgentChannel",
            () => c.agentChannels.delete(w, channel().agentChannel.agent_id, channel().connection.id),
          ],
          ["deleteChannelConnection", () => c.channelConnections.delete(w, channel().connection.id)],
          ["deleteChannelApp", () => c.channelApps.delete(w, channel().app.id)],
          ["deleteChannelAppManager", () => c.channelAppManagers.delete(w, channel().manager.id)],
          [
            "updateChannelToolLevels",
            () =>
              c.channelSettings.setToolLevels(t.org, { provider: "slack", levels: [{ tool: "react", level: "deny" }] }),
          ],
          [
            "updateChannelSettings",
            () => c.channelSettings.update(t.org, { ...CHANNEL_SETTINGS_DEFAULTS, breaker_runs: 5 }),
          ],
          ["deleteGrant", () => c.grants.delete(w, t.agent.id, t.grant.id)],
          ["deleteAgent", () => c.agents.delete(w, t.agent.id)],
          [
            "updateConnectSettings",
            () => c.connectSettings.update(t.org, "sandbox", { return_urls: [], allowed_origins: [] }),
          ],
          [
            "createModelConnection",
            () =>
              c.modelConnections.create(t.org, {
                environment: "sandbox",
                kind: "anthropic",
                label: "x",
                api_key: "sk-test-long-viewer-0000-xx",
              }),
          ],
          ["updateModelConnection", () => c.modelConnections.update(t.org, connection().id, { label: "Viewed" })],
          [
            "replaceModelConnectionKey",
            () => c.modelConnections.replaceKey(t.org, connection().id, { api_key: "sk-test-long-viewer-0000-xx" }),
          ],
          [
            "createModel",
            () =>
              c.models.create(t.org, { connection_id: connection().id, provider_model_id: model().provider_model_id }),
          ],
          ["updateModel", () => c.models.update(t.org, model().id, { name: "Viewed" })],
          ["updateDefaultModel", () => c.models.setDefault(t.org, model().environment, { model_id: null })],
          ["deleteModel", () => c.models.delete(t.org, model().id)],
          ["deleteModelConnection", () => c.modelConnections.delete(t.org, connection().id)],
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
          ["rollWebhookSecret", () => c.webhooks.rollSecret(t.org, t.hook.id)],
          ["deleteWebhook", () => c.webhooks.delete(t.org, t.hook.id)],
          ["testWebhook", () => c.webhooks.sendTest(t.org, t.hook.id)],
          [
            "resendWebhookDelivery",
            async () => {
              const source = must(
                (await owner.webhooks.deliveries(t.org, t.hook.id)).items.find((row) => row.state === "failed"),
                "failed delivery",
              );
              return c.webhooks.resendDelivery(t.org, t.hook.id, source.id);
            },
          ],
        ];
        // What the target doesn't serve yet is left out, not the whole sweep.
        return changes.filter(([operationId]) => serves(operationId));
      };

      /**
       * Changes that are the person's own, with no member role to check: creating their own organization, and
       * answering an invitation to them (they aren't a member until they accept; anyone else gets 404).
       */
      const OWN_CHANGES = new Set(["createOrganization", "acceptInvitation", "declineInvitation"]);
      /** Every change a Console member could make: the route table's, minus the person's own. */
      const MUTATING = routes
        .filter((r) => r.method !== "get" && r.callers.includes("console") && !OWN_CHANGES.has(r.operationId))
        .map((r) => r.operationId)
        .filter(serves);
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
          "createModelConnection",
          "updateModelConnection",
          "replaceModelConnectionKey",
          "deleteModelConnection",
          "createModel",
          "updateModel",
          "deleteModel",
          "updateDefaultModel",
          "createWebhook",
          "updateWebhook",
          "rollWebhookSecret",
          "deleteWebhook",
          "testWebhook",
          "resendWebhookDelivery",
          "updateChannelToolLevels",
          "updateChannelSettings",
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
          as("developer").agents.create(qa, { name: "x", description: "", instructions: "" }),
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

      it("rolls each environment's secret without changing endpoint identity or delivery history", async () => {
        for (const environment of ["sandbox", "production"] as const) {
          const endpoint = await endpointIn(environment);
          expect(endpoint.previous_signing_secret_expires_at).toBeNull();
          const endpoints = await owner.webhooks.list(fx.organization);
          const history = await owner.webhooks.deliveries(fx.organization, endpoint.id);
          const details = await Promise.all(
            history.items.map((row) => owner.webhooks.delivery(fx.organization, endpoint.id, row.id)),
          );
          const attempts = await Promise.all(
            history.items.map((row) => owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, row.id)),
          );
          const workspaces = [...new Set(history.items.flatMap((row) => (row.workspace ? [row.workspace.id] : [])))];
          expect(workspaces).toHaveLength(2);
          const scoped = await Promise.all(
            workspaces.map((workspace_id) => owner.webhooks.deliveries(fx.organization, endpoint.id, { workspace_id })),
          );
          const before = Date.now();
          const rolled = await owner.webhooks.rollSecret(fx.organization, endpoint.id);
          const after = Date.now();
          const expires = Date.parse(must(rolled.endpoint.previous_signing_secret_expires_at, "overlap deadline"));
          expect(expires).toBeGreaterThanOrEqual(before + 24 * 60 * 60_000);
          expect(expires).toBeLessThanOrEqual(after + 24 * 60 * 60_000);
          expect(rolled.signing_secret).toMatch(new RegExp(`^whsec_${environment === "sandbox" ? "test" : "live"}_`));
          expect(rolled.endpoint).toEqual({
            ...endpoint,
            signing_secret_last4: rolled.signing_secret.slice(-4),
            previous_signing_secret_expires_at: rolled.endpoint.previous_signing_secret_expires_at,
          });
          expect(await owner.webhooks.list(fx.organization)).toEqual(
            endpoints.map((row) => (row.id === endpoint.id ? rolled.endpoint : row)),
          );
          expect(await owner.webhooks.deliveries(fx.organization, endpoint.id)).toEqual(history);
          expect(
            await Promise.all(
              history.items.map((row) => owner.webhooks.delivery(fx.organization, endpoint.id, row.id)),
            ),
          ).toEqual(details);
          expect(
            await Promise.all(
              history.items.map((row) => owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, row.id)),
            ),
          ).toEqual(attempts);
          expect(
            await Promise.all(
              workspaces.map((workspace_id) =>
                owner.webhooks.deliveries(fx.organization, endpoint.id, { workspace_id }),
              ),
            ),
          ).toEqual(scoped);
          const events = (await owner.audit.list(fx.organization, { limit: 100 })).items.filter(
            (row) => row.action === "webhook.secret_rolled" && row.target.id === endpoint.id,
          );
          expect(events).toHaveLength(1);
          expect(events[0]).toMatchObject({
            organization_id: fx.organization,
            workspace_id: null,
            workspace: null,
            environment,
            actor: { type: "user", id: (await owner.me.get()).user.id },
            target: { type: "webhook", id: endpoint.id, name: endpoint.url },
          });
        }
      });

      it("shows a rolled secret once and never echoes either secret through reads or updates", async () => {
        const created = await owner.webhooks.create(fx.organization, {
          environment: "sandbox",
          url: "https://hooks.northwind.example/rotation?token=private-query#private-fragment",
          description: "Rotate while disabled",
          events: ["run.completed"],
        });
        expect(created.endpoint.previous_signing_secret_expires_at).toBeNull();
        const endpoint = await owner.webhooks.update(fx.organization, created.endpoint.id, { enabled: false });
        const options = { idempotencyKey: "roll-secret-once" };
        const rolled = await owner.webhooks.rollSecret(fx.organization, endpoint.id, options);
        expect(rolled.signing_secret).not.toBe(created.signing_secret);
        expect(rolled.endpoint).toEqual({
          ...endpoint,
          signing_secret_last4: rolled.signing_secret.slice(-4),
          previous_signing_secret_expires_at: expect.any(String),
        });
        const audit = await owner.audit.list(fx.organization, { limit: 100 });
        expect(await failure(owner.webhooks.rollSecret(fx.organization, endpoint.id, options))).toMatchObject({
          status: 409,
          code: "idempotency_key_completed",
        });
        for (const idempotencyKey of ["roll-secret-again", "roll-secret-yet-again"]) {
          expect(
            await failure(owner.webhooks.rollSecret(fx.organization, endpoint.id, { idempotencyKey })),
          ).toMatchObject({
            status: 409,
            code: "rotation_in_progress",
          });
        }
        const other = await endpointIn("production");
        expect(await failure(owner.webhooks.rollSecret(fx.organization, other.id, options))).toMatchObject({
          status: 422,
          code: "idempotency_key_reused",
        });
        expect(await owner.audit.list(fx.organization, { limit: 100 })).toEqual(audit);
        const event = must(
          audit.items.find((row) => row.action === "webhook.secret_rolled"),
          "rotation audit",
        );
        expect(event).toMatchObject({
          target: { type: "webhook", id: endpoint.id, name: "https://hooks.northwind.example/rotation" },
          workspace_id: null,
          environment: "sandbox",
        });
        expect(JSON.stringify(event)).not.toContain("private-query");
        expect(JSON.stringify(event)).not.toContain("private-fragment");
        const updated = await owner.webhooks.update(fx.organization, endpoint.id, { description: "Still disabled" });
        expect(updated).toEqual({ ...rolled.endpoint, description: "Still disabled" });
        const endpoints = await owner.webhooks.list(fx.organization);
        expect(endpoints.find((row) => row.id === endpoint.id)).toEqual(updated);
        expect(endpoints.find((row) => row.id === other.id)).toEqual(other);
        const exposed = JSON.stringify({
          endpoints,
          updated,
          audit,
          history: await owner.webhooks.deliveries(fx.organization, endpoint.id),
        });
        for (const secret of [created.signing_secret, rolled.signing_secret]) expect(exposed).not.toContain(secret);
      });

      it("serializes concurrent secret rolls per endpoint with one audit event", async () => {
        for (const environment of ["sandbox", "production"] as const) {
          const endpoint = await endpointIn(environment);
          const results = await Promise.allSettled([
            owner.webhooks.rollSecret(fx.organization, endpoint.id, { idempotencyKey: `rotate-${environment}-a` }),
            owner.webhooks.rollSecret(fx.organization, endpoint.id, { idempotencyKey: `rotate-${environment}-b` }),
          ]);
          expect(results.filter((result) => result.status === "fulfilled")).toHaveLength(1);
          const winner = must(
            results.find((result) => result.status === "fulfilled"),
            "winning rotation",
          ).value;
          expect(
            must(
              results.find((result) => result.status === "rejected"),
              "losing rotation",
            ).reason,
          ).toMatchObject({
            status: 409,
            code: "rotation_in_progress",
          });
          expect(await endpointIn(environment)).toEqual(winner.endpoint);
          expect(
            (await owner.audit.list(fx.organization, { limit: 100 })).items.filter(
              (row) => row.action === "webhook.secret_rolled" && row.target.id === endpoint.id,
            ),
          ).toHaveLength(1);
        }
      });

      it("keeps secret rotation inside the owning organization and permits admins in both environments", async () => {
        const mine = await endpointIn("production");
        const outsider = as("outsider");
        const other = must((await outsider.webhooks.list(fx.otherOrganization))[0], "foreign endpoint");
        const before = await owner.webhooks.list(fx.organization);
        const otherBefore = await outsider.webhooks.list(fx.otherOrganization);
        const audit = await owner.audit.list(fx.organization, { limit: 100 });
        const otherAudit = await outsider.audit.list(fx.otherOrganization, { limit: 100 });
        for (const [client, org, hook] of [
          [owner, fx.otherOrganization, other.id],
          [owner, fx.organization, other.id],
          [outsider, fx.organization, mine.id],
          [outsider, fx.otherOrganization, mine.id],
          [owner, fx.organization, "we_missing"],
        ] as const) {
          expect(await failure(client.webhooks.rollSecret(org, hook))).toMatchObject({
            status: 404,
            code: "not_found",
          });
        }
        expect(await failure(as(null).webhooks.rollSecret(fx.organization, mine.id))).toMatchObject({
          status: 401,
          code: "unauthenticated",
        });
        expect(await owner.webhooks.list(fx.organization)).toEqual(before);
        expect(await outsider.webhooks.list(fx.otherOrganization)).toEqual(otherBefore);
        expect(await owner.audit.list(fx.organization, { limit: 100 })).toEqual(audit);
        expect(await outsider.audit.list(fx.otherOrganization, { limit: 100 })).toEqual(otherAudit);
        const person = (await as("developer").me.get()).user;
        const member = must(
          (await owner.members.list(fx.organization)).find((row) => row.user.id === person.id),
          "developer member",
        );
        await owner.members.update(fx.organization, member.id, { role: "admin" });
        for (const environment of ["sandbox", "production"] as const) {
          const endpoint = await endpointIn(environment);
          const rolled = await as("developer").webhooks.rollSecret(fx.organization, endpoint.id);
          expect(rolled.endpoint.id).toBe(endpoint.id);
          const event = must(
            (await owner.audit.list(fx.organization)).items.find(
              (row) => row.action === "webhook.secret_rolled" && row.target.id === endpoint.id,
            ),
            "admin rotation audit",
          );
          expect(event).toMatchObject({
            actor: { type: "user", id: person.id, name: person.name },
            environment,
            workspace_id: null,
          });
        }
      });

      it("resends each terminal outcome with a fresh delivery and the exact original event", async () => {
        for (const environment of ["sandbox", "production"] as const) {
          const endpoint = await endpointIn(environment);
          const original = await owner.webhooks.deliveries(fx.organization, endpoint.id);
          // Unrelated pending work at this endpoint does not prevent resending a terminal delivery.
          expect(original.items.some((row) => row.state === "pending")).toBe(true);
          for (const state of ["succeeded", "failed", "skipped"] as const) {
            const source = must(
              original.items.find((row) => row.state === state && row.event_type !== "webhook.test"),
              `${state} delivery`,
            );
            const detail = await owner.webhooks.delivery(fx.organization, endpoint.id, source.id);
            const attempts = await owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, source.id);
            const resent = await owner.webhooks.resendDelivery(fx.organization, endpoint.id, source.id);
            expect(resent).toEqual({
              id: expect.any(String),
              organization_id: fx.organization,
              endpoint_id: endpoint.id,
              event_id: source.event_id,
              environment,
              event_type: source.event_type,
              event_created_at: source.event_created_at,
              workspace: source.workspace,
              state: "pending",
              attempts: 0,
              last_status_code: null,
              last_error: null,
              last_attempt_at: null,
              next_attempt_at: expect.any(String),
              created_at: expect.any(String),
            });
            expect(resent.id).not.toBe(source.id);
            expect(resent.next_attempt_at).toBe(resent.created_at);
            expect(await owner.webhooks.delivery(fx.organization, endpoint.id, resent.id)).toEqual({
              ...resent,
              event: detail.event,
              attempt_history_complete: true,
            });
            expect(await owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, resent.id)).toEqual({
              items: [],
              pagination: { next_cursor: null, total: 0 },
            });
            expect(await owner.webhooks.delivery(fx.organization, endpoint.id, source.id)).toEqual(detail);
            expect(await owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, source.id)).toEqual(attempts);
          }
          const after = await owner.webhooks.deliveries(fx.organization, endpoint.id);
          expect(after.pagination.total).toBe(must(original.pagination.total, "delivery count") + 3);
          expect(after.items.filter((row) => original.items.some((source) => row.id === source.id))).toEqual(
            original.items,
          );
        }
      });

      it("refuses pending, disabled and unsubscribed resends without changing history or auditing", async () => {
        const endpoint = await endpointIn("sandbox");
        const history = await owner.webhooks.deliveries(fx.organization, endpoint.id);
        const pending = must(
          history.items.find((row) => row.state === "pending"),
          "pending delivery",
        );
        const failed = must(
          history.items.find((row) => row.state === "failed"),
          "failed delivery",
        );
        const test = must(
          history.items.find((row) => row.event_type === "webhook.test"),
          "test delivery",
        );
        const details = await Promise.all(
          history.items.map((row) => owner.webhooks.delivery(fx.organization, endpoint.id, row.id)),
        );
        const attempts = await Promise.all(
          history.items.map((row) => owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, row.id)),
        );
        const refused = async (source: WebhookDeliveryId, code: string) => {
          const audit = await owner.audit.list(fx.organization, { limit: 100 });
          expect(await failure(owner.webhooks.resendDelivery(fx.organization, endpoint.id, source))).toMatchObject({
            status: 409,
            code,
          });
          expect(await owner.webhooks.deliveries(fx.organization, endpoint.id)).toEqual(history);
          expect(await owner.audit.list(fx.organization, { limit: 100 })).toEqual(audit);
        };
        await refused(pending.id, "delivery_pending");
        await owner.webhooks.update(fx.organization, endpoint.id, { enabled: false });
        await refused(failed.id, "webhook_disabled");
        await refused(test.id, "webhook_disabled");
        await owner.webhooks.update(fx.organization, endpoint.id, { enabled: true, events: ["run.failed"] });
        expect(failed.event_type).toBe("run.completed");
        await refused(failed.id, "webhook_not_subscribed");
        expect(
          await Promise.all(history.items.map((row) => owner.webhooks.delivery(fx.organization, endpoint.id, row.id))),
        ).toEqual(details);
        expect(
          await Promise.all(
            history.items.map((row) => owner.webhooks.deliveryAttempts(fx.organization, endpoint.id, row.id)),
          ),
        ).toEqual(attempts);
        // A test event cannot be subscribed to, so it is exempt only from the subscription check.
        const resent = await owner.webhooks.resendDelivery(fx.organization, endpoint.id, test.id);
        expect(resent).toMatchObject({ event_id: test.event_id, event_type: "webhook.test", workspace: null });
        const event = must(
          (await owner.audit.list(fx.organization)).items.find((row) => row.target.id === resent.id),
          "test resend audit",
        );
        expect(event).toMatchObject({
          action: "webhook.delivery_resent",
          target: { type: "webhook_delivery", id: resent.id, name: "webhook.test" },
          workspace_id: null,
          environment: "sandbox",
        });
      });

      it("replays a resend once and audits only a newly created delivery", async () => {
        const endpoint = await endpointIn("production");
        const history = await owner.webhooks.deliveries(fx.organization, endpoint.id);
        const source = must(
          history.items.find((row) => row.state === "failed"),
          "failed delivery",
        );
        const sibling = must(
          history.items.find((row) => row.state === "skipped"),
          "skipped delivery",
        );
        const options = { idempotencyKey: "resend-once" };
        const first = await owner.webhooks.resendDelivery(fx.organization, endpoint.id, source.id, options);
        expect(await owner.webhooks.resendDelivery(fx.organization, endpoint.id, source.id, options)).toEqual(first);
        expect(
          await failure(owner.webhooks.resendDelivery(fx.organization, endpoint.id, sibling.id, options)),
        ).toMatchObject({ status: 422, code: "idempotency_key_reused" });
        expect((await owner.webhooks.deliveries(fx.organization, endpoint.id)).pagination.total).toBe(
          must(history.pagination.total, "delivery count") + 1,
        );
        const me = (await owner.me.get()).user;
        const audit = (await owner.audit.list(fx.organization, { limit: 100 })).items.filter(
          (row) => row.action === "webhook.delivery_resent",
        );
        expect(audit).toHaveLength(1);
        expect(audit[0]).toMatchObject({
          organization_id: fx.organization,
          workspace_id: source.workspace?.id,
          environment: "production",
          actor: { type: "user", id: me.id, name: me.name },
          target: { type: "webhook_delivery", id: first.id, name: source.event_type },
        });
        const again = await owner.webhooks.resendDelivery(fx.organization, endpoint.id, source.id);
        expect(again.id).not.toBe(first.id);
        expect(again.event_id).toBe(source.event_id);
        expect((await owner.webhooks.deliveries(fx.organization, endpoint.id)).pagination.total).toBe(
          must(history.pagination.total, "delivery count") + 2,
        );
        expect(
          (await owner.audit.list(fx.organization, { limit: 100 })).items.filter(
            (row) => row.action === "webhook.delivery_resent",
          ),
        ).toHaveLength(2);
      });

      it("keeps resends inside the original organization, workspace and environment", async () => {
        const endpoint = await endpointIn("production");
        const sandbox = await endpointIn("sandbox");
        const outsider = as("outsider");
        const other = must((await outsider.webhooks.list(fx.otherOrganization))[0], "foreign webhook");
        const source = must(
          (await owner.webhooks.deliveries(fx.organization, endpoint.id, { state: "failed" })).items[0],
          "failed delivery",
        );
        const workspace = must(source.workspace, "source workspace");
        const beta = await workspaceBySlug("beta-labs");
        const sibling = workspace.id === beta.id ? fx.workspaces.acme : beta;
        const inWorkspace = await owner.webhooks.deliveries(fx.organization, endpoint.id, {
          workspace_id: workspace.id,
        });
        const inSibling = await owner.webhooks.deliveries(fx.organization, endpoint.id, { workspace_id: sibling.id });
        const inSandbox = await owner.webhooks.deliveries(fx.organization, sandbox.id);
        const inOther = await outsider.webhooks.deliveries(fx.otherOrganization, other.id);
        const resent = await owner.webhooks.resendDelivery(fx.organization, endpoint.id, source.id);
        expect(
          (await owner.webhooks.deliveries(fx.organization, endpoint.id, { workspace_id: workspace.id })).items,
        ).toEqual([resent, ...inWorkspace.items]);
        expect(await owner.webhooks.deliveries(fx.organization, endpoint.id, { workspace_id: sibling.id })).toEqual(
          inSibling,
        );
        expect(await owner.webhooks.deliveries(fx.organization, sandbox.id)).toEqual(inSandbox);
        expect(await outsider.webhooks.deliveries(fx.otherOrganization, other.id)).toEqual(inOther);
        for (const [client, org, hook] of [
          [owner, fx.organization, sandbox],
          [owner, fx.organization, other],
          [outsider, fx.otherOrganization, other],
        ] as const) {
          expect(await failure(client.webhooks.delivery(org, hook.id, resent.id))).toMatchObject({
            status: 404,
            code: "not_found",
          });
          expect(await failure(client.webhooks.resendDelivery(org, hook.id, resent.id))).toMatchObject({
            status: 404,
            code: "not_found",
          });
        }
      });

      it("lets an admin resend in Production without rewriting the event's workspace snapshot", async () => {
        const endpoint = await endpointIn("production");
        const source = must(
          (await owner.webhooks.deliveries(fx.organization, endpoint.id, { state: "failed" })).items[0],
          "failed delivery",
        );
        const detail = await owner.webhooks.delivery(fx.organization, endpoint.id, source.id);
        const workspace = must(source.workspace, "source workspace");
        await owner.workspaces.update(workspace.id, { external_id: "renamed-after-event" });
        const person = (await as("developer").me.get()).user;
        const member = must(
          (await owner.members.list(fx.organization)).find((row) => row.user.id === person.id),
          "developer member",
        );
        await owner.members.update(fx.organization, member.id, { role: "admin" });
        const resent = await as("developer").webhooks.resendDelivery(fx.organization, endpoint.id, source.id);
        expect(resent.workspace).toEqual(workspace);
        expect((await owner.webhooks.delivery(fx.organization, endpoint.id, resent.id)).event).toEqual(detail.event);
        const audit = must(
          (await owner.audit.list(fx.organization)).items.find((row) => row.target.id === resent.id),
          "admin resend audit",
        );
        expect(audit).toMatchObject({
          actor: { type: "user", id: person.id, name: person.name },
          workspace_id: workspace.id,
          environment: "production",
        });
      });

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
          expect(await failure(client.webhooks.resendDelivery(organization, endpoint.id, delivery))).toMatchObject({
            status: 404,
            code: "not_found",
          });
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
        expect(await failure(as(null).webhooks.resendDelivery(fx.organization, mine.id, ownDelivery.id))).toMatchObject(
          { status: 401, code: "unauthenticated" },
        );
        expect(await failure(owner.webhooks.resendDelivery(fx.organization, mine.id, "whd_missing"))).toMatchObject({
          status: 404,
          code: "not_found",
        });
      });

      it("keeps an API key's webhook history, resends and secret rotations inside its organization and environment", async () => {
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
          expect(await failure(client.webhooks.resendDelivery(fx.organization, other.id, foreign.id))).toMatchObject({
            status: 404,
            code: "not_found",
          });
          const otherBefore = await endpointIn(other.environment);
          expect(await failure(client.webhooks.rollSecret(fx.organization, other.id))).toMatchObject({
            status: 404,
            code: "not_found",
          });
          expect(await failure(client.webhooks.rollSecret(fx.otherOrganization, endpoint.id))).toMatchObject({
            status: 404,
            code: "not_found",
          });
          expect(await failure(client.webhooks.deliveries(fx.otherOrganization, endpoint.id))).toMatchObject({
            status: 404,
            code: "not_found",
          });
          expect(
            await failure(client.webhooks.resendDelivery(fx.otherOrganization, endpoint.id, row.id)),
          ).toMatchObject({
            status: 404,
            code: "not_found",
          });
          const terminal = must(
            list.items.find((delivery) => delivery.state === "failed"),
            "failed delivery",
          );
          const resent = await client.webhooks.resendDelivery(fx.organization, endpoint.id, terminal.id);
          expect(resent).toMatchObject({
            organization_id: fx.organization,
            endpoint_id: endpoint.id,
            environment,
            event_id: terminal.event_id,
            workspace: terminal.workspace,
            state: "pending",
            attempts: 0,
          });
          const audit = must(
            (await owner.audit.list(fx.organization)).items.find((event) => event.target.id === resent.id),
            "API-key resend audit",
          );
          expect(audit).toMatchObject({
            action: "webhook.delivery_resent",
            actor: { type: "api_key", id: key.key.id, name: key.key.name },
            workspace_id: terminal.workspace?.id,
            environment,
          });
          const history = await client.webhooks.deliveries(fx.organization, endpoint.id);
          const options = { idempotencyKey: "roll-with-key" };
          const rolled = await client.webhooks.rollSecret(fx.organization, endpoint.id, options);
          expect(rolled.endpoint).toEqual({
            ...endpoint,
            signing_secret_last4: rolled.signing_secret.slice(-4),
            previous_signing_secret_expires_at: expect.any(String),
          });
          expect(await failure(client.webhooks.rollSecret(fx.organization, endpoint.id, options))).toMatchObject({
            status: 409,
            code: "idempotency_key_completed",
          });
          expect(await failure(client.webhooks.rollSecret(fx.organization, endpoint.id))).toMatchObject({
            status: 409,
            code: "rotation_in_progress",
          });
          expect(await client.webhooks.list(fx.organization)).toEqual([rolled.endpoint]);
          expect(await endpointIn(other.environment)).toEqual(otherBefore);
          expect(await client.webhooks.deliveries(fx.organization, endpoint.id)).toEqual(history);
          const rotations = (await owner.audit.list(fx.organization, { limit: 100 })).items.filter(
            (row) => row.action === "webhook.secret_rolled" && row.target.id === endpoint.id,
          );
          expect(rotations).toHaveLength(1);
          expect(rotations[0]).toMatchObject({
            actor: { type: "api_key", id: key.key.id, name: key.key.name },
            workspace_id: null,
            environment,
            target: { type: "webhook", id: endpoint.id, name: endpoint.url },
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
