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
/**
 * `oauth2`: the organization registers its own OAuth client with the
 * provider. `mcp_oauth`: the MCP server's authorization server is discovered
 * and Zupflow registers a client with it. `token`: the user pastes a token.
 */
export const AuthType = z.enum(["oauth2", "mcp_oauth", "token"]);
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
// Console sign-in (decision 0020)

/** `local`: no sign-in. Every Console request is the server's one built-in owner. */
export const LocalSignIn = named("LocalSignIn", z.object({ mode: z.literal("local") }));
export type LocalSignIn = z.infer<typeof LocalSignIn>;

/**
 * `oidc`: sign in at any OpenID Connect provider (authorization code with
 * PKCE) and send its ID token as the bearer; the API verifies it.
 */
export const OidcSignIn = named(
  "OidcSignIn",
  z.object({
    mode: z.literal("oidc"),
    /** The provider's issuer: the `iss` of its ID tokens, and where its discovery document lives. */
    issuer: z.url(),
    /** The Console's client ID at the provider: the audience of the ID tokens the API accepts. */
    client_id: z.string().min(1),
  }),
);
export type OidcSignIn = z.infer<typeof OidcSignIn>;

/** How the Console signs in to this API. Public values only: anyone may read them. */
export const SignInSettings = named("SignInSettings", z.discriminatedUnion("mode", [LocalSignIn, OidcSignIn]));
export type SignInSettings = z.infer<typeof SignInSettings>;

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
    /** When an invitation stops being accepted: `INVITATION_TTL_DAYS` after `invited_at`. Null once active. */
    expires_at: instant.nullable(),
    joined_at: instant.nullable(),
    last_active_at: instant.nullable(),
  }),
);
export type Member = z.infer<typeof Member>;

/**
 * How long an invitation can be accepted after it was sent. An expired one
 * can still be declined, and inviting the same address again renews it.
 */
export const INVITATION_TTL_DAYS = 7;
/** `GET /v1/me/invitations` lists at most this many, newest first. */
export const INVITATION_LIST_LIMIT = 50;
/** An invitation expired longer ago than this is left out of `GET /v1/me/invitations`. */
export const EXPIRED_INVITATION_SHOWN_DAYS = 30;

/** `expired`: past `expires_at`, so it can't be accepted any more. */
export const InvitationStatus = z.enum(["pending", "expired"]);
export type InvitationStatus = z.infer<typeof InvitationStatus>;

/**
 * An invitation to the signed-in person. Click to accept (decided 2026-10-10):
 * Nobody joins an organization without accepting; signing in with an invited
 * address joins nothing by itself. Only the person whose verified email was
 * invited ever sees it.
 *
 * An invitation grants only what its grantor (`invited_by`, or whoever last
 * changed its role) can still grant: an owner or admin, and an owner for an
 * owner invitation. Once the grantor is removed or demoted below that, it's
 * withdrawn, exactly as if an admin had cancelled it.
 */
export const Invitation = named(
  "Invitation",
  z.object({
    /** The membership it becomes once accepted. */
    id: idSchema("member"),
    organization: named(
      "InvitationOrganization",
      z.object({ id: idSchema("organization"), name: z.string(), slug: z.string() }),
    ),
    /** The role accepting grants. */
    role: MemberRole,
    /** Who sent it, or last changed its role: the grantor. */
    invited_by: named("Inviter", z.object({ name: z.string(), email: z.email() })),
    status: InvitationStatus,
    invited_at: instant,
    expires_at: instant,
  }),
);
export type Invitation = z.infer<typeof Invitation>;

export const InviteMemberInput = z.object({ email: z.string(), role: MemberRole });
export type InviteMemberInput = z.infer<typeof InviteMemberInput>;
export const UpdateMemberInput = z.object({ role: MemberRole });
export const RemoveMemberQuery = z.object({
  status: z
    .enum(["invited"])
    .optional()
    .describe(
      "`invited`: withdraw an invitation only. One accepted in the meantime is left alone, and answers `409 invitation_accepted`.",
    ),
});
export type RemoveMemberQuery = z.infer<typeof RemoveMemberQuery>;
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
    /**
     * Trusted authorization-server issuer, preserved exactly. When set, every
     * callback must send the same `iss` (RFC 9207), including error responses.
     * Omitted or null accepts legacy responses without `iss` only. Public
     * HTTPS, with no credentials, query, fragment or whitespace.
     */
    issuer: z.url().nullable().optional(),
    /** Scopes offered to environment configurations. */
    available_scopes: z.array(OAuthScope),
    pkce: z.boolean(),
    /** Read-only endpoint that identifies the connected account. */
    identity_url: z.url().nullable(),
  }),
);
export type OAuthAuthTemplate = z.infer<typeof OAuthAuthTemplate>;

/**
 * Sign-in through the MCP server: its authorization server and endpoints are
 * discovered when a user signs in, and a client is registered with it once
 * per organization, app and environment (a workspace app's, per workspace).
 * Nothing to configure but the server's URL.
 */
export const McpOAuthAuthTemplate = named(
  "McpOAuthAuthTemplate",
  z.object({
    type: z.literal("mcp_oauth"),
    /** Scopes asked for at sign-in. Empty asks for the server's default. */
    scopes: z.array(OAuthScope),
    /**
     * Read-only endpoint that identifies the connected account. It must share
     * the MCP server's origin (scheme, host and port), so the token stays with
     * the server it was issued for: a template naming another is refused
     * (`422 identity_off_server`), and so is a sign-in after a configuration
     * moved the server away from it. Without one, the account is anonymous,
     * named from the token's subject when it says one; a reconnect whose
     * token names another subject fails with `account_mismatch`, and one
     * without a subject on either side keeps the account it had.
     */
    identity_url: z.url().nullable(),
  }),
);
export type McpOAuthAuthTemplate = z.infer<typeof McpOAuthAuthTemplate>;

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

export const AuthTemplate = named(
  "AuthTemplate",
  z.discriminatedUnion("type", [OAuthAuthTemplate, McpOAuthAuthTemplate, TokenAuthTemplate]),
);
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

/**
 * An app's key: snake case, 1 to 24 characters, starting with a letter
 * (`taskboard`, `google_workspace`).
 */
export const AppKey = named(
  "AppKey",
  z
    .string()
    .min(1)
    .max(24)
    .regex(/^[a-z][a-z0-9]*(_[a-z0-9]+)*$/),
);
export type AppKey = z.infer<typeof AppKey>;

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
    /** Fixed once the app is created. Starts every tool name a model is offered. */
    key: AppKey,
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
  /**
   * Unique across the organization: its catalog and every workspace's apps
   * share one key space (`409 app_key_taken`, which never says where the key
   * is used). It can't be changed later.
   */
  key: z.string(),
  description: z.string(),
  auth_type: AuthType,
});
export type CreateAppInput = z.infer<typeof CreateAppInput>;
export const UpdateAppInput = z.object({
  name: z.string().optional(),
  description: z.string().optional(),
  /**
   * Replaces the app's authentication template, whole. Its `type` must be
   * the app's own (`422 auth_type_immutable`). URLs must be public HTTPS
   * (`422 insecure_url`, `422 destination_not_allowed`); a scope a
   * configuration still requests can't be removed (`409 scope_in_use`). An
   * `mcp_oauth` identity URL must be on each configured MCP server
   * (`422 identity_off_server`).
   * Input patterns use a small grammar both the browser and the server read
   * alike. A catalog app's template is shared by both environments, so an API
   * key can't send this field there (`403 console_only`). Raises the app's
   * `version`.
   */
  auth: AuthTemplate.optional(),
});
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
    /**
     * Present only when the app's authentication type is `oauth2`. An
     * `mcp_oauth` app has no client to configure: it is ready once it has a
     * server URL.
     */
    oauth: OAuthClientSettings.nullable(),
    /** Fields still required before the configuration is ready. */
    missing: z.array(z.string()),
    updated_at: instant.nullable(),
    updated_by: User.nullable(),
  }),
);
export type ClientConfiguration = z.infer<typeof ClientConfiguration>;

/**
 * The `oauth_*` fields apply only to an `oauth2` app; a token or `mcp_oauth`
 * app refuses a client ID or secret (`422 oauth_not_supported`), and an
 * `mcp_oauth` app requests the scopes its template names, so it refuses
 * scopes here too.
 */
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

/** An installation's short ID: six characters of lowercase base32 (`k7m2qd`). */
export const InstallationShortId = named("InstallationShortId", z.string().regex(/^[a-z2-7]{6}$/));
export type InstallationShortId = z.infer<typeof InstallationShortId>;

export const InstallationApp = named(
  "InstallationApp",
  z.object({
    id: idSchema("app"),
    name: z.string(),
    key: AppKey,
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
    /**
     * Six lowercase base32 characters naming the installation within its
     * workspace, for people and models. Read-only: set when the installation
     * is created, kept across reconnects and renames, never reused in the
     * workspace. A label, not an identifier: the API never finds an
     * installation by it.
     */
    short_id: InstallationShortId,
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

/** A page of a list: where to start and how many. */
export const PageQuery = z.object({
  cursor: z.string().optional(),
  limit: z
    .number()
    .int()
    .optional()
    .describe("Page size, 1 to 100 (default 25); values outside are clamped, and a non-integer means the default."),
});
export type PageQuery = z.infer<typeof PageQuery>;

export const InstallationToolStatus = z.enum(["current", "changed", "gone"]);
export type InstallationToolStatus = z.infer<typeof InstallationToolStatus>;

/** What a tool's definition says, as its app's MCP server listed it. */
export const ToolText = named(
  "ToolText",
  z.object({
    title: z.string().nullable(),
    description: z.string().nullable(),
    /** The JSON Schema of the tool's arguments. */
    input_schema: z.record(z.string(), z.unknown()).nullable(),
    /** The JSON Schema of the tool's structured result. */
    output_schema: z.record(z.string(), z.unknown()).nullable(),
    /** The server's hints about the tool, such as `readOnlyHint`. Hints only: never trusted. */
    annotations: z.record(z.string(), z.unknown()).nullable(),
  }),
);
export type ToolText = z.infer<typeof ToolText>;

/**
 * A tool an installation's MCP server listed. The first definition seen is
 * `pinned`; a later one that differs is `latest`, unreviewed, and the tool is
 * `changed`. A tool the server stopped listing is `gone`; an installation keeps
 * at most 500 gone tools, dropping those gone longest.
 */
/** A definition's fingerprint: 64 lowercase hex digits (the backend's is its SHA-256). */
const definitionHash = z.string().regex(/^[0-9a-f]{64}$/);

export const InstallationTool = named(
  "InstallationTool",
  z.object({
    name: z.string(),
    effect: ToolEffect,
    status: InstallationToolStatus,
    pinned: ToolText,
    /** Identifies the pinned definition. */
    pinned_hash: definitionHash,
    latest: ToolText.nullable(),
    /** Identifies the latest definition: send it back to accept exactly the change you reviewed. */
    latest_hash: definitionHash.nullable(),
    first_seen_at: instant,
    seen_at: instant,
    changed_at: instant.nullable(),
    gone_at: instant.nullable(),
  }),
);
export type InstallationTool = z.infer<typeof InstallationTool>;

/** An installation seen from its app's organization-level page. */
export const AppInstallationRef = named(
  "AppInstallationRef",
  z.object({ installation: Installation, workspace: WorkspaceRef }),
);
export type AppInstallationRef = z.infer<typeof AppInstallationRef>;

/**
 * Accepts a changed tool's latest definition: it becomes the pinned one, and
 * the tool is current again. `latest_hash` must be the latest definition's
 * (`409 stale_definition` otherwise, when it changed again since you read it);
 * a tool that isn't changed answers `409 tool_not_changed`, one the
 * installation doesn't have `404`.
 */
export const AcceptToolChangeInput = z.object({ name: z.string(), latest_hash: z.string() });
export type AcceptToolChangeInput = z.infer<typeof AcceptToolChangeInput>;

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
    /**
     * The model this agent overrides its environment's default with: one the
     * organization added in the agent's workspace's environment. `null` runs
     * on the environment's default model.
     */
    model_id: idSchema("model").nullable(),
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

/**
 * `model_id` names a model of the same organization, added in the
 * workspace's environment (`422 unknown_model`, `fields.model_id`, for any
 * other); omitted or `null`, the agent runs on the environment's default.
 */
export const CreateAgentInput = z.object({
  name: z.string(),
  description: z.string(),
  model_id: idSchema("model").nullable().optional(),
  instructions: z.string(),
});
export type CreateAgentInput = z.infer<typeof CreateAgentInput>;
export const UpdateAgentInput = z.object({
  name: z.string().optional(),
  description: z.string().optional(),
  /** As for creating; `null` goes back to the environment's default. Omitted keeps the agent's. */
  model_id: idSchema("model").nullable().optional(),
  instructions: z.string().optional(),
  status: AgentStatus.optional(),
});
export type UpdateAgentInput = z.infer<typeof UpdateAgentInput>;

/**
 * Held inside the organization's policy for a catalog app's tools: a tool it
 * denies can't be granted (`422 tool_denied`, `fields.tools`), and every
 * granted tool it sets to `approve` must be in `approval_required`
 * (`422 approval_required_by_policy`, `fields.approval_required`). A tool
 * without a confirmed level takes the policy's `new_tool_level`. A workspace
 * app's tools have no organization level: the grant alone decides.
 */
export const CreateGrantInput = z.object({
  installation_id: idSchema("installation"),
  tools: z.array(z.string()),
  approval_required: z.array(z.string()),
});
export type CreateGrantInput = z.infer<typeof CreateGrantInput>;
export const UpdateGrantInput = z.object({ tools: z.array(z.string()), approval_required: z.array(z.string()) });
export type UpdateGrantInput = z.infer<typeof UpdateGrantInput>;

// ---------------------------------------------------------------------------
// Model providers: bring your own key (decisions 0049, 0050)

/**
 * The providers a model connection can reach. A registry: adding one is an
 * entry here (and an adapter on the server). `routes_to_others` marks a
 * provider that serves other authors' models, shown "via" it.
 */
export const MODEL_PROVIDER_KINDS = ["openai", "anthropic", "openrouter"] as const;
export const ModelProviderKind = named("ModelProviderKind", z.enum(MODEL_PROVIDER_KINDS));
export type ModelProviderKind = z.infer<typeof ModelProviderKind>;

export interface ModelProviderInfo {
  /** The provider's name, as people know it. */
  name: string;
  /** Serves other authors' models: a model added through it is shown "via" it. */
  routes_to_others: boolean;
  /** Where its API keys are made, for people adding a connection. */
  keys_url: string;
}

export const MODEL_PROVIDERS: Record<ModelProviderKind, ModelProviderInfo> = {
  openai: { name: "OpenAI", routes_to_others: false, keys_url: "https://platform.openai.com/api-keys" },
  anthropic: { name: "Anthropic", routes_to_others: false, keys_url: "https://console.anthropic.com/settings/keys" },
  openrouter: { name: "OpenRouter", routes_to_others: true, keys_url: "https://openrouter.ai/settings/keys" },
};

/** `invalid_key`: the provider refused the key; replace it to use the connection's models again. */
export const ModelConnectionStatus = z.enum(["active", "invalid_key"]);
export type ModelConnectionStatus = z.infer<typeof ModelConnectionStatus>;

/**
 * One organization's account with one provider, in one environment: its
 * models are added in that environment only, and Sandbox and Production never
 * share a connection or a key. Several per organization, several of one kind
 * too. The API key is write-only: responses say whether it's set and its last
 * four characters, never the key.
 */
export const ModelConnection = named(
  "ModelConnection",
  z.object({
    id: idSchema("model_connection"),
    organization_id: idSchema("organization"),
    environment: Environment,
    kind: ModelProviderKind,
    label: z.string(),
    api_key: SecretState,
    /** Raised each time the key is replaced. A model attempt records the version it used. */
    key_version: z.number().int(),
    status: ModelConnectionStatus,
    /** Models the organization added from it. */
    model_count: z.number().int(),
    created_at: instant,
    updated_at: instant,
  }),
);
export type ModelConnection = z.infer<typeof ModelConnection>;

export const ModelConnectionListQuery = z.object({
  environment: Environment.optional().describe(
    "Only this environment's connections. An API key reads its own environment's; naming another is `422 environment_mismatch`.",
  ),
});
export type ModelConnectionListQuery = z.infer<typeof ModelConnectionListQuery>;

/**
 * The key is checked with the provider (by listing its models) before
 * anything is saved: a key it refuses is `422 invalid_api_key`, `fields.api_key`.
 * A deployment that reaches no model providers can't check it:
 * `501 model_providers_not_configured`, which retrying doesn't change.
 */
export const CreateModelConnectionInput = z.object({
  environment: Environment,
  kind: ModelProviderKind,
  label: z.string(),
  /** Write-only. */
  api_key: z.string(),
});
export type CreateModelConnectionInput = z.infer<typeof CreateModelConnectionInput>;

export const UpdateModelConnectionInput = z.object({ label: z.string() });
export type UpdateModelConnectionInput = z.infer<typeof UpdateModelConnectionInput>;

/**
 * Replaces the key with a new version, checked like a new connection's. A key
 * the provider refuses is `422 invalid_api_key` and the connection keeps its
 * key. A key that works makes an `invalid_key` connection `active` again.
 * Two replacements at once: the one that saves second finds the key it
 * replaced gone and is `409 key_changed`, saving nothing.
 */
export const ReplaceModelConnectionKeyInput = z.object({ api_key: z.string() });
export type ReplaceModelConnectionKeyInput = z.infer<typeof ReplaceModelConnectionKeyInput>;

/** Dollars per million tokens, as the provider lists them. */
export const ModelPricing = named(
  "ModelPricing",
  z.object({ input_per_million_usd: z.number(), output_per_million_usd: z.number() }),
);
export type ModelPricing = z.infer<typeof ModelPricing>;

/**
 * A model the provider lists for a connection's key, searched on the server
 * (the server keeps the provider's list for about ten minutes). Only a model
 * that can call tools can be added: an agent's work is calling tools.
 */
export const ProviderModel = named(
  "ProviderModel",
  z.object({
    /** The provider's own ID, sent to it as is: `gpt-5`, `claude-sonnet-4-5`, `openai/gpt-5`. */
    provider_model_id: z.string(),
    name: z.string(),
    /** Who made it (`openai`, `anthropic`, `google`…), whoever serves it. */
    author: z.string(),
    /** Tokens; `null` when the provider doesn't say. */
    context_window: z.number().int().nullable(),
    supports_tools: z.boolean(),
    /** `null` when the provider doesn't list prices. */
    pricing: ModelPricing.nullable(),
    /** The model already added from this connection, if any. */
    added_model_id: idSchema("model").nullable(),
  }),
);
export type ProviderModel = z.infer<typeof ProviderModel>;

export const ProviderModelListQuery = PageQuery.extend({
  /** Matches the provider's model ID, name or author. */
  q: z.string().optional(),
});
export type ProviderModelListQuery = z.infer<typeof ProviderModelListQuery>;

/** `unavailable`: the provider stopped serving it (it answered `404` for it). */
export const ModelStatus = z.enum(["available", "unavailable"]);
export type ModelStatus = z.infer<typeof ModelStatus>;

/** The connection a model is reached through: its route. */
export const ModelRoute = named(
  "ModelRoute",
  z.object({ connection_id: idSchema("model_connection"), kind: ModelProviderKind, label: z.string() }),
);
export type ModelRoute = z.infer<typeof ModelRoute>;

/**
 * A model the organization added from one of its connections, in that
 * connection's environment. The route is its identity: GPT-5 through an
 * OpenAI connection and `openai/gpt-5` through OpenRouter are two models,
 * never swapped for each other.
 */
export const Model = named(
  "Model",
  z.object({
    id: idSchema("model"),
    organization_id: idSchema("organization"),
    environment: Environment,
    route: ModelRoute,
    provider_model_id: z.string(),
    /** Shown to people; the organization may rename it. */
    name: z.string(),
    author: z.string(),
    context_window: z.number().int().nullable(),
    supports_tools: z.boolean(),
    status: ModelStatus,
    /** The environment's default model. */
    is_default: z.boolean(),
    /** Agents that override the default with it, across the environment's workspaces. */
    agent_count: z.number().int(),
    created_at: instant,
    updated_at: instant,
  }),
);
export type Model = z.infer<typeof Model>;

export const ModelListQuery = PageQuery.extend({
  environment: Environment.optional().describe(
    "Only this environment's models. An API key reads its own environment's; naming another is `422 environment_mismatch`.",
  ),
  connection_id: idSchema("model_connection")
    .optional()
    .describe("Only this connection's models. A malformed or unknown ID matches nothing."),
  /** Matches the name, the provider's model ID or the author. */
  q: z.string().optional(),
});
export type ModelListQuery = z.infer<typeof ModelListQuery>;

/**
 * Adds a model the connection's provider lists (`422 unknown_provider_model`
 * otherwise) and that can call tools (`422 tools_unsupported`). The same
 * provider model twice on one connection is `409 model_exists`.
 */
export const CreateModelInput = z.object({
  connection_id: idSchema("model_connection"),
  provider_model_id: z.string(),
  /** Defaults to the provider's name for it. */
  name: z.string().optional(),
});
export type CreateModelInput = z.infer<typeof CreateModelInput>;

export const UpdateModelInput = z.object({ name: z.string() });
export type UpdateModelInput = z.infer<typeof UpdateModelInput>;

/**
 * The model agents in one environment run on unless they override it. Until
 * one is chosen, `model` is `null`, and a run of an agent with no override is
 * refused (`422 model_not_configured`), never given a model silently.
 */
export const DefaultModel = named(
  "DefaultModel",
  z.object({
    environment: Environment,
    model: Model.nullable(),
    updated_at: instant.nullable(),
    /** Who last changed it: null before anyone did or when an API key did; "Former member" once they are gone. */
    updated_by: User.nullable(),
  }),
);
export type DefaultModel = z.infer<typeof DefaultModel>;

/**
 * A model of this organization in this environment (`422 unknown_model`,
 * `fields.model_id`, otherwise), or `null` to have none.
 */
export const UpdateDefaultModelInput = z.object({ model_id: idSchema("model").nullable() });
export type UpdateDefaultModelInput = z.infer<typeof UpdateDefaultModelInput>;

// ---------------------------------------------------------------------------
// Tool permissions (decision 0040)

/**
 * What the organization lets a tool do: `allow` runs it, `approve` pauses the
 * run until your product decides, `deny` keeps it out of every grant.
 */
export const ToolLevel = z.enum(["allow", "approve", "deny"]);
export type ToolLevel = z.infer<typeof ToolLevel>;
/** Who decides a call that needs approval. Only `manual` so far: a person, in your product. */
export const ApprovalMode = z.enum(["manual"]);
export type ApprovalMode = z.infer<typeof ApprovalMode>;
/** The level of a tool nobody has set one for: `approve` or `deny`, never `allow`. */
export const NewToolLevel = z.enum(["approve", "deny"]);
export type NewToolLevel = z.infer<typeof NewToolLevel>;

/**
 * The organization's tool policy: one for the whole organization, both
 * environments. Until someone saves it, it reads as `manual` with new tools
 * at `approve`, and `updated_at` null.
 */
export const ToolPolicy = named(
  "ToolPolicy",
  z.object({
    organization_id: idSchema("organization"),
    mode: ApprovalMode,
    /**
     * The level of a tool whose level nobody has confirmed (new, or only
     * suggested from its hints), and of a changed tool until its change is
     * accepted.
     */
    new_tool_level: NewToolLevel,
    updated_at: instant.nullable(),
    /** The member who last saved it; null before that, or when they're gone. */
    updated_by: User.nullable(),
  }),
);
export type ToolPolicy = z.infer<typeof ToolPolicy>;

export const UpdateToolPolicyInput = z.object({ mode: ApprovalMode, new_tool_level: NewToolLevel });
export type UpdateToolPolicyInput = z.infer<typeof UpdateToolPolicyInput>;

/**
 * The organization's level for one tool of a catalog app, by name. A tool an
 * installation lists for the first time gets a suggested level from its hints
 * (read-only: `allow`, otherwise `approve`; `destructive` flagged), not
 * `confirmed`: hints never decide alone, so until a person sets it, the
 * policy's `new_tool_level` applies instead.
 */
export const AppToolLevel = named(
  "AppToolLevel",
  z.object({
    app_id: idSchema("app"),
    tool: z.string(),
    level: ToolLevel,
    /** Set by a person. Only a confirmed level is in force. */
    confirmed: z.boolean(),
    /** The tool's hints said it may destroy data. A hint, shown to help decide. */
    destructive: z.boolean(),
    updated_at: instant,
    /** Who confirmed it; null while suggested, or when they're gone. */
    updated_by: User.nullable(),
  }),
);
export type AppToolLevel = z.infer<typeof AppToolLevel>;

/**
 * Sets levels by tool name, each confirmed: 1 to 100, names 1 to 128
 * characters, each once. A name no installation has listed yet can be set
 * ahead of it. Grants already holding a tool are not rewritten when its level
 * tightens: a call the level denies is refused when it's made, and one it
 * sets to `approve` waits for approval.
 */
export const UpdateAppToolLevelsInput = z.object({
  levels: z.array(z.object({ tool: z.string(), level: ToolLevel })),
});
export type UpdateAppToolLevelsInput = z.infer<typeof UpdateAppToolLevelsInput>;

// ---------------------------------------------------------------------------
// Runs

/**
 * The tokens a run's model read and wrote, as its provider counted them, and
 * the calls it sent. `input_tokens` is every token the model read, those
 * served from the provider's prompt cache and those written to it included,
 * whichever provider served the run; the two cache counts are parts of it.
 */
const tokenFields = {
  input_tokens: z
    .number()
    .int()
    .describe("Every token the model read, those read from or written to the provider's prompt cache included."),
  output_tokens: z.number().int().describe("Every token the model wrote."),
  cache_read_input_tokens: z
    .number()
    .int()
    .describe("The part of `input_tokens` the provider served from its prompt cache."),
  cache_write_input_tokens: z
    .number()
    .int()
    .describe(
      "The part of `input_tokens` the provider wrote to its prompt cache. Providers that cache on their own report none.",
    ),
};

export const RunUsage = named("RunUsage", z.object({ ...tokenFields, tool_calls: z.number().int() }));
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
    runEvent(
      "run.started",
      z.object({
        /** The provider's model ID the run calls. */
        model: z.string(),
        /**
         * The organization's model and the connection it's reached through,
         * fixed when the run starts: two connections can serve the same
         * provider model ID. `null` only from a server that doesn't hold
         * organizations' models yet.
         */
        model_id: idSchema("model").nullable(),
        connection_id: idSchema("model_connection").nullable(),
      }),
    ),
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

/**
 * The decision your product made on the call a run waits for. A denial's
 * optional `reason` (trimmed, up to 500 characters) goes back to the agent's
 * model as the call's result, so it can continue without it; without one, the
 * model reads that a person denied the call. A reason with an approval is
 * ignored.
 */
export const ResolveApprovalInput = z.object({
  call_id: idSchema("tool_call"),
  decision: ApprovalDecision,
  reason: z.string().max(500).optional(),
});
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
  "app.tools_changed",
] as const;
/**
 * The event types an endpoint can subscribe to. `webhook.test` is sent only
 * by the test button. `app.tools_changed` is sent when a listing of a
 * connected installation's tools changes one or more of them, once per
 * listing.
 */
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
    // A connected installation's app listed a tool with a new definition: review it before calls use it.
    webhookEvent(
      "app.tools_changed",
      installationData(InstallationStatus.extract(["connected"])),
      WebhookEventWorkspace,
    ),
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

/**
 * An endpoint's last answer: its HTTP status, or `0` when none came back (a
 * timeout, a network error, a refused destination). Only the endpoint's own
 * answers count: when Zupflow can't send (its store or key service is down),
 * the test button answers `503 unavailable`, like any route whose store is
 * down, and nothing is recorded against the endpoint.
 */
export const WebhookDelivery = named(
  "WebhookDelivery",
  z.object({ status_code: z.number().int(), delivered_at: instant }),
);
export type WebhookDelivery = z.infer<typeof WebhookDelivery>;

export const WEBHOOK_DELIVERY_STATES = ["pending", "succeeded", "failed", "skipped"] as const;
export const WebhookDeliveryState = z.enum(WEBHOOK_DELIVERY_STATES);
export type WebhookDeliveryState = z.infer<typeof WebhookDeliveryState>;

/** Safe failure classes only: never the endpoint's response text or an internal error. */
export const WebhookDeliveryErrorClass = z.enum([
  "status",
  "refused",
  "redirected",
  "timeout",
  "too_large",
  "unreachable",
  "failed",
  "store_unavailable",
  "secret_unavailable",
]);
export type WebhookDeliveryErrorClass = z.infer<typeof WebhookDeliveryErrorClass>;

/** One recorded attempt, numbered from one within its delivery. */
export const WebhookDeliveryAttempt = named(
  "WebhookDeliveryAttempt",
  z.object({
    number: z.number().int().positive(),
    status_code: z.number().int().min(100).max(999).nullable(),
    error_class: WebhookDeliveryErrorClass.nullable(),
    attempted_at: instant,
    /** Null when this attempt predates duration recording. */
    duration_ms: z.number().int().nonnegative().nullable(),
  }),
);
export type WebhookDeliveryAttempt = z.infer<typeof WebhookDeliveryAttempt>;

/** A delivery to one endpoint; the legacy test-button answer stays `WebhookDelivery`. */
export const WebhookDeliverySummary = named(
  "WebhookDeliverySummary",
  z.object({
    id: idSchema("webhook_delivery"),
    organization_id: idSchema("organization"),
    endpoint_id: idSchema("webhook_endpoint"),
    event_id: idSchema("webhook_event"),
    environment: Environment,
    event_type: z.enum(WEBHOOK_DELIVERY_TYPES),
    event_created_at: instant,
    workspace: WebhookEventWorkspace.nullable(),
    state: WebhookDeliveryState,
    attempts: z.number().int().nonnegative(),
    last_status_code: z.number().int().min(100).max(999).nullable(),
    last_error: WebhookDeliveryErrorClass.nullable(),
    last_attempt_at: instant.nullable(),
    next_attempt_at: instant.nullable(),
    created_at: instant,
  }),
);
export type WebhookDeliverySummary = z.infer<typeof WebhookDeliverySummary>;

export const WebhookDeliveryDetail = named(
  "WebhookDeliveryDetail",
  WebhookDeliverySummary.extend({
    event: WebhookEvent,
    /**
     * Whether every recorded attempt has a history row. Older deliveries can
     * have only a summary. This says nothing about interrupted network calls.
     */
    attempt_history_complete: z.boolean(),
  }),
);
export type WebhookDeliveryDetail = z.infer<typeof WebhookDeliveryDetail>;

export const WebhookDeliveryListQuery = PageQuery.extend({
  workspace_id: idSchema("workspace").optional(),
  state: WebhookDeliveryState.optional(),
  event_type: z.enum(WEBHOOK_DELIVERY_TYPES).optional(),
});
export type WebhookDeliveryListQuery = z.infer<typeof WebhookDeliveryListQuery>;

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
    /** The old secret's 24-hour overlap deadline; null once it is no longer active. */
    previous_signing_secret_expires_at: instant.nullable(),
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

const usageCounts = {
  runs: z.number().int(),
  failed_runs: z.number().int(),
  tool_calls: z.number().int(),
  ...tokenFields,
};

/** One UTC day of runs created that day, with their calls and tokens. */
export const UsageDay = named("UsageDay", z.object({ date: z.iso.date(), ...usageCounts }));
export type UsageDay = z.infer<typeof UsageDay>;

export const UsageReport = named(
  "UsageReport",
  z.object({
    environment: Environment,
    days: z.array(UsageDay),
    totals: z.object(usageCounts),
  }),
);
export type UsageReport = z.infer<typeof UsageReport>;
