/**
 * Storage rows for the in-browser mock API. These mirror what the backend
 * will persist: workspace-scoped rows carry `workspace_id` explicitly, and the
 * wire shapes in `@zupflow/sdk` are derived from them by the handlers.
 */
import type {
  AccountConnection,
  AgentGrant,
  AgentStatus,
  AppIcon,
  AppKind,
  AuditEvent,
  AuthTemplate,
  ConnectIntent,
  ConnectSessionStatus,
  Environment,
  InstallationStatus,
  MemberRole,
  MemberStatus,
  RunError,
  RunEvent,
  RunStatus,
  RunUsage,
  ToolDefinition,
  User,
  WebhookEndpoint,
} from "@zupflow/sdk";

export interface OrganizationRow {
  id: string;
  name: string;
  slug: string;
  created_at: string;
}

export interface MembershipRow {
  id: string;
  organization_id: string;
  user_id: string;
  role: MemberRole;
  status: MemberStatus;
  invited_at: string;
  joined_at: string | null;
  last_active_at: string | null;
}

export interface WorkspaceRow {
  id: string;
  organization_id: string;
  name: string;
  slug: string;
  environment: Environment;
  external_id: string | null;
  created_at: string;
}

export interface AppRow {
  id: string;
  organization_id: string;
  owner_workspace_id: string | null;
  kind: AppKind;
  name: string;
  slug: string;
  description: string;
  icon: AppIcon;
  auth: AuthTemplate;
  tools: ToolDefinition[];
  version: number;
  created_at: string;
  updated_at: string;
}

export interface ConfigurationRow {
  app_id: string;
  organization_id: string;
  /** Set for workspace-owned apps only. */
  workspace_id: string | null;
  environment: Environment;
  version: number;
  mcp_server_url: string | null;
  oauth_client_id: string | null;
  oauth_client_secret_last4: string | null;
  oauth_client_secret_updated_at: string | null;
  oauth_scopes: string[];
  updated_at: string | null;
  updated_by_user_id: string | null;
}

export interface InstallationRow {
  id: string;
  workspace_id: string;
  organization_id: string;
  app_id: string;
  label: string;
  status: InstallationStatus;
  configuration_version: number;
  connection: AccountConnection | null;
  created_at: string;
  updated_at: string;
}

export interface ConnectSessionRow {
  id: string;
  workspace_id: string;
  organization_id: string;
  environment: Environment;
  app_id: string;
  configuration_version: number;
  intent: ConnectIntent;
  installation_id: string | null;
  label: string;
  subject: string;
  status: ConnectSessionStatus;
  return_url: string | null;
  requested_scopes: string[];
  created_at: string;
  expires_at: string;
  completed_at: string | null;
  failure_code: string | null;
}

export interface AgentRow {
  id: string;
  workspace_id: string;
  name: string;
  description: string;
  model: string;
  instructions: string;
  status: AgentStatus;
  created_at: string;
  updated_at: string;
}

/** A tool call decided when the run was planned. */
export interface PlannedCall {
  call_id: string;
  installation_id: string;
  installation_label: string;
  app_name: string;
  tool: string;
  arguments: unknown;
  result: unknown;
  error: RunError | null;
}

/** One scripted step of a simulated run. */
export type RunStep =
  | { kind: "start" }
  | { kind: "message"; text: string }
  | { kind: "approval"; call: PlannedCall }
  | { kind: "tool_call"; call: PlannedCall }
  | { kind: "tool_result"; call: PlannedCall; duration_ms: number }
  | {
      kind: "complete";
      /** What each planned call achieved; denied calls are removed before completion. */
      parts: { call_id: string; text: string }[];
      /** Used when no tool call contributes, e.g. the agent had no access. */
      fallback: string;
      closing: string;
    }
  | { kind: "fail"; error: RunError };

export interface RunRow {
  id: string;
  workspace_id: string;
  agent_id: string;
  session_id: string;
  subject: string | null;
  trigger: "api" | "console";
  status: RunStatus;
  input: string;
  output: string | null;
  usage: RunUsage;
  error: RunError | null;
  created_at: string;
  started_at: string | null;
  finished_at: string | null;
  /** Remaining simulation steps for live runs. Empty once terminal. */
  plan: RunStep[];
}

export interface ConnectSettingsRow {
  organization_id: string;
  environment: Environment;
  return_urls: string[];
  allowed_origins: string[];
  updated_at: string | null;
}

export interface ApiKeyRow {
  id: string;
  organization_id: string;
  environment: Environment;
  name: string;
  prefix: string;
  last4: string;
  created_by_user_id: string;
  created_at: string;
  last_used_at: string | null;
  revoked_at: string | null;
}

export interface Db {
  schema: number;
  users: User[];
  organizations: OrganizationRow[];
  memberships: MembershipRow[];
  workspaces: WorkspaceRow[];
  apps: AppRow[];
  configurations: ConfigurationRow[];
  installations: InstallationRow[];
  connect_sessions: ConnectSessionRow[];
  agents: AgentRow[];
  grants: AgentGrant[];
  runs: RunRow[];
  run_events: Record<string, RunEvent[]>;
  connect_settings: ConnectSettingsRow[];
  api_keys: ApiKeyRow[];
  webhooks: WebhookEndpoint[];
  audit: Omit<AuditEvent, "workspace">[];
}
