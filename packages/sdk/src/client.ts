import type {
  AcceptToolChangeInput,
  Agent,
  AgentChannel,
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
  AvailableChannelProvider,
  ChannelApp,
  ChannelAppId,
  ChannelAppListQuery,
  ChannelAppManager,
  ChannelAppManagerId,
  ChannelConnection,
  ChannelConnectionId,
  ChannelConnectionListQuery,
  ChannelConversation,
  ChannelConversationId,
  ChannelConversationListQuery,
  ChannelMessage,
  ChannelMessageListQuery,
  ChannelProviderSettings,
  ChannelSettings,
  ChannelToolLevel,
  ClientConfiguration,
  ConnectResult,
  ConnectSession,
  ConnectSessionId,
  ConnectSettings,
  CreateAgentInput,
  CreateApiKeyInput,
  CreateAppInput,
  CreateChannelAppInput,
  CreateChannelAppManagerInput,
  CreateConnectSessionInput,
  CreatedApiKey,
  CreatedConnectSession,
  CreatedWebhookEndpoint,
  CreateGrantInput,
  CreateModelConnectionInput,
  CreateModelInput,
  CreateOrganizationInput,
  CreateRunInput,
  CreateWebhookEndpointInput,
  CreateWorkspaceInput,
  DefaultModel,
  Environment,
  GrantId,
  Installation,
  InstallationConnectResult,
  InstallationId,
  InstallationTool,
  Invitation,
  InviteMemberInput,
  Member,
  MemberId,
  Model,
  ModelConnection,
  ModelConnectionId,
  ModelConnectionListQuery,
  ModelId,
  ModelListQuery,
  Organization,
  OrganizationId,
  OrganizationRun,
  OrganizationRunListQuery,
  PageQuery,
  ProviderModel,
  ProviderModelListQuery,
  PutAgentChannelInput,
  RemoveMemberQuery,
  ReplaceModelConnectionKeyInput,
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
  UpdateChannelAppManagerInput,
  UpdateChannelConnectionInput,
  UpdateChannelProviderSettingsInput,
  UpdateChannelSettingsInput,
  UpdateChannelToolLevelsInput,
  UpdateConfigurationInput,
  UpdateConnectSettingsInput,
  UpdateDefaultModelInput,
  UpdateGrantInput,
  UpdateInstallationInput,
  UpdateMemberInput,
  UpdateModelConnectionInput,
  UpdateModelInput,
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
      /**
       * Answers what the session made: an installation for an app's session (the default type), an
       * app manager for a `channel_app_manager` session.
       */
      submitCredentials: <R extends ConnectResult = InstallationConnectResult>(
        reference: string,
        input: SubmitConnectCredentialsInput,
        options?: PostOptions,
      ) => post<R>(`/v1/connect/sessions/${enc(reference)}/credentials`, input, options),
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

    /**
     * The organization's model provider connections, each in one environment.
     * API keys are write-only: a connection shows whether its key is set and
     * its last four characters.
     */
    modelConnections: {
      list: (organizationId: OrganizationId, query: ModelConnectionListQuery = {}) =>
        get<ModelConnection[]>(`${org(organizationId)}/model-connections`, { ...query }),
      create: (organizationId: OrganizationId, input: CreateModelConnectionInput, options?: PostOptions) =>
        post<ModelConnection>(`${org(organizationId)}/model-connections`, input, options),
      get: (organizationId: OrganizationId, connectionId: ModelConnectionId) =>
        get<ModelConnection>(`${org(organizationId)}/model-connections/${enc(connectionId)}`),
      update: (organizationId: OrganizationId, connectionId: ModelConnectionId, input: UpdateModelConnectionInput) =>
        send<ModelConnection>("PATCH", `${org(organizationId)}/model-connections/${enc(connectionId)}`, input),
      /** Replace the key with a new version; the provider checks it first, and a refused key keeps the old one. */
      replaceKey: (
        organizationId: OrganizationId,
        connectionId: ModelConnectionId,
        input: ReplaceModelConnectionKeyInput,
      ) => send<ModelConnection>("PUT", `${org(organizationId)}/model-connections/${enc(connectionId)}/api-key`, input),
      delete: (organizationId: OrganizationId, connectionId: ModelConnectionId) =>
        send<void>("DELETE", `${org(organizationId)}/model-connections/${enc(connectionId)}`),
      /** Search the models the provider lists for this connection's key, a page at a time. */
      providerModels: (
        organizationId: OrganizationId,
        connectionId: ModelConnectionId,
        query: ProviderModelListQuery = {},
      ) =>
        getPage<ProviderModel>(`${org(organizationId)}/model-connections/${enc(connectionId)}/provider-models`, {
          ...query,
        }),
    },

    /** Models the organization added from its connections, and each environment's default. */
    models: {
      list: (organizationId: OrganizationId, query: ModelListQuery = {}) =>
        getPage<Model>(`${org(organizationId)}/models`, { ...query }),
      create: (organizationId: OrganizationId, input: CreateModelInput, options?: PostOptions) =>
        post<Model>(`${org(organizationId)}/models`, input, options),
      get: (organizationId: OrganizationId, modelId: ModelId) =>
        get<Model>(`${org(organizationId)}/models/${enc(modelId)}`),
      update: (organizationId: OrganizationId, modelId: ModelId, input: UpdateModelInput) =>
        send<Model>("PATCH", `${org(organizationId)}/models/${enc(modelId)}`, input),
      delete: (organizationId: OrganizationId, modelId: ModelId) =>
        send<void>("DELETE", `${org(organizationId)}/models/${enc(modelId)}`),
      getDefault: (organizationId: OrganizationId, environment: Environment) =>
        get<DefaultModel>(`${org(organizationId)}/default-models/${environment}`),
      setDefault: (organizationId: OrganizationId, environment: Environment, input: UpdateDefaultModelInput) =>
        send<DefaultModel>("PUT", `${org(organizationId)}/default-models/${environment}`, input),
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
      rollSecret: (organizationId: OrganizationId, endpointId: WebhookEndpointId, options?: PostOptions) =>
        post<CreatedWebhookEndpoint>(
          `${org(organizationId)}/webhooks/${enc(endpointId)}/roll-secret`,
          undefined,
          options,
        ),
      delete: (organizationId: OrganizationId, endpointId: WebhookEndpointId) =>
        send<void>("DELETE", `${org(organizationId)}/webhooks/${enc(endpointId)}`),
      sendTest: (organizationId: OrganizationId, endpointId: WebhookEndpointId, options?: PostOptions) =>
        post<WebhookDelivery>(`${org(organizationId)}/webhooks/${enc(endpointId)}/test`, undefined, options),
    },

    audit: {
      list: (organizationId: OrganizationId, query: AuditListQuery = {}) =>
        getPage<AuditEvent>(`${org(organizationId)}/audit-events`, { ...query }),
    },

    /** The channel providers this deployment reaches: the workspace links teams only there. */
    channelProviders: {
      list: (workspaceId: WorkspaceId) => get<AvailableChannelProvider[]>(`${ws(workspaceId)}/channel-providers`),
    },

    /**
     * Teams linked so Zupflow can make apps for the workspace's agents there.
     * Tokens are write-only: a manager shows whether its configuration token
     * is set and its last four characters.
     */
    channelAppManagers: {
      list: (workspaceId: WorkspaceId, query: PageQuery = {}) =>
        getPage<ChannelAppManager>(`${ws(workspaceId)}/channel-app-managers`, { ...query }),
      create: (workspaceId: WorkspaceId, input: CreateChannelAppManagerInput, options?: PostOptions) =>
        post<ChannelAppManager>(`${ws(workspaceId)}/channel-app-managers`, input, options),
      get: (workspaceId: WorkspaceId, managerId: ChannelAppManagerId) =>
        get<ChannelAppManager>(`${ws(workspaceId)}/channel-app-managers/${enc(managerId)}`),
      /** Replace both tokens; the manager is active again. */
      update: (workspaceId: WorkspaceId, managerId: ChannelAppManagerId, input: UpdateChannelAppManagerInput) =>
        send<ChannelAppManager>("PATCH", `${ws(workspaceId)}/channel-app-managers/${enc(managerId)}`, input),
      delete: (workspaceId: WorkspaceId, managerId: ChannelAppManagerId) =>
        send<void>("DELETE", `${ws(workspaceId)}/channel-app-managers/${enc(managerId)}`),
    },

    /** Each agent's own generated app in a linked team. */
    channelApps: {
      list: (workspaceId: WorkspaceId, query: ChannelAppListQuery = {}) =>
        getPage<ChannelApp>(`${ws(workspaceId)}/channel-apps`, { ...query }),
      /** Answers with the app `creating`; read it again to follow it to `awaiting_install`. */
      create: (workspaceId: WorkspaceId, input: CreateChannelAppInput, options?: PostOptions) =>
        post<ChannelApp>(`${ws(workspaceId)}/channel-apps`, input, options),
      get: (workspaceId: WorkspaceId, appId: ChannelAppId) =>
        get<ChannelApp>(`${ws(workspaceId)}/channel-apps/${enc(appId)}`),
      delete: (workspaceId: WorkspaceId, appId: ChannelAppId) =>
        send<void>("DELETE", `${ws(workspaceId)}/channel-apps/${enc(appId)}`),
    },

    /** Installed apps: the identity an agent acts as in a team, its conversations and messages. */
    channelConnections: {
      list: (workspaceId: WorkspaceId, query: ChannelConnectionListQuery = {}) =>
        getPage<ChannelConnection>(`${ws(workspaceId)}/channel-connections`, { ...query }),
      get: (workspaceId: WorkspaceId, connectionId: ChannelConnectionId) =>
        get<ChannelConnection>(`${ws(workspaceId)}/channel-connections/${enc(connectionId)}`),
      update: (workspaceId: WorkspaceId, connectionId: ChannelConnectionId, input: UpdateChannelConnectionInput) =>
        send<ChannelConnection>("PATCH", `${ws(workspaceId)}/channel-connections/${enc(connectionId)}`, input),
      delete: (workspaceId: WorkspaceId, connectionId: ChannelConnectionId) =>
        send<void>("DELETE", `${ws(workspaceId)}/channel-connections/${enc(connectionId)}`),
      conversations: (
        workspaceId: WorkspaceId,
        connectionId: ChannelConnectionId,
        query: ChannelConversationListQuery = {},
      ) =>
        getPage<ChannelConversation>(`${ws(workspaceId)}/channel-connections/${enc(connectionId)}/conversations`, {
          ...query,
        }),
      messages: (
        workspaceId: WorkspaceId,
        connectionId: ChannelConnectionId,
        conversationId: ChannelConversationId,
        query: ChannelMessageListQuery = {},
      ) =>
        getPage<ChannelMessage>(
          `${ws(workspaceId)}/channel-connections/${enc(connectionId)}/conversations/${enc(conversationId)}/messages`,
          { ...query },
        ),
    },

    /** What an agent does on each of its own channel connections. */
    agentChannels: {
      list: (workspaceId: WorkspaceId, agentId: AgentId) =>
        get<AgentChannel[]>(`${ws(workspaceId)}/agents/${enc(agentId)}/channels`),
      put: (
        workspaceId: WorkspaceId,
        agentId: AgentId,
        connectionId: ChannelConnectionId,
        input: PutAgentChannelInput,
      ) => send<AgentChannel>("PUT", `${ws(workspaceId)}/agents/${enc(agentId)}/channels/${enc(connectionId)}`, input),
      delete: (workspaceId: WorkspaceId, agentId: AgentId, connectionId: ChannelConnectionId) =>
        send<void>("DELETE", `${ws(workspaceId)}/agents/${enc(agentId)}/channels/${enc(connectionId)}`),
    },

    /**
     * The organization's channel tool levels, conversation breaker, message
     * retention and the providers its workspaces may link teams of.
     */
    channelSettings: {
      get: (organizationId: OrganizationId) => get<ChannelSettings>(`${org(organizationId)}/channel-settings`),
      update: (organizationId: OrganizationId, input: UpdateChannelSettingsInput) =>
        send<ChannelSettings>("PUT", `${org(organizationId)}/channel-settings`, input),
      toolLevels: (organizationId: OrganizationId) =>
        get<ChannelToolLevel[]>(`${org(organizationId)}/channel-tool-levels`),
      setToolLevels: (organizationId: OrganizationId, input: UpdateChannelToolLevelsInput) =>
        send<ChannelToolLevel[]>("PUT", `${org(organizationId)}/channel-tool-levels`, input),
      /** Every provider the server ships, whether this deployment reaches it, and whether the organization allows it. */
      providers: (organizationId: OrganizationId) =>
        get<ChannelProviderSettings>(`${org(organizationId)}/channel-provider-settings`),
      /** `allowed_providers` null allows every provider, including later ones; a list, only those. */
      setProviders: (organizationId: OrganizationId, input: UpdateChannelProviderSettingsInput) =>
        send<ChannelProviderSettings>("PUT", `${org(organizationId)}/channel-provider-settings`, input),
    },
  };
}

export type ZupflowClient = ReturnType<typeof createClient>;
