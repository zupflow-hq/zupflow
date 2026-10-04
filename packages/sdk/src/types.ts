import type {
  AgentId,
  AnyId,
  ApiKeyId,
  AppId,
  AuditEventId,
  ConnectSessionId,
  GrantId,
  InstallationId,
  MemberId,
  OrganizationId,
  RunEventId,
  RunId,
  SessionId,
  ToolCallId,
  UserId,
  WebhookEndpointId,
  WorkspaceId,
} from "./ids";

/**
 * Wire contracts for the Zupflow API.
 *
 * Every field is `snake_case`, every instant is an ISO 8601 UTC string and
 * every Zupflow identifier is a prefixed, opaque string (see `ids.ts`). These
 * shapes are the contract the
 * Console is built against; the backend must serve them exactly.
 */

// ---------------------------------------------------------------------------
// Envelope

export interface Pagination {
  next_cursor: string | null;
  total: number | null;
}

export interface Meta {
  pagination: Pagination | null;
}

export interface ApiErrorBody {
  /** Stable lowercase machine code. Branch on this, never on `message`. */
  code: string;
  message: string;
  /** Field-level validation failures, keyed by `snake_case` field name. */
  fields: Record<string, string> | null;
}

export interface Envelope<T> {
  data: T | null;
  meta: Meta | null;
  error: ApiErrorBody | null;
}

export interface Page<T> {
  items: T[];
  pagination: Pagination;
}

// ---------------------------------------------------------------------------
// Enums

export const ENVIRONMENTS = ["sandbox", "production"] as const;
export type Environment = (typeof ENVIRONMENTS)[number];

/** App Kind. Only `"mcp"` is supported; future values are undefined. */
export const APP_KINDS = ["mcp"] as const;
export type AppKind = (typeof APP_KINDS)[number];

export type AppOwnerScope = "organization" | "workspace";
export type AuthType = "oauth2" | "token";
export type ConfigurationStatus = "unconfigured" | "incomplete" | "ready";
export type MemberRole = "owner" | "admin" | "developer";
export type MemberStatus = "active" | "invited";
export type ToolEffect = "read" | "write" | "destructive";

export type InstallationStatus = "connected" | "reconnect_required" | "disconnected";

export type AgentStatus = "active" | "paused";

export const RUN_STATUSES = ["queued", "running", "waiting_for_approval", "completed", "failed", "cancelled"] as const;
export type RunStatus = (typeof RUN_STATUSES)[number];
export const TERMINAL_RUN_STATUSES: readonly RunStatus[] = ["completed", "failed", "cancelled"];

export type ConnectIntent = "create" | "reconnect";
export type ConnectSessionStatus = "pending" | "completed" | "expired" | "cancelled" | "failed";

// ---------------------------------------------------------------------------
// Identity and organizations

export interface User {
  id: UserId;
  name: string;
  email: string;
  avatar_url: string | null;
}

export interface OrganizationSetup {
  has_app: boolean;
  has_ready_configuration: boolean;
  has_workspace: boolean;
  has_api_key: boolean;
  has_installation: boolean;
  has_agent: boolean;
  has_run: boolean;
}

export interface Organization {
  id: OrganizationId;
  name: string;
  slug: string;
  /** The caller's role in this organization. */
  role: MemberRole;
  created_at: string;
  setup: OrganizationSetup;
}

export interface Viewer {
  user: User;
  organizations: Organization[];
}

export interface CreateOrganizationInput {
  name: string;
  slug: string;
}

export interface UpdateOrganizationInput {
  name?: string;
  slug?: string;
}

export interface Member {
  id: MemberId;
  organization_id: OrganizationId;
  user: User;
  role: MemberRole;
  status: MemberStatus;
  invited_at: string;
  joined_at: string | null;
  last_active_at: string | null;
}

export interface InviteMemberInput {
  email: string;
  role: MemberRole;
}

export interface UpdateMemberInput {
  role: MemberRole;
}

// ---------------------------------------------------------------------------
// Workspaces

export interface WorkspaceStats {
  agents: number;
  installations: number;
  runs_last_24h: number;
}

export interface Workspace {
  id: WorkspaceId;
  organization_id: OrganizationId;
  name: string;
  slug: string;
  /** Fixed at creation. Changing it requires a designed migration. */
  environment: Environment;
  /**
   * This customer's ID in your platform (your tenant or account ID), so your
   * backend can find the workspace. Searchable; never used for authorization.
   */
  external_id: string | null;
  created_at: string;
  stats: WorkspaceStats;
}

export interface WorkspaceListQuery {
  environment?: Environment;
  /** Matches name, slug or `external_id`. */
  q?: string;
  /** `name` (default) or `activity`: most runs in the last 24 hours first. */
  sort?: "name" | "activity";
  cursor?: string;
  limit?: number;
}

/** The workspace fields shown beside resources listed across workspaces. */
export type WorkspaceRef = Pick<Workspace, "id" | "name" | "slug" | "environment">;

export interface CreateWorkspaceInput {
  name: string;
  slug: string;
  environment: Environment;
  external_id: string | null;
}

export interface UpdateWorkspaceInput {
  name?: string;
  external_id?: string | null;
}

// ---------------------------------------------------------------------------
// Apps

export interface AppIcon {
  /** One or two characters drawn on the tile. */
  glyph: string;
  /** Hex color of the tile. */
  color: string;
}

export interface OAuthScope {
  name: string;
  description: string;
}

export interface OAuthAuthTemplate {
  type: "oauth2";
  authorize_url: string;
  token_url: string;
  /** Scopes offered to environment configurations. */
  available_scopes: OAuthScope[];
  pkce: boolean;
  /** Read-only endpoint that identifies the connected account. */
  identity_url: string | null;
}

export interface TokenInput {
  key: string;
  label: string;
  secret: boolean;
  help: string | null;
  placeholder: string | null;
  /** Optional regular expression the value must match. */
  pattern: string | null;
}

export interface TokenPlacement {
  location: "header";
  name: string;
  prefix: string;
}

export interface TokenAuthTemplate {
  type: "token";
  inputs: TokenInput[];
  placement: TokenPlacement;
  /** Safe, read-only request used to verify a credential. */
  verify_url: string;
  instructions: string;
  docs_url: string | null;
}

export type AuthTemplate = OAuthAuthTemplate | TokenAuthTemplate;

export interface ToolDefinition {
  name: string;
  title: string;
  description: string;
  effect: ToolEffect;
}

export interface ConfigurationSummary {
  environment: Environment;
  status: ConfigurationStatus;
  version: number;
  updated_at: string | null;
}

export interface AppDefinition {
  id: AppId;
  organization_id: OrganizationId;
  /** `null` for an organization app; the owning workspace for a workspace app. */
  owner_workspace_id: WorkspaceId | null;
  owner_scope: AppOwnerScope;
  kind: AppKind;
  name: string;
  slug: string;
  description: string;
  icon: AppIcon;
  auth: AuthTemplate;
  tools: ToolDefinition[];
  version: number;
  /**
   * Organization apps carry both environments. A workspace app carries only
   * its workspace's environment.
   */
  configurations: ConfigurationSummary[];
  installation_count: number;
  created_at: string;
  updated_at: string;
}

export interface CreateAppInput {
  kind: AppKind;
  name: string;
  slug: string;
  description: string;
  auth_type: AuthType;
}

export interface UpdateAppInput {
  name?: string;
  description?: string;
}

export interface SecretState {
  set: boolean;
  last4: string | null;
  updated_at: string | null;
}

export interface OAuthClientSettings {
  client_id: string | null;
  client_secret: SecretState;
  /** Fixed provider callback registered with the provider. Server-derived. */
  redirect_uri: string;
  scopes: string[];
}

export interface McpSettings {
  server_url: string | null;
  transport: "streamable_http";
}

export interface ClientConfiguration {
  app_id: AppId;
  environment: Environment;
  status: ConfigurationStatus;
  version: number;
  mcp: McpSettings;
  /** Present only when the app's authentication type is OAuth. */
  oauth: OAuthClientSettings | null;
  /** Fields still required before the configuration is ready. */
  missing: string[];
  updated_at: string | null;
  updated_by: User | null;
}

export interface UpdateConfigurationInput {
  mcp_server_url?: string | null;
  oauth_client_id?: string | null;
  /** Write-only. Omit to keep the stored secret. */
  oauth_client_secret?: string;
  oauth_scopes?: string[];
}

/** An app a workspace may install: eligible, ready and visible to the caller. */
export interface AvailableApp {
  app: AppDefinition;
  configuration: ConfigurationSummary;
}

// ---------------------------------------------------------------------------
// Installations and connections

export interface ConnectedAccount {
  external_id: string;
  display_name: string;
  email: string | null;
}

export interface AccountConnection {
  account: ConnectedAccount;
  granted_scopes: string[];
  credential_version: number;
  connected_at: string;
  expires_at: string | null;
}

export interface InstallationApp {
  id: AppId;
  name: string;
  slug: string;
  owner_scope: AppOwnerScope;
  icon: AppIcon;
  auth_type: AuthType;
}

export interface Installation {
  id: InstallationId;
  workspace_id: WorkspaceId;
  app: InstallationApp;
  label: string;
  status: InstallationStatus;
  configuration_version: number;
  connection: AccountConnection | null;
  grant_count: number;
  created_at: string;
  updated_at: string;
}

/** An installation seen from its app's organization-level page. */
export interface AppInstallationRef {
  installation: Installation;
  workspace: WorkspaceRef;
}

export interface UpdateInstallationInput {
  label: string;
}

export interface ConnectSession {
  id: ConnectSessionId;
  workspace_id: WorkspaceId;
  environment: Environment;
  app: InstallationApp;
  intent: ConnectIntent;
  installation_id: InstallationId | null;
  label: string;
  status: ConnectSessionStatus;
  return_url: string | null;
  created_at: string;
  expires_at: string;
  completed_at: string | null;
  failure_code: string | null;
  /** Present on the hosted Connect read so the page can render its form. */
  auth: AuthTemplate | null;
  requested_scopes: string[];
  /** For a reconnect: the account that must be used again. */
  expected_account: ConnectedAccount | null;
}

/**
 * The creating response, the only one that carries the session's link. The
 * link's reference authorizes the hosted page, so it's shown once, like a
 * secret, and never returned by a read.
 */
export interface CreatedConnectSession extends ConnectSession {
  /**
   * The hosted Connect page for this session. Send the user here (redirect or
   * popup); Zupflow runs the provider flow and returns them to `return_url`.
   */
  url: string;
}

export interface CreateConnectSessionInput {
  app_id: AppId;
  intent: ConnectIntent;
  label: string;
  installation_id: InstallationId | null;
  return_url: string | null;
}

export interface SubmitConnectCredentialsInput {
  values: Record<string, string>;
}

export interface ConnectResult {
  session: ConnectSession;
  installation: Installation;
}

export interface AuthorizeConnectResult {
  /** Top-level navigation target for the provider's consent screen. */
  authorization_url: string;
}

// ---------------------------------------------------------------------------
// Agents and grants

export interface AgentGrant {
  id: GrantId;
  workspace_id: WorkspaceId;
  agent_id: AgentId;
  installation_id: InstallationId;
  /** Tools the agent may call through this installation. */
  tools: string[];
  /** Subset of `tools` that pause the run for human approval. */
  approval_required: string[];
  created_at: string;
  updated_at: string;
}

export interface Agent {
  id: AgentId;
  workspace_id: WorkspaceId;
  name: string;
  description: string;
  model: string;
  instructions: string;
  status: AgentStatus;
  grant_count: number;
  runs_last_24h: number;
  last_run_at: string | null;
  created_at: string;
  updated_at: string;
}

export interface CreateAgentInput {
  name: string;
  description: string;
  model: string;
  instructions: string;
}

export interface UpdateAgentInput {
  name?: string;
  description?: string;
  model?: string;
  instructions?: string;
  status?: AgentStatus;
}

export interface CreateGrantInput {
  installation_id: InstallationId;
  tools: string[];
  approval_required: string[];
}

export interface UpdateGrantInput {
  tools: string[];
  approval_required: string[];
}

export interface ModelOption {
  id: string;
  name: string;
  provider: string;
  description: string;
}

// ---------------------------------------------------------------------------
// Runs

export interface RunUsage {
  input_tokens: number;
  output_tokens: number;
  tool_calls: number;
}

export interface RunError {
  code: string;
  message: string;
}

export interface Run {
  id: RunId;
  workspace_id: WorkspaceId;
  agent_id: AgentId;
  agent_name: string;
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
}

export interface CreateRunInput {
  input: string;
}

export interface RunListQuery {
  agent_id?: AgentId;
  status?: RunStatus;
  cursor?: string;
  limit?: number;
}

/** A run listed across an organization's workspaces. */
export interface OrganizationRun extends Run {
  workspace: WorkspaceRef;
}

export interface OrganizationRunListQuery {
  environment?: Environment;
  workspace_id?: WorkspaceId;
  status?: RunStatus;
  cursor?: string;
  limit?: number;
}

interface RunEventBase {
  id: RunEventId;
  run_id: RunId;
  workspace_id: WorkspaceId;
  sequence: number;
  created_at: string;
}

export interface ToolCallRef {
  call_id: ToolCallId;
  installation_id: InstallationId;
  installation_label: string;
  app_name: string;
  tool: string;
}

export type RunEvent = RunEventBase &
  (
    | { type: "run.queued"; data: Record<string, never> }
    | { type: "run.started"; data: { model: string } }
    | { type: "message.assistant"; data: { text: string } }
    | { type: "tool_call.started"; data: ToolCallRef & { arguments: unknown } }
    | {
        type: "tool_call.completed";
        data: ToolCallRef & { result: unknown; duration_ms: number };
      }
    | {
        type: "tool_call.failed";
        data: ToolCallRef & { error: RunError; duration_ms: number };
      }
    | { type: "approval.requested"; data: ToolCallRef & { arguments: unknown } }
    | {
        type: "approval.resolved";
        /** The API key that sent the decision. Who decided is the organization's business, not recorded here. */
        data: { call_id: ToolCallId; decision: ApprovalDecision; api_key_id: ApiKeyId };
      }
    | { type: "run.completed"; data: { usage: RunUsage } }
    | { type: "run.failed"; data: { error: RunError } }
    | { type: "run.cancelled"; data: { cancelled_by: string } }
  );

export type RunEventType = RunEvent["type"];

export type ApprovalDecision = "approved" | "denied";

export interface ResolveApprovalInput {
  call_id: ToolCallId;
  decision: ApprovalDecision;
}

// ---------------------------------------------------------------------------
// Developer settings

export interface ConnectSettings {
  environment: Environment;
  /** Exact URLs Connect may return to. No wildcards. */
  return_urls: string[];
  /** Origins allowed to embed Connect and receive its browser messages. */
  allowed_origins: string[];
  updated_at: string | null;
}

export interface UpdateConnectSettingsInput {
  return_urls: string[];
  allowed_origins: string[];
}

export interface ApiKey {
  id: ApiKeyId;
  organization_id: OrganizationId;
  environment: Environment;
  name: string;
  /** Public selector shown in lists, e.g. `zf_live_9KcB`. */
  prefix: string;
  last4: string;
  created_by: User;
  created_at: string;
  last_used_at: string | null;
  revoked_at: string | null;
}

/** Returned once, on creation. The secret is never readable again. */
export interface CreatedApiKey {
  key: ApiKey;
  secret: string;
}

export interface CreateApiKeyInput {
  name: string;
  environment: Environment;
}

export const WEBHOOK_EVENT_TYPES = [
  "installation.connected",
  "installation.reconnect_required",
  "installation.disconnected",
  "connect_session.completed",
  "connect_session.failed",
  "run.completed",
  "run.failed",
  "run.waiting_for_approval",
] as const;
export type WebhookEventType = (typeof WEBHOOK_EVENT_TYPES)[number];

export interface WebhookDelivery {
  status_code: number;
  delivered_at: string;
}

export interface WebhookEndpoint {
  id: WebhookEndpointId;
  organization_id: OrganizationId;
  environment: Environment;
  url: string;
  description: string;
  events: WebhookEventType[];
  enabled: boolean;
  signing_secret_last4: string;
  created_at: string;
  last_delivery: WebhookDelivery | null;
}

export interface CreatedWebhookEndpoint {
  endpoint: WebhookEndpoint;
  signing_secret: string;
}

export interface CreateWebhookEndpointInput {
  environment: Environment;
  url: string;
  description: string;
  events: WebhookEventType[];
}

export interface UpdateWebhookEndpointInput {
  url?: string;
  description?: string;
  events?: WebhookEventType[];
  enabled?: boolean;
}

// ---------------------------------------------------------------------------
// Audit and usage

export interface AuditActor {
  type: "user" | "api_key" | "system";
  /** The user or API key; `null` for Zupflow itself (`system`). */
  id: AnyId | null;
  name: string;
}

export interface AuditTarget {
  type: string;
  /** `null` for targets that aren't objects, such as an environment's Connect settings. */
  id: AnyId | null;
  name: string;
}

export interface AuditEvent {
  id: AuditEventId;
  organization_id: OrganizationId;
  workspace_id: WorkspaceId | null;
  /** The workspace the event happened in, named so the log reads on its own. */
  workspace: WorkspaceRef | null;
  environment: Environment | null;
  actor: AuditActor;
  action: string;
  target: AuditTarget;
  created_at: string;
}

export interface AuditListQuery {
  workspace_id?: WorkspaceId;
  cursor?: string;
  limit?: number;
}

export interface UsageDay {
  date: string;
  runs: number;
  failed_runs: number;
  tool_calls: number;
}

export interface UsageReport {
  environment: Environment;
  days: UsageDay[];
  totals: {
    runs: number;
    failed_runs: number;
    tool_calls: number;
  };
}
