import type { QueryValue, Transport } from "./transport";
import type {
  Agent,
  AgentGrant,
  ApiKey,
  AppDefinition,
  AppInstallationRef,
  AuditEvent,
  AuditListQuery,
  AuthorizeConnectResult,
  AvailableApp,
  ClientConfiguration,
  ConnectResult,
  ConnectSession,
  ConnectSettings,
  CreateAgentInput,
  CreateApiKeyInput,
  CreateAppInput,
  CreateConnectSessionInput,
  CreatedApiKey,
  CreatedWebhookEndpoint,
  CreateGrantInput,
  CreateOrganizationInput,
  CreateRunInput,
  CreateWebhookEndpointInput,
  CreateWorkspaceInput,
  Environment,
  Installation,
  InviteMemberInput,
  Member,
  ModelOption,
  Organization,
  OrganizationRun,
  OrganizationRunListQuery,
  Page,
  ResolveApprovalInput,
  Run,
  RunEvent,
  RunListQuery,
  SubmitConnectCredentialsInput,
  UpdateAgentInput,
  UpdateAppInput,
  UpdateConfigurationInput,
  UpdateConnectSettingsInput,
  UpdateGrantInput,
  UpdateInstallationInput,
  UpdateMemberInput,
  UpdateOrganizationInput,
  UpdateWebhookEndpointInput,
  UpdateWorkspaceInput,
  UsageReport,
  Viewer,
  WebhookDelivery,
  WebhookEndpoint,
  Workspace,
  WorkspaceListQuery,
} from "./types";

const enc = encodeURIComponent;

/**
 * Typed resource client over a {@link Transport}.
 *
 * Workspace-scoped operations take `workspaceId` as their first argument and
 * carry it in the path. A globally unique resource ID never stands in for the
 * workspace.
 */
export function createClient(transport: Transport) {
  const get = async <T>(path: string, query?: Record<string, QueryValue>) =>
    (await transport.request<T>({ method: "GET", path, query })).data;
  const getPage = async <T>(path: string, query?: Record<string, QueryValue>): Promise<Page<T>> => {
    const { data, meta } = await transport.request<T[]>({ method: "GET", path, query });
    return {
      items: data,
      pagination: meta?.pagination ?? { next_cursor: null, total: data.length },
    };
  };
  const send = async <T>(method: "POST" | "PUT" | "PATCH" | "DELETE", path: string, body?: unknown) =>
    (await transport.request<T>({ method, path, body })).data;

  const org = (organizationId: string) => `/v1/organizations/${enc(organizationId)}`;
  const ws = (workspaceId: string) => `/v1/workspaces/${enc(workspaceId)}`;

  return {
    me: {
      get: () => get<Viewer>("/v1/me"),
    },

    models: {
      list: () => get<ModelOption[]>("/v1/models"),
    },

    organizations: {
      create: (input: CreateOrganizationInput) => send<Organization>("POST", "/v1/organizations", input),
      get: (organizationId: string) => get<Organization>(org(organizationId)),
      update: (organizationId: string, input: UpdateOrganizationInput) =>
        send<Organization>("PATCH", org(organizationId), input),
      usage: (organizationId: string, environment: Environment, days = 14) =>
        get<UsageReport>(`${org(organizationId)}/usage`, { environment, days }),
    },

    members: {
      list: (organizationId: string) => get<Member[]>(`${org(organizationId)}/members`),
      invite: (organizationId: string, input: InviteMemberInput) =>
        send<Member>("POST", `${org(organizationId)}/members`, input),
      update: (organizationId: string, memberId: string, input: UpdateMemberInput) =>
        send<Member>("PATCH", `${org(organizationId)}/members/${enc(memberId)}`, input),
      remove: (organizationId: string, memberId: string) =>
        send<void>("DELETE", `${org(organizationId)}/members/${enc(memberId)}`),
    },

    workspaces: {
      list: (organizationId: string, query: WorkspaceListQuery = {}) =>
        getPage<Workspace>(`${org(organizationId)}/workspaces`, { ...query }),
      create: (organizationId: string, input: CreateWorkspaceInput) =>
        send<Workspace>("POST", `${org(organizationId)}/workspaces`, input),
      get: (workspaceId: string) => get<Workspace>(ws(workspaceId)),
      update: (workspaceId: string, input: UpdateWorkspaceInput) => send<Workspace>("PATCH", ws(workspaceId), input),
      delete: (workspaceId: string) => send<void>("DELETE", ws(workspaceId)),
    },

    organizationApps: {
      list: (organizationId: string) => get<AppDefinition[]>(`${org(organizationId)}/apps`),
      create: (organizationId: string, input: CreateAppInput) =>
        send<AppDefinition>("POST", `${org(organizationId)}/apps`, input),
      get: (organizationId: string, appId: string) => get<AppDefinition>(`${org(organizationId)}/apps/${enc(appId)}`),
      update: (organizationId: string, appId: string, input: UpdateAppInput) =>
        send<AppDefinition>("PATCH", `${org(organizationId)}/apps/${enc(appId)}`, input),
      getConfiguration: (organizationId: string, appId: string, environment: Environment) =>
        get<ClientConfiguration>(`${org(organizationId)}/apps/${enc(appId)}/environments/${environment}/configuration`),
      updateConfiguration: (
        organizationId: string,
        appId: string,
        environment: Environment,
        input: UpdateConfigurationInput,
      ) =>
        send<ClientConfiguration>(
          "PUT",
          `${org(organizationId)}/apps/${enc(appId)}/environments/${environment}/configuration`,
          input,
        ),
      /** Installations of this app across workspaces, newest first. */
      installations: (
        organizationId: string,
        appId: string,
        query: { environment?: Environment; cursor?: string; limit?: number } = {},
      ) => getPage<AppInstallationRef>(`${org(organizationId)}/apps/${enc(appId)}/installations`, { ...query }),
    },

    workspaceApps: {
      list: (workspaceId: string) => get<AppDefinition[]>(`${ws(workspaceId)}/apps`),
      create: (workspaceId: string, input: CreateAppInput) =>
        send<AppDefinition>("POST", `${ws(workspaceId)}/apps`, input),
      get: (workspaceId: string, appId: string) => get<AppDefinition>(`${ws(workspaceId)}/apps/${enc(appId)}`),
      update: (workspaceId: string, appId: string, input: UpdateAppInput) =>
        send<AppDefinition>("PATCH", `${ws(workspaceId)}/apps/${enc(appId)}`, input),
      getConfiguration: (workspaceId: string, appId: string) =>
        get<ClientConfiguration>(`${ws(workspaceId)}/apps/${enc(appId)}/configuration`),
      updateConfiguration: (workspaceId: string, appId: string, input: UpdateConfigurationInput) =>
        send<ClientConfiguration>("PUT", `${ws(workspaceId)}/apps/${enc(appId)}/configuration`, input),
      available: (workspaceId: string) => get<AvailableApp[]>(`${ws(workspaceId)}/available-apps`),
    },

    installations: {
      list: (workspaceId: string) => get<Installation[]>(`${ws(workspaceId)}/installations`),
      get: (workspaceId: string, installationId: string) =>
        get<Installation>(`${ws(workspaceId)}/installations/${enc(installationId)}`),
      update: (workspaceId: string, installationId: string, input: UpdateInstallationInput) =>
        send<Installation>("PATCH", `${ws(workspaceId)}/installations/${enc(installationId)}`, input),
      disconnect: (workspaceId: string, installationId: string) =>
        send<Installation>("POST", `${ws(workspaceId)}/installations/${enc(installationId)}/disconnect`),
      delete: (workspaceId: string, installationId: string) =>
        send<void>("DELETE", `${ws(workspaceId)}/installations/${enc(installationId)}`),
    },

    connectSessions: {
      list: (workspaceId: string) => get<ConnectSession[]>(`${ws(workspaceId)}/connect-sessions`),
      create: (workspaceId: string, input: CreateConnectSessionInput) =>
        send<ConnectSession>("POST", `${ws(workspaceId)}/connect-sessions`, input),
      get: (workspaceId: string, sessionId: string) =>
        get<ConnectSession>(`${ws(workspaceId)}/connect-sessions/${enc(sessionId)}`),
    },

    /**
     * Hosted Connect. Authorized by the session reference alone, so the page
     * can only act on the organization, workspace and app bound to it.
     */
    connect: {
      get: (sessionId: string) => get<ConnectSession>(`/v1/connect/sessions/${enc(sessionId)}`),
      submitCredentials: (sessionId: string, input: SubmitConnectCredentialsInput) =>
        send<ConnectResult>("POST", `/v1/connect/sessions/${enc(sessionId)}/credentials`, input),
      authorize: (sessionId: string) =>
        send<AuthorizeConnectResult>("POST", `/v1/connect/sessions/${enc(sessionId)}/authorize`),
      cancel: (sessionId: string) => send<ConnectSession>("POST", `/v1/connect/sessions/${enc(sessionId)}/cancel`),
    },

    agents: {
      list: (workspaceId: string) => get<Agent[]>(`${ws(workspaceId)}/agents`),
      create: (workspaceId: string, input: CreateAgentInput) => send<Agent>("POST", `${ws(workspaceId)}/agents`, input),
      get: (workspaceId: string, agentId: string) => get<Agent>(`${ws(workspaceId)}/agents/${enc(agentId)}`),
      update: (workspaceId: string, agentId: string, input: UpdateAgentInput) =>
        send<Agent>("PATCH", `${ws(workspaceId)}/agents/${enc(agentId)}`, input),
      delete: (workspaceId: string, agentId: string) =>
        send<void>("DELETE", `${ws(workspaceId)}/agents/${enc(agentId)}`),
    },

    grants: {
      list: (workspaceId: string, agentId: string) =>
        get<AgentGrant[]>(`${ws(workspaceId)}/agents/${enc(agentId)}/grants`),
      create: (workspaceId: string, agentId: string, input: CreateGrantInput) =>
        send<AgentGrant>("POST", `${ws(workspaceId)}/agents/${enc(agentId)}/grants`, input),
      update: (workspaceId: string, agentId: string, grantId: string, input: UpdateGrantInput) =>
        send<AgentGrant>("PATCH", `${ws(workspaceId)}/agents/${enc(agentId)}/grants/${enc(grantId)}`, input),
      delete: (workspaceId: string, agentId: string, grantId: string) =>
        send<void>("DELETE", `${ws(workspaceId)}/agents/${enc(agentId)}/grants/${enc(grantId)}`),
    },

    organizationRuns: {
      /** Runs across every workspace of the organization the caller can see. */
      list: (organizationId: string, query: OrganizationRunListQuery = {}) =>
        getPage<OrganizationRun>(`${org(organizationId)}/runs`, { ...query }),
    },

    runs: {
      list: (workspaceId: string, query: RunListQuery = {}) => getPage<Run>(`${ws(workspaceId)}/runs`, { ...query }),
      create: (workspaceId: string, agentId: string, input: CreateRunInput) =>
        send<Run>("POST", `${ws(workspaceId)}/agents/${enc(agentId)}/runs`, input),
      get: (workspaceId: string, runId: string) => get<Run>(`${ws(workspaceId)}/runs/${enc(runId)}`),
      events: (workspaceId: string, runId: string, afterSequence?: number) =>
        get<RunEvent[]>(`${ws(workspaceId)}/runs/${enc(runId)}/events`, {
          after: afterSequence,
        }),
      cancel: (workspaceId: string, runId: string) => send<Run>("POST", `${ws(workspaceId)}/runs/${enc(runId)}/cancel`),
      /**
       * Record the downstream user's decision. Call it from your backend with an
       * API key, after your product asked that user; Console sessions are refused.
       */
      resolveApproval: (workspaceId: string, runId: string, input: ResolveApprovalInput) =>
        send<Run>("POST", `${ws(workspaceId)}/runs/${enc(runId)}/approvals`, input),
    },

    connectSettings: {
      get: (organizationId: string, environment: Environment) =>
        get<ConnectSettings>(`${org(organizationId)}/connect-settings/${environment}`),
      update: (organizationId: string, environment: Environment, input: UpdateConnectSettingsInput) =>
        send<ConnectSettings>("PUT", `${org(organizationId)}/connect-settings/${environment}`, input),
    },

    apiKeys: {
      list: (organizationId: string) => get<ApiKey[]>(`${org(organizationId)}/api-keys`),
      create: (organizationId: string, input: CreateApiKeyInput) =>
        send<CreatedApiKey>("POST", `${org(organizationId)}/api-keys`, input),
      revoke: (organizationId: string, keyId: string) =>
        send<ApiKey>("POST", `${org(organizationId)}/api-keys/${enc(keyId)}/revoke`),
    },

    webhooks: {
      list: (organizationId: string) => get<WebhookEndpoint[]>(`${org(organizationId)}/webhooks`),
      create: (organizationId: string, input: CreateWebhookEndpointInput) =>
        send<CreatedWebhookEndpoint>("POST", `${org(organizationId)}/webhooks`, input),
      update: (organizationId: string, endpointId: string, input: UpdateWebhookEndpointInput) =>
        send<WebhookEndpoint>("PATCH", `${org(organizationId)}/webhooks/${enc(endpointId)}`, input),
      delete: (organizationId: string, endpointId: string) =>
        send<void>("DELETE", `${org(organizationId)}/webhooks/${enc(endpointId)}`),
      sendTest: (organizationId: string, endpointId: string) =>
        send<WebhookDelivery>("POST", `${org(organizationId)}/webhooks/${enc(endpointId)}/test`),
    },

    audit: {
      list: (organizationId: string, query: AuditListQuery = {}) =>
        getPage<AuditEvent>(`${org(organizationId)}/audit-events`, { ...query }),
    },
  };
}

export type ZupflowClient = ReturnType<typeof createClient>;
