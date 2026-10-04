/**
 * Storage rows for the in-browser mock API. These mirror what the backend
 * will persist: workspace-scoped rows start with `id, organization_id,
 * workspace_id` and carry both explicitly, and the
 * wire shapes in `@zupflow/sdk` are derived from them by the handlers.
 */
import type {
  AccountConnection,
  AgentGrant,
  AgentId,
  AgentStatus,
  ApiKeyId,
  AppIcon,
  AppId,
  AppKind,
  AuditEvent,
  AuthTemplate,
  ConnectIntent,
  ConnectSessionId,
  ConnectSessionStatus,
  Environment,
  GrantId,
  InstallationId,
  InstallationStatus,
  MemberId,
  MemberRole,
  MemberStatus,
  OrganizationId,
  RunError,
  RunEvent,
  RunId,
  RunStatus,
  RunUsage,
  SessionId,
  ToolCallId,
  ToolDefinition,
  User,
  UserId,
  WebhookEndpoint,
  WorkspaceId,
} from "@zupflow/sdk";

export interface OrganizationRow {
  id: OrganizationId;
  name: string;
  slug: string;
  created_at: string;
}

export interface MembershipRow {
  id: MemberId;
  organization_id: OrganizationId;
  user_id: UserId;
  role: MemberRole;
  status: MemberStatus;
  invited_at: string;
  joined_at: string | null;
  last_active_at: string | null;
}

export interface WorkspaceRow {
  id: WorkspaceId;
  organization_id: OrganizationId;
  name: string;
  slug: string;
  environment: Environment;
  external_id: string | null;
  created_at: string;
}

export interface AppRow {
  id: AppId;
  organization_id: OrganizationId;
  owner_workspace_id: WorkspaceId | null;
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

/**
 * One app's client configuration in one environment. Natural key
 * (organization_id, app_id, environment); `workspace_id` is set for a
 * workspace's own app and null for an organization app.
 */
export interface ConfigurationRow {
  organization_id: OrganizationId;
  workspace_id: WorkspaceId | null;
  app_id: AppId;
  environment: Environment;
  version: number;
  mcp_server_url: string | null;
  oauth_client_id: string | null;
  oauth_client_secret_last4: string | null;
  oauth_client_secret_updated_at: string | null;
  oauth_scopes: string[];
  updated_at: string | null;
  updated_by_user_id: UserId | null;
}

export interface InstallationRow {
  id: InstallationId;
  organization_id: OrganizationId;
  workspace_id: WorkspaceId;
  app_id: AppId;
  label: string;
  status: InstallationStatus;
  configuration_version: number;
  connection: AccountConnection | null;
  created_at: string;
  updated_at: string;
}

export interface ConnectSessionRow {
  id: ConnectSessionId;
  organization_id: OrganizationId;
  workspace_id: WorkspaceId;
  /**
   * SHA-256 of the random reference in the hosted Connect link, whose
   * possession authorizes the page. The reference itself is never stored;
   * `null` for sessions with no usable link (seeded history).
   */
  reference_hash: string | null;
  environment: Environment;
  app_id: AppId;
  configuration_version: number;
  intent: ConnectIntent;
  installation_id: InstallationId | null;
  label: string;
  status: ConnectSessionStatus;
  return_url: string | null;
  requested_scopes: string[];
  created_at: string;
  expires_at: string;
  completed_at: string | null;
  failure_code: string | null;
}

export interface AgentRow {
  id: AgentId;
  organization_id: OrganizationId;
  workspace_id: WorkspaceId;
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
  call_id: ToolCallId;
  installation_id: InstallationId;
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
      parts: { call_id: ToolCallId; text: string }[];
      /** Used when no tool call contributes, e.g. the agent had no access. */
      fallback: string;
      closing: string;
    }
  | { kind: "fail"; error: RunError };

export interface RunRow {
  id: RunId;
  organization_id: OrganizationId;
  workspace_id: WorkspaceId;
  agent_id: AgentId;
  session_id: SessionId;
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
  organization_id: OrganizationId;
  environment: Environment;
  return_urls: string[];
  allowed_origins: string[];
  updated_at: string | null;
}

export interface ApiKeyRow {
  id: ApiKeyId;
  organization_id: OrganizationId;
  environment: Environment;
  name: string;
  prefix: string;
  last4: string;
  created_by_user_id: UserId;
  created_at: string;
  last_used_at: string | null;
  revoked_at: string | null;
}

/** A grant as stored: the wire shape plus its organization, which the API doesn't repeat. */
export type GrantRow = { id: GrantId; organization_id: OrganizationId; workspace_id: WorkspaceId } & Omit<
  AgentGrant,
  "id" | "workspace_id"
>;

/** A run event as stored: the wire shape plus its organization, which the API doesn't repeat. */
export type RunEventRow = RunEvent & { organization_id: OrganizationId };

/** What identifies a run's tenant: every lookup about a run carries all three. */
export type RunScope = Pick<RunRow, "id" | "organization_id" | "workspace_id">;

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
  grants: GrantRow[];
  runs: RunRow[];
  /** Each run's timeline, keyed by the run's full scope (`runEventsKey`); every event carries it too. */
  run_events: Record<string, RunEventRow[]>;
  connect_settings: ConnectSettingsRow[];
  api_keys: ApiKeyRow[];
  webhooks: WebhookEndpoint[];
  audit: Omit<AuditEvent, "workspace">[];
  /** Idempotency keys (decision 0016), each scoped to its organization, workspace and the caller that sent it. */
  idempotency_keys: IdempotencyRow[];
  /** The server key the idempotency fingerprint is keyed with, so a stored fingerprint can't be guessed back. */
  idempotency_fingerprint_key: string;
}

/**
 * One `Idempotency-Key` as the caller used it. `organization_id` is null only
 * for creating an organization, whose keys belong to the user alone;
 * `workspace_id` is set on workspace routes and null on organization routes.
 */
export interface IdempotencyRow {
  organization_id: OrganizationId | null;
  workspace_id: WorkspaceId | null;
  /** The credential and environment that sent the key, e.g. `member:usr_…|sandbox` or `connect:cs_…|sandbox`. */
  caller: string;
  key: string;
  /** Method, path and an HMAC-SHA-256 of the body under the server key. */
  fingerprint: string;
  state: "running" | "completed";
  /** The saved answer; `body` is null for a route that shows a secret once. */
  status: number | null;
  body: string | null;
  created_at: string;
  locked_until: string;
}
