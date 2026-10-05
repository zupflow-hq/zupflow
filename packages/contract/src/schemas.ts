/**
 * The Zupflow API contract: every request and response, as one zod schema
 * each. TypeScript types are inferred from these, and `openapi.json` is
 * generated from them; the mock is checked against them at runtime and the
 * backend must serve them exactly.
 *
 * Every field is `snake_case`, every instant an ISO 8601 UTC string, every
 * Zupflow ID a prefixed opaque string (see `ids.ts`). A schema with an `id`
 * in its metadata becomes a named component in the OpenAPI document.
 */
import { z } from "zod";
import { AnyIdSchema, idSchema } from "./ids";
import { registry } from "./registry";

function named<T extends z.ZodType>(id: string, schema: T): T {
  registry.add(schema, { id });
  return schema;
}
/** An instant: ISO 8601 in UTC, ending in `Z`. */
const instant = named("Instant", z.iso.datetime());

// ---------------------------------------------------------------------------
// Envelope

export const Pagination = named(
  "Pagination",
  z.object({ next_cursor: z.string().nullable(), total: z.number().int().nullable() }),
);
export type Pagination = z.infer<typeof Pagination>;

export const Meta = named("Meta", z.object({ pagination: Pagination.nullable() }));
export type Meta = z.infer<typeof Meta>;

export const ApiErrorBody = named(
  "Error",
  z.object({
    /** Stable lowercase machine code. Branch on this, never on `message`. */
    code: z.string(),
    message: z.string(),
    /** Field-level validation failures, keyed by `snake_case` field name. */
    fields: z.record(z.string(), z.string()).nullable(),
  }),
);
export type ApiErrorBody = z.infer<typeof ApiErrorBody>;

/** Every response: exactly one of `data` and `error` is set. */
export interface Envelope<T> {
  data: T | null;
  meta: Meta | null;
  error: ApiErrorBody | null;
}

// ---------------------------------------------------------------------------
// Enums

export const ENVIRONMENTS = ["sandbox", "production"] as const;
export const Environment = named("Environment", z.enum(ENVIRONMENTS));
export type Environment = z.infer<typeof Environment>;

/** App kind. Only `"mcp"` is supported; future values are undefined. */
export const APP_KINDS = ["mcp"] as const;
export const AppKind = z.enum(APP_KINDS);
export type AppKind = z.infer<typeof AppKind>;

export const AppOwnerScope = z.enum(["organization", "workspace"]);
export type AppOwnerScope = z.infer<typeof AppOwnerScope>;
export const AuthType = z.enum(["oauth2", "token"]);
export type AuthType = z.infer<typeof AuthType>;
export const ConfigurationStatus = z.enum(["unconfigured", "incomplete", "ready"]);
export type ConfigurationStatus = z.infer<typeof ConfigurationStatus>;
/** `viewer` is read-only: it sees what a developer sees and changes nothing. */
export const MemberRole = z.enum(["owner", "admin", "developer", "viewer"]);
export type MemberRole = z.infer<typeof MemberRole>;
export const MemberStatus = z.enum(["active", "invited"]);
export type MemberStatus = z.infer<typeof MemberStatus>;
export const ToolEffect = z.enum(["read", "write", "destructive"]);
export type ToolEffect = z.infer<typeof ToolEffect>;
export const InstallationStatus = z.enum(["connected", "reconnect_required", "disconnected"]);
export type InstallationStatus = z.infer<typeof InstallationStatus>;
export const AgentStatus = z.enum(["active", "paused"]);
export type AgentStatus = z.infer<typeof AgentStatus>;

export const RUN_STATUSES = ["queued", "running", "waiting_for_approval", "completed", "failed", "cancelled"] as const;
export const RunStatus = z.enum(RUN_STATUSES);
export type RunStatus = z.infer<typeof RunStatus>;
export const TERMINAL_RUN_STATUSES: readonly RunStatus[] = ["completed", "failed", "cancelled"];

export const ConnectIntent = z.enum(["create", "reconnect"]);
export type ConnectIntent = z.infer<typeof ConnectIntent>;
export const ConnectSessionStatus = z.enum(["pending", "completed", "expired", "cancelled", "failed"]);
export type ConnectSessionStatus = z.infer<typeof ConnectSessionStatus>;

// ---------------------------------------------------------------------------
// Identity and organizations

export const User = named(
  "User",
  z.object({ id: idSchema("user"), name: z.string(), email: z.email(), avatar_url: z.url().nullable() }),
);
export type User = z.infer<typeof User>;

export const OrganizationSetup = named(
  "OrganizationSetup",
  z.object({
    has_app: z.boolean(),
    has_ready_configuration: z.boolean(),
    has_workspace: z.boolean(),
    has_api_key: z.boolean(),
    has_installation: z.boolean(),
    has_agent: z.boolean(),
    has_run: z.boolean(),
  }),
);
export type OrganizationSetup = z.infer<typeof OrganizationSetup>;

export const Organization = named(
  "Organization",
  z.object({
    id: idSchema("organization"),
    name: z.string(),
    slug: z.string(),
    /** The caller's role in this organization. */
    role: MemberRole,
    created_at: instant,
    setup: OrganizationSetup,
  }),
);
export type Organization = z.infer<typeof Organization>;

export const Viewer = named("Viewer", z.object({ user: User, organizations: z.array(Organization) }));
export type Viewer = z.infer<typeof Viewer>;

export const CreateOrganizationInput = z.object({ name: z.string(), slug: z.string() });
export type CreateOrganizationInput = z.infer<typeof CreateOrganizationInput>;
export const UpdateOrganizationInput = z.object({ name: z.string().optional(), slug: z.string().optional() });
export type UpdateOrganizationInput = z.infer<typeof UpdateOrganizationInput>;

export const Member = named(
  "Member",
  z.object({
    id: idSchema("member"),
    organization_id: idSchema("organization"),
    user: User,
    role: MemberRole,
    status: MemberStatus,
    invited_at: instant,
    joined_at: instant.nullable(),
    last_active_at: instant.nullable(),
  }),
);
export type Member = z.infer<typeof Member>;

export const InviteMemberInput = z.object({ email: z.string(), role: MemberRole });
export type InviteMemberInput = z.infer<typeof InviteMemberInput>;
export const UpdateMemberInput = z.object({ role: MemberRole });
export type UpdateMemberInput = z.infer<typeof UpdateMemberInput>;

// ---------------------------------------------------------------------------
// Workspaces

export const WorkspaceStats = named(
  "WorkspaceStats",
  z.object({ agents: z.number().int(), installations: z.number().int(), runs_last_24h: z.number().int() }),
);
export type WorkspaceStats = z.infer<typeof WorkspaceStats>;

const workspaceFields = {
  id: idSchema("workspace"),
  organization_id: idSchema("organization"),
  name: z.string(),
  slug: z.string(),
  /** Fixed at creation. Changing it requires a designed migration. */
  environment: Environment,
  /**
   * This customer's ID in your platform (your tenant or account ID), so your
   * backend can find the workspace. Searchable; never used for authorization.
   */
  external_id: z.string().nullable(),
  created_at: instant,
  stats: WorkspaceStats,
};
export const Workspace = named("Workspace", z.object(workspaceFields));
export type Workspace = z.infer<typeof Workspace>;

/** The workspace fields shown beside resources listed across workspaces. */
export const WorkspaceRef = named(
  "WorkspaceRef",
  z.object({
    id: workspaceFields.id,
    name: workspaceFields.name,
    slug: workspaceFields.slug,
    environment: workspaceFields.environment,
  }),
);
export type WorkspaceRef = z.infer<typeof WorkspaceRef>;

export const WorkspaceListQuery = z.object({
  environment: Environment.optional(),
  /** Matches name, slug or `external_id`. */
  q: z.string().optional(),
  /** `name` (default) or `activity`: most runs in the last 24 hours first. */
  sort: z.enum(["name", "activity"]).optional(),
  cursor: z.string().optional(),
  limit: z
    .number()
    .int()
    .optional()
    .describe("Page size, 1 to 100 (default 25); values outside are clamped, and a non-integer means the default."),
});
export type WorkspaceListQuery = z.infer<typeof WorkspaceListQuery>;

export const CreateWorkspaceInput = z.object({
  name: z.string(),
  slug: z.string(),
  environment: Environment,
  external_id: z.string().nullable(),
});
export type CreateWorkspaceInput = z.infer<typeof CreateWorkspaceInput>;
export const UpdateWorkspaceInput = z.object({
  name: z.string().optional(),
  external_id: z.string().nullable().optional(),
});
export type UpdateWorkspaceInput = z.infer<typeof UpdateWorkspaceInput>;

// ---------------------------------------------------------------------------
// Apps

export const AppIcon = named(
  "AppIcon",
  z.object({
    /** One or two characters drawn on the tile. */
    glyph: z.string(),
    /** Hex color of the tile. */
    color: z.string(),
  }),
);
export type AppIcon = z.infer<typeof AppIcon>;

export const OAuthScope = z.object({ name: z.string(), description: z.string() });
export type OAuthScope = z.infer<typeof OAuthScope>;

export const OAuthAuthTemplate = named(
  "OAuthAuthTemplate",
  z.object({
    type: z.literal("oauth2"),
    /** `null` when not set. */
    authorize_url: z.url().nullable(),
    /** `null` when not set. */
    token_url: z.url().nullable(),
    /** Scopes offered to environment configurations. */
    available_scopes: z.array(OAuthScope),
    pkce: z.boolean(),
    /** Read-only endpoint that identifies the connected account. */
    identity_url: z.url().nullable(),
  }),
);
export type OAuthAuthTemplate = z.infer<typeof OAuthAuthTemplate>;

export const TokenInput = z.object({
  key: z.string(),
  label: z.string(),
  secret: z.boolean(),
  help: z.string().nullable(),
  placeholder: z.string().nullable(),
  /** Optional regular expression the value must match. */
  pattern: z.string().nullable(),
});
export type TokenInput = z.infer<typeof TokenInput>;

export const TokenPlacement = z.object({ location: z.literal("header"), name: z.string(), prefix: z.string() });
export type TokenPlacement = z.infer<typeof TokenPlacement>;

export const TokenAuthTemplate = named(
  "TokenAuthTemplate",
  z.object({
    type: z.literal("token"),
    inputs: z.array(TokenInput),
    placement: TokenPlacement,
    /** Safe, read-only request used to verify a credential; `null` when not set. */
    verify_url: z.url().nullable(),
    instructions: z.string(),
    docs_url: z.url().nullable(),
  }),
);
export type TokenAuthTemplate = z.infer<typeof TokenAuthTemplate>;

export const AuthTemplate = named("AuthTemplate", z.discriminatedUnion("type", [OAuthAuthTemplate, TokenAuthTemplate]));
export type AuthTemplate = z.infer<typeof AuthTemplate>;

export const ToolDefinition = named(
  "ToolDefinition",
  z.object({ name: z.string(), title: z.string(), description: z.string(), effect: ToolEffect }),
);
export type ToolDefinition = z.infer<typeof ToolDefinition>;

export const ConfigurationSummary = named(
  "ConfigurationSummary",
  z.object({
    environment: Environment,
    status: ConfigurationStatus,
    version: z.number().int(),
    updated_at: instant.nullable(),
  }),
);
export type ConfigurationSummary = z.infer<typeof ConfigurationSummary>;

export const AppDefinition = named(
  "App",
  z.object({
    id: idSchema("app"),
    organization_id: idSchema("organization"),
    /** `null` for an organization app; the owning workspace for a workspace app. */
    owner_workspace_id: idSchema("workspace").nullable(),
    owner_scope: AppOwnerScope,
    kind: AppKind,
    name: z.string(),
    slug: z.string(),
    description: z.string(),
    icon: AppIcon,
    auth: AuthTemplate,
    tools: z.array(ToolDefinition),
    version: z.number().int(),
    /**
     * Organization apps carry both environments. A workspace app carries only
     * its workspace's environment.
     */
    configurations: z.array(ConfigurationSummary),
    installation_count: z.number().int(),
    created_at: instant,
    updated_at: instant,
  }),
);
export type AppDefinition = z.infer<typeof AppDefinition>;

export const CreateAppInput = z.object({
  kind: AppKind,
  name: z.string(),
  slug: z.string(),
  description: z.string(),
  auth_type: AuthType,
});
export type CreateAppInput = z.infer<typeof CreateAppInput>;
export const UpdateAppInput = z.object({ name: z.string().optional(), description: z.string().optional() });
export type UpdateAppInput = z.infer<typeof UpdateAppInput>;

export const SecretState = named(
  "SecretState",
  z.object({ set: z.boolean(), last4: z.string().nullable(), updated_at: instant.nullable() }),
);
export type SecretState = z.infer<typeof SecretState>;

export const OAuthClientSettings = z.object({
  client_id: z.string().nullable(),
  client_secret: SecretState,
  /** Fixed provider callback registered with the provider. Server-derived. */
  redirect_uri: z.url(),
  scopes: z.array(z.string()),
});
export type OAuthClientSettings = z.infer<typeof OAuthClientSettings>;

export const McpSettings = z.object({ server_url: z.url().nullable(), transport: z.literal("streamable_http") });
export type McpSettings = z.infer<typeof McpSettings>;

export const ClientConfiguration = named(
  "ClientConfiguration",
  z.object({
    app_id: idSchema("app"),
    environment: Environment,
    status: ConfigurationStatus,
    version: z.number().int(),
    mcp: McpSettings,
    /** Present only when the app's authentication type is OAuth. */
    oauth: OAuthClientSettings.nullable(),
    /** Fields still required before the configuration is ready. */
    missing: z.array(z.string()),
    updated_at: instant.nullable(),
    updated_by: User.nullable(),
  }),
);
export type ClientConfiguration = z.infer<typeof ClientConfiguration>;

export const UpdateConfigurationInput = z.object({
  mcp_server_url: z.string().nullable().optional(),
  oauth_client_id: z.string().nullable().optional(),
  /** Write-only. Omit to keep the stored secret. */
  oauth_client_secret: z.string().optional(),
  oauth_scopes: z.array(z.string()).optional(),
});
export type UpdateConfigurationInput = z.infer<typeof UpdateConfigurationInput>;

/** An app a workspace may install: eligible, ready and visible to the caller. */
export const AvailableApp = named(
  "AvailableApp",
  z.object({ app: AppDefinition, configuration: ConfigurationSummary }),
);
export type AvailableApp = z.infer<typeof AvailableApp>;

// ---------------------------------------------------------------------------
// Installations and connections

export const ConnectedAccount = named(
  "ConnectedAccount",
  z.object({ external_id: z.string(), display_name: z.string(), email: z.email().nullable() }),
);
export type ConnectedAccount = z.infer<typeof ConnectedAccount>;

export const AccountConnection = named(
  "AccountConnection",
  z.object({
    account: ConnectedAccount,
    granted_scopes: z.array(z.string()),
    credential_version: z.number().int(),
    connected_at: instant,
    expires_at: instant.nullable(),
  }),
);
export type AccountConnection = z.infer<typeof AccountConnection>;

export const InstallationApp = named(
  "InstallationApp",
  z.object({
    id: idSchema("app"),
    name: z.string(),
    slug: z.string(),
    owner_scope: AppOwnerScope,
    icon: AppIcon,
    auth_type: AuthType,
  }),
);
export type InstallationApp = z.infer<typeof InstallationApp>;

export const Installation = named(
  "Installation",
  z.object({
    id: idSchema("installation"),
    workspace_id: idSchema("workspace"),
    app: InstallationApp,
    label: z.string(),
    status: InstallationStatus,
    configuration_version: z.number().int(),
    connection: AccountConnection.nullable(),
    grant_count: z.number().int(),
    created_at: instant,
    updated_at: instant,
  }),
);
export type Installation = z.infer<typeof Installation>;

/** An installation seen from its app's organization-level page. */
export const AppInstallationRef = named(
  "AppInstallationRef",
  z.object({ installation: Installation, workspace: WorkspaceRef }),
);
export type AppInstallationRef = z.infer<typeof AppInstallationRef>;

export const UpdateInstallationInput = z.object({ label: z.string() });
export type UpdateInstallationInput = z.infer<typeof UpdateInstallationInput>;

const connectSessionFields = {
  id: idSchema("connect_session"),
  workspace_id: idSchema("workspace"),
  environment: Environment,
  app: InstallationApp,
  intent: ConnectIntent,
  installation_id: idSchema("installation").nullable(),
  label: z.string(),
  status: ConnectSessionStatus,
  return_url: z.url().nullable(),
  created_at: instant,
  expires_at: instant,
  completed_at: instant.nullable(),
  failure_code: z.string().nullable(),
  /** Present on the hosted Connect read so the page can render its form. */
  auth: AuthTemplate.nullable(),
  requested_scopes: z.array(z.string()),
  /** For a reconnect: the account that must be used again. */
  expected_account: ConnectedAccount.nullable(),
};
export const ConnectSession = named("ConnectSession", z.object(connectSessionFields));
export type ConnectSession = z.infer<typeof ConnectSession>;

/**
 * The creating response, the only one that carries the session's link. The
 * link's reference authorizes the hosted page, so it's shown once, like a
 * secret, and never returned by a read.
 */
export const CreatedConnectSession = named(
  "CreatedConnectSession",
  z.object({
    ...connectSessionFields,
    /**
     * The hosted Connect page for this session. Send the user here (redirect
     * or popup); Zupflow runs the provider flow and returns them to
     * `return_url`.
     */
    url: z.url(),
  }),
);
export type CreatedConnectSession = z.infer<typeof CreatedConnectSession>;

export const CreateConnectSessionInput = z.object({
  app_id: idSchema("app"),
  intent: ConnectIntent,
  label: z.string(),
  installation_id: idSchema("installation").nullable(),
  return_url: z.string().nullable(),
});
export type CreateConnectSessionInput = z.infer<typeof CreateConnectSessionInput>;

export const SubmitConnectCredentialsInput = z.object({ values: z.record(z.string(), z.string()) });
export type SubmitConnectCredentialsInput = z.infer<typeof SubmitConnectCredentialsInput>;

export const ConnectResult = named("ConnectResult", z.object({ session: ConnectSession, installation: Installation }));
export type ConnectResult = z.infer<typeof ConnectResult>;

export const AuthorizeConnectResult = named(
  "AuthorizeConnectResult",
  z.object({
    /** Top-level navigation target for the provider's consent screen. */
    authorization_url: z.url(),
  }),
);
export type AuthorizeConnectResult = z.infer<typeof AuthorizeConnectResult>;

// ---------------------------------------------------------------------------
// Agents and grants

export const AgentGrant = named(
  "Grant",
  z.object({
    id: idSchema("grant"),
    workspace_id: idSchema("workspace"),
    agent_id: idSchema("agent"),
    installation_id: idSchema("installation"),
    /** Tools the agent may call through this installation. */
    tools: z.array(z.string()),
    /** Subset of `tools` that pause the run for human approval. */
    approval_required: z.array(z.string()),
    created_at: instant,
    updated_at: instant,
  }),
);
export type AgentGrant = z.infer<typeof AgentGrant>;

export const Agent = named(
  "Agent",
  z.object({
    id: idSchema("agent"),
    workspace_id: idSchema("workspace"),
    name: z.string(),
    description: z.string(),
    model: z.string(),
    instructions: z.string(),
    status: AgentStatus,
    grant_count: z.number().int(),
    runs_last_24h: z.number().int(),
    last_run_at: instant.nullable(),
    created_at: instant,
    updated_at: instant,
  }),
);
export type Agent = z.infer<typeof Agent>;

export const CreateAgentInput = z.object({
  name: z.string(),
  description: z.string(),
  model: z.string(),
  instructions: z.string(),
});
export type CreateAgentInput = z.infer<typeof CreateAgentInput>;
export const UpdateAgentInput = z.object({
  name: z.string().optional(),
  description: z.string().optional(),
  model: z.string().optional(),
  instructions: z.string().optional(),
  status: AgentStatus.optional(),
});
export type UpdateAgentInput = z.infer<typeof UpdateAgentInput>;

export const CreateGrantInput = z.object({
  installation_id: idSchema("installation"),
  tools: z.array(z.string()),
  approval_required: z.array(z.string()),
});
export type CreateGrantInput = z.infer<typeof CreateGrantInput>;
export const UpdateGrantInput = z.object({ tools: z.array(z.string()), approval_required: z.array(z.string()) });
export type UpdateGrantInput = z.infer<typeof UpdateGrantInput>;

export const ModelOption = named(
  "Model",
  z.object({ id: z.string(), name: z.string(), provider: z.string(), description: z.string() }),
);
export type ModelOption = z.infer<typeof ModelOption>;

// ---------------------------------------------------------------------------
// Runs

export const RunUsage = named(
  "RunUsage",
  z.object({ input_tokens: z.number().int(), output_tokens: z.number().int(), tool_calls: z.number().int() }),
);
export type RunUsage = z.infer<typeof RunUsage>;

export const RunError = named("RunError", z.object({ code: z.string(), message: z.string() }));
export type RunError = z.infer<typeof RunError>;

const runFields = {
  id: idSchema("run"),
  workspace_id: idSchema("workspace"),
  agent_id: idSchema("agent"),
  agent_name: z.string(),
  session_id: idSchema("session"),
  trigger: z.enum(["api", "console"]),
  status: RunStatus,
  input: z.string(),
  output: z.string().nullable(),
  usage: RunUsage,
  error: RunError.nullable(),
  created_at: instant,
  started_at: instant.nullable(),
  finished_at: instant.nullable(),
};
export const Run = named("Run", z.object(runFields));
export type Run = z.infer<typeof Run>;

/** A run listed across an organization's workspaces. */
export const OrganizationRun = named("OrganizationRun", z.object({ ...runFields, workspace: WorkspaceRef }));
export type OrganizationRun = z.infer<typeof OrganizationRun>;

export const CreateRunInput = z.object({ input: z.string() });
export type CreateRunInput = z.infer<typeof CreateRunInput>;

export const RunListQuery = z.object({
  agent_id: idSchema("agent").optional().describe("Only this agent's runs. A malformed or unknown ID matches nothing."),
  status: RunStatus.optional(),
  cursor: z.string().optional(),
  limit: z
    .number()
    .int()
    .optional()
    .describe("Page size, 1 to 100 (default 25); values outside are clamped, and a non-integer means the default."),
});
export type RunListQuery = z.infer<typeof RunListQuery>;

export const OrganizationRunListQuery = z.object({
  environment: Environment.optional(),
  workspace_id: idSchema("workspace")
    .optional()
    .describe("Only this workspace's rows. A malformed or unknown ID matches nothing."),
  status: RunStatus.optional(),
  cursor: z.string().optional(),
  limit: z
    .number()
    .int()
    .optional()
    .describe("Page size, 1 to 100 (default 25); values outside are clamped, and a non-integer means the default."),
});
export type OrganizationRunListQuery = z.infer<typeof OrganizationRunListQuery>;

export const ToolCallRef = z.object({
  call_id: idSchema("tool_call"),
  installation_id: idSchema("installation"),
  installation_label: z.string(),
  app_name: z.string(),
  tool: z.string(),
});
export type ToolCallRef = z.infer<typeof ToolCallRef>;

export const ApprovalDecision = z.enum(["approved", "denied"]);
export type ApprovalDecision = z.infer<typeof ApprovalDecision>;

const runEventBase = {
  id: idSchema("run_event"),
  run_id: idSchema("run"),
  workspace_id: idSchema("workspace"),
  sequence: z.number().int(),
  created_at: instant,
};
const runEvent = <T extends string, D extends z.ZodType>(type: T, data: D) =>
  z.object({ ...runEventBase, type: z.literal(type), data });

export const RunEvent = named(
  "RunEvent",
  z.discriminatedUnion("type", [
    runEvent("run.queued", z.object({})),
    runEvent("run.started", z.object({ model: z.string() })),
    runEvent("message.assistant", z.object({ text: z.string() })),
    runEvent("tool_call.started", ToolCallRef.extend({ arguments: z.unknown() })),
    runEvent("tool_call.completed", ToolCallRef.extend({ result: z.unknown(), duration_ms: z.number() })),
    runEvent("tool_call.failed", ToolCallRef.extend({ error: RunError, duration_ms: z.number() })),
    runEvent("approval.requested", ToolCallRef.extend({ arguments: z.unknown() })),
    runEvent(
      "approval.resolved",
      z.object({
        call_id: idSchema("tool_call"),
        decision: ApprovalDecision,
        /** The API key that sent the decision. Who decided is the organization's business, not recorded here. */
        api_key_id: idSchema("api_key"),
      }),
    ),
    runEvent("run.completed", z.object({ usage: RunUsage })),
    runEvent("run.failed", z.object({ error: RunError })),
    runEvent("run.cancelled", z.object({ cancelled_by: z.string() })),
  ]),
);
export type RunEvent = z.infer<typeof RunEvent>;
export type RunEventType = RunEvent["type"];

export const ResolveApprovalInput = z.object({ call_id: idSchema("tool_call"), decision: ApprovalDecision });
export type ResolveApprovalInput = z.infer<typeof ResolveApprovalInput>;

// ---------------------------------------------------------------------------
// Developer settings

export const ConnectSettings = named(
  "ConnectSettings",
  z.object({
    environment: Environment,
    /** Exact URLs Connect may return to. No wildcards. */
    return_urls: z.array(z.url()),
    /** Origins allowed to embed Connect and receive its browser messages. */
    allowed_origins: z.array(z.string()),
    updated_at: instant.nullable(),
  }),
);
export type ConnectSettings = z.infer<typeof ConnectSettings>;

export const UpdateConnectSettingsInput = z.object({
  return_urls: z.array(z.string()),
  allowed_origins: z.array(z.string()),
});
export type UpdateConnectSettingsInput = z.infer<typeof UpdateConnectSettingsInput>;

export const ApiKey = named(
  "ApiKey",
  z.object({
    id: idSchema("api_key"),
    organization_id: idSchema("organization"),
    environment: Environment,
    name: z.string(),
    /** Public selector shown in lists, e.g. `zf_live_9KcB`. */
    prefix: z.string(),
    last4: z.string(),
    created_by: User,
    created_at: instant,
    last_used_at: instant.nullable(),
    revoked_at: instant.nullable(),
  }),
);
export type ApiKey = z.infer<typeof ApiKey>;

/** Returned once, on creation. The secret is never readable again. */
export const CreatedApiKey = named("CreatedApiKey", z.object({ key: ApiKey, secret: z.string() }));
export type CreatedApiKey = z.infer<typeof CreatedApiKey>;

export const CreateApiKeyInput = z.object({ name: z.string(), environment: Environment });
export type CreateApiKeyInput = z.infer<typeof CreateApiKeyInput>;

export const WEBHOOK_EVENT_TYPES = [
  "installation.connected",
  "installation.reconnect_required",
  "installation.disconnected",
  "connect_session.completed",
  "connect_session.failed",
  "run.completed",
  "run.failed",
  "run.waiting_for_approval",
  "run.cancelled",
] as const;
/** The event types an endpoint can subscribe to. `webhook.test` is sent only by the test button. */
export const WebhookEventType = z.enum(WEBHOOK_EVENT_TYPES);
export type WebhookEventType = z.infer<typeof WebhookEventType>;

// ---------------------------------------------------------------------------
// Webhook events: the body of each delivery (decision 0017). Lean on purpose:
// what changed and where, never content; the consumer reads current state.

/** The workspace an event happened in, with your own ID for it so you can route without a lookup. */
export const WebhookEventWorkspace = named(
  "WebhookEventWorkspace",
  z.object({ id: idSchema("workspace"), external_id: z.string().nullable() }),
);

// Each event's `data`, with `status` narrowed to what that event means: a
// `run.completed` is always `completed`, so a server can't send otherwise.
const installationData = <S extends z.ZodType>(status: S) =>
  z.object({ object: z.literal("installation"), id: idSchema("installation"), app_id: idSchema("app"), status });
const connectSessionData = <S extends z.ZodType>(status: S) =>
  z.object({
    object: z.literal("connect_session"),
    id: idSchema("connect_session"),
    status,
    /** The installation it created or reconnected; null when it didn't finish. */
    installation_id: idSchema("installation").nullable(),
    /** Why it didn't finish, e.g. `consent_denied`; null when it completed. */
    failure_code: z.string().nullable(),
  });
const runData = <S extends z.ZodType>(status: S) =>
  z.object({ object: z.literal("run"), id: idSchema("run"), agent_id: idSchema("agent"), status });

const webhookEventName = (type: string) =>
  `${type.replace(/(^|[._])([a-z])/g, (_, __, c: string) => c.toUpperCase())}WebhookEvent`;

const envelopeFields = {
  /** Stable across every attempt and resend: deduplicate on it. */
  id: idSchema("webhook_event"),
  created_at: instant,
  environment: Environment,
  organization_id: idSchema("organization"),
};

function webhookEvent<T extends string, D extends z.ZodType, W extends z.ZodType>(type: T, data: D, workspace: W) {
  return named(webhookEventName(type), z.object({ ...envelopeFields, type: z.literal(type), workspace, data }));
}

/** Every delivery body, by `type`. */
export const WebhookEvent = named(
  "WebhookEvent",
  z.discriminatedUnion("type", [
    webhookEvent(
      "installation.connected",
      installationData(InstallationStatus.extract(["connected"])),
      WebhookEventWorkspace,
    ),
    webhookEvent(
      "installation.reconnect_required",
      installationData(InstallationStatus.extract(["reconnect_required"])),
      WebhookEventWorkspace,
    ),
    webhookEvent(
      "installation.disconnected",
      installationData(InstallationStatus.extract(["disconnected"])),
      WebhookEventWorkspace,
    ),
    webhookEvent(
      "connect_session.completed",
      connectSessionData(ConnectSessionStatus.extract(["completed"])),
      WebhookEventWorkspace,
    ),
    webhookEvent(
      "connect_session.failed",
      connectSessionData(ConnectSessionStatus.extract(["failed", "expired", "cancelled"])),
      WebhookEventWorkspace,
    ),
    webhookEvent("run.completed", runData(RunStatus.extract(["completed"])), WebhookEventWorkspace),
    webhookEvent("run.failed", runData(RunStatus.extract(["failed"])), WebhookEventWorkspace),
    webhookEvent(
      "run.waiting_for_approval",
      runData(RunStatus.extract(["waiting_for_approval"])),
      WebhookEventWorkspace,
    ),
    webhookEvent("run.cancelled", runData(RunStatus.extract(["cancelled"])), WebhookEventWorkspace),
    // The test button's event belongs to no workspace.
    webhookEvent(
      "webhook.test",
      z.object({ object: z.literal("webhook_endpoint"), id: idSchema("webhook_endpoint") }),
      z.null(),
    ),
  ]),
);
export type WebhookEvent = z.infer<typeof WebhookEvent>;
/** Every type a delivery can carry: the subscribable ones and `webhook.test`. */
export type WebhookDeliveryType = WebhookEvent["type"];
export const WEBHOOK_DELIVERY_TYPES = WebhookEvent.options.map((variant) => variant.shape.type.value);

/**
 * What every delivery has, whatever its type, including types added after a
 * client was built. Open, like every contract object: fields added later pass
 * through.
 */
export const WebhookEventEnvelope = named(
  "WebhookEventEnvelope",
  z.looseObject({
    ...envelopeFields,
    type: z.string(),
    workspace: WebhookEventWorkspace.nullable(),
    // Any object's ID, including kinds added after this client: checked as a string, not by prefix.
    data: z.looseObject({ object: z.string(), id: z.string() }),
  }),
);
export type WebhookEventEnvelope = z.infer<typeof WebhookEventEnvelope>;

export const WebhookDelivery = named(
  "WebhookDelivery",
  z.object({ status_code: z.number().int(), delivered_at: instant }),
);
export type WebhookDelivery = z.infer<typeof WebhookDelivery>;

export const WebhookEndpoint = named(
  "WebhookEndpoint",
  z.object({
    id: idSchema("webhook_endpoint"),
    organization_id: idSchema("organization"),
    environment: Environment,
    url: z.url(),
    description: z.string(),
    events: z.array(WebhookEventType),
    enabled: z.boolean(),
    signing_secret_last4: z.string(),
    created_at: instant,
    last_delivery: WebhookDelivery.nullable(),
  }),
);
export type WebhookEndpoint = z.infer<typeof WebhookEndpoint>;

export const CreatedWebhookEndpoint = named(
  "CreatedWebhookEndpoint",
  z.object({ endpoint: WebhookEndpoint, signing_secret: z.string() }),
);
export type CreatedWebhookEndpoint = z.infer<typeof CreatedWebhookEndpoint>;

export const CreateWebhookEndpointInput = z.object({
  environment: Environment,
  url: z.string(),
  description: z.string(),
  events: z.array(WebhookEventType),
});
export type CreateWebhookEndpointInput = z.infer<typeof CreateWebhookEndpointInput>;
export const UpdateWebhookEndpointInput = z.object({
  url: z.string().optional(),
  description: z.string().optional(),
  events: z.array(WebhookEventType).optional(),
  enabled: z.boolean().optional(),
});
export type UpdateWebhookEndpointInput = z.infer<typeof UpdateWebhookEndpointInput>;

// ---------------------------------------------------------------------------
// Audit and usage

export const AuditActor = z.object({
  type: z.enum(["user", "api_key", "system"]),
  /** The user or API key; `null` for Zupflow itself (`system`). */
  id: AnyIdSchema.nullable(),
  name: z.string(),
});
export type AuditActor = z.infer<typeof AuditActor>;

export const AuditTarget = z.object({
  type: z.string(),
  /** `null` for targets that aren't objects, such as an environment's Connect settings. */
  id: AnyIdSchema.nullable(),
  name: z.string(),
});
export type AuditTarget = z.infer<typeof AuditTarget>;

export const AuditEvent = named(
  "AuditEvent",
  z.object({
    id: idSchema("audit_event"),
    organization_id: idSchema("organization"),
    workspace_id: idSchema("workspace").nullable(),
    /** The workspace the event happened in, named so the log reads on its own. */
    workspace: WorkspaceRef.nullable(),
    environment: Environment.nullable(),
    actor: AuditActor,
    action: z.string(),
    target: AuditTarget,
    created_at: instant,
  }),
);
export type AuditEvent = z.infer<typeof AuditEvent>;

export const AuditListQuery = z.object({
  workspace_id: idSchema("workspace")
    .optional()
    .describe("Only this workspace's rows. A malformed or unknown ID matches nothing."),
  cursor: z.string().optional(),
  limit: z
    .number()
    .int()
    .optional()
    .describe("Page size, 1 to 100 (default 50); values outside are clamped, and a non-integer means the default."),
});
export type AuditListQuery = z.infer<typeof AuditListQuery>;

export const UsageDay = named(
  "UsageDay",
  z.object({
    date: z.iso.date(),
    runs: z.number().int(),
    failed_runs: z.number().int(),
    tool_calls: z.number().int(),
  }),
);
export type UsageDay = z.infer<typeof UsageDay>;

export const UsageReport = named(
  "UsageReport",
  z.object({
    environment: Environment,
    days: z.array(UsageDay),
    totals: z.object({ runs: z.number().int(), failed_runs: z.number().int(), tool_calls: z.number().int() }),
  }),
);
export type UsageReport = z.infer<typeof UsageReport>;
