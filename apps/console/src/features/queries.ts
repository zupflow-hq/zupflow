/**
 * Query and mutation hooks. Pages read through these; they never call the SDK
 * directly. Each hook owns its cache key and what it invalidates.
 */
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  type CreateAgentInput,
  type CreateApiKeyInput,
  type CreateAppInput,
  type CreateConnectSessionInput,
  type CreateGrantInput,
  type CreateOrganizationInput,
  type CreateRunInput,
  type CreateWebhookEndpointInput,
  type CreateWorkspaceInput,
  type Environment,
  type InviteMemberInput,
  type MemberRole,
  type OrganizationRunListQuery,
  type RunEvent,
  type RunListQuery,
  TERMINAL_RUN_STATUSES,
  type UpdateAgentInput,
  type UpdateAppInput,
  type UpdateConfigurationInput,
  type UpdateConnectSettingsInput,
  type UpdateGrantInput,
  type UpdateOrganizationInput,
  type UpdateWebhookEndpointInput,
  type UpdateWorkspaceInput,
  type WorkspaceListQuery,
} from "@zupflow/sdk";
import { api } from "@/lib/api";
import { qk } from "@/lib/query-keys";

// ---------------------------------------------------------------------------
// Viewer and organizations

export const viewerQuery = {
  queryKey: qk.viewer,
  queryFn: () => api.me.get(),
  staleTime: 30_000,
};

export function useViewer() {
  return useQuery(viewerQuery);
}

export function useModels() {
  return useQuery({ queryKey: qk.models, queryFn: () => api.models.list(), staleTime: Infinity });
}

export function useCreateOrganization() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateOrganizationInput) => api.organizations.create(input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.viewer }),
  });
}

export function useUpdateOrganization(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateOrganizationInput) => api.organizations.update(orgId, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.viewer }),
  });
}

export function useUsage(orgId: string, env: Environment) {
  return useQuery({
    queryKey: qk.usage(orgId, env),
    queryFn: () => api.organizations.usage(orgId, env, 14),
  });
}

// ---------------------------------------------------------------------------
// Members

export function useMembers(orgId: string) {
  return useQuery({ queryKey: qk.members(orgId), queryFn: () => api.members.list(orgId) });
}

export function useInviteMember(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: InviteMemberInput) => api.members.invite(orgId, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.members(orgId) }),
  });
}

export function useUpdateMember(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ memberId, role }: { memberId: string; role: MemberRole }) =>
      api.members.update(orgId, memberId, { role }),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.members(orgId) }),
  });
}

export function useRemoveMember(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (memberId: string) => api.members.remove(orgId, memberId),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.members(orgId) }),
  });
}

// ---------------------------------------------------------------------------
// Workspaces

/** Workspaces, a page at a time. Organizations have thousands; never load them all. */
export function useWorkspaceList(orgId: string, query: Omit<WorkspaceListQuery, "cursor">) {
  return useInfiniteQuery({
    queryKey: [...qk.workspaces(orgId), "list", query],
    queryFn: ({ pageParam }) => api.workspaces.list(orgId, { ...query, cursor: pageParam ?? undefined }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
    placeholderData: (previous) => previous,
  });
}

/** One page of workspaces: summaries, counts and pickers. */
export function useWorkspacePage(orgId: string, query: Omit<WorkspaceListQuery, "cursor">, enabled = true) {
  return useQuery({
    queryKey: [...qk.workspaces(orgId), "page", query],
    queryFn: () => api.workspaces.list(orgId, query),
    placeholderData: (previous) => previous,
    enabled,
  });
}

export const workspaceQuery = (wsId: string) => ({
  queryKey: qk.workspace(wsId),
  queryFn: () => api.workspaces.get(wsId),
});

export function useWorkspace(wsId: string) {
  return useQuery({ ...workspaceQuery(wsId), enabled: Boolean(wsId) });
}

export function useCreateWorkspace(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateWorkspaceInput) => api.workspaces.create(orgId, input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.workspaces(orgId) });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

export function useUpdateWorkspace(orgId: string, wsId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateWorkspaceInput) => api.workspaces.update(wsId, input),
    onSuccess: (workspace) => {
      qc.setQueryData(qk.workspace(wsId), workspace);
      qc.invalidateQueries({ queryKey: qk.workspaces(orgId) });
    },
  });
}

export function useDeleteWorkspace(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (wsId: string) => api.workspaces.delete(wsId),
    onSuccess: (_, wsId) => {
      qc.removeQueries({ queryKey: qk.workspace(wsId) });
      qc.invalidateQueries({ queryKey: qk.org(orgId) });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

// ---------------------------------------------------------------------------
// Apps

export function useOrgApps(orgId: string) {
  return useQuery({ queryKey: qk.orgApps(orgId), queryFn: () => api.organizationApps.list(orgId) });
}

export function useOrgApp(orgId: string, appId: string) {
  return useQuery({
    queryKey: qk.orgApp(orgId, appId),
    queryFn: () => api.organizationApps.get(orgId, appId),
    enabled: Boolean(appId),
  });
}

export function useCreateOrgApp(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateAppInput) => api.organizationApps.create(orgId, input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.orgApps(orgId) });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

export function useUpdateOrgApp(orgId: string, appId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateAppInput) => api.organizationApps.update(orgId, appId, input),
    onSuccess: (app) => {
      qc.setQueryData(qk.orgApp(orgId, appId), app);
      qc.invalidateQueries({ queryKey: qk.orgApps(orgId) });
    },
  });
}

export function useOrgAppConfiguration(orgId: string, appId: string, env: Environment) {
  return useQuery({
    queryKey: qk.orgAppConfig(orgId, appId, env),
    queryFn: () => api.organizationApps.getConfiguration(orgId, appId, env),
  });
}

export function useUpdateOrgAppConfiguration(orgId: string, appId: string, env: Environment) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateConfigurationInput) => api.organizationApps.updateConfiguration(orgId, appId, env, input),
    onSuccess: (configuration) => {
      qc.setQueryData(qk.orgAppConfig(orgId, appId, env), configuration);
      qc.invalidateQueries({ queryKey: qk.orgApps(orgId) });
      qc.invalidateQueries({ queryKey: qk.orgApp(orgId, appId), exact: true });
      qc.invalidateQueries({ queryKey: ["ws"] });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

export function useOrgAppInstallations(orgId: string, appId: string, environment: Environment) {
  return useInfiniteQuery({
    queryKey: [...qk.orgAppInstallations(orgId, appId), environment],
    queryFn: ({ pageParam }) =>
      api.organizationApps.installations(orgId, appId, { environment, cursor: pageParam ?? undefined }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
  });
}

export function useWorkspaceApps(wsId: string) {
  return useQuery({ queryKey: qk.workspaceApps(wsId), queryFn: () => api.workspaceApps.list(wsId) });
}

export function useWorkspaceApp(wsId: string, appId: string) {
  return useQuery({
    queryKey: qk.workspaceApp(wsId, appId),
    queryFn: () => api.workspaceApps.get(wsId, appId),
    enabled: Boolean(appId),
  });
}

export function useCreateWorkspaceApp(wsId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateAppInput) => api.workspaceApps.create(wsId, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.workspace(wsId) }),
  });
}

export function useUpdateWorkspaceApp(wsId: string, appId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateAppInput) => api.workspaceApps.update(wsId, appId, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.workspace(wsId) }),
  });
}

export function useWorkspaceAppConfiguration(wsId: string, appId: string) {
  return useQuery({
    queryKey: qk.workspaceAppConfig(wsId, appId),
    queryFn: () => api.workspaceApps.getConfiguration(wsId, appId),
  });
}

export function useUpdateWorkspaceAppConfiguration(wsId: string, appId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateConfigurationInput) => api.workspaceApps.updateConfiguration(wsId, appId, input),
    onSuccess: (configuration) => {
      qc.setQueryData(qk.workspaceAppConfig(wsId, appId), configuration);
      qc.invalidateQueries({ queryKey: qk.workspace(wsId) });
    },
  });
}

export function useAvailableApps(wsId: string) {
  return useQuery({
    queryKey: qk.availableApps(wsId),
    queryFn: () => api.workspaceApps.available(wsId),
  });
}

// ---------------------------------------------------------------------------
// Installations and Connect

export function useInstallations(wsId: string) {
  return useQuery({ queryKey: qk.installations(wsId), queryFn: () => api.installations.list(wsId) });
}

export function useInstallation(wsId: string, id: string) {
  return useQuery({
    queryKey: qk.installation(wsId, id),
    queryFn: () => api.installations.get(wsId, id),
  });
}

export function useRenameInstallation(wsId: string, id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (label: string) => api.installations.update(wsId, id, { label }),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.workspace(wsId) }),
  });
}

export function useDisconnectInstallation(wsId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.installations.disconnect(wsId, id),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.workspace(wsId) }),
  });
}

export function useDeleteInstallation(wsId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.installations.delete(wsId, id),
    onSuccess: (_, id) => {
      qc.removeQueries({ queryKey: qk.installation(wsId, id) });
      qc.invalidateQueries({ queryKey: qk.workspace(wsId) });
    },
  });
}

export function useConnectSessions(wsId: string) {
  return useQuery({
    queryKey: qk.connectSessions(wsId),
    queryFn: () => api.connectSessions.list(wsId),
  });
}

export function useCreateConnectSession(wsId: string) {
  return useMutation({
    mutationFn: (input: CreateConnectSessionInput) => api.connectSessions.create(wsId, input),
  });
}

/** Polls a Connect session until it leaves `pending`. */
export function useWatchedConnectSession(wsId: string, sessionId: string | null) {
  return useQuery({
    queryKey: qk.connectSession(wsId, sessionId ?? "none"),
    queryFn: () => api.connectSessions.get(wsId, sessionId ?? ""),
    enabled: sessionId !== null,
    refetchInterval: (query) => (query.state.data?.status === "pending" ? 1500 : false),
  });
}

export function useHostedConnectSession(sessionId: string) {
  return useQuery({
    queryKey: qk.hostedConnect(sessionId),
    queryFn: () => api.connect.get(sessionId),
    retry: false,
    // The provider round-trip changes this session outside the page.
    staleTime: 0,
  });
}

// ---------------------------------------------------------------------------
// Agents and grants

export function useAgents(wsId: string) {
  return useQuery({ queryKey: qk.agents(wsId), queryFn: () => api.agents.list(wsId) });
}

export function useAgent(wsId: string, id: string) {
  return useQuery({ queryKey: qk.agent(wsId, id), queryFn: () => api.agents.get(wsId, id) });
}

export function useCreateAgent(wsId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateAgentInput) => api.agents.create(wsId, input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.workspace(wsId) });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

export function useUpdateAgent(wsId: string, id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateAgentInput) => api.agents.update(wsId, id, input),
    onSuccess: (agent) => {
      qc.setQueryData(qk.agent(wsId, id), agent);
      qc.invalidateQueries({ queryKey: qk.agents(wsId), exact: true });
    },
  });
}

export function useDeleteAgent(wsId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.agents.delete(wsId, id),
    onSuccess: (_, id) => {
      qc.removeQueries({ queryKey: qk.agent(wsId, id) });
      qc.invalidateQueries({ queryKey: qk.workspace(wsId) });
    },
  });
}

export function useGrants(wsId: string, agentId: string) {
  return useQuery({
    queryKey: qk.grants(wsId, agentId),
    queryFn: () => api.grants.list(wsId, agentId),
  });
}

function useInvalidateGrants(wsId: string, agentId: string) {
  const qc = useQueryClient();
  return () => {
    qc.invalidateQueries({ queryKey: qk.grants(wsId, agentId) });
    qc.invalidateQueries({ queryKey: qk.agent(wsId, agentId), exact: true });
    qc.invalidateQueries({ queryKey: qk.agents(wsId), exact: true });
    qc.invalidateQueries({ queryKey: qk.installations(wsId) });
  };
}

export function useCreateGrant(wsId: string, agentId: string) {
  const invalidate = useInvalidateGrants(wsId, agentId);
  return useMutation({
    mutationFn: (input: CreateGrantInput) => api.grants.create(wsId, agentId, input),
    onSuccess: invalidate,
  });
}

export function useUpdateGrant(wsId: string, agentId: string) {
  const invalidate = useInvalidateGrants(wsId, agentId);
  return useMutation({
    mutationFn: ({ grantId, ...input }: UpdateGrantInput & { grantId: string }) =>
      api.grants.update(wsId, agentId, grantId, input),
    onSuccess: invalidate,
  });
}

export function useDeleteGrant(wsId: string, agentId: string) {
  const invalidate = useInvalidateGrants(wsId, agentId);
  return useMutation({
    mutationFn: (grantId: string) => api.grants.delete(wsId, agentId, grantId),
    onSuccess: invalidate,
  });
}

// ---------------------------------------------------------------------------
// Runs

export function useRuns(wsId: string, query: Omit<RunListQuery, "cursor"> = {}) {
  return useInfiniteQuery({
    queryKey: qk.runs(wsId, query),
    queryFn: ({ pageParam }) => api.runs.list(wsId, { ...query, cursor: pageParam ?? undefined }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
    refetchInterval: (q) =>
      q.state.data?.pages[0]?.items.some((r) => !TERMINAL_RUN_STATUSES.includes(r.status)) ? 3000 : false,
  });
}

export function useRun(wsId: string, id: string) {
  return useQuery({
    queryKey: qk.run(wsId, id),
    queryFn: () => api.runs.get(wsId, id),
    refetchInterval: (q) => (q.state.data && !TERMINAL_RUN_STATUSES.includes(q.state.data.status) ? 1000 : false),
  });
}

/** Events for a run, polled while it is live. Realtime replaces polling later. */
export function useRunEvents(wsId: string, id: string, live: boolean) {
  return useQuery<RunEvent[]>({
    queryKey: qk.runEvents(wsId, id),
    queryFn: () => api.runs.events(wsId, id),
    refetchInterval: live ? 800 : false,
  });
}

export function useCreateRun(wsId: string, agentId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateRunInput) => api.runs.create(wsId, agentId, input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["ws", wsId, "runs", "list"] });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

export function useCancelRun(wsId: string, id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api.runs.cancel(wsId, id),
    onSuccess: (run) => {
      qc.setQueryData(qk.run(wsId, id), run);
      qc.invalidateQueries({ queryKey: qk.runEvents(wsId, id) });
    },
  });
}

export function useOrganizationRuns(orgId: string, query: Omit<OrganizationRunListQuery, "cursor">) {
  return useInfiniteQuery({
    queryKey: ["org", orgId, "runs", query],
    queryFn: ({ pageParam }) => api.organizationRuns.list(orgId, { ...query, cursor: pageParam ?? undefined }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
    refetchInterval: (q) =>
      q.state.data?.pages[0]?.items.some((r) => !TERMINAL_RUN_STATUSES.includes(r.status)) ? 3000 : false,
  });
}

// ---------------------------------------------------------------------------
// Developer settings

export function useConnectSettings(orgId: string, env: Environment) {
  return useQuery({
    queryKey: qk.connectSettings(orgId, env),
    queryFn: () => api.connectSettings.get(orgId, env),
  });
}

export function useUpdateConnectSettings(orgId: string, env: Environment) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateConnectSettingsInput) => api.connectSettings.update(orgId, env, input),
    onSuccess: (settings) => qc.setQueryData(qk.connectSettings(orgId, env), settings),
  });
}

export function useApiKeys(orgId: string) {
  return useQuery({ queryKey: qk.apiKeys(orgId), queryFn: () => api.apiKeys.list(orgId) });
}

export function useCreateApiKey(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateApiKeyInput) => api.apiKeys.create(orgId, input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.apiKeys(orgId) });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

export function useRevokeApiKey(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (keyId: string) => api.apiKeys.revoke(orgId, keyId),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.apiKeys(orgId) }),
  });
}

export function useWebhooks(orgId: string) {
  return useQuery({ queryKey: qk.webhooks(orgId), queryFn: () => api.webhooks.list(orgId) });
}

export function useCreateWebhook(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateWebhookEndpointInput) => api.webhooks.create(orgId, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.webhooks(orgId) }),
  });
}

export function useUpdateWebhook(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, ...input }: UpdateWebhookEndpointInput & { id: string }) =>
      api.webhooks.update(orgId, id, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.webhooks(orgId) }),
  });
}

export function useDeleteWebhook(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.webhooks.delete(orgId, id),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.webhooks(orgId) }),
  });
}

export function useTestWebhook(orgId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.webhooks.sendTest(orgId, id),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.webhooks(orgId) }),
  });
}

// ---------------------------------------------------------------------------
// Audit

export function useAuditEvents(orgId: string, workspaceId?: string) {
  return useInfiniteQuery({
    queryKey: qk.audit(orgId, workspaceId),
    queryFn: ({ pageParam }) =>
      api.audit.list(orgId, { workspace_id: workspaceId, cursor: pageParam ?? undefined, limit: 30 }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
  });
}
