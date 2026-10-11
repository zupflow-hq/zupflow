/**
 * Query and mutation hooks. Pages read through these; they never call the SDK
 * directly. Each hook owns its cache key and what it invalidates.
 */
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  type AcceptToolChangeInput,
  type AgentId,
  type ApiKeyId,
  type AppId,
  type ConnectSessionId,
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
  type GrantId,
  type InstallationId,
  type InviteMemberInput,
  type Member,
  type MemberId,
  type MemberRole,
  type OrganizationId,
  type OrganizationRunListQuery,
  type RunEvent,
  type RunId,
  type RunListQuery,
  TERMINAL_RUN_STATUSES,
  type UpdateAgentInput,
  type UpdateAppInput,
  type UpdateAppToolLevelsInput,
  type UpdateConfigurationInput,
  type UpdateConnectSettingsInput,
  type UpdateGrantInput,
  type UpdateOrganizationInput,
  type UpdateToolPolicyInput,
  type UpdateWebhookEndpointInput,
  type UpdateWorkspaceInput,
  type WebhookDeliveryId,
  type WebhookDeliveryListQuery,
  type WebhookEndpointId,
  type WorkspaceId,
  type WorkspaceListQuery,
} from "@zupflow/sdk";
import { useEffect, useRef } from "react";
import { api } from "@/lib/api";
import { qk } from "@/lib/query-keys";

/** For queries that are `enabled` only once an ID exists; reaching this without one is a bug. */
function required<T>(id: T | undefined | null): T {
  if (id === undefined || id === null) throw new Error("This query ran before its ID was known.");
  return id;
}

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

// ---------------------------------------------------------------------------
// Invitations: the signed-in person's own

export function useInvitations() {
  return useQuery({ queryKey: qk.invitations, queryFn: () => api.invitations.list(), staleTime: 30_000 });
}

/**
 * Joins the invitation's organization. Each click sends its own idempotency
 * key, so a retried request joins once. Settles once the viewer and the
 * invitations are fresh, whatever the answer, so the caller can go straight
 * into the organization: after joining, or after finding it already joined.
 */
export function useAcceptInvitation() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ invitationId, idempotencyKey }: { invitationId: MemberId; idempotencyKey: string }) =>
      api.invitations.accept(invitationId, { idempotencyKey }),
    onSettled: () =>
      Promise.all([qc.invalidateQueries({ queryKey: qk.viewer }), qc.invalidateQueries({ queryKey: qk.invitations })]),
  });
}

export function useDeclineInvitation() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (invitationId: MemberId) => api.invitations.decline(invitationId),
    onSettled: () => qc.invalidateQueries({ queryKey: qk.invitations }),
  });
}

export function useUpdateOrganization(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateOrganizationInput) => api.organizations.update(orgId, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.viewer }),
  });
}

export function useUsage(orgId: OrganizationId, env: Environment) {
  return useQuery({
    queryKey: qk.usage(orgId, env),
    queryFn: () => api.organizations.usage(orgId, env, 14),
  });
}

// ---------------------------------------------------------------------------
// Members

export function useMembers(orgId: OrganizationId) {
  return useQuery({ queryKey: qk.members(orgId), queryFn: () => api.members.list(orgId) });
}

export function useInviteMember(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: InviteMemberInput) => api.members.invite(orgId, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.members(orgId) }),
  });
}

export function useUpdateMember(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ memberId, role }: { memberId: MemberId; role: MemberRole }) =>
      api.members.update(orgId, memberId, { role }),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.members(orgId) }),
  });
}

export function useRemoveMember(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    /** An invitation is cancelled as one only: never a member who accepted it meanwhile (`409 invitation_accepted`). */
    mutationFn: (member: Pick<Member, "id" | "status">) =>
      api.members.remove(orgId, member.id, member.status === "invited" ? { status: "invited" } : {}),
    onSettled: () => qc.invalidateQueries({ queryKey: qk.members(orgId) }),
  });
}

// ---------------------------------------------------------------------------
// Workspaces

/** Workspaces, a page at a time. Organizations have thousands; never load them all. */
export function useWorkspaceList(orgId: OrganizationId, query: Omit<WorkspaceListQuery, "cursor">) {
  return useInfiniteQuery({
    queryKey: [...qk.workspaces(orgId), "list", query],
    queryFn: ({ pageParam }) => api.workspaces.list(orgId, { ...query, cursor: pageParam ?? undefined }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
    placeholderData: (previous) => previous,
  });
}

/** One page of workspaces: summaries, counts and pickers. */
export function useWorkspacePage(orgId: OrganizationId, query: Omit<WorkspaceListQuery, "cursor">, enabled = true) {
  return useQuery({
    queryKey: [...qk.workspaces(orgId), "page", query],
    queryFn: () => api.workspaces.list(orgId, query),
    placeholderData: (previous) => previous,
    enabled,
  });
}

export const workspaceQuery = (wsId: WorkspaceId) => ({
  queryKey: qk.workspace(wsId),
  queryFn: () => api.workspaces.get(wsId),
});

/** The workspace, or nothing while there's no ID to load (outside a workspace page, say). */
export function useWorkspace(wsId: WorkspaceId | undefined) {
  return useQuery({
    queryKey: qk.workspace(wsId ?? "none"),
    queryFn: () => api.workspaces.get(required(wsId)),
    enabled: Boolean(wsId),
  });
}

export function useCreateWorkspace(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateWorkspaceInput) => api.workspaces.create(orgId, input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.workspaces(orgId) });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

export function useUpdateWorkspace(orgId: OrganizationId, wsId: WorkspaceId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateWorkspaceInput) => api.workspaces.update(wsId, input),
    onSuccess: (workspace) => {
      qc.setQueryData(qk.workspace(wsId), workspace);
      qc.invalidateQueries({ queryKey: qk.workspaces(orgId) });
    },
  });
}

export function useDeleteWorkspace(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (wsId: WorkspaceId) => api.workspaces.delete(wsId),
    onSuccess: (_, wsId) => {
      qc.removeQueries({ queryKey: qk.workspace(wsId) });
      qc.invalidateQueries({ queryKey: qk.org(orgId) });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

// ---------------------------------------------------------------------------
// Apps

export function useOrgApps(orgId: OrganizationId) {
  return useQuery({ queryKey: qk.orgApps(orgId), queryFn: () => api.organizationApps.list(orgId) });
}

export function useOrgApp(orgId: OrganizationId, appId: AppId | undefined) {
  return useQuery({
    queryKey: qk.orgApp(orgId, appId ?? "none"),
    queryFn: () => api.organizationApps.get(orgId, required(appId)),
    enabled: Boolean(appId),
  });
}

export function useCreateOrgApp(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateAppInput) => api.organizationApps.create(orgId, input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.orgApps(orgId) });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

export function useUpdateOrgApp(orgId: OrganizationId, appId: AppId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateAppInput) => api.organizationApps.update(orgId, appId, input),
    onSuccess: (app) => {
      qc.setQueryData(qk.orgApp(orgId, appId), app);
      qc.invalidateQueries({ queryKey: qk.orgApps(orgId) });
    },
  });
}

export function useOrgAppConfiguration(orgId: OrganizationId, appId: AppId, env: Environment) {
  return useQuery({
    queryKey: qk.orgAppConfig(orgId, appId, env),
    queryFn: () => api.organizationApps.getConfiguration(orgId, appId, env),
  });
}

export function useUpdateOrgAppConfiguration(orgId: OrganizationId, appId: AppId, env: Environment) {
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

export function useOrgAppInstallations(orgId: OrganizationId, appId: AppId, environment: Environment) {
  return useInfiniteQuery({
    queryKey: [...qk.orgAppInstallations(orgId, appId), environment],
    queryFn: ({ pageParam }) =>
      api.organizationApps.installations(orgId, appId, { environment, cursor: pageParam ?? undefined }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
  });
}

export function useWorkspaceApps(wsId: WorkspaceId) {
  return useQuery({ queryKey: qk.workspaceApps(wsId), queryFn: () => api.workspaceApps.list(wsId) });
}

export function useWorkspaceApp(wsId: WorkspaceId, appId: AppId | undefined) {
  return useQuery({
    queryKey: qk.workspaceApp(wsId, appId ?? "none"),
    queryFn: () => api.workspaceApps.get(wsId, required(appId)),
    enabled: Boolean(appId),
  });
}

export function useCreateWorkspaceApp(wsId: WorkspaceId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateAppInput) => api.workspaceApps.create(wsId, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.workspace(wsId) }),
  });
}

export function useUpdateWorkspaceApp(wsId: WorkspaceId, appId: AppId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateAppInput) => api.workspaceApps.update(wsId, appId, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.workspace(wsId) }),
  });
}

export function useWorkspaceAppConfiguration(wsId: WorkspaceId, appId: AppId) {
  return useQuery({
    queryKey: qk.workspaceAppConfig(wsId, appId),
    queryFn: () => api.workspaceApps.getConfiguration(wsId, appId),
  });
}

export function useUpdateWorkspaceAppConfiguration(wsId: WorkspaceId, appId: AppId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateConfigurationInput) => api.workspaceApps.updateConfiguration(wsId, appId, input),
    onSuccess: (configuration) => {
      qc.setQueryData(qk.workspaceAppConfig(wsId, appId), configuration);
      qc.invalidateQueries({ queryKey: qk.workspace(wsId) });
    },
  });
}

export function useAvailableApps(wsId: WorkspaceId) {
  return useQuery({
    queryKey: qk.availableApps(wsId),
    queryFn: () => api.workspaceApps.available(wsId),
  });
}

// ---------------------------------------------------------------------------
// Installations and Connect

export function useInstallations(wsId: WorkspaceId) {
  return useQuery({ queryKey: qk.installations(wsId), queryFn: () => api.installations.list(wsId) });
}

export function useInstallation(wsId: WorkspaceId, id: InstallationId) {
  return useQuery({
    queryKey: qk.installation(wsId, id),
    queryFn: () => api.installations.get(wsId, id),
  });
}

export function useRenameInstallation(wsId: WorkspaceId, id: InstallationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (label: string) => api.installations.update(wsId, id, { label }),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.workspace(wsId) }),
  });
}

/** The tools the installation's app listed to it, a page at a time, by name. */
export function useInstallationTools(wsId: WorkspaceId, id: InstallationId) {
  return useInfiniteQuery({
    queryKey: qk.installationTools(wsId, id),
    queryFn: ({ pageParam }) => api.installations.tools(wsId, id, { cursor: pageParam ?? undefined }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
  });
}

/**
 * Asks the app for the installation's tools again. The server lists them in
 * the background, so the tools are read again now and once more shortly
 * after; that second read is dropped if the page closes first.
 */
export function useRefreshInstallationTools(wsId: WorkspaceId, id: InstallationId) {
  const qc = useQueryClient();
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );
  return useMutation({
    mutationFn: () => api.installations.refreshTools(wsId, id),
    onSuccess: () => {
      const reread = () => qc.invalidateQueries({ queryKey: qk.installationTools(wsId, id) });
      void reread();
      if (timer.current) clearTimeout(timer.current);
      timer.current = setTimeout(() => {
        timer.current = null;
        void reread();
      }, 3_000);
    },
  });
}

/** Accepts a changed tool's reviewed definition; the installation's tools are read again either way. */
export function useAcceptToolChange(wsId: WorkspaceId, id: InstallationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: AcceptToolChangeInput) => api.installations.acceptToolChange(wsId, id, input),
    // Refused because the definition changed again: read it again, so the new one is what's reviewed.
    onSettled: () => qc.invalidateQueries({ queryKey: qk.installationTools(wsId, id) }),
  });
}

export function useDisconnectInstallation(wsId: WorkspaceId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: InstallationId) => api.installations.disconnect(wsId, id),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.workspace(wsId) }),
  });
}

export function useDeleteInstallation(wsId: WorkspaceId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: InstallationId) => api.installations.delete(wsId, id),
    onSuccess: (_, id) => {
      qc.removeQueries({ queryKey: qk.installation(wsId, id) });
      qc.invalidateQueries({ queryKey: qk.workspace(wsId) });
    },
  });
}

export function useConnectSessions(wsId: WorkspaceId) {
  return useQuery({
    queryKey: qk.connectSessions(wsId),
    queryFn: () => api.connectSessions.list(wsId),
  });
}

export function useCreateConnectSession(wsId: WorkspaceId) {
  return useMutation({
    mutationFn: (input: CreateConnectSessionInput) => api.connectSessions.create(wsId, input),
  });
}

/** Polls a Connect session until it leaves `pending`. */
export function useWatchedConnectSession(wsId: WorkspaceId, sessionId: ConnectSessionId | null) {
  return useQuery({
    queryKey: qk.connectSession(wsId, sessionId ?? "none"),
    queryFn: () => api.connectSessions.get(wsId, required(sessionId)),
    enabled: sessionId !== null,
    refetchInterval: (query) => (query.state.data?.status === "pending" ? 1500 : false),
  });
}

/** Hosted Connect, by the session's reference (the random token in its link), never its ID. */
export function useHostedConnectSession(reference: string) {
  return useQuery({
    queryKey: qk.hostedConnect(reference),
    queryFn: () => api.connect.get(reference),
    retry: false,
    // The provider round-trip changes this session outside the page.
    staleTime: 0,
  });
}

// ---------------------------------------------------------------------------
// Agents and grants

export function useAgents(wsId: WorkspaceId) {
  return useQuery({ queryKey: qk.agents(wsId), queryFn: () => api.agents.list(wsId) });
}

export function useAgent(wsId: WorkspaceId, id: AgentId) {
  return useQuery({ queryKey: qk.agent(wsId, id), queryFn: () => api.agents.get(wsId, id) });
}

export function useCreateAgent(wsId: WorkspaceId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateAgentInput) => api.agents.create(wsId, input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.workspace(wsId) });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

export function useUpdateAgent(wsId: WorkspaceId, id: AgentId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateAgentInput) => api.agents.update(wsId, id, input),
    onSuccess: (agent) => {
      qc.setQueryData(qk.agent(wsId, id), agent);
      qc.invalidateQueries({ queryKey: qk.agents(wsId), exact: true });
    },
  });
}

export function useDeleteAgent(wsId: WorkspaceId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: AgentId) => api.agents.delete(wsId, id),
    onSuccess: (_, id) => {
      qc.removeQueries({ queryKey: qk.agent(wsId, id) });
      qc.invalidateQueries({ queryKey: qk.workspace(wsId) });
    },
  });
}

export function useGrants(wsId: WorkspaceId, agentId: AgentId) {
  return useQuery({
    queryKey: qk.grants(wsId, agentId),
    queryFn: () => api.grants.list(wsId, agentId),
  });
}

function useInvalidateGrants(wsId: WorkspaceId, agentId: AgentId) {
  const qc = useQueryClient();
  return () => {
    qc.invalidateQueries({ queryKey: qk.grants(wsId, agentId) });
    qc.invalidateQueries({ queryKey: qk.agent(wsId, agentId), exact: true });
    qc.invalidateQueries({ queryKey: qk.agents(wsId), exact: true });
    qc.invalidateQueries({ queryKey: qk.installations(wsId) });
  };
}

export function useCreateGrant(wsId: WorkspaceId, agentId: AgentId) {
  const invalidate = useInvalidateGrants(wsId, agentId);
  return useMutation({
    mutationFn: (input: CreateGrantInput) => api.grants.create(wsId, agentId, input),
    onSuccess: invalidate,
  });
}

export function useUpdateGrant(wsId: WorkspaceId, agentId: AgentId) {
  const invalidate = useInvalidateGrants(wsId, agentId);
  return useMutation({
    mutationFn: ({ grantId, ...input }: UpdateGrantInput & { grantId: GrantId }) =>
      api.grants.update(wsId, agentId, grantId, input),
    onSuccess: invalidate,
  });
}

export function useDeleteGrant(wsId: WorkspaceId, agentId: AgentId) {
  const invalidate = useInvalidateGrants(wsId, agentId);
  return useMutation({
    mutationFn: (grantId: GrantId) => api.grants.delete(wsId, agentId, grantId),
    onSuccess: invalidate,
  });
}

// ---------------------------------------------------------------------------
// Tool policy

export function useToolPolicy(orgId: OrganizationId) {
  return useQuery({ queryKey: qk.toolPolicy(orgId), queryFn: () => api.toolPolicy.get(orgId) });
}

export function useUpdateToolPolicy(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateToolPolicyInput) => api.toolPolicy.update(orgId, input),
    onSuccess: (policy) => qc.setQueryData(qk.toolPolicy(orgId), policy),
  });
}

/** The organization's levels for a catalog app's tools, a page at a time, by name. */
export function useAppToolLevels(orgId: OrganizationId, appId: AppId | undefined) {
  return useInfiniteQuery({
    queryKey: qk.appToolLevels(orgId, appId ?? ""),
    queryFn: ({ pageParam }) =>
      api.toolPolicy.levels(orgId, required(appId), { cursor: pageParam ?? undefined, limit: 100 }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
    enabled: !!appId,
  });
}

export function useSetAppToolLevels(orgId: OrganizationId, appId: AppId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateAppToolLevelsInput) => api.toolPolicy.setLevels(orgId, appId, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.appToolLevels(orgId, appId) }),
  });
}

// ---------------------------------------------------------------------------
// Runs

export function useRuns(wsId: WorkspaceId, query: Omit<RunListQuery, "cursor"> = {}) {
  return useInfiniteQuery({
    queryKey: qk.runs(wsId, query),
    queryFn: ({ pageParam }) => api.runs.list(wsId, { ...query, cursor: pageParam ?? undefined }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
    refetchInterval: (q) =>
      q.state.data?.pages[0]?.items.some((r) => !TERMINAL_RUN_STATUSES.includes(r.status)) ? 3000 : false,
  });
}

export function useRun(wsId: WorkspaceId, id: RunId) {
  return useQuery({
    queryKey: qk.run(wsId, id),
    queryFn: () => api.runs.get(wsId, id),
    refetchInterval: (q) => (q.state.data && !TERMINAL_RUN_STATUSES.includes(q.state.data.status) ? 1000 : false),
  });
}

/** Events for a run, polled while it is live. Realtime replaces polling later. */
export function useRunEvents(wsId: WorkspaceId, id: RunId, live: boolean) {
  return useQuery<RunEvent[]>({
    queryKey: qk.runEvents(wsId, id),
    queryFn: () => api.runs.events(wsId, id),
    refetchInterval: live ? 800 : false,
  });
}

export function useCreateRun(wsId: WorkspaceId, agentId: AgentId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateRunInput) => api.runs.create(wsId, agentId, input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["ws", wsId, "runs", "list"] });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

export function useCancelRun(wsId: WorkspaceId, id: RunId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api.runs.cancel(wsId, id),
    onSuccess: (run) => {
      qc.setQueryData(qk.run(wsId, id), run);
      qc.invalidateQueries({ queryKey: qk.runEvents(wsId, id) });
    },
  });
}

export function useOrganizationRuns(orgId: OrganizationId, query: Omit<OrganizationRunListQuery, "cursor">) {
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

export function useConnectSettings(orgId: OrganizationId, env: Environment) {
  return useQuery({
    queryKey: qk.connectSettings(orgId, env),
    queryFn: () => api.connectSettings.get(orgId, env),
  });
}

export function useUpdateConnectSettings(orgId: OrganizationId, env: Environment) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: UpdateConnectSettingsInput) => api.connectSettings.update(orgId, env, input),
    onSuccess: (settings) => qc.setQueryData(qk.connectSettings(orgId, env), settings),
  });
}

export function useApiKeys(orgId: OrganizationId) {
  return useQuery({ queryKey: qk.apiKeys(orgId), queryFn: () => api.apiKeys.list(orgId) });
}

export function useCreateApiKey(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateApiKeyInput) => api.apiKeys.create(orgId, input),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.apiKeys(orgId) });
      qc.invalidateQueries({ queryKey: qk.viewer });
    },
  });
}

export function useRevokeApiKey(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (keyId: ApiKeyId) => api.apiKeys.revoke(orgId, keyId),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.apiKeys(orgId) }),
  });
}

export function useWebhooks(orgId: OrganizationId) {
  return useQuery({ queryKey: qk.webhooks(orgId), queryFn: () => api.webhooks.list(orgId) });
}

export function useWebhookDeliveries(
  orgId: OrganizationId,
  endpointId: WebhookEndpointId,
  environment: Environment,
  filters: Omit<WebhookDeliveryListQuery, "cursor" | "limit">,
) {
  return useInfiniteQuery({
    queryKey: qk.webhookDeliveries(orgId, endpointId, environment, filters),
    queryFn: ({ pageParam }) =>
      api.webhooks.deliveries(orgId, endpointId, { ...filters, cursor: pageParam ?? undefined, limit: 25 }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
    refetchInterval: (query) =>
      query.state.data?.pages.some((page) => page.items.some((item) => item.state === "pending")) ? 5_000 : false,
  });
}

export function useWebhookDelivery(
  orgId: OrganizationId,
  endpointId: WebhookEndpointId,
  environment: Environment,
  deliveryId?: WebhookDeliveryId,
) {
  const qc = useQueryClient();
  const query = useQuery({
    queryKey: qk.webhookDelivery(orgId, endpointId, environment, deliveryId),
    enabled: deliveryId !== undefined,
    queryFn: () => api.webhooks.delivery(orgId, endpointId, required(deliveryId)),
    refetchInterval: (query) => (query.state.data?.state === "pending" ? 5_000 : false),
  });
  const recordedAttempts = query.data?.attempts;
  useEffect(() => {
    if (deliveryId && recordedAttempts !== undefined) {
      // Fetch the final attempt even when a completed delivery stops polling.
      void qc.invalidateQueries({ queryKey: qk.webhookAttempts(orgId, endpointId, environment, deliveryId) });
    }
  }, [qc, orgId, endpointId, environment, deliveryId, recordedAttempts]);
  return query;
}

export function useWebhookDeliveryAttempts(
  orgId: OrganizationId,
  endpointId: WebhookEndpointId,
  environment: Environment,
  deliveryId?: WebhookDeliveryId,
  live = false,
) {
  return useInfiniteQuery({
    queryKey: qk.webhookAttempts(orgId, endpointId, environment, deliveryId),
    enabled: deliveryId !== undefined,
    queryFn: ({ pageParam }) =>
      api.webhooks.deliveryAttempts(orgId, endpointId, required(deliveryId), {
        cursor: pageParam ?? undefined,
        limit: 25,
      }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
    refetchInterval: live ? 5_000 : false,
  });
}

export function useCreateWebhook(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateWebhookEndpointInput) => api.webhooks.create(orgId, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.webhooks(orgId) }),
  });
}

export function useUpdateWebhook(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, ...input }: UpdateWebhookEndpointInput & { id: WebhookEndpointId }) =>
      api.webhooks.update(orgId, id, input),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.webhooks(orgId) }),
  });
}

export function useDeleteWebhook(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: WebhookEndpointId) => api.webhooks.delete(orgId, id),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.webhooks(orgId) }),
  });
}

export function useTestWebhook(orgId: OrganizationId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: WebhookEndpointId) => api.webhooks.sendTest(orgId, id),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.webhooks(orgId) }),
  });
}

export function useResendWebhookDelivery(orgId: OrganizationId, endpointId: WebhookEndpointId) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (deliveryId: WebhookDeliveryId) => api.webhooks.resendDelivery(orgId, endpointId, deliveryId),
    onSuccess: () => qc.invalidateQueries({ queryKey: qk.webhooks(orgId) }),
  });
}

// ---------------------------------------------------------------------------
// Audit

export function useAuditEvents(orgId: OrganizationId, workspaceId?: WorkspaceId) {
  return useInfiniteQuery({
    queryKey: qk.audit(orgId, workspaceId),
    queryFn: ({ pageParam }) =>
      api.audit.list(orgId, { workspace_id: workspaceId, cursor: pageParam ?? undefined, limit: 30 }),
    initialPageParam: null as string | null,
    getNextPageParam: (last) => last.pagination.next_cursor,
  });
}
