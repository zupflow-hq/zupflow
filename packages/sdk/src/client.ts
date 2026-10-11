import type {
  AcceptToolChangeInput,
  Agent,
  AgentGrant,
  AgentId,
  ApiKey,
  ApiKeyId,
  AppDefinition,
  AppId,
  AppInstallationRef,
  AppToolLevel,
  AuditEvent,
  AuditListQuery,
  AuthorizeConnectResult,
  AvailableApp,
  ClientConfiguration,
  ConnectResult,
  ConnectSession,
  ConnectSessionId,
  ConnectSettings,
  CreateAgentInput,
  CreateApiKeyInput,
  CreateAppInput,
  CreateConnectSessionInput,
  CreatedApiKey,
  CreatedConnectSession,
  CreatedWebhookEndpoint,
  CreateGrantInput,
  CreateOrganizationInput,
  CreateRunInput,
  CreateWebhookEndpointInput,
  CreateWorkspaceInput,
  Environment,
  GrantId,
  Installation,
  InstallationId,
  InstallationTool,
  Invitation,
  InviteMemberInput,
  Member,
  MemberId,
  ModelOption,
  Organization,
  OrganizationId,
  OrganizationRun,
  OrganizationRunListQuery,
  PageQuery,
  RemoveMemberQuery,
  ResolveApprovalInput,
  Run,
  RunEvent,
  RunId,
  RunListQuery,
  SignInSettings,
  SubmitConnectCredentialsInput,
  ToolPolicy,
  UpdateAgentInput,
  UpdateAppInput,
  UpdateAppToolLevelsInput,
  UpdateConfigurationInput,
  UpdateConnectSettingsInput,
  UpdateGrantInput,
  UpdateInstallationInput,
  UpdateMemberInput,
  UpdateOrganizationInput,
  UpdateToolPolicyInput,
  UpdateWebhookEndpointInput,
  UpdateWorkspaceInput,
  UsageReport,
  Viewer,
  WebhookDelivery,
  WebhookDeliveryAttempt,
  WebhookDeliveryDetail,
  WebhookDeliveryId,
  WebhookDeliveryListQuery,
  WebhookDeliverySummary,
  WebhookEndpoint,
  WebhookEndpointId,
  Workspace,
  WorkspaceId,
  WorkspaceListQuery,
} from "@zupflow/contract";
import type { Page } from "./page";
import type { QueryValue, Transport } from "./transport";

/** Options every `POST` takes. */
export interface PostOptions {
  /**
   * Your own `Idempotency-Key`, to make a retry safe across process restarts:
   * store it with the work before sending, and send the same key again. By
   * default the SDK makes a fresh key per call and reuses it on its own retries.
   */
  idempotencyKey?: string;
}

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
  const send = async <T>(
    method: "PUT" | "PATCH" | "DELETE",
    path: string,
    body?: unknown,
    query?: Record<string, QueryValue>,
  ) => (await transport.request<T>({ method, path, body, query })).data;
  const post = async <T>(path: string, body: unknown, options?: PostOptions) =>
    (await transport.request<T>({ method: "POST", path, body, idempotencyKey: options?.idempotencyKey })).data;

  const org = (organizationId: OrganizationId) => `/v1/organizations/${enc(organizationId)}`;
  const ws = (workspaceId: WorkspaceId) => `/v1/workspaces/${enc(workspaceId)}`;

  return {
    /** How the Console signs in to this API. Needs no credential. */
    signIn: {
      settings: () => get<SignInSettings>("/v1/sign-in-settings"),
    },

    me: {
      get: () => get<Viewer>("/v1/me"),
    },

    /** The signed-in person's own invitations. Accepting one is the only way into an organization. */
    invitations: {
      list: () => get<Invitation[]>("/v1/me/invitations"),
      accept: (invitationId: MemberId, options?: PostOptions) =>
        post<Organization>(`/v1/me/invitations/${enc(invitationId)}/accept`, undefined, options),
      decline: (invitationId: MemberId, options?: PostOptions) =>
        post<void>(`/v1/me/invitations/${enc(invitationId)}/decline`, undefined, options),
    },

    models: {
      list: () => get<ModelOption[]>("/v1/models"),
    },

    organizations: {
      create: (input: CreateOrganizationInput, options?: PostOptions) =>
        post<Organization>("/v1/organizations", input, options),
      get: (organizationId: OrganizationId) => get<Organization>(org(organizationId)),
      update: (organizationId: OrganizationId, input: UpdateOrganizationInput) =>
        send<Organization>("PATCH", org(organizationId), input),
      usage: (organizationId: OrganizationId, environment: Environment, days = 14) =>
        get<UsageReport>(`${org(organizationId)}/usage`, { environment, days }),
    },

    members: {
      list: (organizationId: OrganizationId) => get<Member[]>(`${org(organizationId)}/members`),
      invite: (organizationId: OrganizationId, input: InviteMemberInput, options?: PostOptions) =>
        post<Member>(`${org(organizationId)}/members`, input, options),
      update: (organizationId: OrganizationId, memberId: MemberId, input: UpdateMemberInput) =>
        send<Member>("PATCH", `${org(organizationId)}/members/${enc(memberId)}`, input),
      /** With `{ status: "invited" }`, withdraws an invitation only, never a member who has just accepted it. */
      remove: (organizationId: OrganizationId, memberId: MemberId, query: RemoveMemberQuery = {}) =>
        send<void>("DELETE", `${org(organizationId)}/members/${enc(memberId)}`, undefined, { ...query }),
    },

    workspaces: {
      list: (organizationId: OrganizationId, query: WorkspaceListQuery = {}) =>
        getPage<Workspace>(`${org(organizationId)}/workspaces`, { ...query }),
      create: (organizationId: OrganizationId, input: CreateWorkspaceInput, options?: PostOptions) =>
        post<Workspace>(`${org(organizationId)}/workspaces`, input, options),
      get: (workspaceId: WorkspaceId) => get<Workspace>(ws(workspaceId)),
      update: (workspaceId: WorkspaceId, input: UpdateWorkspaceInput) =>
        send<Workspace>("PATCH", ws(workspaceId), input),
      delete: (workspaceId: WorkspaceId) => send<void>("DELETE", ws(workspaceId)),
    },

    organizationApps: {
      list: (organizationId: OrganizationId) => get<AppDefinition[]>(`${org(organizationId)}/apps`),
      create: (organizationId: OrganizationId, input: CreateAppInput, options?: PostOptions) =>
        post<AppDefinition>(`${org(organizationId)}/apps`, input, options),
      get: (organizationId: OrganizationId, appId: AppId) =>
        get<AppDefinition>(`${org(organizationId)}/apps/${enc(appId)}`),
      update: (organizationId: OrganizationId, appId: AppId, input: UpdateAppInput) =>
        send<AppDefinition>("PATCH", `${org(organizationId)}/apps/${enc(appId)}`, input),
      getConfiguration: (organizationId: OrganizationId, appId: AppId, environment: Environment) =>
        get<ClientConfiguration>(`${org(organizationId)}/apps/${enc(appId)}/environments/${environment}/configuration`),
      updateConfiguration: (
        organizationId: OrganizationId,
        appId: AppId,
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
        organizationId: OrganizationId,
        appId: AppId,
        query: { environment?: Environment; cursor?: string; limit?: number } = {},
      ) => getPage<AppInstallationRef>(`${org(organizationId)}/apps/${enc(appId)}/installations`, { ...query }),
    },

    /** The organization's tool policy, and its levels for each catalog app's tools. */
    toolPolicy: {
      get: (organizationId: OrganizationId) => get<ToolPolicy>(`${org(organizationId)}/tool-policy`),
      update: (organizationId: OrganizationId, input: UpdateToolPolicyInput) =>
        send<ToolPolicy>("PUT", `${org(organizationId)}/tool-policy`, input),
      levels: (organizationId: OrganizationId, appId: AppId, query: PageQuery = {}) =>
        getPage<AppToolLevel>(`${org(organizationId)}/apps/${enc(appId)}/tool-levels`, { ...query }),
      setLevels: (organizationId: OrganizationId, appId: AppId, input: UpdateAppToolLevelsInput) =>
        send<AppToolLevel[]>("PATCH", `${org(organizationId)}/apps/${enc(appId)}/tool-levels`, input),
    },

    workspaceApps: {
      list: (workspaceId: WorkspaceId) => get<AppDefinition[]>(`${ws(workspaceId)}/apps`),
      create: (workspaceId: WorkspaceId, input: CreateAppInput, options?: PostOptions) =>
        post<AppDefinition>(`${ws(workspaceId)}/apps`, input, options),
      get: (workspaceId: WorkspaceId, appId: AppId) => get<AppDefinition>(`${ws(workspaceId)}/apps/${enc(appId)}`),
      update: (workspaceId: WorkspaceId, appId: AppId, input: UpdateAppInput) =>
        send<AppDefinition>("PATCH", `${ws(workspaceId)}/apps/${enc(appId)}`, input),
      getConfiguration: (workspaceId: WorkspaceId, appId: AppId) =>
        get<ClientConfiguration>(`${ws(workspaceId)}/apps/${enc(appId)}/configuration`),
      updateConfiguration: (workspaceId: WorkspaceId, appId: AppId, input: UpdateConfigurationInput) =>
        send<ClientConfiguration>("PUT", `${ws(workspaceId)}/apps/${enc(appId)}/configuration`, input),
      available: (workspaceId: WorkspaceId) => get<AvailableApp[]>(`${ws(workspaceId)}/available-apps`),
    },

    installations: {
      list: (workspaceId: WorkspaceId) => get<Installation[]>(`${ws(workspaceId)}/installations`),
      get: (workspaceId: WorkspaceId, installationId: InstallationId) =>
        get<Installation>(`${ws(workspaceId)}/installations/${enc(installationId)}`),
      update: (workspaceId: WorkspaceId, installationId: InstallationId, input: UpdateInstallationInput) =>
        send<Installation>("PATCH", `${ws(workspaceId)}/installations/${enc(installationId)}`, input),
      disconnect: (workspaceId: WorkspaceId, installationId: InstallationId, options?: PostOptions) =>
        post<Installation>(`${ws(workspaceId)}/installations/${enc(installationId)}/disconnect`, undefined, options),
      delete: (workspaceId: WorkspaceId, installationId: InstallationId) =>
        send<void>("DELETE", `${ws(workspaceId)}/installations/${enc(installationId)}`),
      /** A page of the tools its app's MCP server listed, by name; a changed definition stays unreviewed beside the pinned one. */
      tools: (workspaceId: WorkspaceId, installationId: InstallationId, query: PageQuery = {}) =>
        getPage<InstallationTool>(`${ws(workspaceId)}/installations/${enc(installationId)}/tools`, { ...query }),
      /** Ask the app's MCP server for the installation's tools again; the listing arrives in the background. */
      refreshTools: (workspaceId: WorkspaceId, installationId: InstallationId, options?: PostOptions) =>
        post<Installation>(`${ws(workspaceId)}/installations/${enc(installationId)}/refresh-tools`, undefined, options),
      /** Accept a changed tool's latest definition, named by the hash you reviewed: it becomes the pinned one. */
      acceptToolChange: (
        workspaceId: WorkspaceId,
        installationId: InstallationId,
        input: AcceptToolChangeInput,
        options?: PostOptions,
      ) =>
        post<InstallationTool>(
          `${ws(workspaceId)}/installations/${enc(installationId)}/accept-tool-change`,
          input,
          options,
        ),
    },

    connectSessions: {
      list: (workspaceId: WorkspaceId) => get<ConnectSession[]>(`${ws(workspaceId)}/connect-sessions`),
      create: (workspaceId: WorkspaceId, input: CreateConnectSessionInput, options?: PostOptions) =>
        post<CreatedConnectSession>(`${ws(workspaceId)}/connect-sessions`, input, options),
      get: (workspaceId: WorkspaceId, sessionId: ConnectSessionId) =>
        get<ConnectSession>(`${ws(workspaceId)}/connect-sessions/${enc(sessionId)}`),
    },

    /**
     * Hosted Connect. Authorized by the session's reference alone: the random
     * token at the end of the session's `url`, never its ID. The page can only
     * act on the organization, workspace and app bound to it.
     */
    connect: {
      get: (reference: string) => get<ConnectSession>(`/v1/connect/sessions/${enc(reference)}`),
      submitCredentials: (reference: string, input: SubmitConnectCredentialsInput, options?: PostOptions) =>
        post<ConnectResult>(`/v1/connect/sessions/${enc(reference)}/credentials`, input, options),
      authorize: (reference: string, options?: PostOptions) =>
        post<AuthorizeConnectResult>(`/v1/connect/sessions/${enc(reference)}/authorize`, undefined, options),
      cancel: (reference: string, options?: PostOptions) =>
        post<ConnectSession>(`/v1/connect/sessions/${enc(reference)}/cancel`, undefined, options),
    },

    agents: {
      list: (workspaceId: WorkspaceId) => get<Agent[]>(`${ws(workspaceId)}/agents`),
      create: (workspaceId: WorkspaceId, input: CreateAgentInput, options?: PostOptions) =>
        post<Agent>(`${ws(workspaceId)}/agents`, input, options),
      get: (workspaceId: WorkspaceId, agentId: AgentId) => get<Agent>(`${ws(workspaceId)}/agents/${enc(agentId)}`),
      update: (workspaceId: WorkspaceId, agentId: AgentId, input: UpdateAgentInput) =>
        send<Agent>("PATCH", `${ws(workspaceId)}/agents/${enc(agentId)}`, input),
      delete: (workspaceId: WorkspaceId, agentId: AgentId) =>
        send<void>("DELETE", `${ws(workspaceId)}/agents/${enc(agentId)}`),
    },

    grants: {
      list: (workspaceId: WorkspaceId, agentId: AgentId) =>
        get<AgentGrant[]>(`${ws(workspaceId)}/agents/${enc(agentId)}/grants`),
      create: (workspaceId: WorkspaceId, agentId: AgentId, input: CreateGrantInput, options?: PostOptions) =>
        post<AgentGrant>(`${ws(workspaceId)}/agents/${enc(agentId)}/grants`, input, options),
      update: (workspaceId: WorkspaceId, agentId: AgentId, grantId: GrantId, input: UpdateGrantInput) =>
        send<AgentGrant>("PATCH", `${ws(workspaceId)}/agents/${enc(agentId)}/grants/${enc(grantId)}`, input),
      delete: (workspaceId: WorkspaceId, agentId: AgentId, grantId: GrantId) =>
        send<void>("DELETE", `${ws(workspaceId)}/agents/${enc(agentId)}/grants/${enc(grantId)}`),
    },

    organizationRuns: {
      /** Runs across every workspace of the organization the caller can see. */
      list: (organizationId: OrganizationId, query: OrganizationRunListQuery = {}) =>
        getPage<OrganizationRun>(`${org(organizationId)}/runs`, { ...query }),
    },

    runs: {
      list: (workspaceId: WorkspaceId, query: RunListQuery = {}) =>
        getPage<Run>(`${ws(workspaceId)}/runs`, { ...query }),
      create: (workspaceId: WorkspaceId, agentId: AgentId, input: CreateRunInput, options?: PostOptions) =>
        post<Run>(`${ws(workspaceId)}/agents/${enc(agentId)}/runs`, input, options),
      get: (workspaceId: WorkspaceId, runId: RunId) => get<Run>(`${ws(workspaceId)}/runs/${enc(runId)}`),
      events: (workspaceId: WorkspaceId, runId: RunId, afterSequence?: number) =>
        get<RunEvent[]>(`${ws(workspaceId)}/runs/${enc(runId)}/events`, {
          after: afterSequence,
        }),
      cancel: (workspaceId: WorkspaceId, runId: RunId, options?: PostOptions) =>
        post<Run>(`${ws(workspaceId)}/runs/${enc(runId)}/cancel`, undefined, options),
      /**
       * Record a decision your product made, however it decides. Call it from
       * your backend with an API key; Console sessions are refused.
       */
      resolveApproval: (workspaceId: WorkspaceId, runId: RunId, input: ResolveApprovalInput, options?: PostOptions) =>
        post<Run>(`${ws(workspaceId)}/runs/${enc(runId)}/approvals`, input, options),
    },

    connectSettings: {
      get: (organizationId: OrganizationId, environment: Environment) =>
        get<ConnectSettings>(`${org(organizationId)}/connect-settings/${environment}`),
      update: (organizationId: OrganizationId, environment: Environment, input: UpdateConnectSettingsInput) =>
        send<ConnectSettings>("PUT", `${org(organizationId)}/connect-settings/${environment}`, input),
    },

    apiKeys: {
      list: (organizationId: OrganizationId) => get<ApiKey[]>(`${org(organizationId)}/api-keys`),
      create: (organizationId: OrganizationId, input: CreateApiKeyInput, options?: PostOptions) =>
        post<CreatedApiKey>(`${org(organizationId)}/api-keys`, input, options),
      revoke: (organizationId: OrganizationId, keyId: ApiKeyId, options?: PostOptions) =>
        post<ApiKey>(`${org(organizationId)}/api-keys/${enc(keyId)}/revoke`, undefined, options),
    },

    webhooks: {
      list: (organizationId: OrganizationId) => get<WebhookEndpoint[]>(`${org(organizationId)}/webhooks`),
      deliveries: (
        organizationId: OrganizationId,
        endpointId: WebhookEndpointId,
        query: WebhookDeliveryListQuery = {},
      ) =>
        getPage<WebhookDeliverySummary>(`${org(organizationId)}/webhooks/${enc(endpointId)}/deliveries`, { ...query }),
      delivery: (organizationId: OrganizationId, endpointId: WebhookEndpointId, deliveryId: WebhookDeliveryId) =>
        get<WebhookDeliveryDetail>(`${org(organizationId)}/webhooks/${enc(endpointId)}/deliveries/${enc(deliveryId)}`),
      deliveryAttempts: (
        organizationId: OrganizationId,
        endpointId: WebhookEndpointId,
        deliveryId: WebhookDeliveryId,
        query: PageQuery = {},
      ) =>
        getPage<WebhookDeliveryAttempt>(
          `${org(organizationId)}/webhooks/${enc(endpointId)}/deliveries/${enc(deliveryId)}/attempts`,
          { ...query },
        ),
      resendDelivery: (
        organizationId: OrganizationId,
        endpointId: WebhookEndpointId,
        deliveryId: WebhookDeliveryId,
        options?: PostOptions,
      ) =>
        post<WebhookDeliverySummary>(
          `${org(organizationId)}/webhooks/${enc(endpointId)}/deliveries/${enc(deliveryId)}/resend`,
          undefined,
          options,
        ),
      create: (organizationId: OrganizationId, input: CreateWebhookEndpointInput, options?: PostOptions) =>
        post<CreatedWebhookEndpoint>(`${org(organizationId)}/webhooks`, input, options),
      update: (organizationId: OrganizationId, endpointId: WebhookEndpointId, input: UpdateWebhookEndpointInput) =>
        send<WebhookEndpoint>("PATCH", `${org(organizationId)}/webhooks/${enc(endpointId)}`, input),
      delete: (organizationId: OrganizationId, endpointId: WebhookEndpointId) =>
        send<void>("DELETE", `${org(organizationId)}/webhooks/${enc(endpointId)}`),
      sendTest: (organizationId: OrganizationId, endpointId: WebhookEndpointId, options?: PostOptions) =>
        post<WebhookDelivery>(`${org(organizationId)}/webhooks/${enc(endpointId)}/test`, undefined, options),
    },

    audit: {
      list: (organizationId: OrganizationId, query: AuditListQuery = {}) =>
        getPage<AuditEvent>(`${org(organizationId)}/audit-events`, { ...query }),
    },
  };
}

export type ZupflowClient = ReturnType<typeof createClient>;
