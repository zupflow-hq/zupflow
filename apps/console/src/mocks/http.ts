/**
 * Shared plumbing for mock route handlers: the response envelope, errors,
 * authentication, authorization and the row → wire mappers.
 */
import type {
  Agent,
  ApiKey,
  AppDefinition,
  ClientConfiguration,
  ConfigurationStatus,
  ConfigurationSummary,
  ConnectSession,
  Environment,
  Installation,
  InstallationApp,
  Member,
  MemberRole,
  Organization,
  Run,
  User,
  Workspace,
} from "@zupflow/sdk";
import { delay, HttpResponse } from "msw";
import type { z } from "zod";
import { db, persist } from "./db";
import type {
  AgentRow,
  ApiKeyRow,
  AppRow,
  ConfigurationRow,
  ConnectSessionRow,
  InstallationRow,
  MembershipRow,
  OrganizationRow,
  RunRow,
  WorkspaceRow,
} from "./model";

export const MOCK_SESSION_KEY = "zupflow.mock.session";
export const OAUTH_CALLBACK_URL = "https://connect.zupflow.com/oauth/callback";

export class ApiFailure extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly fields: Record<string, string> | null = null,
  ) {
    super(message);
  }
}

export const notFound = (what: string) => new ApiFailure(404, "not_found", `${what} was not found.`);

export function ok<T>(data: T, init?: { status?: number; meta?: unknown }) {
  return HttpResponse.json({ data, meta: init?.meta ?? null, error: null }, { status: init?.status ?? 200 });
}

export function created<T>(data: T) {
  return ok(data, { status: 201 });
}

export function noContent() {
  return new HttpResponse(null, { status: 204 });
}

function failure(error: ApiFailure) {
  return HttpResponse.json(
    {
      data: null,
      meta: null,
      error: { code: error.code, message: error.message, fields: error.fields },
    },
    { status: error.status },
  );
}

/** Simulated network latency, so loading states are exercised for real. */
let latency: [number, number] = [180, 520];
export function setLatency(range: [number, number]) {
  latency = range;
}

/**
 * Wrap a resolver: add latency, turn {@link ApiFailure} into an error
 * envelope and persist after successful mutations.
 */
type ResolverArgs = {
  request: Request;
  params: Record<string, string | readonly string[] | undefined>;
};

export function route<A extends ResolverArgs>(fn: (args: A) => Promise<Response> | Response) {
  return async (args: A) => {
    const [min, max] = latency;
    if (max > 0) await delay(min + Math.floor(Math.random() * (max - min)));
    try {
      const response = await fn(args);
      if (args.request.method !== "GET" && response.ok) persist();
      return response;
    } catch (error) {
      if (error instanceof ApiFailure) return failure(error);
      console.error("[mock api]", error);
      return failure(new ApiFailure(500, "internal_error", "Something went wrong on our side."));
    }
  };
}

export async function body<T extends z.ZodType>(request: Request, schema: T): Promise<z.infer<T>> {
  let json: unknown;
  try {
    json = await request.json();
  } catch {
    throw new ApiFailure(400, "invalid_json", "The request body is not valid JSON.");
  }
  const parsed = schema.safeParse(json);
  if (!parsed.success) {
    const fields: Record<string, string> = {};
    for (const issue of parsed.error.issues) {
      const key = issue.path.join(".") || "body";
      fields[key] ??= issue.message;
    }
    throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", fields);
  }
  return parsed.data;
}

// ---------------------------------------------------------------------------
// Authentication and authorization

export function authenticate(request: Request): User {
  const header = request.headers.get("Authorization") ?? "";
  const token = header.startsWith("Bearer ") ? header.slice(7) : null;
  const user = token?.startsWith("mock_") ? db().users.find((u) => u.id === token.slice(5)) : undefined;
  if (!user) throw new ApiFailure(401, "unauthenticated", "Sign in to continue.");
  return user;
}

export interface OrgAccess {
  user: User;
  org: OrganizationRow;
  membership: MembershipRow;
}

/** Membership is checked before anything else; non-members see a 404. */
export function orgAccess(request: Request, organizationId: string): OrgAccess {
  const user = authenticate(request);
  const membership = db().memberships.find(
    (m) => m.organization_id === organizationId && m.user_id === user.id && m.status === "active",
  );
  const org = db().organizations.find((o) => o.id === organizationId);
  if (!membership || !org) throw notFound("Organization");
  membership.last_active_at = new Date().toISOString();
  return { user, org, membership };
}

export interface WorkspaceAccess extends OrgAccess {
  workspace: WorkspaceRow;
}

export function workspaceAccess(request: Request, workspaceId: string): WorkspaceAccess {
  const user = authenticate(request);
  const workspace = db().workspaces.find((w) => w.id === workspaceId);
  if (!workspace) throw notFound("Workspace");
  const membership = db().memberships.find(
    (m) => m.organization_id === workspace.organization_id && m.user_id === user.id && m.status === "active",
  );
  const org = db().organizations.find((o) => o.id === workspace.organization_id);
  if (!membership || !org) throw notFound("Workspace");
  return { user, org, membership, workspace };
}

export function requireRole(access: OrgAccess, roles: MemberRole[], action: string) {
  if (!roles.includes(access.membership.role)) {
    throw new ApiFailure(
      403,
      "forbidden",
      `Your role (${access.membership.role}) can't ${action}. Ask an organization owner.`,
    );
  }
}

export const ADMINS: MemberRole[] = ["owner", "admin"];

export function audit(
  access: OrgAccess,
  action: string,
  target: { type: string; id: string; name: string },
  scope: { workspace?: WorkspaceRow; environment?: Environment } = {},
) {
  db().audit.unshift({
    id: crypto.randomUUID(),
    organization_id: access.org.id,
    workspace_id: scope.workspace?.id ?? null,
    environment: scope.environment ?? scope.workspace?.environment ?? null,
    actor: { type: "user", id: access.user.id, name: access.user.name },
    action,
    target,
    created_at: new Date().toISOString(),
  });
}

// ---------------------------------------------------------------------------
// Validation helpers

export const SLUG_PATTERN = /^[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?$/;
export const RESERVED_ORG_SLUGS = new Set([
  "onboarding",
  "sign-in",
  "sign-up",
  "connect",
  "settings",
  "api",
  "new",
  "help",
]);

const PRIVATE_HOST = /^(localhost|127\.|10\.|192\.168\.|169\.254\.|172\.(1[6-9]|2\d|3[01])\.|0\.|\[::1\]|metadata\.)/i;

/** Provider destinations must be public HTTPS hosts. */
export function assertPublicHttps(value: string, field: string) {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", {
      [field]: "Enter a full URL, including https://.",
    });
  }
  if (url.protocol !== "https:") {
    throw new ApiFailure(422, "insecure_url", "Provider URLs must use HTTPS.", {
      [field]: "Use an https:// URL.",
    });
  }
  if (PRIVATE_HOST.test(url.hostname)) {
    throw new ApiFailure(422, "destination_not_allowed", "Private and link-local hosts are blocked.", {
      [field]: "Use a public host. Private, loopback and link-local addresses are blocked.",
    });
  }
}

// ---------------------------------------------------------------------------
// Row → wire mappers

export function toUser(id: string): User {
  return (
    db().users.find((u) => u.id === id) ?? {
      id,
      name: "Former member",
      email: "",
      avatar_url: null,
    }
  );
}

export function toOrganization(org: OrganizationRow, membership: MembershipRow): Organization {
  const d = db();
  const workspaceIds = new Set(d.workspaces.filter((w) => w.organization_id === org.id).map((w) => w.id));
  return {
    id: org.id,
    name: org.name,
    slug: org.slug,
    role: membership.role,
    created_at: org.created_at,
    setup: {
      has_app: d.apps.some((a) => a.organization_id === org.id),
      has_ready_configuration: d.configurations.some(
        (c) => c.organization_id === org.id && configurationStatus(c) === "ready",
      ),
      has_workspace: workspaceIds.size > 0,
      has_api_key: d.api_keys.some((k) => k.organization_id === org.id && !k.revoked_at),
      has_installation: d.installations.some((i) => workspaceIds.has(i.workspace_id) && i.status === "connected"),
      has_agent: d.agents.some((a) => workspaceIds.has(a.workspace_id)),
      has_run: d.runs.some((r) => workspaceIds.has(r.workspace_id)),
    },
  };
}

export function toMember(m: MembershipRow): Member {
  return {
    id: m.id,
    organization_id: m.organization_id,
    user: toUser(m.user_id),
    role: m.role,
    status: m.status,
    invited_at: m.invited_at,
    joined_at: m.joined_at,
    last_active_at: m.last_active_at,
  };
}

export function toWorkspace(w: WorkspaceRow): Workspace {
  const d = db();
  const since = Date.now() - 24 * 3600_000;
  return {
    id: w.id,
    organization_id: w.organization_id,
    name: w.name,
    slug: w.slug,
    environment: w.environment,
    external_id: w.external_id,
    created_at: w.created_at,
    stats: {
      agents: d.agents.filter((a) => a.workspace_id === w.id).length,
      installations: d.installations.filter((i) => i.workspace_id === w.id).length,
      runs_last_24h: d.runs.filter((r) => r.workspace_id === w.id && Date.parse(r.created_at) >= since).length,
    },
  };
}

export function missingFields(c: ConfigurationRow, app: AppRow): string[] {
  const missing: string[] = [];
  if (!c.mcp_server_url) missing.push("mcp_server_url");
  if (app.auth.type === "oauth2") {
    if (!c.oauth_client_id) missing.push("oauth_client_id");
    if (!c.oauth_client_secret_last4) missing.push("oauth_client_secret");
  }
  return missing;
}

export function configurationStatus(c: ConfigurationRow): ConfigurationStatus {
  const app = db().apps.find((a) => a.id === c.app_id);
  if (!app) return "unconfigured";
  const missing = missingFields(c, app);
  const required = app.auth.type === "oauth2" ? 3 : 1;
  if (missing.length === 0) return "ready";
  if (missing.length === required) return "unconfigured";
  return "incomplete";
}

export function toConfigurationSummary(c: ConfigurationRow): ConfigurationSummary {
  return {
    environment: c.environment,
    status: configurationStatus(c),
    version: c.version,
    updated_at: c.updated_at,
  };
}

export function toConfiguration(c: ConfigurationRow, app: AppRow): ClientConfiguration {
  return {
    app_id: c.app_id,
    environment: c.environment,
    status: configurationStatus(c),
    version: c.version,
    mcp: { server_url: c.mcp_server_url, transport: "streamable_http" },
    oauth:
      app.auth.type === "oauth2"
        ? {
            client_id: c.oauth_client_id,
            client_secret: {
              set: c.oauth_client_secret_last4 !== null,
              last4: c.oauth_client_secret_last4,
              updated_at: c.oauth_client_secret_updated_at,
            },
            redirect_uri: OAUTH_CALLBACK_URL,
            scopes: c.oauth_scopes,
          }
        : null,
    missing: missingFields(c, app),
    updated_at: c.updated_at,
    updated_by: c.updated_by_user_id ? toUser(c.updated_by_user_id) : null,
  };
}

export function toApp(app: AppRow): AppDefinition {
  const d = db();
  const environments: Environment[] = app.owner_workspace_id
    ? [d.workspaces.find((w) => w.id === app.owner_workspace_id)?.environment ?? "sandbox"]
    : ["sandbox", "production"];
  return {
    id: app.id,
    organization_id: app.organization_id,
    owner_workspace_id: app.owner_workspace_id,
    owner_scope: app.owner_workspace_id ? "workspace" : "organization",
    kind: app.kind,
    name: app.name,
    slug: app.slug,
    description: app.description,
    icon: app.icon,
    auth: app.auth,
    tools: app.tools,
    version: app.version,
    configurations: environments.map((environment) => {
      const c = d.configurations.find((x) => x.app_id === app.id && x.environment === environment);
      return c
        ? toConfigurationSummary(c)
        : { environment, status: "unconfigured" as const, version: 0, updated_at: null };
    }),
    installation_count: d.installations.filter((i) => i.app_id === app.id).length,
    created_at: app.created_at,
    updated_at: app.updated_at,
  };
}

export function toInstallationApp(app: AppRow): InstallationApp {
  return {
    id: app.id,
    name: app.name,
    slug: app.slug,
    owner_scope: app.owner_workspace_id ? "workspace" : "organization",
    icon: app.icon,
    auth_type: app.auth.type,
  };
}

export function toInstallation(i: InstallationRow): Installation {
  const d = db();
  const app = d.apps.find((a) => a.id === i.app_id);
  return {
    id: i.id,
    workspace_id: i.workspace_id,
    app: app
      ? toInstallationApp(app)
      : {
          id: i.app_id,
          name: "Removed app",
          slug: "removed",
          owner_scope: "organization",
          icon: { glyph: "?", color: "#64748b" },
          auth_type: "token",
        },
    label: i.label,
    status: i.status,
    configuration_version: i.configuration_version,
    connection: i.connection,
    grant_count: d.grants.filter((g) => g.workspace_id === i.workspace_id && g.installation_id === i.id).length,
    created_at: i.created_at,
    updated_at: i.updated_at,
  };
}

export function toConnectSession(s: ConnectSessionRow, withAuth = false): ConnectSession {
  const app = db().apps.find((a) => a.id === s.app_id);
  return {
    id: s.id,
    url: `${window.location.origin}/connect/${s.id}`,
    workspace_id: s.workspace_id,
    environment: s.environment,
    app: app
      ? toInstallationApp(app)
      : {
          id: s.app_id,
          name: "Removed app",
          slug: "removed",
          owner_scope: "organization",
          icon: { glyph: "?", color: "#64748b" },
          auth_type: "token",
        },
    intent: s.intent,
    installation_id: s.installation_id,
    label: s.label,
    subject: s.subject,
    status: s.status,
    return_url: s.return_url,
    created_at: s.created_at,
    expires_at: s.expires_at,
    completed_at: s.completed_at,
    failure_code: s.failure_code,
    auth: withAuth && app ? app.auth : null,
    requested_scopes: s.requested_scopes,
    expected_account:
      s.intent === "reconnect"
        ? (db().installations.find((i) => i.workspace_id === s.workspace_id && i.id === s.installation_id)?.connection
            ?.account ?? null)
        : null,
  };
}

export function toAgent(a: AgentRow): Agent {
  const d = db();
  const runs = d.runs.filter((r) => r.workspace_id === a.workspace_id && r.agent_id === a.id);
  const since = Date.now() - 24 * 3600_000;
  return {
    id: a.id,
    workspace_id: a.workspace_id,
    name: a.name,
    description: a.description,
    model: a.model,
    instructions: a.instructions,
    status: a.status,
    grant_count: d.grants.filter((g) => g.workspace_id === a.workspace_id && g.agent_id === a.id).length,
    runs_last_24h: runs.filter((r) => Date.parse(r.created_at) >= since).length,
    last_run_at: runs.reduce<string | null>(
      (latest, r) => (latest === null || r.created_at > latest ? r.created_at : latest),
      null,
    ),
    created_at: a.created_at,
    updated_at: a.updated_at,
  };
}

export function toRun(r: RunRow): Run {
  const agent = db().agents.find((a) => a.workspace_id === r.workspace_id && a.id === r.agent_id);
  const { plan: _plan, ...rest } = r;
  return { ...rest, agent_name: agent?.name ?? "Deleted agent" };
}

export function toApiKey(k: ApiKeyRow): ApiKey {
  return {
    id: k.id,
    organization_id: k.organization_id,
    environment: k.environment,
    name: k.name,
    prefix: k.prefix,
    last4: k.last4,
    created_by: toUser(k.created_by_user_id),
    created_at: k.created_at,
    last_used_at: k.last_used_at,
    revoked_at: k.revoked_at,
  };
}
