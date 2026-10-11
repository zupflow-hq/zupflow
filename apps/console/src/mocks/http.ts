/**
 * Shared plumbing for mock route handlers: the response envelope, errors,
 * authentication, authorization and the row → wire mappers.
 */

import { matchRoute } from "@zupflow/contract";
import type {
  Agent,
  AgentGrant,
  AnyId,
  ApiKey,
  AppDefinition,
  AppId,
  ClientConfiguration,
  ConfigurationStatus,
  ConfigurationSummary,
  ConnectSession,
  Environment,
  Installation,
  InstallationApp,
  Invitation,
  Member,
  MemberRole,
  Organization,
  OrganizationId,
  Run,
  RunEvent,
  User,
  UserId,
  Workspace,
  WorkspaceId,
} from "@zupflow/sdk";
import {
  EXPIRED_INVITATION_SHOWN_DAYS,
  INVITATION_LIST_LIMIT,
  INVITATION_TTL_DAYS,
  ORGANIZATION_HEADER,
  WebhookEvent,
} from "@zupflow/sdk";
import { delay, HttpResponse } from "msw";
import type { z } from "zod";
import { keyedDigest } from "./credentials";
import { db, persist } from "./db";
import { isPublicHost } from "./destinations";
import { idempotently } from "./idempotency";
import { hashReference, isId, newId } from "./ids";
import type {
  AgentRow,
  ApiKeyRow,
  AppRow,
  ConfigurationRow,
  ConnectSessionRow,
  GrantRow,
  InstallationRow,
  MembershipRow,
  OrganizationRow,
  RunEventRow,
  RunRow,
  WorkspaceRow,
} from "./model";
import { appIn, configurationOf, inWorkspace, sameWorkspace } from "./scope";

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

function failureFor(error: unknown) {
  if (error instanceof ApiFailure) return failure(error);
  console.error("[mock api]", error);
  return failure(new ApiFailure(500, "internal_error", "Something went wrong on our side."));
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
 * Wrap a resolver: add latency, apply `Idempotency-Key` to every `POST`, turn
 * {@link ApiFailure} into an error envelope and persist after every mutating
 * request. A refused one can still change state on purpose (a Connect session
 * fails with its reason), and another tab, like Connect's own window and the
 * Console watching it, only sees what was persisted. A keyed `POST` is also
 * persisted by the idempotency wrapper; this is what covers requests without a
 * key (the mock-only simulated consent) and every `PUT`, `PATCH` and `DELETE`.
 */
type ResolverArgs = {
  request: Request;
  params: Record<string, string | readonly string[] | undefined>;
};

export function route<A extends ResolverArgs>(fn: (args: A) => Promise<Response> | Response) {
  return async (args: A) => {
    const [min, max] = latency;
    if (max > 0) await delay(min + Math.floor(Math.random() * (max - min)));
    const handle = async () => {
      try {
        return await fn(args);
      } catch (error) {
        return failureFor(error);
      } finally {
        if (args.request.method !== "GET") persist();
      }
    };
    try {
      const path = new URL(args.request.url).pathname;
      const contract = matchRoute(args.request.method, path.slice(Math.max(0, path.indexOf("/v1/"))));
      if (contract?.callers.some((kind) => kind === "console" || kind === "api_key")) {
        const caller = await authenticateCaller(args.request);
        // Like Rust's ConsoleUser extractor: before path IDs, bodies and idempotency.
        if (caller.kind === "api_key" && !contract.callers.includes("api_key")) throw consoleOnly();
      }
      return await idempotently(args.request, args.params, handle);
    } catch (error) {
      return failureFor(error);
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

type Caller = { kind: "console"; user: User } | { kind: "api_key"; key: ApiKeyRow };

// WebCrypto is asynchronous. Resolve once at the HTTP boundary, before the
// synchronous access helpers and the single idempotency wrapper use the caller.
const callers = new WeakMap<Request, Caller>();

export const consoleOnly = () =>
  new ApiFailure(403, "console_only", "This route is for Console sessions; an API key can't call it.");

function consoleCaller(request: Request): Caller & { kind: "console" } {
  const header = request.headers.get("Authorization") ?? "";
  const token = header.startsWith("Bearer ") ? header.slice(7) : null;
  const user = token?.startsWith("mock_") ? db().users.find((u) => u.id === token.slice(5)) : undefined;
  if (!user) throw new ApiFailure(401, "unauthenticated", "Sign in to continue.");
  return { kind: "console", user };
}

/**
 * The one cross-tenant API-key lookup: the credential establishes organization
 * and environment. Revoked and unknown hashes never become Console sessions.
 */
async function findActiveApiKeyByHashCrossTenant(secretHash: string): Promise<ApiKeyRow> {
  const key = db().api_keys.find((row) => row.secret_hash === secretHash && row.revoked_at === null);
  if (!key) throw new ApiFailure(401, "unauthenticated", "Sign in to continue.");
  const now = Date.now();
  if (key.last_used_at === null || Date.parse(key.last_used_at) <= now - 60_000) {
    key.last_used_at = new Date(now).toISOString();
    persist();
  }
  return key;
}

async function authenticateCaller(request: Request): Promise<Caller> {
  const header = request.headers.get("Authorization") ?? "";
  const token = header.startsWith("Bearer ") ? header.slice(7) : null;
  const resolved: Caller = token?.startsWith("zf_")
    ? { kind: "api_key", key: await findActiveApiKeyByHashCrossTenant(await keyedDigest("api_key", token)) }
    : consoleCaller(request);
  callers.set(request, resolved);
  return resolved;
}

/** The resolved credential; unwrapped Console-only helpers can still authenticate a member. */
export function callerOf(request: Request): Caller {
  return callers.get(request) ?? consoleCaller(request);
}

/** Console-only identity, never the person who happened to create an API key. */
export function authenticate(request: Request): User {
  const caller = callerOf(request);
  if (caller.kind === "api_key") throw consoleOnly();
  return caller.user;
}

export interface ConsoleOrgAccess {
  kind: "console";
  user: User;
  org: OrganizationRow;
  membership: MembershipRow;
}

export type OrgAccess = ConsoleOrgAccess | { kind: "api_key"; key: ApiKeyRow; org: OrganizationRow };

/** Console-only organization routes keep their member's identity and role. */
export function consoleOrgAccess(request: Request, organizationParam: string): ConsoleOrgAccess {
  return membershipIn(authenticate(request), organizationParam, "Organization");
}

/** Credential first, then the organization explicitly named by the route. */
export function orgAccess(request: Request, organizationParam: string): OrgAccess {
  const caller = callerOf(request);
  return caller.kind === "console"
    ? membershipIn(caller.user, organizationParam, "Organization")
    : keyIn(caller.key, organizationParam, "Organization");
}

function keyIn(key: ApiKeyRow, organizationParam: string, label: string): OrgAccess {
  if (key.organization_id !== organizationParam) throw notFound(label);
  const org = db().organizations.find((row) => row.id === key.organization_id);
  if (!org) throw notFound(label);
  return { kind: "api_key", key, org };
}

/** The caller's active membership in the named organization, or an indistinguishable 404. */
function membershipIn(user: User, organizationParam: string, notFoundLabel: string): ConsoleOrgAccess {
  if (!isId("organization", organizationParam)) throw notFound(notFoundLabel);
  const organizationId: OrganizationId = organizationParam;
  const membership = db().memberships.find(
    (m) => m.organization_id === organizationId && m.user_id === user.id && m.status === "active",
  );
  const org = db().organizations.find((o) => o.id === organizationId);
  if (!membership || !org) throw notFound(notFoundLabel);
  membership.last_active_at = new Date().toISOString();
  return { kind: "console", user, org, membership };
}

export type WorkspaceAccess = OrgAccess & { workspace: WorkspaceRow };

export function keyEnvironment(access: OrgAccess): Environment | undefined {
  return access.kind === "api_key" ? access.key.environment : undefined;
}

/** Explicit other-environment requests refuse; omitted filters inherit a key's environment. */
export function requireEnvironment(access: OrgAccess, environment: string | null | undefined) {
  const own = keyEnvironment(access);
  if (own && environment && own !== environment) {
    throw new ApiFailure(422, "environment_mismatch", "This API key can't act in that environment.");
  }
  return environment || own;
}

/**
 * Establish organization from the credential/header before finding a workspace.
 * A key needs no header, but a supplied one must name its own organization.
 * Another organization or environment is always the same workspace 404.
 */
export function workspaceAccess(request: Request, workspaceParam: string): WorkspaceAccess {
  const caller = callerOf(request);
  const named = request.headers.get(ORGANIZATION_HEADER);
  let access: OrgAccess;
  if (caller.kind === "api_key") {
    access = keyIn(caller.key, named === null ? caller.key.organization_id : named.trim(), "Workspace");
  } else {
    if (!named?.trim()) {
      throw new ApiFailure(
        400,
        "organization_required",
        `Name the organization this request acts in with the ${ORGANIZATION_HEADER} header.`,
      );
    }
    access = membershipIn(caller.user, named, "Workspace");
  }
  if (!isId("workspace", workspaceParam)) throw notFound("Workspace");
  const workspaceId: WorkspaceId = workspaceParam;
  const own = keyEnvironment(access);
  const workspace = db().workspaces.find(
    (w) => w.organization_id === access.org.id && w.id === workspaceId && (!own || w.environment === own),
  );
  if (!workspace) throw notFound("Workspace");
  return { ...access, workspace };
}

/**
 * Cross-tenant by design: hosted Connect starts with no organization or
 * workspace, only the link's reference (a random token, never the session's
 * ID). It reads exactly one session and nothing else. Everything after it is
 * scoped to that session's organization and workspace (see `sessionScope`).
 */
export async function findSessionByReferenceCrossTenant(reference: string): Promise<ConnectSessionRow> {
  const referenceHash = reference ? await hashReference(reference) : null;
  const session = referenceHash
    ? db().connect_sessions.find((s) => s.reference_hash !== null && s.reference_hash === referenceHash)
    : undefined;
  if (!session) throw notFound("Connect session");
  return withExpiry(session.organization_id, session.workspace_id, session);
}

/** A pending session past its expiry is expired, whoever reads it. */
export function withExpiry(
  organizationId: OrganizationId,
  workspaceId: WorkspaceId,
  session: ConnectSessionRow,
): ConnectSessionRow {
  if (session.organization_id !== organizationId || session.workspace_id !== workspaceId)
    throw notFound("Connect session");
  if (session.status === "pending" && Date.parse(session.expires_at) <= Date.now()) {
    session.status = "expired";
    announceSession(organizationId, workspaceId, session, session.expires_at);
    persist();
  }
  return session;
}

/** The mock records each lean snapshot synchronously with its transition. */
export function announceLifecycle(
  organizationId: OrganizationId,
  workspaceId: WorkspaceId,
  event: Pick<WebhookEvent, "type" | "data">,
  at: string,
) {
  const d = db();
  const workspace = d.workspaces.find((w) => w.organization_id === organizationId && w.id === workspaceId);
  if (!workspace) throw notFound("Workspace");
  const id = newId("webhook_event");
  // The caller supplies only the event-specific fields, never the envelope.
  const snapshot = WebhookEvent.parse({
    ...event,
    id,
    organization_id: organizationId,
    environment: workspace.environment,
    created_at: at,
    workspace: { id: workspaceId, external_id: workspace.external_id },
  });
  d.webhook_events.push({
    id,
    organization_id: organizationId,
    workspace_id: workspaceId,
    environment: workspace.environment,
    body: snapshot,
  });
  for (const endpoint of d.webhooks) {
    if (
      endpoint.organization_id !== organizationId ||
      endpoint.environment !== workspace.environment ||
      !endpoint.enabled ||
      snapshot.type === "webhook.test" ||
      !endpoint.events.includes(snapshot.type)
    )
      continue;
    d.webhook_deliveries.push({
      id: newId("webhook_delivery"),
      organization_id: organizationId,
      endpoint_id: endpoint.id,
      event_id: id,
      state: "pending",
      attempts: 0,
      last_status_code: null,
      last_error: null,
      last_attempt_at: null,
      next_attempt_at: at,
      created_at: at,
    });
  }
}

export function announceSession(
  organizationId: OrganizationId,
  workspaceId: WorkspaceId,
  session: ConnectSessionRow,
  at: string,
) {
  if (session.organization_id !== organizationId || session.workspace_id !== workspaceId)
    throw notFound("Connect session");
  if (session.status === "pending") return;
  announceLifecycle(
    organizationId,
    workspaceId,
    {
      type: session.status === "completed" ? "connect_session.completed" : "connect_session.failed",
      data: {
        object: "connect_session",
        id: session.id,
        status: session.status,
        installation_id: session.status === "completed" ? session.installation_id : null,
        failure_code: session.failure_code,
      },
    },
    at,
  );
}

export function requireRole(access: OrgAccess, roles: MemberRole[], action: string) {
  if (access.kind === "api_key") return;
  if (!roles.includes(access.membership.role)) {
    throw new ApiFailure(
      403,
      "forbidden",
      `Your role (${access.membership.role}) can't ${action}. Ask an organization owner.`,
    );
  }
}

export const ADMINS: MemberRole[] = ["owner", "admin"];
/** Everyone who may change anything: every role but the read-only viewer. */
export const DEVELOPERS: MemberRole[] = ["owner", "admin", "developer"];

export function audit(
  access: OrgAccess,
  action: string,
  target: { type: string; id: AnyId | null; name: string },
  scope: { workspace?: Pick<WorkspaceRow, "id" | "environment">; environment?: Environment } = {},
) {
  db().audit.unshift({
    id: newId("audit_event"),
    organization_id: access.org.id,
    workspace_id: scope.workspace?.id ?? null,
    environment: scope.environment ?? scope.workspace?.environment ?? null,
    actor:
      access.kind === "api_key"
        ? { type: "api_key", id: access.key.id, name: access.key.name }
        : { type: "user", id: access.user.id, name: access.user.name },
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
  if (!isPublicHost(url.hostname) || url.username !== "" || url.password !== "") {
    throw new ApiFailure(422, "destination_not_allowed", "Private and link-local hosts are blocked.", {
      [field]: "Use a public host. Private, loopback and link-local addresses are blocked.",
    });
  }
}

// ---------------------------------------------------------------------------
// Row → wire mappers

export function toUser(id: UserId): User {
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
      has_installation: d.installations.some(
        (i) => i.organization_id === org.id && workspaceIds.has(i.workspace_id) && i.status === "connected",
      ),
      has_agent: d.agents.some((a) => a.organization_id === org.id && workspaceIds.has(a.workspace_id)),
      has_run: d.runs.some((r) => r.organization_id === org.id && workspaceIds.has(r.workspace_id)),
    },
  };
}

/** When an invitation stops being accepted; null once it's a membership. */
export function invitationExpiresAt(m: MembershipRow): string | null {
  if (m.status !== "invited") return null;
  return new Date(Date.parse(m.invited_at) + INVITATION_TTL_DAYS * 86_400_000).toISOString();
}

export function isExpiredInvitation(m: MembershipRow, now = Date.now()): boolean {
  const expiresAt = invitationExpiresAt(m);
  return expiresAt !== null && Date.parse(expiresAt) <= now;
}

export function toMember(m: MembershipRow): Member {
  return {
    id: m.id,
    organization_id: m.organization_id,
    user: toUser(m.user_id),
    role: m.role,
    status: m.status,
    invited_at: m.invited_at,
    expires_at: invitationExpiresAt(m),
    joined_at: m.joined_at,
    last_active_at: m.last_active_at,
  };
}

/** Whether a member with role `grantor` can grant `role`: an owner or admin, and an owner for an owner. */
export function canGrant(grantor: MemberRole, role: MemberRole): boolean {
  return (grantor === "owner" || grantor === "admin") && (role !== "owner" || grantor === "owner");
}

/**
 * Whether an invitation still stands: its grantor is an active member of its
 * organization who can still grant its role. One that doesn't is withdrawn,
 * whatever row is left: an invitation with no grantor known grants nothing.
 */
export function grantorAllows(m: MembershipRow): boolean {
  if (m.invited_by === null) return false;
  const grantor = db().memberships.find(
    (g) => g.organization_id === m.organization_id && g.user_id === m.invited_by && g.status === "active",
  );
  return grantor !== undefined && canGrant(grantor.role, m.role);
}

/**
 * Withdraws the pending invitations `grantor` granted in `organizationId`
 * that `role` can't grant: all of them once they're removed (`null`).
 */
export function withdrawUngrantable(organizationId: OrganizationId, grantor: UserId, role: MemberRole | null) {
  const d = db();
  d.memberships = d.memberships.filter(
    (m) =>
      !(
        m.organization_id === organizationId &&
        m.status === "invited" &&
        m.invited_by === grantor &&
        (role === null || !canGrant(role, m.role))
      ),
  );
}

/**
 * The person's own invitations as `GET /v1/me/invitations` lists them: still
 * granted, not expired more than `EXPIRED_INVITATION_SHOWN_DAYS` ago, newest
 * first, at most `INVITATION_LIST_LIMIT`.
 */
export function invitationsOf(userId: UserId, now = Date.now()): MembershipRow[] {
  const shownUntil = (m: MembershipRow) =>
    Date.parse(m.invited_at) + (INVITATION_TTL_DAYS + EXPIRED_INVITATION_SHOWN_DAYS) * 86_400_000;
  return db()
    .memberships.filter((m) => m.user_id === userId && m.status === "invited")
    .filter((m) => grantorAllows(m) && shownUntil(m) > now)
    .sort((a, b) => b.invited_at.localeCompare(a.invited_at) || b.id.localeCompare(a.id))
    .slice(0, INVITATION_LIST_LIMIT);
}

/** An invitation as its invitee sees it: the organization's name, never its members. */
export function toInvitation(m: MembershipRow, org: OrganizationRow): Invitation {
  const inviter = db().users.find((u) => u.id === m.invited_by);
  if (!inviter) throw new Error("An invitation is shown only while its grantor can grant it");
  return {
    id: m.id,
    organization: { id: org.id, name: org.name, slug: org.slug },
    role: m.role,
    invited_by: { name: inviter.name, email: inviter.email },
    status: isExpiredInvitation(m) ? "expired" : "pending",
    invited_at: m.invited_at,
    expires_at: invitationExpiresAt(m) ?? m.invited_at,
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
      agents: d.agents.filter((a) => inWorkspace(a, w)).length,
      installations: d.installations.filter((i) => inWorkspace(i, w)).length,
      runs_last_24h: d.runs.filter((r) => inWorkspace(r, w) && Date.parse(r.created_at) >= since).length,
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
  const app = db().apps.find(
    (a) => a.organization_id === c.organization_id && a.owner_workspace_id === c.workspace_id && a.id === c.app_id,
  );
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

export function toApp(app: AppRow, visibleEnvironment?: Environment): AppDefinition {
  const d = db();
  // Installations count only where the app can be installed: its organization's workspaces, or its own workspace.
  const appWorkspaces = new Set(
    d.workspaces
      .filter(
        (w) =>
          w.organization_id === app.organization_id &&
          (!visibleEnvironment || w.environment === visibleEnvironment) &&
          (app.owner_workspace_id === null || w.id === app.owner_workspace_id),
      )
      .map((w) => w.id),
  );
  const environments: Environment[] = app.owner_workspace_id
    ? [
        d.workspaces.find((w) => w.organization_id === app.organization_id && w.id === app.owner_workspace_id)
          ?.environment ?? "sandbox",
      ]
    : visibleEnvironment
      ? [visibleEnvironment]
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
      const c = configurationOf(d.configurations, app, environment);
      return c
        ? toConfigurationSummary(c)
        : { environment, status: "unconfigured" as const, version: 0, updated_at: null };
    }),
    installation_count: d.installations.filter(
      (i) => i.organization_id === app.organization_id && appWorkspaces.has(i.workspace_id) && i.app_id === app.id,
    ).length,
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

/**
 * The app a workspace-scoped row refers to, looked up within that row's own
 * organization and workspace: an organization app, or the workspace's own.
 */
export function appInScope(row: { organization_id: OrganizationId; workspace_id: WorkspaceId; app_id: AppId }) {
  return appIn(db().apps, row);
}

export function toInstallation(i: InstallationRow): Installation {
  const d = db();
  const app = appInScope(i);
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
    grant_count: d.grants.filter((g) => sameWorkspace(g, i) && g.installation_id === i.id).length,
    created_at: i.created_at,
    updated_at: i.updated_at,
  };
}

/** A stored grant as the API returns it: the organization is known from the request, not repeated. */
export function toGrant({ organization_id: _organization, ...grant }: GrantRow): AgentGrant {
  return grant;
}

export function toConnectSession(s: ConnectSessionRow, withAuth = false): ConnectSession {
  const app = appInScope(s);
  return {
    id: s.id,
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
        ? (db().installations.find(
            (i) =>
              i.organization_id === s.organization_id &&
              i.workspace_id === s.workspace_id &&
              i.id === s.installation_id,
          )?.connection?.account ?? null)
        : null,
  };
}

export function toAgent(a: AgentRow): Agent {
  const d = db();
  const runs = d.runs.filter((r) => sameWorkspace(r, a) && r.agent_id === a.id);
  const since = Date.now() - 24 * 3600_000;
  return {
    id: a.id,
    workspace_id: a.workspace_id,
    name: a.name,
    description: a.description,
    model: a.model,
    instructions: a.instructions,
    status: a.status,
    grant_count: d.grants.filter((g) => sameWorkspace(g, a) && g.agent_id === a.id).length,
    runs_last_24h: runs.filter((r) => Date.parse(r.created_at) >= since).length,
    last_run_at: runs.reduce<string | null>(
      (latest, r) => (latest === null || r.created_at > latest ? r.created_at : latest),
      null,
    ),
    created_at: a.created_at,
    updated_at: a.updated_at,
  };
}

/** A stored run as the API returns it: no plan, and no organization (known from the request). */
export function toRun(r: RunRow): Run {
  const agent = db().agents.find((a) => sameWorkspace(a, r) && a.id === r.agent_id);
  const { plan: _plan, organization_id: _organization, ...rest } = r;
  return { ...rest, agent_name: agent?.name ?? "Deleted agent" };
}

/** A stored run event as the API returns it. */
export function toRunEvent({ organization_id: _organization, ...event }: RunEventRow): RunEvent {
  return event as RunEvent;
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
