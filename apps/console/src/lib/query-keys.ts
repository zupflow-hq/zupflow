import type { Environment, ModelListQuery, RunListQuery, WebhookDeliveryListQuery } from "@zupflow/sdk";

/**
 * Every workspace-scoped key starts with `["ws", workspaceId]`, so invalidating
 * a workspace never touches another one.
 */
export const qk = {
  viewer: ["viewer"] as const,
  /** The signed-in person's own invitations: not an organization's. */
  invitations: ["invitations"] as const,

  org: (orgId: string) => ["org", orgId] as const,
  usage: (orgId: string, env: Environment) => ["org", orgId, "usage", env] as const,
  members: (orgId: string) => ["org", orgId, "members"] as const,
  workspaces: (orgId: string) => ["org", orgId, "workspaces"] as const,
  orgApps: (orgId: string) => ["org", orgId, "apps"] as const,
  orgApp: (orgId: string, appId: string) => ["org", orgId, "apps", appId] as const,
  orgAppConfig: (orgId: string, appId: string, env: Environment) =>
    ["org", orgId, "apps", appId, "configuration", env] as const,
  orgAppInstallations: (orgId: string, appId: string) => ["org", orgId, "apps", appId, "installations"] as const,
  toolPolicy: (orgId: string) => ["org", orgId, "tool-policy"] as const,
  appToolLevels: (orgId: string, appId: string) => ["org", orgId, "apps", appId, "tool-levels"] as const,
  connectSettings: (orgId: string, env: Environment) => ["org", orgId, "connect-settings", env] as const,
  apiKeys: (orgId: string) => ["org", orgId, "api-keys"] as const,
  webhooks: (orgId: string) => ["org", orgId, "webhooks"] as const,
  webhookDeliveries: (orgId: string, endpointId: string, env: Environment, query: WebhookDeliveryListQuery) =>
    ["org", orgId, "webhooks", endpointId, env, "deliveries", query] as const,
  webhookDelivery: (orgId: string, endpointId: string, env: Environment, deliveryId?: string) =>
    ["org", orgId, "webhooks", endpointId, env, "delivery", deliveryId] as const,
  webhookAttempts: (orgId: string, endpointId: string, env: Environment, deliveryId?: string) =>
    ["org", orgId, "webhooks", endpointId, env, "delivery", deliveryId, "attempts"] as const,
  /** Model providers: connections, their models and each environment's default. One prefix, so a change refreshes all three. */
  modelProviders: (orgId: string) => ["org", orgId, "model-providers"] as const,
  modelConnections: (orgId: string, env: Environment) => ["org", orgId, "model-providers", "connections", env] as const,
  modelConnection: (orgId: string, connectionId: string) =>
    ["org", orgId, "model-providers", "connection", connectionId] as const,
  providerModels: (orgId: string, connectionId: string, q: string) =>
    ["org", orgId, "model-providers", "connection", connectionId, "provider-models", q] as const,
  models: (orgId: string, query: Omit<ModelListQuery, "cursor">) =>
    ["org", orgId, "model-providers", "models", query] as const,
  defaultModel: (orgId: string, env: Environment) => ["org", orgId, "model-providers", "default", env] as const,
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
