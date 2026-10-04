/**
 * Every endpoint of the Zupflow API: method, path, who may call it, what it
 * takes and what it answers. The OpenAPI document is generated from this
 * table, and the mock's contract tests check each response against it.
 */
import { z } from "zod";
import { idSchema } from "./ids";
import * as s from "./schemas";

export type Method = "get" | "post" | "put" | "patch" | "delete";

/**
 * Who may call a route: a Console member's session, an organization's API
 * key, or hosted Connect, authorized by a session's reference alone.
 */
export type Caller = "console" | "api_key" | "connect";

export interface Route {
  operationId: string;
  method: Method;
  /** OpenAPI form: `/v1/workspaces/{ws}`. */
  path: string;
  summary: string;
  callers: readonly Caller[];
  params: Record<string, z.ZodType>;
  query?: z.ZodObject;
  body?: z.ZodType;
  /** `data` is the response's `data`; `list` wraps it in an array; `paginated` adds `meta.pagination`. */
  response: { status: 200 | 201 | 204; data?: z.ZodType; list?: boolean; paginated?: boolean };
  /**
   * The response shows a secret once. Idempotency records that its key
   * completed but never saves the body, so a retry answers
   * `409 idempotency_key_completed` instead of showing the secret again.
   */
  showsSecretOnce?: true;
}

const org = { org: idSchema("organization") };
const ws = { ws: idSchema("workspace") };
/** Hosted Connect's bearer secret: the random token at the end of a session's `url`, never its ID. */
const reference = z
  .string()
  .describe("The session's reference: the random token at the end of its `url`. Possession authorizes the page.");
const both: Caller[] = ["console", "api_key"];
const consoleOnly: Caller[] = ["console"];
const connect: Caller[] = ["connect"];

const ok = (data: z.ZodType, extra: Partial<Route["response"]> = {}) => ({ status: 200 as const, data, ...extra });
const created = (data: z.ZodType) => ({ status: 201 as const, data });
const noContent = { status: 204 as const };

export const routes: readonly Route[] = [
  // Identity
  {
    operationId: "getViewer",
    method: "get",
    path: "/v1/me",
    summary: "The signed-in member and their organizations",
    callers: consoleOnly,
    params: {},
    response: ok(s.Viewer),
  },
  {
    operationId: "listModels",
    method: "get",
    path: "/v1/models",
    summary: "Models an agent can use",
    callers: both,
    params: {},
    response: ok(s.ModelOption, { list: true }),
  },

  // Organizations
  {
    operationId: "createOrganization",
    method: "post",
    path: "/v1/organizations",
    summary: "Create an organization; the caller becomes its owner",
    callers: consoleOnly,
    params: {},
    body: s.CreateOrganizationInput,
    response: created(s.Organization),
  },
  {
    operationId: "getOrganization",
    method: "get",
    path: "/v1/organizations/{org}",
    summary: "An organization",
    callers: consoleOnly,
    params: org,
    response: ok(s.Organization),
  },
  {
    operationId: "updateOrganization",
    method: "patch",
    path: "/v1/organizations/{org}",
    summary: "Rename an organization",
    callers: consoleOnly,
    params: org,
    body: s.UpdateOrganizationInput,
    response: ok(s.Organization),
  },
  {
    operationId: "getUsage",
    method: "get",
    path: "/v1/organizations/{org}/usage",
    summary: "Runs, failures and tool calls per UTC day",
    callers: both,
    params: org,
    query: z.object({
      environment: s.Environment,
      days: z
        .number()
        .int()
        .optional()
        .describe("1 to 90 (default 14); values outside are clamped, and a non-integer means the default."),
    }),
    response: ok(s.UsageReport),
  },
  {
    operationId: "listMembers",
    method: "get",
    path: "/v1/organizations/{org}/members",
    summary: "Members and invitations",
    callers: consoleOnly,
    params: org,
    response: ok(s.Member, { list: true }),
  },
  {
    operationId: "inviteMember",
    method: "post",
    path: "/v1/organizations/{org}/members",
    summary: "Invite a member",
    callers: consoleOnly,
    params: org,
    body: s.InviteMemberInput,
    response: created(s.Member),
  },
  {
    operationId: "updateMember",
    method: "patch",
    path: "/v1/organizations/{org}/members/{member}",
    summary: "Change a member's role",
    callers: consoleOnly,
    params: { ...org, member: idSchema("member") },
    body: s.UpdateMemberInput,
    response: ok(s.Member),
  },
  {
    operationId: "removeMember",
    method: "delete",
    path: "/v1/organizations/{org}/members/{member}",
    summary: "Remove a member or withdraw an invitation",
    callers: consoleOnly,
    params: { ...org, member: idSchema("member") },
    response: noContent,
  },
  {
    operationId: "listOrganizationRuns",
    method: "get",
    path: "/v1/organizations/{org}/runs",
    summary: "Runs across the organization's workspaces, newest first",
    callers: both,
    params: org,
    query: s.OrganizationRunListQuery,
    response: ok(s.OrganizationRun, { list: true, paginated: true }),
  },
  {
    operationId: "listAuditEvents",
    method: "get",
    path: "/v1/organizations/{org}/audit-events",
    summary: "The audit log",
    callers: consoleOnly,
    params: org,
    query: s.AuditListQuery,
    response: ok(s.AuditEvent, { list: true, paginated: true }),
  },

  // Workspaces
  {
    operationId: "listWorkspaces",
    method: "get",
    path: "/v1/organizations/{org}/workspaces",
    summary: "Workspaces, searched and paged on the server",
    callers: both,
    params: org,
    query: s.WorkspaceListQuery,
    response: ok(s.Workspace, { list: true, paginated: true }),
  },
  {
    operationId: "createWorkspace",
    method: "post",
    path: "/v1/organizations/{org}/workspaces",
    summary: "Create a workspace in one environment",
    callers: both,
    params: org,
    body: s.CreateWorkspaceInput,
    response: created(s.Workspace),
  },
  {
    operationId: "getWorkspace",
    method: "get",
    path: "/v1/workspaces/{ws}",
    summary: "A workspace",
    callers: both,
    params: ws,
    response: ok(s.Workspace),
  },
  {
    operationId: "updateWorkspace",
    method: "patch",
    path: "/v1/workspaces/{ws}",
    summary: "Rename a workspace or change its External ID",
    callers: both,
    params: ws,
    body: s.UpdateWorkspaceInput,
    response: ok(s.Workspace),
  },
  {
    operationId: "deleteWorkspace",
    method: "delete",
    path: "/v1/workspaces/{ws}",
    summary: "Delete a workspace and everything in it",
    callers: both,
    params: ws,
    response: noContent,
  },

  // Organization apps
  {
    operationId: "listOrganizationApps",
    method: "get",
    path: "/v1/organizations/{org}/apps",
    summary: "The organization's app catalog",
    callers: both,
    params: org,
    response: ok(s.AppDefinition, { list: true }),
  },
  {
    operationId: "createOrganizationApp",
    method: "post",
    path: "/v1/organizations/{org}/apps",
    summary: "Add an app to the catalog",
    callers: both,
    params: org,
    body: s.CreateAppInput,
    response: created(s.AppDefinition),
  },
  {
    operationId: "getOrganizationApp",
    method: "get",
    path: "/v1/organizations/{org}/apps/{app}",
    summary: "A catalog app",
    callers: both,
    params: { ...org, app: idSchema("app") },
    response: ok(s.AppDefinition),
  },
  {
    operationId: "updateOrganizationApp",
    method: "patch",
    path: "/v1/organizations/{org}/apps/{app}",
    summary: "Rename or describe a catalog app",
    callers: both,
    params: { ...org, app: idSchema("app") },
    body: s.UpdateAppInput,
    response: ok(s.AppDefinition),
  },
  {
    operationId: "getOrganizationAppConfiguration",
    method: "get",
    path: "/v1/organizations/{org}/apps/{app}/environments/{env}/configuration",
    summary: "A catalog app's client configuration in one environment",
    callers: both,
    params: { ...org, app: idSchema("app"), env: s.Environment },
    response: ok(s.ClientConfiguration),
  },
  {
    operationId: "updateOrganizationAppConfiguration",
    method: "put",
    path: "/v1/organizations/{org}/apps/{app}/environments/{env}/configuration",
    summary: "Set a catalog app's client configuration in one environment",
    callers: both,
    params: { ...org, app: idSchema("app"), env: s.Environment },
    body: s.UpdateConfigurationInput,
    response: ok(s.ClientConfiguration),
  },
  {
    operationId: "listOrganizationAppInstallations",
    method: "get",
    path: "/v1/organizations/{org}/apps/{app}/installations",
    summary: "Installations of a catalog app across workspaces",
    callers: both,
    params: { ...org, app: idSchema("app") },
    query: z.object({
      environment: s.Environment.optional(),
      cursor: z.string().optional(),
      limit: z
        .number()
        .int()
        .optional()
        .describe("Page size, 1 to 100 (default 25); values outside are clamped, and a non-integer means the default."),
    }),
    response: ok(s.AppInstallationRef, { list: true, paginated: true }),
  },

  // Workspace apps
  {
    operationId: "listWorkspaceApps",
    method: "get",
    path: "/v1/workspaces/{ws}/apps",
    summary: "The workspace's own apps",
    callers: both,
    params: ws,
    response: ok(s.AppDefinition, { list: true }),
  },
  {
    operationId: "createWorkspaceApp",
    method: "post",
    path: "/v1/workspaces/{ws}/apps",
    summary: "Add an app only this workspace can use",
    callers: both,
    params: ws,
    body: s.CreateAppInput,
    response: created(s.AppDefinition),
  },
  {
    operationId: "getWorkspaceApp",
    method: "get",
    path: "/v1/workspaces/{ws}/apps/{app}",
    summary: "A workspace app",
    callers: both,
    params: { ...ws, app: idSchema("app") },
    response: ok(s.AppDefinition),
  },
  {
    operationId: "updateWorkspaceApp",
    method: "patch",
    path: "/v1/workspaces/{ws}/apps/{app}",
    summary: "Rename or describe a workspace app",
    callers: both,
    params: { ...ws, app: idSchema("app") },
    body: s.UpdateAppInput,
    response: ok(s.AppDefinition),
  },
  {
    operationId: "getWorkspaceAppConfiguration",
    method: "get",
    path: "/v1/workspaces/{ws}/apps/{app}/configuration",
    summary: "A workspace app's client configuration",
    callers: both,
    params: { ...ws, app: idSchema("app") },
    response: ok(s.ClientConfiguration),
  },
  {
    operationId: "updateWorkspaceAppConfiguration",
    method: "put",
    path: "/v1/workspaces/{ws}/apps/{app}/configuration",
    summary: "Set a workspace app's client configuration",
    callers: both,
    params: { ...ws, app: idSchema("app") },
    body: s.UpdateConfigurationInput,
    response: ok(s.ClientConfiguration),
  },
  {
    operationId: "listAvailableApps",
    method: "get",
    path: "/v1/workspaces/{ws}/available-apps",
    summary: "Apps ready to install in this workspace",
    callers: both,
    params: ws,
    response: ok(s.AvailableApp, { list: true }),
  },

  // Installations
  {
    operationId: "listInstallations",
    method: "get",
    path: "/v1/workspaces/{ws}/installations",
    summary: "Connected accounts in a workspace",
    callers: both,
    params: ws,
    response: ok(s.Installation, { list: true }),
  },
  {
    operationId: "getInstallation",
    method: "get",
    path: "/v1/workspaces/{ws}/installations/{inst}",
    summary: "An installation",
    callers: both,
    params: { ...ws, inst: idSchema("installation") },
    response: ok(s.Installation),
  },
  {
    operationId: "updateInstallation",
    method: "patch",
    path: "/v1/workspaces/{ws}/installations/{inst}",
    summary: "Rename an installation",
    callers: both,
    params: { ...ws, inst: idSchema("installation") },
    body: s.UpdateInstallationInput,
    response: ok(s.Installation),
  },
  {
    operationId: "disconnectInstallation",
    method: "post",
    path: "/v1/workspaces/{ws}/installations/{inst}/disconnect",
    summary: "Stop using an installation; its grants are kept",
    callers: both,
    params: { ...ws, inst: idSchema("installation") },
    response: ok(s.Installation),
  },
  {
    operationId: "deleteInstallation",
    method: "delete",
    path: "/v1/workspaces/{ws}/installations/{inst}",
    summary: "Delete an installation, its credential and its grants",
    callers: both,
    params: { ...ws, inst: idSchema("installation") },
    response: noContent,
  },

  // Connect sessions
  {
    operationId: "listConnectSessions",
    method: "get",
    path: "/v1/workspaces/{ws}/connect-sessions",
    summary: "A workspace's Connect sessions",
    callers: both,
    params: ws,
    response: ok(s.ConnectSession, { list: true }),
  },
  {
    operationId: "createConnectSession",
    method: "post",
    path: "/v1/workspaces/{ws}/connect-sessions",
    summary: "Start Connect; the response's url is the hosted page, shown once",
    callers: both,
    params: ws,
    body: s.CreateConnectSessionInput,
    response: created(s.CreatedConnectSession),
    showsSecretOnce: true,
  },
  {
    operationId: "getConnectSession",
    method: "get",
    path: "/v1/workspaces/{ws}/connect-sessions/{session}",
    summary: "A Connect session, for polling its outcome",
    callers: both,
    params: { ...ws, session: idSchema("connect_session") },
    response: ok(s.ConnectSession),
  },

  // Hosted Connect
  {
    operationId: "getHostedConnect",
    method: "get",
    path: "/v1/connect/sessions/{reference}",
    summary: "The hosted Connect page's session, by reference",
    callers: connect,
    params: { reference },
    response: ok(s.ConnectSession),
  },
  {
    operationId: "submitConnectCredentials",
    method: "post",
    path: "/v1/connect/sessions/{reference}/credentials",
    summary: "Submit a token app's credentials",
    callers: connect,
    params: { reference },
    body: s.SubmitConnectCredentialsInput,
    response: ok(s.ConnectResult),
  },
  {
    operationId: "authorizeConnect",
    method: "post",
    path: "/v1/connect/sessions/{reference}/authorize",
    summary: "Start an OAuth app's consent",
    callers: connect,
    params: { reference },
    response: ok(s.AuthorizeConnectResult),
    // The authorization URL carries the reference, and on the backend the session-bound OAuth state.
    showsSecretOnce: true,
  },
  {
    operationId: "cancelConnect",
    method: "post",
    path: "/v1/connect/sessions/{reference}/cancel",
    summary: "Cancel a Connect session",
    callers: connect,
    params: { reference },
    response: ok(s.ConnectSession),
  },

  // Agents and grants
  {
    operationId: "listAgents",
    method: "get",
    path: "/v1/workspaces/{ws}/agents",
    summary: "A workspace's agents",
    callers: both,
    params: ws,
    response: ok(s.Agent, { list: true }),
  },
  {
    operationId: "createAgent",
    method: "post",
    path: "/v1/workspaces/{ws}/agents",
    summary: "Create an agent",
    callers: both,
    params: ws,
    body: s.CreateAgentInput,
    response: created(s.Agent),
  },
  {
    operationId: "getAgent",
    method: "get",
    path: "/v1/workspaces/{ws}/agents/{agent}",
    summary: "An agent",
    callers: both,
    params: { ...ws, agent: idSchema("agent") },
    response: ok(s.Agent),
  },
  {
    operationId: "updateAgent",
    method: "patch",
    path: "/v1/workspaces/{ws}/agents/{agent}",
    summary: "Change an agent, or pause it",
    callers: both,
    params: { ...ws, agent: idSchema("agent") },
    body: s.UpdateAgentInput,
    response: ok(s.Agent),
  },
  {
    operationId: "deleteAgent",
    method: "delete",
    path: "/v1/workspaces/{ws}/agents/{agent}",
    summary: "Delete an agent and its grants",
    callers: both,
    params: { ...ws, agent: idSchema("agent") },
    response: noContent,
  },
  {
    operationId: "listGrants",
    method: "get",
    path: "/v1/workspaces/{ws}/agents/{agent}/grants",
    summary: "An agent's grants",
    callers: both,
    params: { ...ws, agent: idSchema("agent") },
    response: ok(s.AgentGrant, { list: true }),
  },
  {
    operationId: "createGrant",
    method: "post",
    path: "/v1/workspaces/{ws}/agents/{agent}/grants",
    summary: "Grant an agent tools of one installation",
    callers: both,
    params: { ...ws, agent: idSchema("agent") },
    body: s.CreateGrantInput,
    response: created(s.AgentGrant),
  },
  {
    operationId: "updateGrant",
    method: "patch",
    path: "/v1/workspaces/{ws}/agents/{agent}/grants/{grant}",
    summary: "Change a grant's tools and approvals",
    callers: both,
    params: { ...ws, agent: idSchema("agent"), grant: idSchema("grant") },
    body: s.UpdateGrantInput,
    response: ok(s.AgentGrant),
  },
  {
    operationId: "deleteGrant",
    method: "delete",
    path: "/v1/workspaces/{ws}/agents/{agent}/grants/{grant}",
    summary: "Revoke a grant",
    callers: both,
    params: { ...ws, agent: idSchema("agent"), grant: idSchema("grant") },
    response: noContent,
  },

  // Runs
  {
    operationId: "startRun",
    method: "post",
    path: "/v1/workspaces/{ws}/agents/{agent}/runs",
    summary: "Start a run",
    callers: both,
    params: { ...ws, agent: idSchema("agent") },
    body: s.CreateRunInput,
    response: created(s.Run),
  },
  {
    operationId: "listRuns",
    method: "get",
    path: "/v1/workspaces/{ws}/runs",
    summary: "A workspace's runs, newest first",
    callers: both,
    params: ws,
    query: s.RunListQuery,
    response: ok(s.Run, { list: true, paginated: true }),
  },
  {
    operationId: "getRun",
    method: "get",
    path: "/v1/workspaces/{ws}/runs/{run}",
    summary: "A run",
    callers: both,
    params: { ...ws, run: idSchema("run") },
    response: ok(s.Run),
  },
  {
    operationId: "listRunEvents",
    method: "get",
    path: "/v1/workspaces/{ws}/runs/{run}/events",
    summary: "A run's timeline, in order",
    callers: both,
    params: { ...ws, run: idSchema("run") },
    query: z.object({ after: z.number().int().optional() }),
    response: ok(s.RunEvent, { list: true }),
  },
  {
    operationId: "cancelRun",
    method: "post",
    path: "/v1/workspaces/{ws}/runs/{run}/cancel",
    summary: "Cancel a run",
    callers: both,
    params: { ...ws, run: idSchema("run") },
    response: ok(s.Run),
  },
  {
    operationId: "resolveApproval",
    method: "post",
    path: "/v1/workspaces/{ws}/runs/{run}/approvals",
    summary: "Record a decision your product made; API keys only",
    callers: ["api_key"],
    params: { ...ws, run: idSchema("run") },
    body: s.ResolveApprovalInput,
    response: ok(s.Run),
  },

  // Developer settings
  {
    operationId: "getConnectSettings",
    method: "get",
    path: "/v1/organizations/{org}/connect-settings/{env}",
    summary: "Connect's return URLs and origins in one environment",
    callers: both,
    params: { ...org, env: s.Environment },
    response: ok(s.ConnectSettings),
  },
  {
    operationId: "updateConnectSettings",
    method: "put",
    path: "/v1/organizations/{org}/connect-settings/{env}",
    summary: "Set Connect's return URLs and origins in one environment",
    callers: both,
    params: { ...org, env: s.Environment },
    body: s.UpdateConnectSettingsInput,
    response: ok(s.ConnectSettings),
  },
  {
    operationId: "listApiKeys",
    method: "get",
    path: "/v1/organizations/{org}/api-keys",
    summary: "API keys",
    callers: consoleOnly,
    params: org,
    response: ok(s.ApiKey, { list: true }),
  },
  {
    operationId: "createApiKey",
    method: "post",
    path: "/v1/organizations/{org}/api-keys",
    summary: "Create an API key; the secret is shown once",
    callers: consoleOnly,
    params: org,
    body: s.CreateApiKeyInput,
    response: created(s.CreatedApiKey),
    showsSecretOnce: true,
  },
  {
    operationId: "revokeApiKey",
    method: "post",
    path: "/v1/organizations/{org}/api-keys/{key}/revoke",
    summary: "Revoke an API key",
    callers: consoleOnly,
    params: { ...org, key: idSchema("api_key") },
    response: ok(s.ApiKey),
  },
  {
    operationId: "listWebhooks",
    method: "get",
    path: "/v1/organizations/{org}/webhooks",
    summary: "Webhook endpoints",
    callers: both,
    params: org,
    response: ok(s.WebhookEndpoint, { list: true }),
  },
  {
    operationId: "createWebhook",
    method: "post",
    path: "/v1/organizations/{org}/webhooks",
    summary: "Add a webhook endpoint; the signing secret is shown once",
    callers: both,
    params: org,
    body: s.CreateWebhookEndpointInput,
    response: created(s.CreatedWebhookEndpoint),
    showsSecretOnce: true,
  },
  {
    operationId: "updateWebhook",
    method: "patch",
    path: "/v1/organizations/{org}/webhooks/{hook}",
    summary: "Change a webhook endpoint",
    callers: both,
    params: { ...org, hook: idSchema("webhook_endpoint") },
    body: s.UpdateWebhookEndpointInput,
    response: ok(s.WebhookEndpoint),
  },
  {
    operationId: "deleteWebhook",
    method: "delete",
    path: "/v1/organizations/{org}/webhooks/{hook}",
    summary: "Delete a webhook endpoint",
    callers: both,
    params: { ...org, hook: idSchema("webhook_endpoint") },
    response: noContent,
  },
  {
    operationId: "testWebhook",
    method: "post",
    path: "/v1/organizations/{org}/webhooks/{hook}/test",
    summary: "Send a test delivery",
    callers: both,
    params: { ...org, hook: idSchema("webhook_endpoint") },
    response: ok(s.WebhookDelivery),
  },
];

/** The route a request hits, by method and concrete path (`/v1/workspaces/ws_…`). */
export function matchRoute(method: string, pathname: string): Route | undefined {
  const m = method.toLowerCase();
  return routes.find((route) => {
    if (route.method !== m) return false;
    const pattern = new RegExp(`^${route.path.replace(/\{[^}]+\}/g, "[^/]+")}$`);
    return pattern.test(pathname);
  });
}

/** The schema a successful response's `data` must match, or `undefined` for 204. */
export function responseDataSchema(route: Route): z.ZodType | undefined {
  const { data, list } = route.response;
  if (!data) return undefined;
  return list ? z.array(data) : data;
}
