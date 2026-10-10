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
  RUN_STATUSES,
  TERMINAL_RUN_STATUSES,
  type UsageReport,
  WEBHOOK_EVENT_TYPES,
  type WebhookEndpoint,
  type WorkspaceId,
} from "@zupflow/sdk";
import { HttpResponse, http } from "msw";
import { z } from "zod";
import { checkedAuthTemplate } from "./auth-templates";
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
  toMember,
  toOrganization,
  toRun,
  toRunEvent,
  toWorkspace,
  type WorkspaceAccess,
  withExpiry,
  workspaceAccess,
} from "./http";
import { hashReference, isId, newId, newReference } from "./ids";
import type { AgentRow, AppRow, ConfigurationRow, ConnectSessionRow, GrantRow, InstallationRow, RunRow } from "./model";
import { hash, token } from "./random";
import { cancelRun, enqueueRun, planRun } from "./runner";
import { configurationOf, inWorkspace, runEventsKey, timelineOf } from "./scope";

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

const slug = z.string().regex(SLUG_PATTERN, "Use 1–40 lowercase letters, numbers and hyphens.");
const environment = z.enum(ENVIRONMENTS, "Choose Sandbox or Production.");
const name = (max = 60) => z.string().trim().min(1, "Required.").max(max, `Use ${max} characters or fewer.`);
const MINUTE = 60_000;

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
  app: ["app", "App"],
  inst: ["installation", "Installation"],
  session: ["connect_session", "Connect session"],
  agent: ["agent", "Agent"],
  grant: ["grant", "Grant"],
  run: ["run", "Run"],
  key: ["api_key", "API key"],
  hook: ["webhook_endpoint", "Webhook endpoint"],
} as const satisfies Record<string, readonly [IdKind, string]>;

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
  if (app.auth.type !== "oauth2" && (input.oauth_client_id || input.oauth_client_secret)) {
    throw new ApiFailure(422, "oauth_not_supported", `${app.name} authenticates with a token, not OAuth.`);
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
  const auth =
    input.auth === undefined
      ? undefined
      : checkedAuthTemplate(
          app.auth,
          input.auth,
          db()
            .configurations.filter(
              (c) =>
                c.organization_id === app.organization_id &&
                c.workspace_id === app.owner_workspace_id &&
                c.app_id === app.id,
            )
            .map((c) => ({ environment: c.environment, scopes: c.oauth_scopes })),
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
  auth_type: z.enum(["oauth2", "token"], "Choose OAuth or token."),
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
      expires_at: app.auth.type === "oauth2" ? new Date(Date.now() + 60 * MINUTE).toISOString() : null,
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
        expires_at: app.auth.type === "oauth2" ? new Date(Date.now() + 60 * MINUTE).toISOString() : null,
      },
      created_at: now,
      updated_at: now,
    };
    d.installations.push(installation);
    session.installation_id = installation.id;
  }

  session.status = "completed";
  session.completed_at = now;
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

function webhookSecret(env: Environment) {
  return `whsec_${env === "production" ? "live" : "test"}_${token(32)}`;
}

/** An integer query parameter clamped to 1..max; missing or not an integer means the default. */
function clampedInt(url: URL, name: string, fallback: number, max: number): number {
  const value = Number(url.searchParams.get(name) ?? fallback);
  return Number.isInteger(value) ? Math.min(Math.max(value, 1), max) : fallback;
}

function paginate<T>(items: T[], cursor: string | null, limit: number) {
  const offset = cursor ? Number.parseInt(atob(cursor), 10) || 0 : 0;
  const page = items.slice(offset, offset + limit);
  const next = offset + limit < items.length ? btoa(String(offset + limit)) : null;
  return { page, meta: { pagination: { next_cursor: next, total: items.length } } };
}

export function createHandlers(apiBase: string) {
  const u = (path: string) => `${apiBase.replace(/\/$/, "")}${path}`;

  return [
    // --- Identity ---------------------------------------------------------
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
        if (user && db().memberships.some((m) => m.organization_id === access.org.id && m.user_id === user?.id)) {
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
        if (!member) throw notFound("Member");
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
        d.connect_sessions = d.connect_sessions.filter((s) => !here(s));
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
          { environment: access.workspace.environment },
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

    http.delete(
      u("/v1/workspaces/:ws/installations/:inst"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, ADMINS, "remove installations");
        const row = findInstallation(access, idParam(params, "inst"));
        const d = db();
        d.grants = d.grants.filter((g) => !(inWorkspace(g, access.workspace) && g.installation_id === row.id));
        d.installations = d.installations.filter((i) => i !== row);
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
          requested_scopes: app?.auth.type === "oauth2" ? (config?.oauth_scopes ?? []) : [],
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
      route(async ({ params }) =>
        ok(toConnectSession(await findSessionByReferenceCrossTenant(param(params, "reference")), true)),
      ),
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
          throw new ApiFailure(422, "wrong_auth_type", `${app.name} connects with OAuth.`);
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
        if (app.auth.type !== "oauth2") {
          throw new ApiFailure(422, "wrong_auth_type", `${app.name} connects with a token.`);
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
        return ok(db().webhooks.filter((w) => w.organization_id === access.org.id));
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
          events: input.events,
          enabled: true,
          signing_secret_last4: secret.slice(-4),
          created_at: new Date().toISOString(),
          last_delivery: null,
        };
        db().webhooks.push(endpoint);
        audit(
          access,
          "webhook.created",
          { type: "webhook", id: endpoint.id, name: endpoint.url },
          { environment: endpoint.environment },
        );
        return created({ endpoint, signing_secret: secret });
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
        if (input.url) assertPublicHttps(input.url, "url");
        Object.assign(endpoint, Object.fromEntries(Object.entries(input).filter(([, v]) => v !== undefined)));
        return ok(endpoint);
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
        audit(
          access,
          "webhook.deleted",
          { type: "webhook", id: endpoint.id, name: endpoint.url },
          { environment: endpoint.environment },
        );
        return noContent();
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

function validateTools(
  access: WorkspaceAccess,
  installation: InstallationRow,
  tools: string[],
  approvalRequired: string[],
) {
  if (!inWorkspace(installation, access.workspace)) throw notFound("Installation");
  const app = appInScope(installation);
  const known = new Set(app?.tools.map((t) => t.name));
  const unknown = tools.find((t) => !known.has(t));
  if (unknown) {
    throw new ApiFailure(422, "unknown_tool", `${unknown} isn't a tool of ${app?.name ?? "this app"}.`);
  }
  const outside = approvalRequired.find((t) => !tools.includes(t));
  if (outside) {
    throw new ApiFailure(422, "approval_outside_grant", `${outside} requires approval but isn't granted.`);
  }
}
