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
  ModelConnectionId,
  ModelConnectionStatus,
  ModelId,
  ModelProviderKind,
  ModelStatus,
  NewToolLevel,
  OrganizationId,
  RunError,
  RunEvent,
  RunId,
  RunStatus,
  RunUsage,
  SessionId,
  ToolCallId,
  ToolDefinition,
  ToolEffect,
  ToolLevel,
  ToolText,
  User,
  UserId,
  WebhookDeliveryAttempt,
  WebhookDeliveryId,
  WebhookDeliverySummary,
  WebhookEndpoint,
  WebhookEndpointId,
  WebhookEvent,
  WebhookEventId,
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
  /** Who sent the invitation; null for an organization's creator, or once that user is gone. */
  invited_by: UserId | null;
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
  /** Snake case, fixed once created (decision 0047). */
  key: string;
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
  /** Unique in its workspace, kept for the installation's life (decision 0047). */
  short_id: string;
  label: string;
  status: InstallationStatus;
  configuration_version: number;
  connection: AccountConnection | null;
  created_at: string;
  updated_at: string;
}

/** A short ID issued in a workspace. Natural key (organization_id, workspace_id, short_id). */
export interface InstallationShortIdRow {
  organization_id: OrganizationId;
  workspace_id: WorkspaceId;
  short_id: string;
}

/**
 * A tool an installation's MCP server listed, kept as that installation's
 * own (decision 0038). Natural key (organization_id, workspace_id,
 * installation_id, name). The first definition seen is `pinned`; a later one
 * that differs is `latest`, unreviewed.
 */
export interface InstallationToolRow {
  organization_id: OrganizationId;
  workspace_id: WorkspaceId;
  installation_id: InstallationId;
  name: string;
  /** Seeded from the definition when the tool is first listed; a listing never changes it. */
  effect: ToolEffect;
  pinned: ToolText;
  latest: ToolText | null;
  first_seen_at: string;
  seen_at: string;
  changed_at: string | null;
  gone_at: string | null;
}

/**
 * The organization's tool policy (decision 0040), one per organization once
 * saved; an organization without one reads the defaults.
 */
export interface ToolPolicyRow {
  organization_id: OrganizationId;
  mode: "manual";
  new_tool_level: NewToolLevel;
  updated_at: string;
  updated_by_user_id: UserId | null;
}

/**
 * The organization's level for one tool of one of its catalog apps. Natural
 * key (organization_id, app_id, tool). Suggested from a tool's hints when an
 * installation first lists it (`confirmed: false`); in force only once a
 * person sets it.
 */
export interface AppToolLevelRow {
  organization_id: OrganizationId;
  app_id: AppId;
  tool: string;
  level: ToolLevel;
  confirmed: boolean;
  destructive: boolean;
  updated_at: string;
  updated_by_user_id: UserId | null;
}

/** An accepted refresh of an installation's tools: another is refused until `until`. */
export interface ToolRefreshRow {
  organization_id: OrganizationId;
  workspace_id: WorkspaceId;
  installation_id: InstallationId;
  until: string;
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
  /** An override: a model of the same organization, in the workspace's environment. Null runs on the default. */
  model_id: ModelId | null;
  instructions: string;
  status: AgentStatus;
  created_at: string;
  updated_at: string;
}

/**
 * One organization's account with a model provider in one environment
 * (decision 0050). Organization-scoped and environment-bound. The API key is
 * never stored by the mock, not even sealed: only what responses show (its
 * last four characters) and its version.
 */
export interface ModelConnectionRow {
  id: ModelConnectionId;
  organization_id: OrganizationId;
  environment: Environment;
  kind: ModelProviderKind;
  label: string;
  key_last4: string;
  key_version: number;
  key_updated_at: string;
  status: ModelConnectionStatus;
  created_at: string;
  updated_at: string;
}

/**
 * A model the organization added from one of its connections. Natural key
 * (organization_id, connection_id, provider_model_id); its environment is its
 * connection's.
 */
export interface ModelRow {
  id: ModelId;
  organization_id: OrganizationId;
  environment: Environment;
  connection_id: ModelConnectionId;
  provider_model_id: string;
  name: string;
  author: string;
  context_window: number | null;
  supports_tools: boolean;
  status: ModelStatus;
  created_at: string;
  updated_at: string;
}

/** An environment's default model, once someone chose one (or none). Natural key (organization_id, environment). */
export interface DefaultModelRow {
  organization_id: OrganizationId;
  environment: Environment;
  model_id: ModelId | null;
  updated_at: string;
  updated_by_user_id: UserId | null;
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
  /** The model the run calls, fixed when it's created: a queued run never switches to a later default. */
  model: RunModelRow;
  /**
   * What the run's last model turn read, in tokens: the next turn sends it all
   * again, so its provider serves it from the prompt cache.
   */
  prompt_tokens: number;
  /** Remaining simulation steps for live runs. Empty once terminal. */
  plan: RunStep[];
}

/** A run's model and its route, as resolved when the run was created. */
export interface RunModelRow {
  model_id: ModelId;
  connection_id: ModelConnectionId;
  provider_model_id: string;
  /** The connection's key version the run called with; null until it starts. */
  key_version: number | null;
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
  /** HMAC only; null for display-only historical fixture keys whose secrets were discarded. */
  secret_hash: string | null;
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

/** Organization-scoped outbox events, with a workspace only for non-test events. */
export interface WebhookEventRow {
  id: WebhookEventId;
  organization_id: OrganizationId;
  workspace_id: WorkspaceId | null;
  environment: Environment;
  body: WebhookEvent;
}

export type WebhookDeliveryRow = Pick<
  WebhookDeliverySummary,
  | "id"
  | "organization_id"
  | "endpoint_id"
  | "event_id"
  | "state"
  | "attempts"
  | "last_status_code"
  | "last_error"
  | "last_attempt_at"
  | "next_attempt_at"
  | "created_at"
>;

/** Natural key (organization, endpoint, delivery, number). */
export type WebhookDeliveryAttemptRow = {
  organization_id: OrganizationId;
  endpoint_id: WebhookEndpointId;
  delivery_id: WebhookDeliveryId;
} & WebhookDeliveryAttempt;

export interface Db {
  schema: number;
  users: User[];
  organizations: OrganizationRow[];
  memberships: MembershipRow[];
  workspaces: WorkspaceRow[];
  apps: AppRow[];
  configurations: ConfigurationRow[];
  installations: InstallationRow[];
  /**
   * Every short ID an installation was ever given, under its workspace's full
   * scope. Rows outlive their installation, so a short ID is never issued
   * twice in a workspace.
   */
  installation_short_ids: InstallationShortIdRow[];
  /** Each installation's discovered tools, under the installation's full scope. */
  installation_tools: InstallationToolRow[];
  /** When each installation's tools may next be refreshed, under the installation's full scope. */
  tool_refreshes: ToolRefreshRow[];
  /** Each organization's saved tool policy. */
  tool_policies: ToolPolicyRow[];
  /** Each organization's levels for its catalog apps' tools. */
  app_tool_levels: AppToolLevelRow[];
  connect_sessions: ConnectSessionRow[];
  agents: AgentRow[];
  grants: GrantRow[];
  runs: RunRow[];
  /** Each run's timeline, keyed by the run's full scope (`runEventsKey`); every event carries it too. */
  run_events: Record<string, RunEventRow[]>;
  connect_settings: ConnectSettingsRow[];
  /** Each organization's model provider connections, each in one environment. */
  model_connections: ModelConnectionRow[];
  /** Models each organization added from its connections. */
  models: ModelRow[];
  /** Each organization's chosen default model per environment. */
  default_models: DefaultModelRow[];
  api_keys: ApiKeyRow[];
  webhooks: WebhookEndpoint[];
  webhook_events: WebhookEventRow[];
  webhook_deliveries: WebhookDeliveryRow[];
  webhook_delivery_attempts: WebhookDeliveryAttemptRow[];
  audit: Omit<AuditEvent, "workspace">[];
  /** Idempotency keys (decision 0016), each scoped to its organization, workspace and the caller that sent it. */
  idempotency_keys: IdempotencyRow[];
  /** Local mock server key; API keys and idempotency use separate HMAC domains. */
  secret_key: string;
}

/**
 * One `Idempotency-Key` as the caller used it. `organization_id` is null only
 * for creating an organization, whose keys belong to the user alone;
 * `workspace_id` is set on workspace routes and null on organization routes.
 */
export interface IdempotencyRow {
  organization_id: OrganizationId | null;
  workspace_id: WorkspaceId | null;
  /** The credential and environment that sent the key, e.g. `member:usr_…|sandbox`, `api_key:key_…|sandbox` or `connect:cs_…|sandbox`. */
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
