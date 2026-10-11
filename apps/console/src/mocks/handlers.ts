/**
 * The mock Zupflow API. Each handler serves the wire contract in
 * `@zupflow/contract` (re-exported by `@zupflow/sdk`) and enforces the rules the backend must: membership before
 * existence (404), role before action (403), explicit workspace scope on every
 * lookup, environment-matched configuration with no fallback, exact return-URL
 * allowlisting and single-use Connect sessions.
 */
import {
  APP_KINDS,
  type AppInstallationRef,
  type AvailableApp,
  type ConnectResult,
  ENVIRONMENTS,
  type Environment,
  type Id,
  type IdKind,
  MemberRole,
  type ModelOption,
  type OrganizationId,
  RemoveMemberQuery,
  RUN_STATUSES,
  type SignInSettings,
  TERMINAL_RUN_STATUSES,
  type UsageReport,
  WEBHOOK_EVENT_TYPES,
  WebhookDeliveryAttempt,
  WebhookDeliveryErrorClass,
  type WebhookDeliveryId,
  type WebhookDeliverySummary,
  WebhookEndpoint,
  type WebhookEndpointId,
  WebhookEvent,
  type WorkspaceId,
} from "@zupflow/sdk";
import { HttpResponse, http } from "msw";
import { z } from "zod";
import { checkedAuthTemplate, sameOrigin } from "./auth-templates";
import { defaultToolsFor } from "./catalog";
import { cancelScheduledRun, db, resetDb, scheduleRun } from "./db";
import {
  ADMINS,
  ApiFailure,
  appInScope,
  assertPublicHttps,
  audit,
  authenticate,
  body,
  configurationStatus,
  created,
  DEVELOPERS,
  findSessionByReferenceCrossTenant,
  grantorAllows,
  invitationsOf,
  isExpiredInvitation,
  noContent,
  notFound,
  type OrgAccess,
  ok,
  orgAccess,
  RESERVED_ORG_SLUGS,
  requireRole,
  route,
  SLUG_PATTERN,
  toAgent,
  toApiKey,
  toApp,
  toConfiguration,
  toConfigurationSummary,
  toConnectSession,
  toGrant,
  toInstallation,
  toInvitation,
  toMember,
  toOrganization,
  toRun,
  toRunEvent,
  toUser,
  toWorkspace,
  type WorkspaceAccess,
  withdrawUngrantable,
  withExpiry,
  workspaceAccess,
} from "./http";
import { hashReference, isId, newId, newReference } from "./ids";
import type {
  AgentRow,
  AppRow,
  ConfigurationRow,
  ConnectSessionRow,
  GrantRow,
  InstallationRow,
  MembershipRow,
  OrganizationRow,
  RunRow,
  WebhookDeliveryRow,
} from "./model";
import { levelInForce, levelRowsOf, levelsInput, policyRowOf, setLevels, toAppToolLevel, toToolPolicy } from "./policy";
import { hash, token } from "./random";
import { cancelRun, enqueueRun, planRun, runTarget } from "./runner";
import { appIn, configurationOf, inWorkspace, runEventsKey, timelineOf } from "./scope";
import {
  acceptChange,
  claimRefresh,
  compareNames,
  discoverTools,
  forgetTools,
  toInstallationTool,
  toolsOf,
} from "./tools";

const MODELS: ModelOption[] = [
  {
    id: "claude-sonnet-5-5",
    name: "Claude Sonnet 5.5",
    provider: "Anthropic",
    description: "Balanced default for tool-heavy work.",
  },
  {
    id: "claude-opus-5-5",
    name: "Claude Opus 5.5",
    provider: "Anthropic",
    description: "Most capable. Long, multi-step tasks.",
  },
  {
    id: "claude-haiku-4-5-20251001",
    name: "Claude Haiku 4.5",
    provider: "Anthropic",
    description: "Fast and inexpensive. Simple lookups.",
  },
];

/** What the mock says about sign-in: a fictional provider, never one it reaches. */
export const MOCK_SIGN_IN: SignInSettings = {
  mode: "oidc",
  issuer: "https://sign-in.zupflow.example",
  client_id: "zupflow-console",
};

const slug = z.string().regex(SLUG_PATTERN, "Use 1–40 lowercase letters, numbers and hyphens.");
const environment = z.enum(ENVIRONMENTS, "Choose Sandbox or Production.");
const name = (max = 60) => z.string().trim().min(1, "Required.").max(max, `Use ${max} characters or fewer.`);
const MINUTE = 60_000;

/** How long past its link's expiry a used link still shows its outcome. */
export const OUTCOME_GRACE_MS = 5 * MINUTE;

/**
 * A link shows its session while it's pending, and once it ended until
 * `OUTCOME_GRACE_MS` past the link's expiry (the OAuth callback sends the
 * browser back to it to read the outcome). An expired link, or one used
 * longer ago, is the same 404 as a reference that names no session.
 */
function assertLinkShows(session: ConnectSessionRow) {
  if (session.status === "pending") return;
  if (session.status !== "expired" && Date.now() < Date.parse(session.expires_at) + OUTCOME_GRACE_MS) return;
  throw notFound("Connect session");
}

/** The host of an MCP server whose sign-in the mock can't use: it answers `discovery_failed`. */
export const NO_SIGN_IN_HOST = "no-sign-in.example";
/** The host of an MCP server the mock can't reach: sign-in through it answers `provider_unavailable`. */
export const UNREACHABLE_HOST = "unreachable.example";

function param(params: Record<string, string | readonly string[] | undefined>, key: string): string {
  const value = params[key];
  return typeof value === "string" ? value : "";
}

/**
 * An ID in a request body. A malformed one is accepted here and simply matches
 * nothing, so it answers 404 like any ID that doesn't exist.
 */
const idOf = <K extends IdKind>(_kind: K) =>
  // Unchecked on purpose: the lookup that follows is the check.
  z.string().transform((value) => value as Id<K>);

/** Path parameters that carry an ID, with the object type each must be. */
const ID_PARAMS = {
  member: ["member", "Member"],
  invitation: ["member", "Invitation"],
  app: ["app", "App"],
  inst: ["installation", "Installation"],
  session: ["connect_session", "Connect session"],
  agent: ["agent", "Agent"],
  grant: ["grant", "Grant"],
  run: ["run", "Run"],
  key: ["api_key", "API key"],
  hook: ["webhook_endpoint", "Webhook endpoint"],
  delivery: ["webhook_delivery", "Webhook delivery"],
} as const satisfies Record<string, readonly [IdKind, string]>;

/**
 * The signed-in person's own invitation, or their membership once it's
 * accepted: 401 first, then a 404 that says nothing about whose it is.
 * Found by the invitee alone, across organizations by design.
 */
function ownInvitation(
  request: Request,
  params: Record<string, string | readonly string[] | undefined>,
): { user: ReturnType<typeof authenticate>; invitation: MembershipRow; org: OrganizationRow } {
  const user = authenticate(request);
  const id = idParam(params, "invitation");
  const invitation = db().memberships.find((m) => m.id === id && m.user_id === user.id);
  const org = invitation ? db().organizations.find((o) => o.id === invitation.organization_id) : undefined;
  if (!invitation || !org) throw notFound("Invitation");
  // Withdrawn once its grantor can't grant it any more: as gone as one an admin cancelled.
  if (invitation.status === "invited" && !grantorAllows(invitation)) throw notFound("Invitation");
  return { user, invitation, org };
}

const invitationAccepted = () => new ApiFailure(409, "invitation_accepted", "You've already accepted this invitation.");

/**
 * A path ID of the right type. Anything else (a wrong prefix, a bad length, a
 * bare UUID) can't exist, so it answers 404 exactly like a missing object.
 * Only for child IDs read after access is established, so a missing
 * credential still answers 401 first; `orgAccess` and `workspaceAccess` parse
 * their own IDs after authenticating.
 */
function idParam<P extends keyof typeof ID_PARAMS>(
  params: Record<string, string | readonly string[] | undefined>,
  key: P,
): Id<(typeof ID_PARAMS)[P][0]> {
  const [kind, label] = ID_PARAMS[key];
  const value = param(params, key);
  if (!isId(kind, value)) throw notFound(label);
  return value as Id<(typeof ID_PARAMS)[P][0]>;
}

/**
 * An app's configuration in one environment, empty until first written.
 * Scope comes from the request: the organization, and the workspace for a
 * workspace app (null for an organization app). A `"read"` never stores the
 * empty row; only a write does.
 */
function configurationFor(
  scope: { organization_id: OrganizationId; workspace_id: WorkspaceId | null },
  app: AppRow,
  env: Environment,
  mode: "read" | "write" = "write",
): ConfigurationRow {
  if (app.organization_id !== scope.organization_id || app.owner_workspace_id !== scope.workspace_id) {
    throw notFound("App");
  }
  const d = db();
  let row = configurationOf(d.configurations, app, env);
  if (!row) {
    row = {
      organization_id: scope.organization_id,
      workspace_id: scope.workspace_id,
      app_id: app.id,
      environment: env,
      version: 0,
      mcp_server_url: null,
      oauth_client_id: null,
      oauth_client_secret_last4: null,
      oauth_client_secret_updated_at: null,
      oauth_scopes: [],
      updated_at: null,
      updated_by_user_id: null,
    };
    if (mode === "write") d.configurations.push(row);
  }
  return row;
}

/** The union of eligible organization apps and the workspace's own apps. */
function availableApps(access: WorkspaceAccess): AvailableApp[] {
  const { workspace } = access;
  return db()
    .apps.filter(
      (a) =>
        a.organization_id === workspace.organization_id &&
        (a.owner_workspace_id === null || a.owner_workspace_id === workspace.id),
    )
    .flatMap((app) => {
      const config = configurationOf(db().configurations, app, workspace.environment);
      if (!config || configurationStatus(config) !== "ready") return [];
      return [{ app: toApp(app), configuration: toConfigurationSummary(config) }];
    });
}

/**
 * In Production the Console observes; it never acts on a live customer's
 * accounts. Starting runs and Connect sessions there is for the customer's
 * backend, with an API key. The mock has no API keys, so every caller is a
 * Console session.
 */
function assertConsoleMayAct(access: WorkspaceAccess, action: string) {
  if (access.workspace.environment === "production") {
    throw new ApiFailure(
      403,
      "console_read_only_in_production",
      `The Console can't ${action} in Production. Your product does that through the API.`,
    );
  }
}

function findOrgApp(access: OrgAccess, appId: string): AppRow {
  const app = db().apps.find(
    (a) => a.id === appId && a.organization_id === access.org.id && a.owner_workspace_id === null,
  );
  if (!app) throw notFound("App");
  return app;
}

function findWorkspaceApp(access: WorkspaceAccess, appId: string): AppRow {
  const app = db().apps.find(
    (a) =>
      a.id === appId &&
      a.organization_id === access.workspace.organization_id &&
      a.owner_workspace_id === access.workspace.id,
  );
  if (!app) throw notFound("App");
  return app;
}

function findInstallation(access: WorkspaceAccess, installationId: string): InstallationRow {
  const row = db().installations.find((i) => inWorkspace(i, access.workspace) && i.id === installationId);
  if (!row) throw notFound("Installation");
  return row;
}

function findAgent(access: WorkspaceAccess, agentId: string): AgentRow {
  const row = db().agents.find((a) => inWorkspace(a, access.workspace) && a.id === agentId);
  if (!row) throw notFound("Agent");
  return row;
}

function findRun(access: WorkspaceAccess, runId: string): RunRow {
  const row = db().runs.find((r) => inWorkspace(r, access.workspace) && r.id === runId);
  if (!row) throw notFound("Run");
  return row;
}

const configurationInput = z.object({
  mcp_server_url: z.string().trim().max(500).nullable().optional(),
  oauth_client_id: z.string().trim().max(300).nullable().optional(),
  oauth_client_secret: z.string().min(8, "Paste the full client secret.").max(500).optional(),
  oauth_scopes: z.array(z.string().max(300)).max(50).optional(),
});

function applyConfiguration(
  access: OrgAccess,
  app: AppRow,
  row: ConfigurationRow,
  input: z.infer<typeof configurationInput>,
) {
  if (input.mcp_server_url) assertPublicHttps(input.mcp_server_url, "mcp_server_url");
  if (app.auth.type === "token" && (input.oauth_client_id || input.oauth_client_secret)) {
    throw new ApiFailure(422, "oauth_not_supported", `${app.name} authenticates with a token, not OAuth.`);
  }
  if (
    app.auth.type === "mcp_oauth" &&
    (input.oauth_client_id || input.oauth_client_secret || (input.oauth_scopes?.length ?? 0) > 0)
  ) {
    throw new ApiFailure(
      422,
      "oauth_not_supported",
      `${app.name} signs in through its MCP server: there's no OAuth client to configure, and its scopes are set in its authentication settings.`,
    );
  }
  if (input.oauth_scopes && app.auth.type === "oauth2") {
    const known = new Set(app.auth.available_scopes.map((s) => s.name));
    const unknown = input.oauth_scopes.find((s) => !known.has(s));
    if (unknown) {
      throw new ApiFailure(422, "unknown_scope", `${unknown} is not offered by this app.`, {
        oauth_scopes: `${unknown} is not offered by this app.`,
      });
    }
  }
  const now = new Date().toISOString();
  if (input.mcp_server_url !== undefined) row.mcp_server_url = input.mcp_server_url || null;
  if (input.oauth_client_id !== undefined) row.oauth_client_id = input.oauth_client_id || null;
  if (input.oauth_client_secret) {
    row.oauth_client_secret_last4 = input.oauth_client_secret.slice(-4);
    row.oauth_client_secret_updated_at = now;
  }
  if (input.oauth_scopes) row.oauth_scopes = input.oauth_scopes;
  row.version += 1;
  row.updated_at = now;
  row.updated_by_user_id = access.user.id;
  app.updated_at = now;
}

const appUpdateInput = z.object({
  name: name().optional(),
  description: z.string().trim().max(280).optional(),
  auth: z.unknown().optional(),
});

/**
 * Renames, describes and re-templates an app, all or nothing: the template is
 * checked against the scopes the app's own configurations request before
 * anything changes. A new template raises the app's version.
 */
function applyAppUpdate(app: AppRow, input: z.infer<typeof appUpdateInput>) {
  const configurations = db().configurations.filter(
    (c) =>
      c.organization_id === app.organization_id && c.workspace_id === app.owner_workspace_id && c.app_id === app.id,
  );
  const auth =
    input.auth === undefined
      ? undefined
      : checkedAuthTemplate(
          app.auth,
          input.auth,
          configurations.map((c) => ({ environment: c.environment, scopes: c.oauth_scopes })),
          configurations.flatMap((c) =>
            c.mcp_server_url ? [{ environment: c.environment, url: c.mcp_server_url }] : [],
          ),
        );
  if (input.name) app.name = input.name;
  if (input.description !== undefined) app.description = input.description;
  if (auth) {
    app.auth = auth;
    app.version += 1;
  }
  app.updated_at = new Date().toISOString();
}

const appInput = z.object({
  kind: z.string(),
  name: name(),
  slug,
  description: z.string().trim().max(280),
  auth_type: z.enum(["oauth2", "mcp_oauth", "token"], "Choose OAuth, sign-in through the MCP server, or token."),
});

function createApp(access: OrgAccess, input: z.infer<typeof appInput>, ownerWorkspaceId: WorkspaceId | null) {
  if (!(APP_KINDS as readonly string[]).includes(input.kind)) {
    throw new ApiFailure(400, "unsupported_app_kind", `App kind "${input.kind}" is not supported.`, {
      kind: 'Only "mcp" is supported.',
    });
  }
  const taken = db().apps.some(
    (a) => a.organization_id === access.org.id && a.owner_workspace_id === ownerWorkspaceId && a.slug === input.slug,
  );
  if (taken) {
    throw new ApiFailure(409, "slug_taken", "That slug is already used by another app.", {
      slug: "Already used by another app here.",
    });
  }
  const now = new Date().toISOString();
  const app: AppRow = {
    id: newId("app"),
    organization_id: access.org.id,
    owner_workspace_id: ownerWorkspaceId,
    kind: "mcp",
    name: input.name,
    slug: input.slug,
    description: input.description,
    icon: {
      glyph: input.name.trim().slice(0, 1).toUpperCase(),
      color: ["#0E7490", "#7C3AED", "#B45309", "#15803D", "#BE123C"][hash(input.slug) % 5] ?? "#0E7490",
    },
    auth:
      input.auth_type === "oauth2"
        ? {
            type: "oauth2",
            authorize_url: null,
            token_url: null,
            available_scopes: [],
            pkce: true,
            identity_url: null,
          }
        : input.auth_type === "mcp_oauth"
          ? { type: "mcp_oauth", scopes: [], identity_url: null }
          : {
              type: "token",
              inputs: [
                {
                  key: "token",
                  label: "API token",
                  secret: true,
                  help: null,
                  placeholder: null,
                  pattern: null,
                },
              ],
              placement: { location: "header", name: "Authorization", prefix: "Bearer " },
              verify_url: null,
              instructions: `Paste an API token for ${input.name}.`,
              docs_url: null,
            },
    tools: defaultToolsFor(input.name),
    version: 1,
    created_at: now,
    updated_at: now,
  };
  db().apps.push(app);
  return app;
}

/**
 * What a new session asks for: an OAuth app's configured scopes, an
 * `mcp_oauth` app's template scopes, a token app nothing.
 */
function requestedScopes(app: AppRow | undefined, config: ConfigurationRow | undefined): string[] {
  if (app?.auth.type === "oauth2") return config?.oauth_scopes ?? [];
  if (app?.auth.type === "mcp_oauth") return app.auth.scopes.map((s) => s.name);
  return [];
}

/** Simulated provider identity for a credential or a consenting account. */
function accountFor(app: AppRow, seed: string, displayName: string, email: string | null) {
  return {
    external_id: String(100000000 + (hash(`${app.id}:${seed}`) % 899999999)),
    display_name: displayName,
    email,
  };
}

function assertSessionPending(session: ConnectSessionRow) {
  if (session.status !== "pending") {
    throw new ApiFailure(
      409,
      "session_not_pending",
      session.status === "expired"
        ? "This Connect link has expired. Start again from the app."
        : "This Connect session was already used. Start again from the app.",
    );
  }
}

/** The session's own tenant: every statement after hosted Connect attributes a session goes through these. */
function sessionScope(session: ConnectSessionRow) {
  const { organization_id, workspace_id } = session;
  return {
    app: () => appInScope(session),
    workspace: () => db().workspaces.find((w) => w.organization_id === organization_id && w.id === workspace_id),
    installation: () =>
      db().installations.find(
        (i) =>
          i.organization_id === organization_id && i.workspace_id === workspace_id && i.id === session.installation_id,
      ),
  };
}

/**
 * Sign-in through an `mcp_oauth` app's server. The mock discovers nothing: a
 * server on `no-sign-in.example` stands for one that offers no sign-in
 * Zupflow can use, one on `unreachable.example` for one that's down, and
 * every other server's is found. Either failure ends the session. An
 * identity URL the configuration has moved the server away from is refused
 * first, before anything would be sent.
 */
function startSignIn(session: ConnectSessionRow, app: AppRow) {
  if (app.auth.type !== "mcp_oauth") return;
  const server = configurationOf(db().configurations, app, session.environment)?.mcp_server_url;
  if (!server) return;
  if (app.auth.identity_url && !sameOrigin(app.auth.identity_url, server)) {
    session.status = "failed";
    session.failure_code = "identity_off_server";
    throw new ApiFailure(
      422,
      "identity_off_server",
      `${app.name}'s identity URL isn't on its MCP server. Ask whoever set up the app to use an address on the server, or none.`,
    );
  }
  if (new URL(server).hostname === UNREACHABLE_HOST) {
    session.status = "failed";
    session.failure_code = "provider_unavailable";
    throw new ApiFailure(502, "provider_unavailable", `${app.name}'s server couldn't be reached. Start again shortly.`);
  }
  if (new URL(server).hostname === NO_SIGN_IN_HOST) {
    session.status = "failed";
    session.failure_code = "discovery_failed";
    throw new ApiFailure(
      502,
      "discovery_failed",
      `${app.name}'s server doesn't offer a sign-in Zupflow can use. Ask whoever set up the app to check its server.`,
    );
  }
}

/** Revalidate eligibility at completion, not only at session creation. */
function assertStillEligible(session: ConnectSessionRow): AppRow {
  const scope = sessionScope(session);
  const app = scope.app();
  const workspace = scope.workspace();
  const config = app ? configurationOf(db().configurations, app, session.environment) : undefined;
  if (!app || !workspace || workspace.environment !== session.environment) {
    throw new ApiFailure(422, "app_not_eligible", "This app is no longer available to this workspace.");
  }
  if (!config || configurationStatus(config) !== "ready") {
    throw new ApiFailure(
      422,
      "configuration_not_ready",
      `${app.name} isn't configured for ${session.environment} anymore.`,
    );
  }
  return app;
}

function completeSession(
  session: ConnectSessionRow,
  app: AppRow,
  account: ReturnType<typeof accountFor>,
): ConnectResult {
  const now = new Date().toISOString();
  const d = db();
  const config = configurationOf(d.configurations, app, session.environment);
  let installation: InstallationRow;

  if (session.intent === "reconnect") {
    const existing = sessionScope(session).installation();
    if (!existing) throw notFound("Installation");
    const previous = existing.connection?.account.external_id;
    if (previous && previous !== account.external_id) {
      session.status = "failed";
      session.failure_code = "account_mismatch";
      throw new ApiFailure(
        409,
        "account_mismatch",
        `That's a different ${app.name} account. Reconnect with ${existing.connection?.account.display_name}, or add a new installation for the other account.`,
      );
    }
    existing.status = "connected";
    existing.configuration_version = config?.version ?? existing.configuration_version;
    existing.connection = {
      account,
      granted_scopes: session.requested_scopes,
      credential_version: (existing.connection?.credential_version ?? 0) + 1,
      connected_at: now,
      expires_at: app.auth.type !== "token" ? new Date(Date.now() + 60 * MINUTE).toISOString() : null,
    };
    existing.updated_at = now;
    installation = existing;
  } else {
    installation = {
      id: newId("installation"),
      organization_id: session.organization_id,
      workspace_id: session.workspace_id,
      app_id: app.id,
      label: session.label,
      status: "connected",
      configuration_version: config?.version ?? 1,
      connection: {
        account,
        granted_scopes: session.requested_scopes,
        credential_version: 1,
        connected_at: now,
        expires_at: app.auth.type !== "token" ? new Date(Date.now() + 60 * MINUTE).toISOString() : null,
      },
      created_at: now,
      updated_at: now,
    };
    d.installations.push(installation);
    session.installation_id = installation.id;
  }

  session.status = "completed";
  session.completed_at = now;
  // The backend queues discovery; the mock lists the app's tools at once.
  discoverTools(d, installation, now);
  d.audit.unshift({
    id: newId("audit_event"),
    organization_id: session.organization_id,
    workspace_id: session.workspace_id,
    environment: session.environment,
    actor: { type: "system", id: null, name: "Connect" },
    action: session.intent === "reconnect" ? "installation.reconnected" : "installation.connected",
    target: { type: "installation", id: installation.id, name: `${app.name} · ${installation.label}` },
    created_at: now,
  });
  return { session: toConnectSession(session), installation: toInstallation(installation) };
}

/**
 * A webhook URL as the audit log names it: scheme, host and path. A query
 * or fragment may carry a token, so neither is ever recorded.
 */
export function auditUrl(url: string): string {
  try {
    const parsed = new URL(url);
    return `${parsed.protocol}//${parsed.host}${parsed.pathname}`;
  } catch {
    return "Webhook endpoint";
  }
}

function webhookSecret(env: Environment) {
  return `whsec_${env === "production" ? "live" : "test"}_${token(32)}`;
}

/** An integer query parameter clamped to 1..max; missing or not an integer means the default. */
function clampedInt(url: URL, name: string, fallback: number, max: number): number {
  const value = Number(url.searchParams.get(name) ?? fallback);
  return Number.isInteger(value) ? Math.min(Math.max(value, 1), max) : fallback;
}

/** A tool list's cursor: the page's last name, base64url-encoded. */
function encodeToolCursor(name: string): string {
  const binary = Array.from(new TextEncoder().encode(name), (b) => String.fromCharCode(b)).join("");
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

function toolCursorName(cursor: string): string {
  try {
    const binary = atob(cursor.replaceAll("-", "+").replaceAll("_", "/"));
    const name = new TextDecoder("utf-8", { fatal: true }).decode(Uint8Array.from(binary, (c) => c.charCodeAt(0)));
    if (name) return name;
  } catch {
    // Falls through to the refusal.
  }
  throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", {
    cursor: "Not a cursor from this list.",
  });
}

function paginate<T>(items: T[], cursor: string | null, limit: number) {
  const offset = cursor ? Number.parseInt(atob(cursor), 10) || 0 : 0;
  const page = items.slice(offset, offset + limit);
  const next = offset + limit < items.length ? btoa(String(offset + limit)) : null;
  return { page, meta: { pagination: { next_cursor: next, total: items.length } } };
}

function findWebhook(organizationId: OrganizationId, endpointId: WebhookEndpointId) {
  const endpoint = db().webhooks.find((row) => row.organization_id === organizationId && row.id === endpointId);
  if (!endpoint) throw notFound("Webhook endpoint");
  return endpoint;
}

function toWebhookEndpoint(endpoint: WebhookEndpoint): WebhookEndpoint {
  const expires = endpoint.previous_signing_secret_expires_at;
  return WebhookEndpoint.parse({
    ...endpoint,
    previous_signing_secret_expires_at: expires !== null && Date.parse(expires) > Date.now() ? expires : null,
  });
}

/** Joins an outbox event only within the endpoint's organization and environment. */
function webhookEventFor(organizationId: OrganizationId, endpoint: WebhookEndpoint, row: WebhookDeliveryRow) {
  if (row.organization_id !== organizationId || row.endpoint_id !== endpoint.id) return null;
  const event = db().webhook_events.find(
    (event) =>
      event.organization_id === organizationId &&
      event.id === row.event_id &&
      event.environment === endpoint.environment,
  );
  if (!event) return null;
  const parsed = WebhookEvent.safeParse(event.body);
  if (!parsed.success) return null;
  const body = parsed.data;
  if (
    body.organization_id !== organizationId ||
    body.environment !== endpoint.environment ||
    body.id !== event.id ||
    (body.workspace?.id ?? null) !== event.workspace_id
  )
    return null;
  if (
    event.workspace_id !== null &&
    !db().workspaces.some(
      (workspace) =>
        workspace.organization_id === organizationId &&
        workspace.id === event.workspace_id &&
        workspace.environment === endpoint.environment,
    )
  )
    return null;
  return body;
}

function webhookSummary(row: WebhookDeliveryRow, event: WebhookEvent): WebhookDeliverySummary {
  return {
    id: row.id,
    organization_id: row.organization_id,
    endpoint_id: row.endpoint_id,
    event_id: row.event_id,
    state: row.state,
    attempts: row.attempts,
    last_status_code: row.last_status_code,
    last_attempt_at: row.last_attempt_at,
    next_attempt_at: row.next_attempt_at,
    created_at: row.created_at,
    environment: event.environment,
    event_type: event.type,
    event_created_at: event.created_at,
    workspace: event.workspace,
    last_error: row.last_error === null ? null : (WebhookDeliveryErrorClass.safeParse(row.last_error).data ?? "failed"),
  };
}

function findWebhookDelivery(organizationId: OrganizationId, endpoint: WebhookEndpoint, deliveryId: WebhookDeliveryId) {
  const row = db().webhook_deliveries.find(
    (row) => row.organization_id === organizationId && row.endpoint_id === endpoint.id && row.id === deliveryId,
  );
  const event = row ? webhookEventFor(organizationId, endpoint, row) : null;
  if (!row || !event) throw notFound("Webhook delivery");
  return { row, event };
}

function webhookAttempts(organizationId: OrganizationId, endpointId: WebhookEndpointId, deliveryId: WebhookDeliveryId) {
  return db()
    .webhook_delivery_attempts.filter(
      (row) =>
        row.organization_id === organizationId && row.endpoint_id === endpointId && row.delivery_id === deliveryId,
    )
    .sort((a, b) => a.number - b.number)
    .map((row) =>
      WebhookDeliveryAttempt.parse({
        ...row,
        error_class:
          row.error_class === null ? null : (WebhookDeliveryErrorClass.safeParse(row.error_class).data ?? "failed"),
      }),
    );
}

function webhookCursor<T extends z.ZodType>(cursor: string | null, scope: string, shape: T): z.infer<T> | null {
  if (cursor === null) return null;
  try {
    const decoded: unknown = JSON.parse(toolCursorName(cursor));
    const parsed = z
      .object({ scope: z.literal(scope), position: z.unknown() })
      .strict()
      .safeParse(decoded);
    if (parsed.success) return shape.parse(parsed.data.position);
  } catch {
    // Invalid cursors never become an unfiltered first page.
  }
  throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", {
    cursor: "Not a cursor from this list.",
  });
}

const nextWebhookCursor = (scope: string, position: unknown) => encodeToolCursor(JSON.stringify({ scope, position }));

export function createHandlers(apiBase: string) {
  const u = (path: string) => `${apiBase.replace(/\/$/, "")}${path}`;

  return [
    // --- Identity ---------------------------------------------------------
    // Public: no credential, public values only. The mock's simulated sign-in
    // stands in for an OpenID Connect provider, so it answers `oidc`; the
    // Console on the mock signs in through the simulation instead.
    http.get(
      u("/v1/sign-in-settings"),
      route(() => ok(MOCK_SIGN_IN)),
    ),

    http.get(
      u("/v1/me"),
      route(({ request }) => {
        const user = authenticate(request);
        const organizations = db()
          .memberships.filter((m) => m.user_id === user.id && m.status === "active")
          .flatMap((m) => {
            const org = db().organizations.find((o) => o.id === m.organization_id);
            return org ? [toOrganization(org, m)] : [];
          });
        return ok({ user, organizations });
      }),
    ),

    // --- Invitations: the signed-in person's own ----------------------------
    // Click to accept: nobody joins an organization without accepting, and
    // signing in with an invited address joins nothing by itself. An
    // invitation is the invitee's pending membership, found by the invitee
    // alone: anyone else's, and one withdrawn or declined, is the same 404.
    // There's no member role to check: it's the person's own answer.
    http.get(
      u("/v1/me/invitations"),
      route(({ request }) => {
        const user = authenticate(request);
        const invitations = invitationsOf(user.id).flatMap((m) => {
          const org = db().organizations.find((o) => o.id === m.organization_id);
          return org ? [toInvitation(m, org)] : [];
        });
        return ok(invitations);
      }),
    ),

    http.post(
      u("/v1/me/invitations/:invitation/accept"),
      route(({ request, params }) => {
        const { user, invitation, org } = ownInvitation(request, params);
        if (invitation.status === "active") throw invitationAccepted();
        if (isExpiredInvitation(invitation)) {
          throw new ApiFailure(
            422,
            "invitation_expired",
            "This invitation has expired. Ask an admin of the organization to invite you again.",
          );
        }
        // The role is the invitation's as it stands now: an owner may have changed it since it was sent.
        const now = new Date().toISOString();
        invitation.status = "active";
        invitation.joined_at = now;
        invitation.last_active_at = now;
        audit({ user, org, membership: invitation }, "member.joined", {
          type: "member",
          id: invitation.id,
          name: user.email,
        });
        return ok(toOrganization(org, invitation));
      }),
    ),

    http.post(
      u("/v1/me/invitations/:invitation/decline"),
      route(({ request, params }) => {
        const { user, invitation, org } = ownInvitation(request, params);
        if (invitation.status === "active") throw invitationAccepted();
        // Expired or not, declining withdraws it.
        const d = db();
        d.memberships = d.memberships.filter((m) => m !== invitation);
        audit({ user, org, membership: invitation }, "member.declined", {
          type: "member",
          id: invitation.id,
          name: user.email,
        });
        return noContent();
      }),
    ),

    http.get(
      u("/v1/models"),
      route(({ request }) => {
        authenticate(request);
        return ok(MODELS);
      }),
    ),

    // --- Organizations -------------------------------------------------------
    http.post(
      u("/v1/organizations"),
      route(async ({ request }) => {
        const user = authenticate(request);
        const input = await body(request, z.object({ name: name(), slug }));
        if (RESERVED_ORG_SLUGS.has(input.slug)) {
          throw new ApiFailure(422, "slug_reserved", "That URL is reserved.", {
            slug: "Reserved. Choose another.",
          });
        }
        if (db().organizations.some((o) => o.slug === input.slug)) {
          throw new ApiFailure(409, "slug_taken", "That URL is taken.", { slug: "Already taken." });
        }
        const now = new Date().toISOString();
        const org = { id: newId("organization"), name: input.name, slug: input.slug, created_at: now };
        const membership = {
          id: newId("member"),
          organization_id: org.id,
          user_id: user.id,
          role: "owner" as const,
          status: "active" as const,
          invited_at: now,
          invited_by: null,
          joined_at: now,
          last_active_at: now,
        };
        db().organizations.push(org);
        db().memberships.push(membership);
        for (const env of ENVIRONMENTS) {
          db().connect_settings.push({
            organization_id: org.id,
            environment: env,
            return_urls: [],
            allowed_origins: [],
            updated_at: null,
          });
        }
        audit({ user, org, membership }, "organization.created", {
          type: "organization",
          id: org.id,
          name: org.name,
        });
        return created(toOrganization(org, membership));
      }),
    ),

    http.get(
      u("/v1/organizations/:org"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        return ok(toOrganization(access.org, access.membership));
      }),
    ),

    http.patch(
      u("/v1/organizations/:org"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "change organization settings");
        const input = await body(request, z.object({ name: name().optional(), slug: slug.optional() }));
        if (input.slug && input.slug !== access.org.slug) {
          if (RESERVED_ORG_SLUGS.has(input.slug)) {
            throw new ApiFailure(422, "slug_reserved", "That URL is reserved.", { slug: "Reserved." });
          }
          if (db().organizations.some((o) => o.slug === input.slug)) {
            throw new ApiFailure(409, "slug_taken", "That URL is taken.", { slug: "Already taken." });
          }
          access.org.slug = input.slug;
        }
        if (input.name) access.org.name = input.name;
        audit(access, "organization.updated", { type: "organization", id: access.org.id, name: access.org.name });
        return ok(toOrganization(access.org, access.membership));
      }),
    ),

    http.get(
      u("/v1/organizations/:org/usage"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        const url = new URL(request.url);
        const env = environment.safeParse(url.searchParams.get("environment"));
        if (!env.success) {
          throw new ApiFailure(400, "validation_failed", "Choose an environment.", {
            environment: "Required.",
          });
        }
        const days = clampedInt(url, "days", 14, 90);
        const workspaceIds = new Set(
          db()
            .workspaces.filter((w) => w.organization_id === access.org.id && w.environment === env.data)
            .map((w) => w.id),
        );
        const buckets = new Map<string, UsageReport["days"][number]>();
        const today = new Date();
        today.setUTCHours(0, 0, 0, 0);
        for (let i = days - 1; i >= 0; i--) {
          const date = new Date(today.getTime() - i * 86_400_000).toISOString().slice(0, 10);
          buckets.set(date, { date, runs: 0, failed_runs: 0, tool_calls: 0 });
        }
        for (const run of db().runs) {
          if (run.organization_id !== access.org.id || !workspaceIds.has(run.workspace_id)) continue;
          const bucket = buckets.get(run.created_at.slice(0, 10));
          if (!bucket) continue;
          bucket.runs += 1;
          if (run.status === "failed") bucket.failed_runs += 1;
          bucket.tool_calls += run.usage.tool_calls;
        }
        const list = [...buckets.values()];
        const report: UsageReport = {
          environment: env.data,
          days: list,
          totals: {
            runs: list.reduce((n, d) => n + d.runs, 0),
            failed_runs: list.reduce((n, d) => n + d.failed_runs, 0),
            tool_calls: list.reduce((n, d) => n + d.tool_calls, 0),
          },
        };
        return ok(report);
      }),
    ),

    // --- Members -------------------------------------------------------------
    http.get(
      u("/v1/organizations/:org/members"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        return ok(
          db()
            .memberships.filter((m) => m.organization_id === access.org.id)
            .map(toMember),
        );
      }),
    ),

    http.post(
      u("/v1/organizations/:org/members"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "invite members");
        const input = await body(
          request,
          z.object({
            email: z.string().trim().toLowerCase().email("Enter a valid email address."),
            role: MemberRole,
          }),
        );
        if (input.role === "owner" && access.membership.role !== "owner") {
          throw new ApiFailure(403, "forbidden", "Only owners can invite owners.");
        }
        let user = db().users.find((x) => x.email === input.email);
        const existing = user
          ? db().memberships.find((m) => m.organization_id === access.org.id && m.user_id === user?.id)
          : undefined;
        if (existing && isExpiredInvitation(existing)) {
          // An expired invitation is sent again: as new, with this role and this grantor.
          existing.role = input.role;
          existing.invited_at = new Date().toISOString();
          existing.invited_by = access.user.id;
          audit(access, "member.invited", { type: "member", id: existing.id, name: input.email });
          return created(toMember(existing));
        }
        if (existing) {
          throw new ApiFailure(409, "already_member", `${input.email} is already in this organization.`, {
            email: "Already a member or invited.",
          });
        }
        if (!user) {
          user = {
            id: newId("user"),
            name: input.email.split("@")[0] ?? input.email,
            email: input.email,
            avatar_url: null,
          };
          db().users.push(user);
        }
        const membership = {
          id: newId("member"),
          organization_id: access.org.id,
          user_id: user.id,
          role: input.role,
          status: "invited" as const,
          invited_at: new Date().toISOString(),
          invited_by: access.user.id,
          joined_at: null,
          last_active_at: null,
        };
        db().memberships.push(membership);
        audit(access, "member.invited", { type: "member", id: membership.id, name: input.email });
        return created(toMember(membership));
      }),
    ),

    http.patch(
      u("/v1/organizations/:org/members/:member"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ["owner"], "change roles");
        const input = await body(request, z.object({ role: MemberRole }));
        const member = db().memberships.find(
          (m) => m.organization_id === access.org.id && m.id === idParam(params, "member"),
        );
        if (!member) throw notFound("Member");
        const owners = db().memberships.filter(
          (m) => m.organization_id === access.org.id && m.role === "owner" && m.status === "active",
        );
        // Only an active owner can be the last one; an invited owner can always be changed.
        if (member.role === "owner" && member.status === "active" && input.role !== "owner" && owners.length <= 1) {
          throw new ApiFailure(422, "last_owner", "An organization needs at least one owner.");
        }
        member.role = input.role;
        if (member.status === "invited") {
          // Whoever changes an invitation's role grants it now.
          member.invited_by = access.user.id;
        } else {
          withdrawUngrantable(access.org.id, member.user_id, input.role);
        }
        audit(access, "member.role_changed", { type: "member", id: member.id, name: toMember(member).user.email });
        return ok(toMember(member));
      }),
    ),

    http.delete(
      u("/v1/organizations/:org/members/:member"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "remove members");
        const d = db();
        const member = d.memberships.find(
          (m) => m.organization_id === access.org.id && m.id === idParam(params, "member"),
        );
        const query = RemoveMemberQuery.safeParse({
          status: new URL(request.url).searchParams.get("status") ?? undefined,
        });
        if (!query.success) {
          throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", { status: "Use `invited`." });
        }
        if (!member) throw notFound("Member");
        // Withdrawing an invitation never removes someone who has just accepted it.
        if (query.data.status === "invited" && member.status !== "invited") {
          throw new ApiFailure(409, "invitation_accepted", "That invitation was accepted: they're a member now.");
        }
        // Only owners remove an owner, invited or active (403 before any 422).
        if (member.role === "owner") requireRole(access, ["owner"], "remove an owner");
        if (member.role === "owner" && member.status === "active") {
          // Active owners only: an invited owner can't run the organization yet.
          const owners = d.memberships.filter(
            (m) => m.organization_id === access.org.id && m.role === "owner" && m.status === "active",
          );
          if (owners.length <= 1) {
            throw new ApiFailure(422, "last_owner", "An organization needs at least one owner.");
          }
        }
        d.memberships = d.memberships.filter((m) => m !== member);
        // What they granted goes with them.
        if (member.status === "active") withdrawUngrantable(access.org.id, member.user_id, null);
        audit(access, "member.removed", { type: "member", id: member.id, name: toMember(member).user.email });
        return noContent();
      }),
    ),

    // --- Workspaces ----------------------------------------------------------
    http.get(
      u("/v1/organizations/:org/workspaces"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        const url = new URL(request.url);
        const env = url.searchParams.get("environment");
        if (env && !environment.safeParse(env).success) {
          throw new ApiFailure(400, "validation_failed", "Unknown environment.", { environment: "Unknown." });
        }
        const q = (url.searchParams.get("q") ?? "").trim().toLowerCase();
        const sort = url.searchParams.get("sort") === "activity" ? "activity" : "name";
        const limit = clampedInt(url, "limit", 25, 100);
        let rows = db().workspaces.filter(
          (w) =>
            w.organization_id === access.org.id &&
            (!env || w.environment === env) &&
            (!q || w.name.toLowerCase().includes(q) || w.slug.includes(q) || w.external_id?.toLowerCase().includes(q)),
        );
        if (sort === "activity") {
          const since = Date.now() - 24 * 3600_000;
          const recent = new Map<string, number>();
          for (const r of db().runs) {
            if (r.organization_id === access.org.id && Date.parse(r.created_at) >= since) {
              recent.set(r.workspace_id, (recent.get(r.workspace_id) ?? 0) + 1);
            }
          }
          rows = rows.sort((a, b) => (recent.get(b.id) ?? 0) - (recent.get(a.id) ?? 0) || a.name.localeCompare(b.name));
        } else {
          rows = rows.sort((a, b) => a.name.localeCompare(b.name) || a.environment.localeCompare(b.environment));
        }
        const { page, meta } = paginate(rows, url.searchParams.get("cursor"), limit);
        return ok(page.map(toWorkspace), { meta });
      }),
    ),

    http.post(
      u("/v1/organizations/:org/workspaces"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "create workspaces");
        const input = await body(
          request,
          z.object({
            name: name(),
            slug,
            environment,
            external_id: z.string().trim().max(120).nullable(),
          }),
        );
        if (db().workspaces.some((w) => w.organization_id === access.org.id && w.slug === input.slug)) {
          throw new ApiFailure(409, "slug_taken", "Another workspace uses that slug.", {
            slug: "Already used in this organization.",
          });
        }
        const workspace = {
          id: newId("workspace"),
          organization_id: access.org.id,
          name: input.name,
          slug: input.slug,
          environment: input.environment,
          external_id: input.external_id || null,
          created_at: new Date().toISOString(),
        };
        db().workspaces.push(workspace);
        audit(
          access,
          "workspace.created",
          { type: "workspace", id: workspace.id, name: workspace.name },
          { workspace },
        );
        return created(toWorkspace(workspace));
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws"),
      route(({ request, params }) => ok(toWorkspace(workspaceAccess(request, param(params, "ws")).workspace))),
    ),

    http.patch(
      u("/v1/workspaces/:ws"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, ADMINS, "change workspace settings");
        const raw = (await request
          .clone()
          .json()
          .catch(() => ({}))) as Record<string, unknown>;
        if ("environment" in raw) {
          throw new ApiFailure(
            422,
            "environment_immutable",
            "A workspace's environment can't change. Create a workspace in the other environment instead.",
          );
        }
        const input = await body(
          request,
          z.object({ name: name().optional(), external_id: z.string().trim().max(120).nullable().optional() }),
        );
        if (input.name) access.workspace.name = input.name;
        if (input.external_id !== undefined) access.workspace.external_id = input.external_id || null;
        audit(
          access,
          "workspace.updated",
          { type: "workspace", id: access.workspace.id, name: access.workspace.name },
          { workspace: access.workspace },
        );
        return ok(toWorkspace(access.workspace));
      }),
    ),

    http.delete(
      u("/v1/workspaces/:ws"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, ["owner"], "delete workspaces");
        const d = db();
        const workspace = access.workspace;
        const here = (row: { organization_id: OrganizationId; workspace_id: WorkspaceId }) =>
          inWorkspace(row, workspace);
        for (const run of d.runs.filter(here)) {
          cancelScheduledRun(run);
          const key = runEventsKey(run);
          const rest = (d.run_events[key] ?? []).filter((e) => !here(e));
          if (rest.length > 0) d.run_events[key] = rest;
          else delete d.run_events[key];
        }
        d.runs = d.runs.filter((r) => !here(r));
        d.grants = d.grants.filter((g) => !here(g));
        d.agents = d.agents.filter((a) => !here(a));
        d.installations = d.installations.filter((i) => !here(i));
        d.installation_tools = d.installation_tools.filter((t) => !here(t));
        d.tool_refreshes = d.tool_refreshes.filter((r) => !here(r));
        d.connect_sessions = d.connect_sessions.filter((s) => !here(s));
        const deletedEvents = new Set(
          d.webhook_events
            .filter(
              (event) => event.organization_id === workspace.organization_id && event.workspace_id === workspace.id,
            )
            .map((event) => event.id),
        );
        const deletedDeliveries = new Set(
          d.webhook_deliveries
            .filter(
              (delivery) =>
                delivery.organization_id === workspace.organization_id && deletedEvents.has(delivery.event_id),
            )
            .map((delivery) => delivery.id),
        );
        d.webhook_delivery_attempts = d.webhook_delivery_attempts.filter(
          (attempt) =>
            !(attempt.organization_id === workspace.organization_id && deletedDeliveries.has(attempt.delivery_id)),
        );
        d.webhook_deliveries = d.webhook_deliveries.filter(
          (delivery) =>
            !(delivery.organization_id === workspace.organization_id && deletedEvents.has(delivery.event_id)),
        );
        d.webhook_events = d.webhook_events.filter(
          (event) => !(event.organization_id === workspace.organization_id && event.workspace_id === workspace.id),
        );
        // Saved responses describe the workspace's rows, so they go with them.
        d.idempotency_keys = d.idempotency_keys.filter(
          (r) => !(r.organization_id === workspace.organization_id && r.workspace_id === workspace.id),
        );
        // The workspace's own apps and their configurations, which carry the workspace too.
        d.configurations = d.configurations.filter(
          (c) => !(c.organization_id === workspace.organization_id && c.workspace_id === workspace.id),
        );
        d.apps = d.apps.filter(
          (a) => !(a.organization_id === workspace.organization_id && a.owner_workspace_id === workspace.id),
        );
        d.workspaces = d.workspaces.filter(
          (w) => !(w.organization_id === workspace.organization_id && w.id === workspace.id),
        );
        audit(
          access,
          "workspace.deleted",
          { type: "workspace", id: workspace.id, name: workspace.name },
          // Kept with the workspace's ID, so its history still finds it.
          { workspace: access.workspace },
        );
        return noContent();
      }),
    ),

    // --- Organization apps -----------------------------------------------------
    http.get(
      u("/v1/organizations/:org/apps"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        return ok(
          db()
            .apps.filter((a) => a.organization_id === access.org.id && a.owner_workspace_id === null)
            .sort((a, b) => a.name.localeCompare(b.name))
            .map(toApp),
        );
      }),
    ),

    http.post(
      u("/v1/organizations/:org/apps"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "add apps to the catalog");
        const app = createApp(access, await body(request, appInput), null);
        for (const env of ENVIRONMENTS)
          configurationFor({ organization_id: access.org.id, workspace_id: null }, app, env);
        audit(access, "app.created", { type: "app", id: app.id, name: app.name });
        return created(toApp(app));
      }),
    ),

    http.get(
      u("/v1/organizations/:org/apps/:app"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        return ok(toApp(findOrgApp(access, idParam(params, "app"))));
      }),
    ),

    http.patch(
      u("/v1/organizations/:org/apps/:app"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "edit apps");
        const app = findOrgApp(access, idParam(params, "app"));
        applyAppUpdate(app, await body(request, appUpdateInput));
        audit(access, "app.updated", { type: "app", id: app.id, name: app.name });
        return ok(toApp(app));
      }),
    ),

    // --- Tool policy --------------------------------------------------------------
    http.get(
      u("/v1/organizations/:org/tool-policy"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        return ok(toToolPolicy(db(), access.org.id, toUser));
      }),
    ),

    http.put(
      u("/v1/organizations/:org/tool-policy"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "change the tool policy");
        const input = await body(
          request,
          z.object({
            mode: z.enum(["manual"], "Choose manual."),
            new_tool_level: z.enum(["approve", "deny"], "Choose approve or deny."),
          }),
        );
        const now = new Date().toISOString();
        const row = policyRowOf(db(), access.org.id);
        if (row) {
          row.mode = input.mode;
          row.new_tool_level = input.new_tool_level;
          row.updated_at = now;
          row.updated_by_user_id = access.user.id;
        } else {
          db().tool_policies.push({
            organization_id: access.org.id,
            ...input,
            updated_at: now,
            updated_by_user_id: access.user.id,
          });
        }
        audit(access, "tool_policy.updated", { type: "tool_policy", id: null, name: "Tool policy" });
        return ok(toToolPolicy(db(), access.org.id, toUser));
      }),
    ),

    http.get(
      u("/v1/organizations/:org/apps/:app/tool-levels"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        const app = findOrgApp(access, idParam(params, "app"));
        const url = new URL(request.url);
        const limit = clampedInt(url, "limit", 25, 100);
        const cursor = url.searchParams.get("cursor");
        const after = cursor ? toolCursorName(cursor) : null;
        const all = levelRowsOf(db(), access.org.id, app.id).sort((a, b) => compareNames(a.tool, b.tool));
        const rest = after === null ? all : all.filter((l) => compareNames(l.tool, after) > 0);
        const page = rest.slice(0, limit);
        const last = page.at(-1);
        const next = rest.length > limit && last ? encodeToolCursor(last.tool) : null;
        return ok(
          page.map((l) => toAppToolLevel(l, toUser)),
          { meta: { pagination: { next_cursor: next, total: all.length } } },
        );
      }),
    ),

    http.patch(
      u("/v1/organizations/:org/apps/:app/tool-levels"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "set tool levels");
        const app = findOrgApp(access, idParam(params, "app"));
        const input = await body(request, z.object({ levels: z.unknown() }));
        const levels = levelsInput(input.levels);
        const rows = setLevels(db(), access.org.id, app.id, levels, access.user.id, new Date().toISOString());
        audit(access, "app.tool_levels_updated", { type: "app", id: app.id, name: app.name });
        return ok(rows.sort((a, b) => compareNames(a.tool, b.tool)).map((l) => toAppToolLevel(l, toUser)));
      }),
    ),

    http.get(
      u("/v1/organizations/:org/apps/:app/environments/:env/configuration"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        const app = findOrgApp(access, idParam(params, "app"));
        const env = environment.safeParse(param(params, "env"));
        if (!env.success) throw notFound("Environment");
        return ok(
          toConfiguration(
            configurationFor({ organization_id: access.org.id, workspace_id: null }, app, env.data, "read"),
            app,
          ),
        );
      }),
    ),

    http.put(
      u("/v1/organizations/:org/apps/:app/environments/:env/configuration"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "configure apps");
        const app = findOrgApp(access, idParam(params, "app"));
        const env = environment.safeParse(param(params, "env"));
        if (!env.success) throw notFound("Environment");
        const row = configurationFor({ organization_id: access.org.id, workspace_id: null }, app, env.data);
        applyConfiguration(access, app, row, await body(request, configurationInput));
        audit(
          access,
          "app_configuration.updated",
          { type: "app", id: app.id, name: app.name },
          { environment: env.data },
        );
        return ok(toConfiguration(row, app));
      }),
    ),

    http.get(
      u("/v1/organizations/:org/apps/:app/installations"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        const app = findOrgApp(access, idParam(params, "app"));
        const url = new URL(request.url);
        const env = url.searchParams.get("environment");
        if (env && !environment.safeParse(env).success) {
          throw new ApiFailure(400, "validation_failed", "Unknown environment.", { environment: "Unknown." });
        }
        const limit = clampedInt(url, "limit", 25, 100);
        const refs: AppInstallationRef[] = db()
          .installations.filter((i) => i.organization_id === access.org.id && i.app_id === app.id)
          .sort((a, b) => b.created_at.localeCompare(a.created_at))
          .flatMap((i) => {
            const w = db().workspaces.find((x) => x.organization_id === access.org.id && x.id === i.workspace_id);
            return w && (!env || w.environment === env)
              ? [
                  {
                    installation: toInstallation(i),
                    workspace: { id: w.id, name: w.name, slug: w.slug, environment: w.environment },
                  },
                ]
              : [];
          });
        const { page, meta } = paginate(refs, url.searchParams.get("cursor"), limit);
        return ok(page, { meta });
      }),
    ),

    // --- Workspace apps ----------------------------------------------------------
    http.get(
      u("/v1/workspaces/:ws/apps"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        return ok(
          db()
            .apps.filter((a) => a.organization_id === access.org.id && a.owner_workspace_id === access.workspace.id)
            .sort((a, b) => a.name.localeCompare(b.name))
            .map(toApp),
        );
      }),
    ),

    http.post(
      u("/v1/workspaces/:ws/apps"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, ADMINS, "create workspace apps");
        const app = createApp(access, await body(request, appInput), access.workspace.id);
        configurationFor(
          { organization_id: access.org.id, workspace_id: access.workspace.id },
          app,
          access.workspace.environment,
        );
        audit(access, "app.created", { type: "app", id: app.id, name: app.name }, { workspace: access.workspace });
        return created(toApp(app));
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/apps/:app"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        return ok(toApp(findWorkspaceApp(access, idParam(params, "app"))));
      }),
    ),

    http.patch(
      u("/v1/workspaces/:ws/apps/:app"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, ADMINS, "edit apps");
        const app = findWorkspaceApp(access, idParam(params, "app"));
        applyAppUpdate(app, await body(request, appUpdateInput));
        audit(access, "app.updated", { type: "app", id: app.id, name: app.name }, { workspace: access.workspace });
        return ok(toApp(app));
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/apps/:app/configuration"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const app = findWorkspaceApp(access, idParam(params, "app"));
        return ok(
          toConfiguration(
            configurationFor(
              { organization_id: access.org.id, workspace_id: access.workspace.id },
              app,
              access.workspace.environment,
              "read",
            ),
            app,
          ),
        );
      }),
    ),

    http.put(
      u("/v1/workspaces/:ws/apps/:app/configuration"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, ADMINS, "configure apps");
        const app = findWorkspaceApp(access, idParam(params, "app"));
        const raw = (await request
          .clone()
          .json()
          .catch(() => ({}))) as Record<string, unknown>;
        if ("environment" in raw && raw.environment !== access.workspace.environment) {
          throw new ApiFailure(
            422,
            "environment_mismatch",
            `Workspace apps inherit the workspace's environment (${access.workspace.environment}).`,
          );
        }
        const row = configurationFor(
          { organization_id: access.org.id, workspace_id: access.workspace.id },
          app,
          access.workspace.environment,
        );
        applyConfiguration(access, app, row, await body(request, configurationInput));
        audit(
          access,
          "app_configuration.updated",
          { type: "app", id: app.id, name: app.name },
          { workspace: access.workspace },
        );
        return ok(toConfiguration(row, app));
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/available-apps"),
      route(({ request, params }) => ok(availableApps(workspaceAccess(request, param(params, "ws"))))),
    ),

    // --- Installations -----------------------------------------------------------
    http.get(
      u("/v1/workspaces/:ws/installations"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        return ok(
          db()
            .installations.filter((i) => inWorkspace(i, access.workspace))
            .sort((a, b) => b.created_at.localeCompare(a.created_at))
            .map(toInstallation),
        );
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/installations/:inst"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        return ok(toInstallation(findInstallation(access, idParam(params, "inst"))));
      }),
    ),

    http.patch(
      u("/v1/workspaces/:ws/installations/:inst"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "rename installations");
        const row = findInstallation(access, idParam(params, "inst"));
        const input = await body(request, z.object({ label: name(40) }));
        row.label = input.label;
        row.updated_at = new Date().toISOString();
        const app = appInScope(row);
        audit(
          access,
          "installation.updated",
          { type: "installation", id: row.id, name: `${app?.name ?? "App"} · ${row.label}` },
          { workspace: access.workspace },
        );
        return ok(toInstallation(row));
      }),
    ),

    http.post(
      u("/v1/workspaces/:ws/installations/:inst/disconnect"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "disconnect installations");
        const row = findInstallation(access, idParam(params, "inst"));
        row.status = "disconnected";
        row.updated_at = new Date().toISOString();
        const app = appInScope(row);
        audit(
          access,
          "installation.disconnected",
          { type: "installation", id: row.id, name: `${app?.name ?? "App"} · ${row.label}` },
          { workspace: access.workspace },
        );
        return ok(toInstallation(row));
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/installations/:inst/tools"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const row = findInstallation(access, idParam(params, "inst"));
        const url = new URL(request.url);
        const limit = clampedInt(url, "limit", 25, 100);
        const cursor = url.searchParams.get("cursor");
        const after = cursor ? toolCursorName(cursor) : null;
        const all = toolsOf(db(), row);
        // Keyset by name: the tools after the last one of the page before.
        const rest = after === null ? all : all.filter((t) => compareNames(t.name, after) > 0);
        const page = rest.slice(0, limit);
        const last = page.at(-1);
        const next = rest.length > limit && last ? encodeToolCursor(last.name) : null;
        return ok(page.map(toInstallationTool), { meta: { pagination: { next_cursor: next, total: all.length } } });
      }),
    ),

    http.post(
      u("/v1/workspaces/:ws/installations/:inst/refresh-tools"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "refresh tools");
        assertConsoleMayAct(access, "refresh tools");
        const row = findInstallation(access, idParam(params, "inst"));
        if (row.status !== "connected" || !row.connection) {
          throw new ApiFailure(
            409,
            "installation_not_connected",
            "This installation isn't connected, so its tools can't be listed. Reconnect it first.",
          );
        }
        if (!claimRefresh(db(), row, Date.now())) {
          throw new ApiFailure(
            429,
            "refresh_too_soon",
            "These tools were refreshed less than a minute ago. Try again in a minute, as a new request.",
          );
        }
        // The backend queues discovery and answers at once; the mock lists the app's tools right away.
        discoverTools(db(), row, new Date().toISOString());
        return ok(toInstallation(row));
      }),
    ),

    http.post(
      u("/v1/workspaces/:ws/installations/:inst/accept-tool-change"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, ADMINS, "review tool changes");
        const row = findInstallation(access, idParam(params, "inst"));
        const input = await body(
          request,
          z.object({ name: z.string().min(1, "Required."), latest_hash: z.string().min(1, "Required.") }),
        );
        const accepted = acceptChange(db(), row, input.name, input.latest_hash);
        if (accepted === "not_found") throw notFound("Tool");
        if (accepted === "not_changed") {
          throw new ApiFailure(409, "tool_not_changed", `${input.name} has no change to accept.`);
        }
        if (accepted === "stale") {
          throw new ApiFailure(
            409,
            "stale_definition",
            `${input.name} changed again since you reviewed it. Review its latest definition, then accept that.`,
          );
        }
        const app = appInScope(row);
        audit(
          access,
          "installation.tool_change_accepted",
          { type: "installation", id: row.id, name: `${app?.name ?? "App"} · ${row.label} · ${input.name}` },
          { workspace: access.workspace },
        );
        return ok(toInstallationTool(accepted));
      }),
    ),

    http.delete(
      u("/v1/workspaces/:ws/installations/:inst"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, ADMINS, "remove installations");
        const row = findInstallation(access, idParam(params, "inst"));
        const d = db();
        d.grants = d.grants.filter((g) => !(inWorkspace(g, access.workspace) && g.installation_id === row.id));
        d.installations = d.installations.filter((i) => i !== row);
        forgetTools(d, row);
        const app = appInScope(row);
        audit(
          access,
          "installation.deleted",
          { type: "installation", id: row.id, name: `${app?.name ?? "App"} · ${row.label}` },
          { workspace: access.workspace },
        );
        return noContent();
      }),
    ),

    // --- Connect -------------------------------------------------------------------
    http.get(
      u("/v1/workspaces/:ws/connect-sessions"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        return ok(
          db()
            .connect_sessions.filter((s) => inWorkspace(s, access.workspace))
            .map((s) => toConnectSession(withExpiry(s))),
        );
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/connect-sessions/:session"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const session = db().connect_sessions.find(
          (s) => inWorkspace(s, access.workspace) && s.id === idParam(params, "session"),
        );
        if (!session) throw notFound("Connect session");
        return ok(toConnectSession(withExpiry(session)));
      }),
    ),

    http.post(
      u("/v1/workspaces/:ws/connect-sessions"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "start Connect sessions");
        assertConsoleMayAct(access, "connect accounts");
        const input = await body(
          request,
          z.object({
            app_id: idOf("app"),
            intent: z.enum(["create", "reconnect"]),
            label: name(40),
            installation_id: idOf("installation").nullable(),
            return_url: z.string().url().nullable(),
          }),
        );
        const available = availableApps(access).find((a) => a.app.id === input.app_id);
        if (!available) {
          const app = db().apps.find((a) => a.id === input.app_id && a.organization_id === access.org.id);
          if (!app || (app.owner_workspace_id && app.owner_workspace_id !== access.workspace.id)) {
            throw notFound("App");
          }
          throw new ApiFailure(
            422,
            "configuration_not_ready",
            `${app.name} isn't configured for ${access.workspace.environment} yet. Finish its ${access.workspace.environment} configuration first.`,
          );
        }
        let label = input.label;
        if (input.intent === "create" && input.installation_id !== null) {
          // A new installation has no existing one to point at; never store an unchecked reference.
          throw new ApiFailure(400, "validation_failed", "A new installation can't name an existing one.", {
            installation_id: "Leave empty unless you're reconnecting.",
          });
        }
        if (input.intent === "reconnect") {
          if (!input.installation_id) {
            throw new ApiFailure(400, "validation_failed", "Choose the installation to reconnect.", {
              installation_id: "Required to reconnect.",
            });
          }
          const existing = findInstallation(access, input.installation_id);
          if (existing.app_id !== input.app_id) {
            throw new ApiFailure(422, "app_mismatch", "That installation belongs to a different app.");
          }
          label = existing.label;
        }
        if (input.return_url) {
          const settings = db().connect_settings.find(
            (c) => c.organization_id === access.org.id && c.environment === access.workspace.environment,
          );
          if (!settings?.return_urls.includes(input.return_url)) {
            throw new ApiFailure(422, "return_url_not_allowed", "That return URL isn't on the allowlist.", {
              return_url: `Add it to the ${access.workspace.environment} return URLs first.`,
            });
          }
        }
        // Both already resolved within this organization and workspace by `availableApps`.
        const app = appInScope({
          organization_id: access.org.id,
          workspace_id: access.workspace.id,
          app_id: available.app.id,
        });
        const config = app ? configurationOf(db().configurations, app, access.workspace.environment) : undefined;
        const now = Date.now();
        const reference = newReference();
        const session: ConnectSessionRow = {
          id: newId("connect_session"),
          organization_id: access.org.id,
          workspace_id: access.workspace.id,
          reference_hash: await hashReference(reference),
          environment: access.workspace.environment,
          app_id: input.app_id,
          configuration_version: config?.version ?? 1,
          intent: input.intent,
          installation_id: input.installation_id,
          label,
          status: "pending",
          return_url: input.return_url,
          requested_scopes: requestedScopes(app, config),
          created_at: new Date(now).toISOString(),
          expires_at: new Date(now + 15 * MINUTE).toISOString(),
          completed_at: null,
          failure_code: null,
        };
        db().connect_sessions.unshift(session);
        // The link is shown once, here: it carries the reference that authorizes the hosted page.
        return created({ ...toConnectSession(session), url: `${window.location.origin}/connect/${reference}` });
      }),
    ),

    // Hosted Connect: authorized by the session reference alone.
    http.get(
      u("/v1/connect/sessions/:reference"),
      route(async ({ params }) => {
        const session = await findSessionByReferenceCrossTenant(param(params, "reference"));
        assertLinkShows(session);
        return ok(toConnectSession(session, true));
      }),
    ),

    http.post(
      u("/v1/connect/sessions/:reference/cancel"),
      route(async ({ params }) => {
        const session = await findSessionByReferenceCrossTenant(param(params, "reference"));
        assertSessionPending(session);
        session.status = "cancelled";
        return ok(toConnectSession(session));
      }),
    ),

    http.post(
      u("/v1/connect/sessions/:reference/credentials"),
      route(async ({ request, params }) => {
        const session = await findSessionByReferenceCrossTenant(param(params, "reference"));
        assertSessionPending(session);
        const app = assertStillEligible(session);
        if (app.auth.type !== "token") {
          throw new ApiFailure(
            422,
            "wrong_auth_type",
            app.auth.type === "oauth2"
              ? `${app.name} connects with OAuth.`
              : `${app.name} connects by signing in through its MCP server.`,
          );
        }
        const input = await body(request, z.object({ values: z.record(z.string(), z.string()) }));
        const fields: Record<string, string> = {};
        for (const field of app.auth.inputs) {
          const value = input.values[field.key]?.trim() ?? "";
          if (!value) fields[field.key] = "Required.";
          else if ([...value].length > 4096) fields[field.key] = "Use 4,096 characters or fewer.";
          else if (field.pattern && !new RegExp(field.pattern).test(value)) {
            fields[field.key] = `This doesn't look like a ${field.label.toLowerCase()}.`;
          }
        }
        if (Object.keys(fields).length > 0) {
          throw new ApiFailure(400, "validation_failed", "Check the highlighted fields.", fields);
        }
        const first = input.values[app.auth.inputs[0]?.key ?? ""] ?? "";
        if (/invalid/i.test(first)) {
          throw new ApiFailure(
            422,
            "credential_rejected",
            `${app.name} rejected that credential. Check it and try again.`,
            {
              [app.auth.inputs[0]?.key ?? "token"]: "Rejected by the provider.",
            },
          );
        }
        const previous = session.intent === "reconnect" ? sessionScope(session).installation() : undefined;
        const seedValue = previous?.connection?.account.external_id ?? first;
        const account =
          previous?.connection?.account ?? accountFor(app, seedValue, `${app.name} account ••${first.slice(-4)}`, null);
        return ok(completeSession(session, app, account));
      }),
    ),

    http.post(
      u("/v1/connect/sessions/:reference/authorize"),
      route(async ({ params }) => {
        const session = await findSessionByReferenceCrossTenant(param(params, "reference"));
        assertSessionPending(session);
        const app = assertStillEligible(session);
        if (app.auth.type === "token") {
          throw new ApiFailure(422, "wrong_auth_type", `${app.name} connects with a token.`);
        }
        if (app.auth.type === "mcp_oauth") startSignIn(session, app);
        if (app.auth.type === "oauth2" && (!app.auth.authorize_url || !app.auth.token_url)) {
          throw new ApiFailure(
            422,
            "configuration_not_ready",
            `${app.name} has no authorization or token URL. Set them in the app's authentication settings.`,
          );
        }
        return ok({
          // The caller's own reference: the stored hash can't be turned back into a link.
          authorization_url: `${window.location.origin}/connect/${encodeURIComponent(param(params, "reference"))}/simulated-consent`,
        });
      }),
    ),

    // Mock-only stand-in for the provider redirecting to the OAuth callback.
    http.post(
      u("/v1/connect/sessions/:reference/simulate-consent"),
      route(async ({ params, request }) => {
        const session = await findSessionByReferenceCrossTenant(param(params, "reference"));
        assertSessionPending(session);
        const app = assertStillEligible(session);
        const input = await body(
          request,
          z.object({ decision: z.enum(["approved", "denied"]), email: z.string().email().nullable() }),
        );
        if (input.decision === "denied" || !input.email) {
          session.status = "failed";
          session.failure_code = "consent_denied";
          throw new ApiFailure(422, "consent_denied", "Access was not granted. Nothing was connected.");
        }
        if (app.auth.type === "mcp_oauth" && !app.auth.identity_url) {
          // Nothing identifies the account (decision 0039): a new installation's is anonymous, and a
          // reconnect keeps the account it had, unchecked.
          const previous = session.intent === "reconnect" ? sessionScope(session).installation() : undefined;
          const account =
            previous?.connection?.account ??
            accountFor(app, `${session.id}:${input.email}`, `${app.name} account`, null);
          return ok(completeSession(session, app, account));
        }
        const account = accountFor(app, input.email, input.email.split("@")[0] ?? input.email, input.email);
        return ok(completeSession(session, app, account));
      }),
    ),

    // --- Agents ----------------------------------------------------------------------
    http.get(
      u("/v1/workspaces/:ws/agents"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        return ok(
          db()
            .agents.filter((a) => inWorkspace(a, access.workspace))
            .sort((a, b) => a.name.localeCompare(b.name))
            .map(toAgent),
        );
      }),
    ),

    http.post(
      u("/v1/workspaces/:ws/agents"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "create agents");
        const input = await body(
          request,
          z.object({
            name: name(),
            description: z.string().trim().max(280),
            model: z.string().refine((m) => MODELS.some((x) => x.id === m), "Choose a model."),
            instructions: z.string().max(20_000),
          }),
        );
        const now = new Date().toISOString();
        const row: AgentRow = {
          id: newId("agent"),
          organization_id: access.org.id,
          workspace_id: access.workspace.id,
          ...input,
          status: "active",
          created_at: now,
          updated_at: now,
        };
        db().agents.push(row);
        audit(access, "agent.created", { type: "agent", id: row.id, name: row.name }, { workspace: access.workspace });
        return created(toAgent(row));
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/agents/:agent"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        return ok(toAgent(findAgent(access, idParam(params, "agent"))));
      }),
    ),

    http.patch(
      u("/v1/workspaces/:ws/agents/:agent"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "change agents");
        const row = findAgent(access, idParam(params, "agent"));
        const input = await body(
          request,
          z.object({
            name: name().optional(),
            description: z.string().trim().max(280).optional(),
            model: z
              .string()
              .refine((m) => MODELS.some((x) => x.id === m), "Choose a model.")
              .optional(),
            instructions: z.string().max(20_000).optional(),
            status: z.enum(["active", "paused"]).optional(),
          }),
        );
        Object.assign(row, Object.fromEntries(Object.entries(input).filter(([, v]) => v !== undefined)));
        row.updated_at = new Date().toISOString();
        audit(
          access,
          input.status ? `agent.${input.status === "paused" ? "paused" : "resumed"}` : "agent.updated",
          { type: "agent", id: row.id, name: row.name },
          { workspace: access.workspace },
        );
        return ok(toAgent(row));
      }),
    ),

    http.delete(
      u("/v1/workspaces/:ws/agents/:agent"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "delete agents");
        const row = findAgent(access, idParam(params, "agent"));
        const d = db();
        d.grants = d.grants.filter((g) => !(inWorkspace(g, access.workspace) && g.agent_id === row.id));
        d.agents = d.agents.filter((a) => a !== row);
        audit(access, "agent.deleted", { type: "agent", id: row.id, name: row.name }, { workspace: access.workspace });
        return noContent();
      }),
    ),

    // --- Grants ----------------------------------------------------------------------
    http.get(
      u("/v1/workspaces/:ws/agents/:agent/grants"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const agent = findAgent(access, idParam(params, "agent"));
        return ok(
          db()
            .grants.filter((g) => inWorkspace(g, access.workspace) && g.agent_id === agent.id)
            .map(toGrant),
        );
      }),
    ),

    http.post(
      u("/v1/workspaces/:ws/agents/:agent/grants"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "grant access");
        const agent = findAgent(access, idParam(params, "agent"));
        const input = await body(
          request,
          z.object({
            installation_id: idOf("installation"),
            tools: z.array(z.string()).min(1, "Choose at least one tool."),
            approval_required: z.array(z.string()),
          }),
        );
        // Workspace-coherent: an installation from another workspace is absent.
        const installation = findInstallation(access, input.installation_id);
        validateTools(access, installation, input.tools, input.approval_required);
        if (
          db().grants.some(
            (g) => inWorkspace(g, access.workspace) && g.agent_id === agent.id && g.installation_id === installation.id,
          )
        ) {
          throw new ApiFailure(
            409,
            "grant_exists",
            "This agent already has access to that installation. Edit the existing grant.",
          );
        }
        const now = new Date().toISOString();
        const grant: GrantRow = {
          id: newId("grant"),
          organization_id: access.org.id,
          workspace_id: access.workspace.id,
          agent_id: agent.id,
          installation_id: installation.id,
          tools: input.tools,
          approval_required: input.approval_required,
          created_at: now,
          updated_at: now,
        };
        db().grants.push(grant);
        audit(
          access,
          "agent_grant.created",
          { type: "agent", id: agent.id, name: agent.name },
          { workspace: access.workspace },
        );
        return created(toGrant(grant));
      }),
    ),

    http.patch(
      u("/v1/workspaces/:ws/agents/:agent/grants/:grant"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "change access");
        const agent = findAgent(access, idParam(params, "agent"));
        const grant = db().grants.find(
          (g) => inWorkspace(g, access.workspace) && g.agent_id === agent.id && g.id === idParam(params, "grant"),
        );
        if (!grant) throw notFound("Grant");
        const input = await body(
          request,
          z.object({
            tools: z.array(z.string()).min(1, "Choose at least one tool, or remove the grant."),
            approval_required: z.array(z.string()),
          }),
        );
        validateTools(access, findInstallation(access, grant.installation_id), input.tools, input.approval_required);
        grant.tools = input.tools;
        grant.approval_required = input.approval_required;
        grant.updated_at = new Date().toISOString();
        audit(
          access,
          "agent_grant.updated",
          { type: "agent", id: agent.id, name: agent.name },
          { workspace: access.workspace },
        );
        return ok(toGrant(grant));
      }),
    ),

    http.delete(
      u("/v1/workspaces/:ws/agents/:agent/grants/:grant"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "remove access");
        const agent = findAgent(access, idParam(params, "agent"));
        const d = db();
        const grant = d.grants.find(
          (g) => inWorkspace(g, access.workspace) && g.agent_id === agent.id && g.id === idParam(params, "grant"),
        );
        if (!grant) throw notFound("Grant");
        d.grants = d.grants.filter((g) => g !== grant);
        audit(
          access,
          "agent_grant.revoked",
          { type: "agent", id: agent.id, name: agent.name },
          { workspace: access.workspace },
        );
        return noContent();
      }),
    ),

    // --- Runs ------------------------------------------------------------------------
    http.get(
      u("/v1/workspaces/:ws/runs"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const url = new URL(request.url);
        const agentId = url.searchParams.get("agent_id");
        const status = url.searchParams.get("status");
        if (status && !(RUN_STATUSES as readonly string[]).includes(status)) {
          throw new ApiFailure(400, "validation_failed", "Unknown status.", { status: "Unknown status." });
        }
        const limit = clampedInt(url, "limit", 25, 100);
        const rows = db()
          .runs.filter(
            (r) =>
              inWorkspace(r, access.workspace) &&
              (!agentId || r.agent_id === agentId) &&
              (!status || r.status === status),
          )
          .sort((a, b) => b.created_at.localeCompare(a.created_at));
        const { page, meta } = paginate(rows, url.searchParams.get("cursor"), limit);
        return ok(page.map(toRun), { meta });
      }),
    ),

    // Organization-wide run log. Scoped to the caller's organization; each row
    // names its workspace so nothing is shown out of context.
    http.get(
      u("/v1/organizations/:org/runs"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        const url = new URL(request.url);
        const env = url.searchParams.get("environment");
        const workspaceId = url.searchParams.get("workspace_id");
        const status = url.searchParams.get("status");
        if (status && !(RUN_STATUSES as readonly string[]).includes(status)) {
          throw new ApiFailure(400, "validation_failed", "Unknown status.", { status: "Unknown status." });
        }
        if (env && !(ENVIRONMENTS as readonly string[]).includes(env)) {
          throw new ApiFailure(400, "validation_failed", "Unknown environment.", {
            environment: "Unknown environment.",
          });
        }
        const workspaces = new Map(
          db()
            .workspaces.filter(
              (w) =>
                w.organization_id === access.org.id &&
                (!env || w.environment === env) &&
                (!workspaceId || w.id === workspaceId),
            )
            .map((w) => [w.id, w]),
        );
        const limit = clampedInt(url, "limit", 25, 100);
        const rows = db()
          .runs.filter(
            (r) =>
              r.organization_id === access.org.id && workspaces.has(r.workspace_id) && (!status || r.status === status),
          )
          .sort((a, b) => b.created_at.localeCompare(a.created_at));
        const { page, meta } = paginate(rows, url.searchParams.get("cursor"), limit);
        return ok(
          page.map((r) => {
            const w = workspaces.get(r.workspace_id);
            return {
              ...toRun(r),
              workspace: {
                id: r.workspace_id,
                name: w?.name ?? "",
                slug: w?.slug ?? "",
                environment: w?.environment ?? "sandbox",
              },
            };
          }),
          { meta },
        );
      }),
    ),

    http.post(
      u("/v1/workspaces/:ws/agents/:agent/runs"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "start runs");
        assertConsoleMayAct(access, "start runs");
        const agent = findAgent(access, idParam(params, "agent"));
        if (agent.status === "paused") {
          throw new ApiFailure(422, "agent_paused", `${agent.name} is paused. Resume it to start runs.`);
        }
        const input = await body(
          request,
          z.object({
            input: z.string().trim().min(1, "Write a message.").max(8000),
          }),
        );
        const run: RunRow = {
          id: newId("run"),
          organization_id: access.org.id,
          workspace_id: access.workspace.id,
          agent_id: agent.id,
          session_id: newId("session"),
          trigger: "console",
          status: "queued",
          input: input.input,
          output: null,
          usage: { input_tokens: 0, output_tokens: 0, tool_calls: 0 },
          error: null,
          created_at: new Date().toISOString(),
          started_at: null,
          finished_at: null,
          plan: [],
        };
        run.plan = planRun(db(), run);
        db().runs.unshift(run);
        enqueueRun(db(), run);
        scheduleRun(run);
        return created(toRun(run));
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/runs/:run"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        return ok(toRun(findRun(access, idParam(params, "run"))));
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/runs/:run/events"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const run = findRun(access, idParam(params, "run"));
        const after = Number(new URL(request.url).searchParams.get("after") ?? 0);
        return ok(
          timelineOf(db(), run)
            .filter((e) => inWorkspace(e, access.workspace) && e.run_id === run.id && e.sequence > after)
            .map(toRunEvent),
        );
      }),
    ),

    http.post(
      u("/v1/workspaces/:ws/runs/:run/cancel"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "cancel runs");
        const run = findRun(access, idParam(params, "run"));
        if (TERMINAL_RUN_STATUSES.includes(run.status)) {
          throw new ApiFailure(409, "run_finished", "This run already finished.");
        }
        cancelScheduledRun(run);
        cancelRun(db(), run, access.user.name, new Date().toISOString());
        audit(access, "run.cancelled", runTarget(db(), run), { workspace: access.workspace });
        return ok(toRun(run));
      }),
    ),

    // Approvals are decided in the customer's product, however it wants, and
    // recorded by its backend with an API key. A Console session
    // only observes them. The mock has no API keys, so every caller here is a
    // Console session and is refused; waiting runs stay paused until cancelled.
    http.post(
      u("/v1/workspaces/:ws/runs/:run/approvals"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        findRun(access, idParam(params, "run"));
        throw new ApiFailure(
          403,
          "approval_requires_api_key",
          "Approvals come from your backend with an API key, once your product has the decision.",
        );
      }),
    ),

    // --- Connect settings ------------------------------------------------------------
    http.get(
      u("/v1/organizations/:org/connect-settings/:env"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        const env = environment.safeParse(param(params, "env"));
        if (!env.success) throw notFound("Environment");
        // A read changes nothing: until the first write, the settings are the empty defaults.
        const row = db().connect_settings.find(
          (c) => c.organization_id === access.org.id && c.environment === env.data,
        ) ?? {
          organization_id: access.org.id,
          environment: env.data,
          return_urls: [],
          allowed_origins: [],
          updated_at: null,
        };
        const { organization_id: _o, ...wire } = row;
        return ok(wire);
      }),
    ),

    http.put(
      u("/v1/organizations/:org/connect-settings/:env"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "change Connect settings");
        const env = environment.safeParse(param(params, "env"));
        if (!env.success) throw notFound("Environment");
        const input = await body(
          request,
          z.object({ return_urls: z.array(z.string()).max(20), allowed_origins: z.array(z.string()).max(20) }),
        );
        const fields: Record<string, string> = {};
        // The longest a session can carry; checked before anything else, as the backend does.
        for (const [list, values] of [
          ["return_urls", input.return_urls],
          ["allowed_origins", input.allowed_origins],
        ] as const) {
          values.forEach((value, i) => {
            if ([...value].length > 2000) fields[`${list}.${i}`] = "Use 2,000 characters or fewer.";
          });
        }
        if (Object.keys(fields).length > 0) {
          throw new ApiFailure(400, "validation_failed", "Some entries are invalid.", fields);
        }
        const allowHttp = (url: URL) => env.data === "sandbox" && url.hostname === "localhost";
        input.return_urls.forEach((value, i) => {
          if (value.includes("*")) fields[`return_urls.${i}`] = "Wildcards aren't allowed. List each URL exactly.";
          else {
            try {
              const url = new URL(value);
              if (url.protocol !== "https:" && !allowHttp(url))
                fields[`return_urls.${i}`] = "Use https:// (http://localhost is allowed in Sandbox).";
              if (url.hash) fields[`return_urls.${i}`] = "Remove the #fragment.";
            } catch {
              fields[`return_urls.${i}`] = "Enter a full URL.";
            }
          }
        });
        input.allowed_origins.forEach((value, i) => {
          try {
            const url = new URL(value);
            if (url.origin !== value.replace(/\/$/, ""))
              fields[`allowed_origins.${i}`] = "Use an origin only: scheme, host and port.";
            else if (url.protocol !== "https:" && !allowHttp(url))
              fields[`allowed_origins.${i}`] = "Use https:// (http://localhost is allowed in Sandbox).";
          } catch {
            fields[`allowed_origins.${i}`] = "Enter an origin like https://app.example.com.";
          }
        });
        if (Object.keys(fields).length > 0) {
          throw new ApiFailure(400, "validation_failed", "Some entries are invalid.", fields);
        }
        let row = db().connect_settings.find((c) => c.organization_id === access.org.id && c.environment === env.data);
        if (!row) {
          row = {
            organization_id: access.org.id,
            environment: env.data,
            return_urls: [],
            allowed_origins: [],
            updated_at: null,
          };
          db().connect_settings.push(row);
        }
        row.return_urls = [...new Set(input.return_urls)];
        row.allowed_origins = [...new Set(input.allowed_origins.map((o) => o.replace(/\/$/, "")))];
        row.updated_at = new Date().toISOString();
        audit(
          access,
          "connect_settings.updated",
          { type: "connect_settings", id: null, name: `Connect (${env.data})` },
          { environment: env.data },
        );
        const { organization_id: _o, ...wire } = row;
        return ok(wire);
      }),
    ),

    // --- API keys ----------------------------------------------------------------------
    http.get(
      u("/v1/organizations/:org/api-keys"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        return ok(
          db()
            .api_keys.filter((k) => k.organization_id === access.org.id)
            .sort((a, b) => b.created_at.localeCompare(a.created_at))
            .map(toApiKey),
        );
      }),
    ),

    http.post(
      u("/v1/organizations/:org/api-keys"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, DEVELOPERS, "create API keys");
        const input = await body(request, z.object({ name: name(), environment }));
        if (input.environment === "production") requireRole(access, ADMINS, "create Production keys");
        const live = input.environment === "production";
        const selector = token(4);
        const secretPart = token(40);
        const secret = `zf_${live ? "live" : "test"}_${selector}${secretPart}`;
        const row = {
          id: newId("api_key"),
          organization_id: access.org.id,
          environment: input.environment,
          name: input.name,
          prefix: `zf_${live ? "live" : "test"}_${selector}`,
          last4: secret.slice(-4),
          created_by_user_id: access.user.id,
          created_at: new Date().toISOString(),
          last_used_at: null,
          revoked_at: null,
        };
        db().api_keys.unshift(row);
        audit(
          access,
          "api_key.created",
          { type: "api_key", id: row.id, name: row.name },
          { environment: row.environment },
        );
        return created({ key: toApiKey(row), secret });
      }),
    ),

    http.post(
      u("/v1/organizations/:org/api-keys/:key/revoke"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, DEVELOPERS, "revoke API keys");
        const row = db().api_keys.find((k) => k.organization_id === access.org.id && k.id === idParam(params, "key"));
        if (!row) throw notFound("API key");
        if (row.environment === "production") requireRole(access, ADMINS, "revoke Production keys");
        if (row.revoked_at) throw new ApiFailure(409, "already_revoked", "This key is already revoked.");
        row.revoked_at = new Date().toISOString();
        audit(
          access,
          "api_key.revoked",
          { type: "api_key", id: row.id, name: row.name },
          { environment: row.environment },
        );
        return ok(toApiKey(row));
      }),
    ),

    // --- Webhooks ----------------------------------------------------------------------
    http.get(
      u("/v1/organizations/:org/webhooks"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        return ok(
          db()
            .webhooks.filter((w) => w.organization_id === access.org.id)
            .map(toWebhookEndpoint),
        );
      }),
    ),

    http.post(
      u("/v1/organizations/:org/webhooks"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "add webhook endpoints");
        const input = await body(
          request,
          z.object({
            environment,
            url: z.string().trim(),
            description: z.string().trim().max(200),
            events: z.array(z.enum(WEBHOOK_EVENT_TYPES)).min(1, "Choose at least one event."),
          }),
        );
        assertPublicHttps(input.url, "url");
        const secret = webhookSecret(input.environment);
        const endpoint: WebhookEndpoint = {
          id: newId("webhook_endpoint"),
          organization_id: access.org.id,
          environment: input.environment,
          url: input.url,
          description: input.description,
          // Each type once, in the order first asked for.
          events: [...new Set(input.events)],
          enabled: true,
          signing_secret_last4: secret.slice(-4),
          previous_signing_secret_expires_at: null,
          created_at: new Date().toISOString(),
          last_delivery: null,
        };
        db().webhooks.push(endpoint);
        audit(
          access,
          "webhook.created",
          { type: "webhook", id: endpoint.id, name: auditUrl(endpoint.url) },
          { environment: endpoint.environment },
        );
        return created({ endpoint: toWebhookEndpoint(endpoint), signing_secret: secret });
      }),
    ),

    http.patch(
      u("/v1/organizations/:org/webhooks/:hook"),
      route(async ({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "edit webhook endpoints");
        const endpoint = db().webhooks.find(
          (w) => w.organization_id === access.org.id && w.id === idParam(params, "hook"),
        );
        if (!endpoint) throw notFound("Webhook endpoint");
        const input = await body(
          request,
          z.object({
            url: z.string().trim().optional(),
            description: z.string().trim().max(200).optional(),
            events: z.array(z.enum(WEBHOOK_EVENT_TYPES)).min(1, "Choose at least one event.").optional(),
            enabled: z.boolean().optional(),
          }),
        );
        // An empty URL is refused like any other that isn't one, never saved.
        if (input.url !== undefined) assertPublicHttps(input.url, "url");
        if (input.events) input.events = [...new Set(input.events)];
        Object.assign(endpoint, Object.fromEntries(Object.entries(input).filter(([, v]) => v !== undefined)));
        audit(
          access,
          "webhook.updated",
          { type: "webhook", id: endpoint.id, name: auditUrl(endpoint.url) },
          { environment: endpoint.environment },
        );
        return ok(toWebhookEndpoint(endpoint));
      }),
    ),

    http.post(
      u("/v1/organizations/:org/webhooks/:hook/roll-secret"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "roll webhook signing secrets");
        const endpoint = findWebhook(access.org.id, idParam(params, "hook"));
        const now = Date.now();
        if (
          endpoint.previous_signing_secret_expires_at !== null &&
          Date.parse(endpoint.previous_signing_secret_expires_at) > now
        ) {
          throw new ApiFailure(
            409,
            "rotation_in_progress",
            "The previous signing secret is still valid. Wait until its 24-hour overlap ends before rolling again.",
          );
        }
        // No await between checking and updating: concurrent rolls have one winner.
        // The mock keeps only public metadata, never either signing secret.
        const secret = webhookSecret(endpoint.environment);
        endpoint.signing_secret_last4 = secret.slice(-4);
        endpoint.previous_signing_secret_expires_at = new Date(now + 24 * 60 * MINUTE).toISOString();
        audit(
          access,
          "webhook.secret_rolled",
          { type: "webhook", id: endpoint.id, name: auditUrl(endpoint.url) },
          { environment: endpoint.environment },
        );
        return ok({ endpoint: toWebhookEndpoint(endpoint), signing_secret: secret });
      }),
    ),

    http.delete(
      u("/v1/organizations/:org/webhooks/:hook"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "delete webhook endpoints");
        const d = db();
        const endpoint = d.webhooks.find(
          (w) => w.organization_id === access.org.id && w.id === idParam(params, "hook"),
        );
        if (!endpoint) throw notFound("Webhook endpoint");
        d.webhooks = d.webhooks.filter((w) => w !== endpoint);
        d.webhook_deliveries = d.webhook_deliveries.filter(
          (row) => !(row.organization_id === access.org.id && row.endpoint_id === endpoint.id),
        );
        d.webhook_delivery_attempts = d.webhook_delivery_attempts.filter(
          (row) => !(row.organization_id === access.org.id && row.endpoint_id === endpoint.id),
        );
        audit(
          access,
          "webhook.deleted",
          { type: "webhook", id: endpoint.id, name: auditUrl(endpoint.url) },
          { environment: endpoint.environment },
        );
        return noContent();
      }),
    ),

    http.get(
      u("/v1/organizations/:org/webhooks/:hook/deliveries"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        const endpoint = findWebhook(access.org.id, idParam(params, "hook"));
        const url = new URL(request.url);
        const workspaceId = url.searchParams.get("workspace_id");
        const state = url.searchParams.get("state");
        const eventType = url.searchParams.get("event_type");
        const scope = JSON.stringify([access.org.id, endpoint.id, endpoint.environment, workspaceId, state, eventType]);
        const after = webhookCursor(
          url.searchParams.get("cursor"),
          scope,
          z
            .object({
              created_at: z.iso.datetime(),
              id: z.string().refine((value) => isId("webhook_delivery", value)),
            })
            .strict(),
        );
        const limit = clampedInt(url, "limit", 25, 100);
        const rows = db()
          .webhook_deliveries.filter((row) => row.organization_id === access.org.id && row.endpoint_id === endpoint.id)
          .flatMap((row) => {
            const event = webhookEventFor(access.org.id, endpoint, row);
            if (
              !event ||
              (workspaceId !== null && event.workspace?.id !== workspaceId) ||
              (state !== null && row.state !== state) ||
              (eventType !== null && event.type !== eventType)
            )
              return [];
            return [webhookSummary(row, event)];
          })
          .sort((a, b) => compareNames(b.created_at, a.created_at) || compareNames(b.id, a.id));
        const remaining = after
          ? rows.filter(
              (row) => row.created_at < after.created_at || (row.created_at === after.created_at && row.id < after.id),
            )
          : rows;
        const page = remaining.slice(0, limit);
        const last = page.at(-1);
        return ok(page, {
          meta: {
            pagination: {
              total: rows.length,
              next_cursor:
                remaining.length > limit && last
                  ? nextWebhookCursor(scope, { created_at: last.created_at, id: last.id })
                  : null,
            },
          },
        });
      }),
    ),

    http.get(
      u("/v1/organizations/:org/webhooks/:hook/deliveries/:delivery"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        const endpoint = findWebhook(access.org.id, idParam(params, "hook"));
        const { row, event } = findWebhookDelivery(access.org.id, endpoint, idParam(params, "delivery"));
        const attempts = webhookAttempts(access.org.id, endpoint.id, row.id);
        return ok({
          ...webhookSummary(row, event),
          event,
          attempt_history_complete:
            attempts.length === row.attempts && attempts.every((attempt, index) => attempt.number === index + 1),
        });
      }),
    ),

    http.get(
      u("/v1/organizations/:org/webhooks/:hook/deliveries/:delivery/attempts"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        const endpoint = findWebhook(access.org.id, idParam(params, "hook"));
        const { row } = findWebhookDelivery(access.org.id, endpoint, idParam(params, "delivery"));
        const url = new URL(request.url);
        const scope = JSON.stringify([access.org.id, endpoint.id, endpoint.environment, row.id]);
        const after = webhookCursor(url.searchParams.get("cursor"), scope, z.number().int().positive());
        const limit = clampedInt(url, "limit", 25, 100);
        const attempts = webhookAttempts(access.org.id, endpoint.id, row.id);
        const remaining = after === null ? attempts : attempts.filter((attempt) => attempt.number > after);
        const page = remaining.slice(0, limit);
        const last = page.at(-1);
        return ok(page, {
          meta: {
            pagination: {
              total: attempts.length,
              next_cursor: remaining.length > limit && last ? nextWebhookCursor(scope, last.number) : null,
            },
          },
        });
      }),
    ),

    http.post(
      u("/v1/organizations/:org/webhooks/:hook/deliveries/:delivery/resend"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "resend webhook deliveries");
        const endpoint = findWebhook(access.org.id, idParam(params, "hook"));
        const { row, event } = findWebhookDelivery(access.org.id, endpoint, idParam(params, "delivery"));
        if (row.state === "pending") {
          throw new ApiFailure(
            409,
            "delivery_pending",
            "This delivery is still pending. Wait until it finishes before resending.",
          );
        }
        if (!endpoint.enabled) {
          throw new ApiFailure(409, "webhook_disabled", "Enable this webhook endpoint before resending.");
        }
        if (event.type !== "webhook.test" && !endpoint.events.includes(event.type)) {
          throw new ApiFailure(
            409,
            "webhook_not_subscribed",
            "Subscribe this endpoint to the event type before resending.",
          );
        }
        const now = new Date().toISOString();
        const delivery: WebhookDeliveryRow = {
          id: newId("webhook_delivery"),
          organization_id: access.org.id,
          endpoint_id: endpoint.id,
          event_id: row.event_id,
          state: "pending",
          attempts: 0,
          last_status_code: null,
          last_error: null,
          last_attempt_at: null,
          next_attempt_at: now,
          created_at: now,
        };
        // Reuse the event row verbatim; a resend neither rewrites it nor invents an attempt.
        db().webhook_deliveries.push(delivery);
        audit(
          access,
          "webhook.delivery_resent",
          { type: "webhook_delivery", id: delivery.id, name: event.type },
          {
            workspace: event.workspace ? { id: event.workspace.id, environment: endpoint.environment } : undefined,
            environment: endpoint.environment,
          },
        );
        return created(webhookSummary(delivery, event));
      }),
    ),

    http.post(
      u("/v1/organizations/:org/webhooks/:hook/test"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "send test events");
        const endpoint = db().webhooks.find(
          (w) => w.organization_id === access.org.id && w.id === idParam(params, "hook"),
        );
        if (!endpoint) throw notFound("Webhook endpoint");
        const delivery = {
          status_code: endpoint.url.includes("staging") ? 503 : 200,
          delivered_at: new Date().toISOString(),
        };
        endpoint.last_delivery = delivery;
        const eventId = newId("webhook_event");
        const deliveryId = newId("webhook_delivery");
        db().webhook_events.push({
          id: eventId,
          organization_id: access.org.id,
          workspace_id: null,
          environment: endpoint.environment,
          body: {
            id: eventId,
            organization_id: access.org.id,
            environment: endpoint.environment,
            created_at: delivery.delivered_at,
            type: "webhook.test",
            workspace: null,
            data: { object: "webhook_endpoint", id: endpoint.id },
          },
        });
        const succeeded = delivery.status_code >= 200 && delivery.status_code < 300;
        db().webhook_deliveries.push({
          id: deliveryId,
          organization_id: access.org.id,
          endpoint_id: endpoint.id,
          event_id: eventId,
          state: succeeded ? "succeeded" : "failed",
          attempts: 1,
          last_status_code: delivery.status_code,
          last_error: succeeded ? null : "status",
          last_attempt_at: delivery.delivered_at,
          next_attempt_at: null,
          created_at: delivery.delivered_at,
        });
        db().webhook_delivery_attempts.push({
          organization_id: access.org.id,
          endpoint_id: endpoint.id,
          delivery_id: deliveryId,
          number: 1,
          status_code: delivery.status_code,
          error_class: succeeded ? null : "status",
          attempted_at: delivery.delivered_at,
          duration_ms: 15,
        });
        return ok(delivery);
      }),
    ),

    // --- Audit -------------------------------------------------------------------------
    http.get(
      u("/v1/organizations/:org/audit-events"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "read the audit log");
        const url = new URL(request.url);
        const workspaceId = url.searchParams.get("workspace_id");
        const limit = clampedInt(url, "limit", 50, 100);
        const rows = db().audit.filter(
          (e) => e.organization_id === access.org.id && (!workspaceId || e.workspace_id === workspaceId),
        );
        const { page, meta } = paginate(rows, url.searchParams.get("cursor"), limit);
        return ok(
          page.map((e) => {
            const w = e.workspace_id
              ? db().workspaces.find((x) => x.organization_id === access.org.id && x.id === e.workspace_id)
              : undefined;
            return {
              ...e,
              workspace: w ? { id: w.id, name: w.name, slug: w.slug, environment: w.environment } : null,
            };
          }),
          { meta },
        );
      }),
    ),

    // --- Mock controls -------------------------------------------------------------------
    http.post(
      u("/__mock/reset"),
      route(() => {
        resetDb();
        return HttpResponse.json({ data: { reset: true }, meta: null, error: null });
      }),
    ),
  ];
}

/**
 * A grant names tools in its installation's registry (decision 0040): a tool
 * that isn't there, or is gone, is refused; a changed one can be granted, and
 * calls get its pinned definition. Grants already holding a tool that later
 * goes aren't rewritten: calls to it are refused when they're made.
 */
function validateTools(
  access: WorkspaceAccess,
  installation: InstallationRow,
  tools: string[],
  approvalRequired: string[],
) {
  if (!inWorkspace(installation, access.workspace)) throw notFound("Installation");
  const registry = new Map(toolsOf(db(), installation).map((t) => [t.name, t]));
  for (const name of tools) {
    const tool = registry.get(name);
    if (!tool) {
      const message = `${name} isn't a tool of ${installation.label}. Refresh its tools if the app added it.`;
      throw new ApiFailure(422, "unknown_tool", message, { tools: message });
    }
    if (tool.gone_at !== null) {
      const message = `${name} is no longer offered by ${installation.label}. Remove it from the grant.`;
      throw new ApiFailure(422, "tool_gone", message, { tools: message });
    }
  }
  const outside = approvalRequired.find((t) => !tools.includes(t));
  if (outside) {
    const message = `${outside} requires approval but isn't granted.`;
    throw new ApiFailure(422, "approval_outside_grant", message, { approval_required: message });
  }
  // Held inside the organization's levels for a catalog app's tools; a workspace app's have none.
  const app = appIn(db().apps, installation);
  if (!app) return;
  const denied = tools.find((t) => levelInForce(db(), app, t) === "deny");
  if (denied) {
    const message = `Your organization blocks ${denied}. Remove it from the grant.`;
    throw new ApiFailure(422, "tool_denied", message, { tools: message });
  }
  const unapproved = tools.find((t) => levelInForce(db(), app, t) === "approve" && !approvalRequired.includes(t));
  if (unapproved) {
    const message = `Your organization requires approval for ${unapproved}. Add it to approval required.`;
    throw new ApiFailure(422, "approval_required_by_policy", message, { approval_required: message });
  }
}
