import type { Environment, RunListQuery } from "@zupflow/sdk";

/**
 * Every workspace-scoped key starts with `["ws", workspaceId]`, so invalidating
 * a workspace never touches another one.
 */
export const qk = {
  viewer: ["viewer"] as const,
  models: ["models"] as const,

  org: (orgId: string) => ["org", orgId] as const,
  usage: (orgId: string, env: Environment) => ["org", orgId, "usage", env] as const,
  members: (orgId: string) => ["org", orgId, "members"] as const,
  workspaces: (orgId: string) => ["org", orgId, "workspaces"] as const,
  orgApps: (orgId: string) => ["org", orgId, "apps"] as const,
  orgApp: (orgId: string, appId: string) => ["org", orgId, "apps", appId] as const,
  orgAppConfig: (orgId: string, appId: string, env: Environment) =>
    ["org", orgId, "apps", appId, "configuration", env] as const,
  orgAppInstallations: (orgId: string, appId: string) => ["org", orgId, "apps", appId, "installations"] as const,
  connectSettings: (orgId: string, env: Environment) => ["org", orgId, "connect-settings", env] as const,
  apiKeys: (orgId: string) => ["org", orgId, "api-keys"] as const,
  webhooks: (orgId: string) => ["org", orgId, "webhooks"] as const,
  audit: (orgId: string, workspaceId?: string) => ["org", orgId, "audit", workspaceId ?? "all"] as const,

  workspace: (wsId: string) => ["ws", wsId] as const,
  workspaceApps: (wsId: string) => ["ws", wsId, "apps"] as const,
  workspaceApp: (wsId: string, appId: string) => ["ws", wsId, "apps", appId] as const,
  workspaceAppConfig: (wsId: string, appId: string) => ["ws", wsId, "apps", appId, "configuration"] as const,
  availableApps: (wsId: string) => ["ws", wsId, "available-apps"] as const,
  installations: (wsId: string) => ["ws", wsId, "installations"] as const,
  installation: (wsId: string, id: string) => ["ws", wsId, "installations", id] as const,
  installationTools: (wsId: string, id: string) => ["ws", wsId, "installations", id, "tools"] as const,
  connectSessions: (wsId: string) => ["ws", wsId, "connect-sessions"] as const,
  connectSession: (wsId: string, id: string) => ["ws", wsId, "connect-sessions", id] as const,
  agents: (wsId: string) => ["ws", wsId, "agents"] as const,
  agent: (wsId: string, id: string) => ["ws", wsId, "agents", id] as const,
  grants: (wsId: string, agentId: string) => ["ws", wsId, "agents", agentId, "grants"] as const,
  runs: (wsId: string, query: RunListQuery = {}) => ["ws", wsId, "runs", "list", query] as const,
  run: (wsId: string, id: string) => ["ws", wsId, "runs", id] as const,
  runEvents: (wsId: string, id: string) => ["ws", wsId, "runs", id, "events"] as const,

  hostedConnect: (reference: string) => ["connect", reference] as const,
};
