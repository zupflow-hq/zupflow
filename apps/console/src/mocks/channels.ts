/**
 * The mock's channels: linked teams (app managers), each agent's generated
 * app, connections with their conversations and messages, agent channels,
 * the organization's channel tool levels and settings, the provider's inbound
 * endpoint, and channel Connect sessions. A fake provider stands in for
 * Slack: it accepts any token of the right shape, refuses one containing
 * `invalid`, makes apps at once and installs them on consent.
 *
 * Every lookup carries the organization and the workspace; the one
 * exception is finding an app by its inbound reference, which is how a
 * provider's event finds its tenant, named as cross-tenant below.
 */
import {
  type AgentChannel,
  type AgentId,
  agentChannelDefaults,
  CHANNEL_PROVIDER_NAMES,
  CHANNEL_PROVIDERS,
  CHANNEL_SETTINGS_DEFAULTS,
  CHANNEL_TOOL_NAMES,
  CHANNEL_TOOLS,
  type ChannelApp,
  type ChannelAppId,
  type ChannelAppManager,
  type ChannelAppManagerConnectResult,
  type ChannelAppStatus,
  type ChannelConnection,
  type ChannelConnectionConnectResult,
  type ChannelConnectionStatus,
  ChannelConversationKind,
  type ChannelMessage,
  type ChannelProvider,
  type ChannelSettings,
  type ChannelToolLevel,
  type ChannelToolName,
  type ConnectTarget,
  type ChannelConversationKind as ConversationKind,
  type Id,
  type IdKind,
  type OrganizationId,
  type PutAgentChannelInput,
  ToolLevel,
  type WorkspaceId,
} from "@zupflow/sdk";
import { http } from "msw";
import { z } from "zod";
import { db } from "./db";
import { FAKE_SCOPES } from "./fake-channel";
import {
  ADMINS,
  ApiFailure,
  announceLifecycle,
  announceSession,
  assertConsoleMayAct,
  audit,
  body,
  clampedInt,
  consoleOrgAccess,
  created,
  DEVELOPERS,
  noContent,
  notFound,
  type OrgAccess,
  ok,
  orgAccess,
  paginate,
  param,
  requireRole,
  route,
  toUser,
  validated,
  type WorkspaceAccess,
  withExpiry,
  workspaceAccess,
} from "./http";
import { isId, newId } from "./ids";
import type {
  AgentChannelRow,
  AgentRow,
  ChannelAppManagerRow,
  ChannelAppRow,
  ChannelConnectionRow,
  ChannelConversationRow,
  ChannelMessageRow,
  ChannelPersonRow,
  ConnectSessionRow,
  Db,
} from "./model";
import { hash, token } from "./random";
import { inWorkspace, issueShortId, type Scoped, sameWorkspace } from "./scope";

type Params = Record<string, string | readonly string[] | undefined>;

// ---------------------------------------------------------------------------
// The fake provider

/** How long a configuration token lasts before it must be rotated, as Slack's do. */
const TOKEN_LIFETIME_MS = 12 * 60 * 60_000;
/** How far a signed request's timestamp may be from the clock. */
export const INBOUND_TOLERANCE_SECONDS = 5 * 60;

const BASE32 = "abcdefghijklmnopqrstuvwxyz234567";

/** 32 random base32 characters: an app's inbound reference, unique across every tenant. */
function newInboundRef(): string {
  let ref = "";
  for (const byte of crypto.getRandomValues(new Uint8Array(32))) ref += BASE32[byte % 32];
  return ref;
}

const upper = (value: string, length: number) =>
  Array.from({ length }, (_, i) => "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"[hash(`${value}:${i}`) % 32]).join("");

/** The team the fake provider installs into: the manager's, once learned, else one derived from it. */
function fakeTeam(manager: ChannelAppManagerRow, workspaceName: string) {
  return { id: manager.team_id ?? `T${upper(manager.id, 9)}`, name: manager.team_name ?? workspaceName };
}

const tokenInput = z.string().trim().min(1, "Required.").max(4096, "Use 4,096 characters or fewer.");

/** What a deployment's operator does so the server reaches a provider. */
const SETUP_HINT: Record<ChannelProvider, string> = {
  slack: "Ask its operator to set ZUPFLOW_PUBLIC_URL to the https:// address Slack reaches it at.",
};

/** The providers this deployment reaches, each with how a team is linked there. */
function availableProviders() {
  return CHANNEL_PROVIDERS.filter((provider) => db().channel_providers.includes(provider)).map((provider) => ({
    provider,
    methods: ["configuration_token" as const],
  }));
}

/**
 * Checks both tokens with the fake provider: one containing `invalid` is
 * refused. A provider this deployment doesn't reach checks nothing.
 */
function checkTokens(provider: ChannelProvider, input: { configuration_token: string; refresh_token: string }) {
  if (!db().channel_providers.includes(provider)) {
    const name = CHANNEL_PROVIDER_NAMES[provider];
    throw new ApiFailure(
      501,
      "channel_providers_not_configured",
      `This deployment can't reach ${name}, so it can't link teams there. ${SETUP_HINT[provider]}`,
    );
  }
  if (/invalid/i.test(input.configuration_token) || /invalid/i.test(input.refresh_token)) {
    const name = CHANNEL_PROVIDER_NAMES[provider];
    throw new ApiFailure(
      422,
      "invalid_configuration_token",
      `${name} refused that configuration token. Generate a new one in ${name}'s app settings and paste it with its refresh token.`,
      { configuration_token: `Refused by ${name}. Generate a new one and paste it with its refresh token.` },
    );
  }
}

const managerInput = z.object({
  provider: z.enum(CHANNEL_PROVIDERS, "Choose a provider."),
  method: z.enum(["configuration_token"], "Choose configuration_token."),
  configuration_token: tokenInput,
  refresh_token: tokenInput,
});

const tokensInput = z.object({ configuration_token: tokenInput, refresh_token: tokenInput });

// ---------------------------------------------------------------------------
// Lookups: every one within the request's organization and workspace

function pathId<K extends IdKind>(params: Params, key: string, kind: K, label: string): Id<K> {
  const value = param(params, key);
  if (!isId(kind, value)) throw notFound(label);
  return value;
}

function findManager(access: WorkspaceAccess, params: Params): ChannelAppManagerRow {
  const id = pathId(params, "manager", "channel_app_manager", "Channel app manager");
  const row = db().channel_app_managers.find((m) => inWorkspace(m, access.workspace) && m.id === id);
  if (!row) throw notFound("Channel app manager");
  return row;
}

/** A live app: a deleted one is as gone as one that never was. */
function findApp(access: WorkspaceAccess, id: string | null | undefined): ChannelAppRow {
  const row = db().channel_apps.find((a) => inWorkspace(a, access.workspace) && a.id === id && a.status !== "deleted");
  if (!row) throw notFound("Channel app");
  return row;
}

function findConnection(access: WorkspaceAccess, params: Params): ChannelConnectionRow {
  const id = pathId(params, "channel_connection", "channel_connection", "Channel connection");
  const row = db().channel_connections.find((c) => inWorkspace(c, access.workspace) && c.id === id);
  if (!row) throw notFound("Channel connection");
  return row;
}

function findAgent(access: WorkspaceAccess, id: string | null | undefined): AgentRow {
  const row = db().agents.find((a) => inWorkspace(a, access.workspace) && a.id === id);
  if (!row) throw notFound("Agent");
  return row;
}

/** An app's manager, in the app's own organization and workspace. */
function managerOf(d: Db, app: ChannelAppRow) {
  return d.channel_app_managers.find((m) => sameWorkspace(m, app) && m.id === app.manager_id);
}

/** The app's connection that isn't disconnected: an app is installed once at a time. */
function liveConnectionOf(d: Db, app: Pick<ChannelAppRow, "id" | "organization_id" | "workspace_id">) {
  return d.channel_connections.find((c) => sameWorkspace(c, app) && c.app_id === app.id && c.status !== "disconnected");
}

function appOf(d: Db, connection: ChannelConnectionRow) {
  return connection.app_id === null
    ? undefined
    : d.channel_apps.find((a) => sameWorkspace(a, connection) && a.id === connection.app_id);
}

// ---------------------------------------------------------------------------
// Row → wire

export function toChannelAppManager(row: ChannelAppManagerRow): ChannelAppManager {
  const d = db();
  return {
    id: row.id,
    workspace_id: row.workspace_id,
    provider: row.provider,
    method: row.method,
    team: { id: row.team_id, name: row.team_name },
    status: row.status,
    configuration_token: { set: true, last4: row.token_last4, updated_at: row.token_updated_at },
    token_expires_at: row.token_expires_at,
    app_count: d.channel_apps.filter((a) => sameWorkspace(a, row) && a.manager_id === row.id && a.status !== "deleted")
      .length,
    created_by: row.created_by_user_id ? toUser(row.created_by_user_id) : null,
    created_at: row.created_at,
    updated_at: row.updated_at,
  };
}

export function toChannelApp(row: ChannelAppRow): ChannelApp {
  return {
    id: row.id,
    workspace_id: row.workspace_id,
    manager_id: row.manager_id,
    agent_id: row.agent_id,
    provider: row.provider,
    external_app_id: row.external_app_id,
    status: row.status,
    last_error: row.last_error,
    connection_id: liveConnectionOf(db(), row)?.id ?? null,
    created_at: row.created_at,
    updated_at: row.updated_at,
  };
}

export function toChannelConnection(row: ChannelConnectionRow): ChannelConnection {
  return {
    id: row.id,
    workspace_id: row.workspace_id,
    provider: row.provider,
    app_id: row.app_id,
    agent_id: appOf(db(), row)?.agent_id ?? null,
    short_id: row.short_id,
    label: row.label,
    handler: row.handler,
    status: row.status,
    team: { id: row.team_id, name: row.team_name, enterprise_id: row.enterprise_id },
    identity: { external_id: row.identity_external_id, handle: row.handle },
    granted_scopes: row.granted_scopes,
    connected_at: row.connected_at,
    last_event_at: row.last_event_at,
    created_at: row.created_at,
    updated_at: row.updated_at,
  };
}

function toChannelMessage(row: ChannelMessageRow): ChannelMessage {
  const author =
    row.author_person_id === null
      ? undefined
      : db().channel_people.find(
          (p) => sameWorkspace(p, row) && p.connection_id === row.connection_id && p.id === row.author_person_id,
        );
  return {
    id: row.id,
    workspace_id: row.workspace_id,
    connection_id: row.connection_id,
    conversation_id: row.conversation_id,
    external_id: row.external_id,
    thread_key: row.thread_key,
    direction: row.direction,
    author: author
      ? {
          id: author.id,
          handle: author.handle,
          display_name: author.display_name,
          is_automated: author.is_automated,
        }
      : null,
    text: row.deleted_at ? "" : row.text,
    mentions: row.mentions,
    run_id: row.run_id,
    tool_call_id: row.tool_call_id,
    agent_hop: row.agent_hop,
    sent_at: row.sent_at,
    edited_at: row.edited_at,
    deleted_at: row.deleted_at,
    created_at: row.created_at,
  };
}

function toAgentChannel({ organization_id: _organization, ...row }: AgentChannelRow): AgentChannel {
  return row;
}

// ---------------------------------------------------------------------------
// Tool levels and settings: the organization's

function levelRowOf(d: Db, organizationId: OrganizationId, provider: ChannelProvider, tool: string) {
  return d.channel_tool_levels.find(
    (l) => l.organization_id === organizationId && l.provider === provider && l.tool === tool,
  );
}

/** The level in force: the one someone set, else Zupflow's default for the tool. */
export function channelLevelInForce(d: Db, organizationId: OrganizationId, provider: ChannelProvider, tool: string) {
  const known = CHANNEL_TOOLS[tool as ChannelToolName];
  return levelRowOf(d, organizationId, provider, tool)?.level ?? known?.level ?? "deny";
}

function toolLevels(organizationId: OrganizationId): ChannelToolLevel[] {
  const d = db();
  return CHANNEL_PROVIDERS.flatMap((provider) =>
    CHANNEL_TOOL_NAMES.map((tool) => {
      const row = levelRowOf(d, organizationId, provider, tool);
      return {
        provider,
        tool,
        level: row?.level ?? CHANNEL_TOOLS[tool].level,
        confirmed: row !== undefined,
        effect: CHANNEL_TOOLS[tool].effect,
        updated_at: row?.updated_at ?? null,
        updated_by: row?.updated_by_user_id ? toUser(row.updated_by_user_id) : null,
      };
    }),
  );
}

function toChannelSettings(organizationId: OrganizationId): ChannelSettings {
  const row = db().channel_settings.find((s) => s.organization_id === organizationId);
  return {
    organization_id: organizationId,
    breaker_runs: row?.breaker_runs ?? CHANNEL_SETTINGS_DEFAULTS.breaker_runs,
    breaker_window_minutes: row?.breaker_window_minutes ?? CHANNEL_SETTINGS_DEFAULTS.breaker_window_minutes,
    breaker_pause_minutes: row?.breaker_pause_minutes ?? CHANNEL_SETTINGS_DEFAULTS.breaker_pause_minutes,
    message_retention_days: row?.message_retention_days ?? CHANNEL_SETTINGS_DEFAULTS.message_retention_days,
    updated_at: row?.updated_at ?? null,
    updated_by: row?.updated_by_user_id ? toUser(row.updated_by_user_id) : null,
  };
}

const settingsInput = z.object({
  breaker_runs: z.number().int("Use a whole number.").min(1, "Use 1 to 200.").max(200, "Use 1 to 200."),
  breaker_window_minutes: z.number().int("Use a whole number.").min(1, "Use 1 to 1,440.").max(1440, "Use 1 to 1,440."),
  breaker_pause_minutes: z.number().int("Use a whole number.").min(1, "Use 1 to 1,440.").max(1440, "Use 1 to 1,440."),
  message_retention_days: z.number().int("Use a whole number.").min(1, "Use 1 to 365.").max(365, "Use 1 to 365."),
});

const levelsInput = z.object({
  provider: z.enum(CHANNEL_PROVIDERS, "Choose a provider."),
  levels: z
    .array(z.object({ tool: z.string(), level: ToolLevel }))
    .min(1, "Set at least one level.")
    .max(CHANNEL_TOOL_NAMES.length, `Set ${CHANNEL_TOOL_NAMES.length} levels or fewer.`),
});

// ---------------------------------------------------------------------------
// Agent channels

const agentChannelInput = z.object({
  tools: z.array(z.string()),
  approval_required: z.array(z.string()),
  audience: z.enum(["origin", "listed", "anywhere"], "Choose origin, listed or anywhere."),
  audience_conversations: z.array(z.string()),
  persona: z
    .object({
      name: z.string().trim().min(1, "Required.").max(80, "Use 80 characters or fewer."),
      avatar_url: z
        .string()
        .regex(/^https:\/\/\S+$/, "Use an https:// URL.")
        .nullable(),
    })
    .nullable(),
  answers_dms: z.boolean(),
  answers_mentions: z.boolean(),
  thread_follow_up: z.enum(["off", "when_last_addressed", "always"], "Choose off, when_last_addressed or always."),
  agent_messages: z.enum(["ignore", "mentions"], "Choose ignore or mentions."),
  max_agent_hops: z.number().int("Use a whole number.").min(1, "Use 1 to 5.").max(5, "Use 1 to 5."),
  mid_run: z.enum(["steer", "collect"], "Choose steer or collect."),
  senders: z.enum(["anyone", "listed"], "Choose anyone or listed."),
  sender_list: z
    .array(z.email("Enter an email address.").max(320, "Use 320 characters or fewer."))
    .max(500, "List 500 addresses or fewer. To let more people in, use anyone."),
  allow_automated: z.boolean(),
  allow_external: z.boolean(),
  reply_with_output: z.boolean(),
});

/** The rules an agent channel's settings hold to, beyond their shape. */
function checkAgentChannel(
  d: Db,
  access: WorkspaceAccess,
  connection: ChannelConnectionRow,
  input: z.infer<typeof agentChannelInput>,
): PutAgentChannelInput {
  const refuse = (code: string, message: string, field: string): never => {
    throw new ApiFailure(422, code, message, { [field]: message });
  };
  const tools = [...new Set(input.tools)];
  const approval = [...new Set(input.approval_required)];
  for (const tool of tools) {
    if (!(tool in CHANNEL_TOOLS)) {
      refuse("unknown_tool", `${tool} isn't a channel tool. Use the names channel tool levels list.`, "tools");
    }
  }
  const outside = approval.find((t) => !tools.includes(t));
  if (outside) refuse("approval_outside_grant", `${outside} requires approval but isn't granted.`, "approval_required");
  const denied = tools.find((t) => channelLevelInForce(d, access.org.id, connection.provider, t) === "deny");
  if (denied) refuse("tool_denied", `Your organization blocks ${denied}. Remove it from the channel's tools.`, "tools");
  const unapproved = tools.find(
    (t) => channelLevelInForce(d, access.org.id, connection.provider, t) === "approve" && !approval.includes(t),
  );
  if (unapproved) {
    refuse(
      "approval_required_by_policy",
      `Your organization requires approval for ${unapproved}. Add it to approval required.`,
      "approval_required",
    );
  }
  const senders = input.sender_list.map((email) => email.trim());
  if (input.senders === "listed" && senders.length === 0) {
    refuse("sender_list_required", "Add the email addresses of the people who may talk to the agent.", "sender_list");
  }
  const seen = new Set<string>();
  for (const email of senders) {
    const key = email.toLowerCase();
    if (seen.has(key)) refuse("duplicate_sender", `${email} is listed twice. List each address once.`, "sender_list");
    seen.add(key);
  }
  const conversations = [...new Set(input.audience_conversations)];
  for (const id of conversations) {
    const known = d.channel_conversations.some(
      (c) => inWorkspace(c, access.workspace) && c.connection_id === connection.id && c.id === id,
    );
    if (!known) {
      refuse(
        "unknown_conversation",
        "One of the listed conversations isn't one this connection has seen. Pick from its conversations.",
        "audience_conversations",
      );
    }
  }
  return {
    ...input,
    tools,
    approval_required: approval,
    audience_conversations: conversations as PutAgentChannelInput["audience_conversations"],
    sender_list: senders,
  };
}

// ---------------------------------------------------------------------------
// Transitions shared by routes and hosted Connect

function setAppStatus(app: ChannelAppRow, status: ChannelAppStatus, at: string) {
  if (app.status === status) return;
  app.status = status;
  app.updated_at = at;
  announceLifecycle(
    app.organization_id,
    app.workspace_id,
    {
      type: "channel_app.status_changed",
      data: { object: "channel_app", id: app.id, agent_id: app.agent_id, status },
    },
    at,
  );
}

function setConnectionStatus(connection: ChannelConnectionRow, status: ChannelConnectionStatus, at: string) {
  if (connection.status === status) return;
  connection.status = status;
  connection.updated_at = at;
  announceLifecycle(
    connection.organization_id,
    connection.workspace_id,
    {
      type: "channel_connection.status_changed",
      data: { object: "channel_connection", id: connection.id, app_id: connection.app_id, status },
    },
    at,
  );
}

function setManagerStatus(manager: ChannelAppManagerRow, status: ChannelAppManagerRow["status"], at: string) {
  if (manager.status === status) return;
  manager.status = status;
  announceLifecycle(
    manager.organization_id,
    manager.workspace_id,
    {
      type: "channel_app_manager.status_changed",
      data: { object: "channel_app_manager", id: manager.id, status },
    },
    at,
  );
}

/** Rows that belong to one connection, under its full scope. */
const ofConnection = (connection: ChannelConnectionRow) => (row: Scoped & { connection_id: string }) =>
  sameWorkspace(row, connection) && row.connection_id === connection.id;

/** Removes a connection and everything it holds. */
function removeConnection(d: Db, connection: ChannelConnectionRow) {
  const mine = ofConnection(connection);
  d.agent_channels = d.agent_channels.filter((r) => !mine(r));
  d.channel_messages = d.channel_messages.filter((r) => !mine(r));
  d.channel_people = d.channel_people.filter((r) => !mine(r));
  d.channel_conversations = d.channel_conversations.filter((r) => !mine(r));
  d.channel_connections = d.channel_connections.filter((c) => c !== connection);
}

/**
 * Deletes an app: the fake provider deletes it at once. Its live connection
 * is disconnected (its history stays readable) and the agent's channel on it
 * removed.
 */
function deleteApp(d: Db, app: ChannelAppRow, at: string) {
  const connection = liveConnectionOf(d, app);
  if (connection) {
    setConnectionStatus(connection, "disconnected", at);
    d.agent_channels = d.agent_channels.filter((r) => !ofConnection(connection)(r));
  }
  setAppStatus(app, "deleted", at);
}

/** An agent's apps go with it. */
export function deleteAgentChannels(d: Db, agent: AgentRow, at: string) {
  for (const app of d.channel_apps) {
    if (sameWorkspace(app, agent) && app.agent_id === agent.id && app.status !== "deleted") deleteApp(d, app, at);
  }
  d.agent_channels = d.agent_channels.filter((r) => !(sameWorkspace(r, agent) && r.agent_id === agent.id));
}

/** Everything a workspace's channels hold goes with the workspace. */
export function deleteWorkspaceChannels(d: Db, workspace: { id: WorkspaceId; organization_id: OrganizationId }) {
  const here = (row: Scoped) => inWorkspace(row, workspace);
  d.agent_channels = d.agent_channels.filter((r) => !here(r));
  d.channel_messages = d.channel_messages.filter((r) => !here(r));
  d.channel_people = d.channel_people.filter((r) => !here(r));
  d.channel_conversations = d.channel_conversations.filter((r) => !here(r));
  d.channel_connections = d.channel_connections.filter((r) => !here(r));
  d.channel_inbound_events = d.channel_inbound_events.filter((r) => !here(r));
  d.channel_apps = d.channel_apps.filter((r) => !here(r));
  d.channel_app_managers = d.channel_app_managers.filter((r) => !here(r));
}

/** The fake provider makes the app at once: the job's work, after the `202` was written. */
function provisionApp(app: ChannelAppRow, at: string) {
  // A provider this deployment doesn't reach makes nothing: the job fails it.
  if (!db().channel_providers.includes(app.provider)) {
    app.last_error = "provisioner_unavailable";
    setAppStatus(app, "failed", at);
    return;
  }
  app.external_app_id = `A0${upper(app.id, 9)}`;
  setAppStatus(app, "awaiting_install", at);
}

/**
 * Installs an app in its manager's team: the connection (new, or the same
 * one again on a reinstall), and the agent's channel on it with the
 * defaults. The manager learns its team from the first install.
 */
function installApp(d: Db, app: ChannelAppRow, label: string, at: string): ChannelConnectionRow {
  const manager = managerOf(d, app);
  const workspace = d.workspaces.find((w) => w.organization_id === app.organization_id && w.id === app.workspace_id);
  const agent = d.agents.find((a) => sameWorkspace(a, app) && a.id === app.agent_id);
  if (!manager || !workspace || !agent) throw notFound("Channel app");
  const team = fakeTeam(manager, workspace.name);
  manager.team_id = team.id;
  manager.team_name = team.name;
  let connection = liveConnectionOf(d, app);
  if (connection) {
    connection.connected_at = at;
    connection.granted_scopes = [...FAKE_SCOPES];
    connection.updated_at = at;
    setConnectionStatus(connection, "connected", at);
  } else {
    connection = {
      id: newId("channel_connection"),
      organization_id: app.organization_id,
      workspace_id: app.workspace_id,
      provider: app.provider,
      app_id: app.id,
      short_id: issueShortId(d.installation_short_ids, app),
      label: label.slice(0, 40) || agent.name.slice(0, 40),
      handler: "mention",
      status: "connected",
      team_id: team.id,
      team_name: team.name,
      enterprise_id: null,
      identity_external_id: `U${upper(app.id, 10)}`,
      handle:
        agent.name
          .toLowerCase()
          .replace(/[^a-z0-9]+/g, "-")
          .replace(/^-|-$/g, "") || "agent",
      granted_scopes: [...FAKE_SCOPES],
      connected_at: at,
      last_event_at: null,
      created_at: at,
      updated_at: at,
    };
    d.channel_connections.unshift(connection);
    announceLifecycle(
      app.organization_id,
      app.workspace_id,
      {
        type: "channel_connection.status_changed",
        data: { object: "channel_connection", id: connection.id, app_id: app.id, status: "connected" },
      },
      at,
    );
  }
  const live = connection;
  if (!d.agent_channels.some((r) => ofConnection(live)(r))) {
    d.agent_channels.push({
      id: newId("agent_channel"),
      organization_id: app.organization_id,
      workspace_id: app.workspace_id,
      agent_id: app.agent_id,
      connection_id: live.id,
      ...agentChannelDefaults(),
      created_at: at,
      updated_at: at,
    });
  }
  setAppStatus(app, "installed", at);
  return live;
}

// ---------------------------------------------------------------------------
// Channel Connect sessions

const targetInput = z.object({
  target: z.discriminatedUnion(
    "kind",
    [
      z.object({ kind: z.literal("channel_app_manager"), provider: z.enum(CHANNEL_PROVIDERS, "Choose a provider.") }),
      z.object({ kind: z.literal("channel_app"), app: z.string() }),
    ],
    "Choose channel_app_manager or channel_app.",
  ),
  return_url: z.string().url("Enter a full URL.").nullable(),
});

/**
 * What a new channel session sets up, checked within the workspace: an app
 * to install must be waiting for an install, or installed (a reinstall), and
 * its manager active. Returns the session's target and label.
 */
export function channelSessionTarget(access: WorkspaceAccess, json: unknown) {
  const input = validated(json, targetInput);
  if (input.target.kind === "channel_app_manager") {
    const target: ConnectTarget = { kind: "channel_app_manager", provider: input.target.provider, manager: null };
    return { target, label: CHANNEL_PROVIDER_NAMES[input.target.provider], return_url: input.return_url };
  }
  const d = db();
  const app = findApp(access, input.target.app);
  if (app.status !== "awaiting_install" && app.status !== "installed") {
    throw new ApiFailure(
      409,
      "channel_app_not_ready",
      "This app isn't ready to install. Wait until it's made, or make it again.",
    );
  }
  if (managerOf(d, app)?.status !== "active") {
    throw new ApiFailure(
      409,
      "channel_app_manager_inactive",
      "The team's configuration token has expired or was removed. Paste a new one, then try again.",
    );
  }
  const agent = findAgent(access, app.agent_id);
  const target: ConnectTarget = { kind: "channel_app", provider: app.provider, app: app.id, connection: null };
  return { target, label: agent.name.slice(0, 40), return_url: input.return_url };
}

function assertChannelSessionPending(session: ConnectSessionRow) {
  const live = withExpiry(session.organization_id, session.workspace_id, session);
  if (live.status !== "pending") {
    throw new ApiFailure(
      409,
      "session_not_pending",
      live.status === "expired"
        ? "This Connect link has expired. Start again from the app."
        : "This Connect session was already used. Start again from the app.",
    );
  }
}

function completeChannelSession(session: ConnectSessionRow, target: ConnectTarget, at: string, name: string) {
  session.target = target;
  session.status = "completed";
  session.completed_at = at;
  announceSession(session.organization_id, session.workspace_id, session, at);
  db().audit.unshift({
    id: newId("audit_event"),
    organization_id: session.organization_id,
    workspace_id: session.workspace_id,
    environment: session.environment,
    actor: { type: "system", id: null, name: "Connect" },
    action: target.kind === "channel_app_manager" ? "channel_app_manager.created" : "channel_connection.connected",
    target: {
      type: target.kind === "channel_app_manager" ? "channel_app_manager" : "channel_connection",
      id: target.kind === "channel_app_manager" ? target.manager : target.connection,
      name,
    },
    created_at: at,
  });
}

/** The app a channel session installs, in the session's own scope, still installable. */
function sessionApp(session: ConnectSessionRow) {
  if (session.target?.kind !== "channel_app") throw new Error("Not an app's channel session");
  const d = db();
  const appId = session.target.app;
  const app = d.channel_apps.find((a) => sameWorkspace(a, session) && a.id === appId && a.status !== "deleted");
  if (
    !app ||
    (app.status !== "awaiting_install" && app.status !== "installed") ||
    managerOf(d, app)?.status !== "active"
  ) {
    throw new ApiFailure(422, "app_not_eligible", "This app can't be installed any more. Start again from the agent.");
  }
  return app;
}

/** A pasted configuration token: links the team, and the session is done. */
export async function submitChannelCredentials(
  session: ConnectSessionRow,
  request: Request,
): Promise<ChannelAppManagerConnectResult> {
  assertChannelSessionPending(session);
  if (session.target?.kind !== "channel_app_manager") {
    throw new ApiFailure(
      422,
      "wrong_auth_type",
      `This agent's app is installed by allowing it in ${CHANNEL_PROVIDER_NAMES[session.target?.provider ?? "slack"]}, not with tokens.`,
    );
  }
  const input = await body(request, z.object({ values: tokensInput }));
  checkTokens(session.target.provider, input.values);
  const at = new Date().toISOString();
  const manager = newManager(
    { organization_id: session.organization_id, workspace_id: session.workspace_id },
    session.target.provider,
    input.values.configuration_token,
    null,
    at,
  );
  completeChannelSession(
    session,
    { ...session.target, manager: manager.id },
    at,
    CHANNEL_PROVIDER_NAMES[session.target.provider],
  );
  return { session: toConnectSessionOf(session), channel_app_manager: toChannelAppManager(manager) };
}

/** Where the person allows the app: the provider's page, simulated by the mock. */
export function authorizeChannel(session: ConnectSessionRow, reference: string) {
  assertChannelSessionPending(session);
  if (session.target?.kind !== "channel_app") {
    throw new ApiFailure(422, "wrong_auth_type", "Linking a team takes a configuration token, pasted on this page.");
  }
  sessionApp(session);
  return {
    authorization_url: `${window.location.origin}/connect/${encodeURIComponent(reference)}/simulated-consent`,
  };
}

/** The person allowed the app (or didn't) on the provider's page. */
export function consentChannel(
  session: ConnectSessionRow,
  decision: "approved" | "denied",
): ChannelConnectionConnectResult {
  assertChannelSessionPending(session);
  if (session.target?.kind !== "channel_app") {
    throw new ApiFailure(422, "wrong_auth_type", "Linking a team takes a configuration token, pasted on this page.");
  }
  const app = sessionApp(session);
  const at = new Date().toISOString();
  if (decision === "denied") {
    session.status = "failed";
    session.failure_code = "consent_denied";
    announceSession(session.organization_id, session.workspace_id, session, at);
    throw new ApiFailure(422, "consent_denied", "The app wasn't allowed. Nothing was connected.");
  }
  const d = db();
  const connection = installApp(d, app, session.label, at);
  completeChannelSession(session, { ...session.target, connection: connection.id }, at, connection.label);
  return { session: toConnectSessionOf(session), channel_connection: toChannelConnection(connection) };
}

// Set by `createChannelHandlers`, so this module needs nothing from the session serializer's module.
let toConnectSessionOf: (session: ConnectSessionRow) => ChannelAppManagerConnectResult["session"] = () => {
  throw new Error("Channel handlers aren't set up");
};

function newManager(
  scope: Scoped,
  provider: ChannelProvider,
  configurationToken: string,
  createdBy: ChannelAppManagerRow["created_by_user_id"],
  at: string,
): ChannelAppManagerRow {
  const row: ChannelAppManagerRow = {
    id: newId("channel_app_manager"),
    organization_id: scope.organization_id,
    workspace_id: scope.workspace_id,
    provider,
    method: "configuration_token",
    team_id: null,
    team_name: null,
    status: "active",
    token_last4: configurationToken.slice(-4),
    token_updated_at: at,
    credential_version: 1,
    token_expires_at: new Date(Date.parse(at) + TOKEN_LIFETIME_MS).toISOString(),
    created_by_user_id: createdBy,
    created_at: at,
    updated_at: at,
  };
  db().channel_app_managers.unshift(row);
  // Linked: announced as active, so a backend learns of a team linked through hosted Connect without polling.
  announceLifecycle(
    row.organization_id,
    row.workspace_id,
    {
      type: "channel_app_manager.status_changed",
      data: { object: "channel_app_manager", id: row.id, status: "active" },
    },
    at,
  );
  return row;
}

// ---------------------------------------------------------------------------
// Inbound

/**
 * Cross-tenant by design: a provider's event names only the app's inbound
 * reference, which is how it finds its organization and workspace. Reads
 * exactly one app; everything after is scoped to it.
 */
function findAppByInboundRefCrossTenant(ref: string): ChannelAppRow | undefined {
  return ref.length === 32 ? db().channel_apps.find((a) => a.inbound_ref === ref && a.status !== "deleted") : undefined;
}

async function hmacHex(secret: string, value: string): Promise<string> {
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const mac = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(value));
  return Array.from(new Uint8Array(mac), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

/** Slack's request signature: `v0=` and the hex HMAC-SHA256 of `v0:{timestamp}:{body}` under the signing secret. */
export async function slackSignature(secret: string, timestamp: string, rawBody: string): Promise<string> {
  return `v0=${await hmacHex(secret, `v0:${timestamp}:${rawBody}`)}`;
}

function constantTimeEqual(a: string, b: string) {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

const unauthenticatedEvent = () =>
  new ApiFailure(
    401,
    "invalid_signature",
    "The request isn't signed by the app it was sent to. Sign it with that app's signing secret.",
  );

/** Signs against for an unknown reference, so it costs what a known one does and answers the same. */
const UNKNOWN_APP_SECRET = "zupflow-unknown-inbound-reference";

const SLACK_KINDS: Record<string, ConversationKind> = {
  channel: "channel",
  group: "private_channel",
  im: "dm",
  mpim: "group_dm",
};

const inboundMessage = z.object({
  type: z.enum(["message", "app_mention"]),
  subtype: z.string().optional(),
  channel: z.string().min(1).max(100),
  channel_type: z.string().optional(),
  user: z.string().min(1).max(100).optional(),
  bot_id: z.string().optional(),
  text: z.string().max(40_000).default(""),
  ts: z.string().min(1).max(100),
  thread_ts: z.string().max(100).optional(),
});

/** Saves the conversation, the person and the message, once: a message arrives as `message` and `app_mention` both. */
function receiveMessage(d: Db, connection: ChannelConnectionRow, raw: unknown, at: string) {
  const parsed = inboundMessage.safeParse(raw);
  // Edits, deletes and other subtypes are the real provider's to normalize; the fake ignores them.
  if (!parsed.success || (parsed.data.subtype !== undefined && parsed.data.subtype !== "bot_message")) return;
  const event = parsed.data;
  const mine = ofConnection(connection);
  let conversation = d.channel_conversations.find((c) => mine(c) && c.external_id === event.channel);
  const kind = SLACK_KINDS[event.channel_type ?? ""] ?? (event.channel.startsWith("D") ? "dm" : "channel");
  if (!conversation) {
    conversation = {
      id: newId("channel_conversation"),
      organization_id: connection.organization_id,
      workspace_id: connection.workspace_id,
      connection_id: connection.id,
      external_id: event.channel,
      kind,
      name: null,
      is_external: false,
      identity_is_member: true,
      last_message_at: null,
      paused_until: null,
      created_at: at,
      updated_at: at,
    } satisfies ChannelConversationRow;
    d.channel_conversations.push(conversation);
  }
  const live = conversation;
  if (d.channel_messages.some((m) => mine(m) && m.conversation_id === live.id && m.external_id === event.ts)) return;
  const own = event.user === connection.identity_external_id;
  let author: ChannelPersonRow | undefined;
  const externalAuthor = event.user ?? event.bot_id;
  if (!own && externalAuthor) {
    author = d.channel_people.find((p) => mine(p) && p.external_id === externalAuthor);
    if (!author) {
      author = {
        id: newId("channel_person"),
        organization_id: connection.organization_id,
        workspace_id: connection.workspace_id,
        connection_id: connection.id,
        external_id: externalAuthor,
        handle: null,
        display_name: null,
        email: null,
        is_automated: event.user === undefined || event.subtype === "bot_message",
        first_seen_at: at,
        updated_at: at,
      };
      d.channel_people.push(author);
    }
  }
  if (!own && !author) return;
  const sentAt = new Date(Math.round(Number.parseFloat(event.ts) * 1000) || Date.parse(at)).toISOString();
  const message: ChannelMessageRow = {
    id: newId("channel_message"),
    organization_id: connection.organization_id,
    workspace_id: connection.workspace_id,
    connection_id: connection.id,
    conversation_id: live.id,
    external_id: event.ts,
    thread_key: event.thread_ts ?? (live.kind === "dm" ? "" : event.ts),
    direction: own ? "outbound" : "inbound",
    author_person_id: own ? null : (author?.id ?? null),
    text: event.text,
    mentions: [...event.text.matchAll(/<@([A-Z0-9]+)(?:\|[^>]*)?>/g)].map((m) => m[1] ?? "").filter(Boolean),
    run_id: null,
    tool_call_id: null,
    agent_hop: 0,
    sent_at: sentAt,
    edited_at: null,
    deleted_at: null,
    created_at: at,
  };
  d.channel_messages.push(message);
  live.last_message_at = sentAt;
  live.updated_at = at;
  announceLifecycle(
    connection.organization_id,
    connection.workspace_id,
    {
      type: message.direction === "inbound" ? "channel_message.received" : "channel_message.sent",
      data: {
        object: "channel_message",
        id: message.id,
        connection_id: connection.id,
        conversation_id: live.id,
      },
    },
    at,
  );
}

const callback = z.object({
  type: z.literal("event_callback"),
  event_id: z.string().min(1).max(200),
  // Slack names the team on every event: one without it is ignored, never read as this connection's.
  team_id: z.string().min(1),
  event: z.looseObject({ type: z.string() }),
});

// ---------------------------------------------------------------------------
// Routes

const page = (url: URL) => ({ cursor: url.searchParams.get("cursor"), limit: clampedInt(url, "limit", 25, 100) });
const newestFirst = <T extends { created_at: string; id: string }>(a: T, b: T) =>
  b.created_at.localeCompare(a.created_at) || b.id.localeCompare(a.id);

export function createChannelHandlers(
  u: (path: string) => string,
  serializeSession: (session: ConnectSessionRow) => ChannelAppManagerConnectResult["session"],
) {
  toConnectSessionOf = serializeSession;
  const now = () => new Date().toISOString();
  const where = (access: WorkspaceAccess) => ({ workspace: access.workspace });

  return [
    // --- Providers -------------------------------------------------------------
    http.get(
      u("/v1/workspaces/:ws/channel-providers"),
      route(({ request, params }) => {
        workspaceAccess(request, param(params, "ws"));
        return ok(availableProviders());
      }),
    ),

    // --- App managers ---------------------------------------------------------
    http.get(
      u("/v1/workspaces/:ws/channel-app-managers"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const url = new URL(request.url);
        const { cursor, limit } = page(url);
        const rows = db()
          .channel_app_managers.filter((m) => inWorkspace(m, access.workspace))
          .sort(newestFirst);
        const { page: items, meta } = paginate(rows, cursor, limit);
        return ok(items.map(toChannelAppManager), { meta });
      }),
    ),

    http.post(
      u("/v1/workspaces/:ws/channel-app-managers"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "link teams");
        assertConsoleMayAct(access, "link teams");
        const input = await body(request, managerInput);
        checkTokens(input.provider, input);
        const row = newManager(
          { organization_id: access.org.id, workspace_id: access.workspace.id },
          input.provider,
          input.configuration_token,
          access.kind === "console" ? access.user.id : null,
          now(),
        );
        audit(
          access,
          "channel_app_manager.created",
          { type: "channel_app_manager", id: row.id, name: CHANNEL_PROVIDER_NAMES[row.provider] },
          where(access),
        );
        return created(toChannelAppManager(row));
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/channel-app-managers/:manager"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        return ok(toChannelAppManager(findManager(access, params)));
      }),
    ),

    http.patch(
      u("/v1/workspaces/:ws/channel-app-managers/:manager"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "replace a team's tokens");
        assertConsoleMayAct(access, "replace a team's tokens");
        const row = findManager(access, params);
        const input = await body(request, tokensInput);
        checkTokens(row.provider, input);
        const at = now();
        row.token_last4 = input.configuration_token.slice(-4);
        row.token_updated_at = at;
        row.token_expires_at = new Date(Date.parse(at) + TOKEN_LIFETIME_MS).toISOString();
        row.credential_version += 1;
        row.updated_at = at;
        setManagerStatus(row, "active", at);
        audit(
          access,
          "channel_app_manager.updated",
          { type: "channel_app_manager", id: row.id, name: row.team_name ?? CHANNEL_PROVIDER_NAMES[row.provider] },
          where(access),
        );
        return ok(toChannelAppManager(row));
      }),
    ),

    http.delete(
      u("/v1/workspaces/:ws/channel-app-managers/:manager"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "unlink teams");
        const row = findManager(access, params);
        const d = db();
        const apps = d.channel_apps.filter(
          (a) => sameWorkspace(a, row) && a.manager_id === row.id && a.status !== "deleted",
        );
        if (apps.length > 0) {
          throw new ApiFailure(
            409,
            "in_use",
            `This team still has ${apps.length === 1 ? "an agent's app" : `${apps.length} agents' apps`}. Remove ${apps.length === 1 ? "it" : "them"} from the agents first.`,
          );
        }
        d.channel_app_managers = d.channel_app_managers.filter((m) => m !== row);
        audit(
          access,
          "channel_app_manager.deleted",
          { type: "channel_app_manager", id: row.id, name: row.team_name ?? CHANNEL_PROVIDER_NAMES[row.provider] },
          where(access),
        );
        return noContent();
      }),
    ),

    // --- Apps -------------------------------------------------------------------
    http.get(
      u("/v1/workspaces/:ws/channel-apps"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const url = new URL(request.url);
        const agentId = url.searchParams.get("agent_id");
        const managerId = url.searchParams.get("manager_id");
        const { cursor, limit } = page(url);
        const rows = db()
          .channel_apps.filter(
            (a) =>
              inWorkspace(a, access.workspace) &&
              a.status !== "deleted" &&
              (!agentId || a.agent_id === agentId) &&
              (!managerId || a.manager_id === managerId),
          )
          .sort(newestFirst);
        const { page: items, meta } = paginate(rows, cursor, limit);
        return ok(items.map(toChannelApp), { meta });
      }),
    ),

    http.post(
      u("/v1/workspaces/:ws/channel-apps"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "add agents to teams");
        assertConsoleMayAct(access, "add agents to teams");
        const input = await body(request, z.object({ agent_id: z.string(), manager_id: z.string() }));
        const d = db();
        const agent = findAgent(access, input.agent_id);
        const manager = d.channel_app_managers.find(
          (m) => inWorkspace(m, access.workspace) && m.id === input.manager_id,
        );
        if (!manager) throw notFound("Channel app manager");
        if (manager.status !== "active") {
          throw new ApiFailure(
            409,
            "channel_app_manager_inactive",
            "The team's configuration token has expired or was removed. Paste a new one, then try again.",
          );
        }
        const live = d.channel_apps.some(
          (a) =>
            sameWorkspace(a, manager) &&
            a.manager_id === manager.id &&
            a.agent_id === agent.id &&
            a.status !== "deleted" &&
            a.status !== "failed",
        );
        if (live) {
          throw new ApiFailure(409, "channel_app_exists", `${agent.name} already has an app in this team.`);
        }
        const at = now();
        const row: ChannelAppRow = {
          id: newId("channel_app"),
          organization_id: access.org.id,
          workspace_id: access.workspace.id,
          manager_id: manager.id,
          agent_id: agent.id as AgentId,
          provider: manager.provider,
          external_app_id: null,
          inbound_ref: newInboundRef(),
          signing_secret: token(32),
          status: "creating",
          last_error: null,
          created_at: at,
          updated_at: at,
        };
        d.channel_apps.unshift(row);
        audit(access, "channel_app.created", { type: "channel_app", id: row.id, name: agent.name }, where(access));
        // Accepted as `creating`; then the fake provider's job makes it at once.
        const response = ok(toChannelApp(row), { status: 202 });
        provisionApp(row, at);
        return response;
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/channel-apps/:app"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        return ok(toChannelApp(findApp(access, pathId(params, "app", "channel_app", "Channel app"))));
      }),
    ),

    http.delete(
      u("/v1/workspaces/:ws/channel-apps/:app"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "remove agents from teams");
        const row = findApp(access, pathId(params, "app", "channel_app", "Channel app"));
        const agent = db().agents.find((a) => sameWorkspace(a, row) && a.id === row.agent_id);
        deleteApp(db(), row, now());
        audit(
          access,
          "channel_app.deleted",
          { type: "channel_app", id: row.id, name: agent?.name ?? "Deleted agent" },
          where(access),
        );
        return noContent();
      }),
    ),

    // --- Connections --------------------------------------------------------------
    http.get(
      u("/v1/workspaces/:ws/channel-connections"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const url = new URL(request.url);
        const agentId = url.searchParams.get("agent_id");
        const { cursor, limit } = page(url);
        const d = db();
        const rows = d.channel_connections
          .filter((c) => inWorkspace(c, access.workspace) && (!agentId || appOf(d, c)?.agent_id === agentId))
          .sort(newestFirst);
        const { page: items, meta } = paginate(rows, cursor, limit);
        return ok(items.map(toChannelConnection), { meta });
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/channel-connections/:channel_connection"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        return ok(toChannelConnection(findConnection(access, params)));
      }),
    ),

    http.patch(
      u("/v1/workspaces/:ws/channel-connections/:channel_connection"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "change channel connections");
        const row = findConnection(access, params);
        const input = await body(
          request,
          z.object({
            label: z.string().trim().min(1, "Required.").max(40, "Use 40 characters or fewer.").optional(),
            handler: z.enum(["mention"], "Choose mention.").optional(),
          }),
        );
        if (input.label !== undefined) row.label = input.label;
        if (input.handler !== undefined) row.handler = input.handler;
        row.updated_at = now();
        audit(
          access,
          "channel_connection.updated",
          { type: "channel_connection", id: row.id, name: row.label },
          where(access),
        );
        return ok(toChannelConnection(row));
      }),
    ),

    http.delete(
      u("/v1/workspaces/:ws/channel-connections/:channel_connection"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "disconnect channels");
        const row = findConnection(access, params);
        const d = db();
        const at = now();
        setConnectionStatus(row, "disconnected", at);
        const app = appOf(d, row);
        if (app && app.status === "installed") setAppStatus(app, "awaiting_install", at);
        removeConnection(d, row);
        audit(
          access,
          "channel_connection.deleted",
          { type: "channel_connection", id: row.id, name: row.label },
          where(access),
        );
        return noContent();
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/channel-connections/:channel_connection/conversations"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const connection = findConnection(access, params);
        const url = new URL(request.url);
        const q = url.searchParams.get("q")?.trim().toLowerCase() ?? "";
        const kind = url.searchParams.get("kind");
        if (kind !== null && !ChannelConversationKind.safeParse(kind).success) {
          const message = "That isn't a kind of conversation. Use channel, private_channel, dm or group_dm.";
          throw new ApiFailure(400, "validation_failed", message, { kind: message });
        }
        const { cursor, limit } = page(url);
        const rows = db()
          .channel_conversations.filter(
            (c) =>
              ofConnection(connection)(c) &&
              (!kind || c.kind === kind) &&
              (!q || (c.name ?? "").toLowerCase().includes(q)),
          )
          .sort((a, b) => (b.last_message_at ?? "").localeCompare(a.last_message_at ?? "") || b.id.localeCompare(a.id));
        const { page: items, meta } = paginate(rows, cursor, limit);
        return ok(
          items.map(({ organization_id: _o, updated_at: _u, ...c }) => c),
          { meta },
        );
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/channel-connections/:channel_connection/conversations/:conversation/messages"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const connection = findConnection(access, params);
        const conversationId = pathId(params, "conversation", "channel_conversation", "Conversation");
        const conversation = db().channel_conversations.find(
          (c) => ofConnection(connection)(c) && c.id === conversationId,
        );
        if (!conversation) throw notFound("Conversation");
        const url = new URL(request.url);
        const thread = url.searchParams.get("thread");
        const { cursor, limit } = page(url);
        const rows = db()
          .channel_messages.filter(
            (m) =>
              ofConnection(connection)(m) &&
              m.conversation_id === conversation.id &&
              (thread === null || m.thread_key === thread),
          )
          .sort((a, b) => b.sent_at.localeCompare(a.sent_at) || b.id.localeCompare(a.id));
        const { page: items, meta } = paginate(rows, cursor, limit);
        return ok(items.map(toChannelMessage), { meta });
      }),
    ),

    // --- Agent channels -------------------------------------------------------
    http.get(
      u("/v1/workspaces/:ws/agents/:agent/channels"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const agent = findAgent(access, pathId(params, "agent", "agent", "Agent"));
        return ok(
          db()
            .agent_channels.filter((r) => sameWorkspace(r, agent) && r.agent_id === agent.id)
            .sort((a, b) => a.created_at.localeCompare(b.created_at) || a.id.localeCompare(b.id))
            .map(toAgentChannel),
        );
      }),
    ),

    http.put(
      u("/v1/workspaces/:ws/agents/:agent/channels/:channel_connection"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "change agents' channels");
        const agent = findAgent(access, pathId(params, "agent", "agent", "Agent"));
        const connection = findConnection(access, params);
        const json = await body(request, z.unknown());
        const d = db();
        if (appOf(d, connection)?.agent_id !== agent.id) {
          throw new ApiFailure(
            422,
            "not_agents_app",
            `This connection is another agent's app. Add ${agent.name} to ${CHANNEL_PROVIDER_NAMES[connection.provider]} to give it its own.`,
          );
        }
        if (connection.status === "disconnected") {
          throw new ApiFailure(
            409,
            "channel_connection_disconnected",
            "This connection is disconnected. Install the agent's app again first.",
          );
        }
        const settings = checkAgentChannel(d, access, connection, validated(json, agentChannelInput));
        const at = now();
        let row = d.agent_channels.find((r) => ofConnection(connection)(r) && r.agent_id === agent.id);
        if (row) {
          Object.assign(row, settings, { updated_at: at });
        } else {
          row = {
            id: newId("agent_channel"),
            organization_id: access.org.id,
            workspace_id: access.workspace.id,
            agent_id: agent.id,
            connection_id: connection.id,
            ...settings,
            created_at: at,
            updated_at: at,
          };
          d.agent_channels.push(row);
        }
        audit(
          access,
          "agent_channel.updated",
          { type: "agent_channel", id: row.id, name: `${agent.name} · ${connection.label}` },
          where(access),
        );
        return ok(toAgentChannel(row));
      }),
    ),

    http.delete(
      u("/v1/workspaces/:ws/agents/:agent/channels/:channel_connection"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "change agents' channels");
        const agent = findAgent(access, pathId(params, "agent", "agent", "Agent"));
        const connection = findConnection(access, params);
        const d = db();
        const row = d.agent_channels.find((r) => ofConnection(connection)(r) && r.agent_id === agent.id);
        if (!row) throw notFound("Agent channel");
        d.agent_channels = d.agent_channels.filter((r) => r !== row);
        audit(
          access,
          "agent_channel.deleted",
          { type: "agent_channel", id: row.id, name: `${agent.name} · ${connection.label}` },
          where(access),
        );
        return noContent();
      }),
    ),

    // --- Organization: tool levels and settings ---------------------------------
    http.get(
      u("/v1/organizations/:org/channel-tool-levels"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        return ok(toolLevels(access.org.id));
      }),
    ),

    http.put(
      u("/v1/organizations/:org/channel-tool-levels"),
      route(async ({ request, params }) => {
        const access = consoleOrgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "set channel tool levels");
        const input = await body(request, levelsInput);
        const names = new Set<string>();
        for (const { tool } of input.levels) {
          if (!(tool in CHANNEL_TOOLS)) {
            const message = `${tool} isn't a channel tool. Use the names this list answers with.`;
            throw new ApiFailure(422, "unknown_tool", message, { levels: message });
          }
          if (names.has(tool)) {
            throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", {
              levels: "Name each tool once.",
            });
          }
          names.add(tool);
        }
        const d = db();
        const at = now();
        for (const { tool, level } of input.levels) {
          const row = levelRowOf(d, access.org.id, input.provider, tool);
          if (row) Object.assign(row, { level, updated_at: at, updated_by_user_id: access.user.id });
          else {
            d.channel_tool_levels.push({
              organization_id: access.org.id,
              provider: input.provider,
              tool,
              level,
              updated_at: at,
              updated_by_user_id: access.user.id,
            });
          }
        }
        audit(access, "channel_tool_levels.updated", {
          type: "channel_tool_levels",
          id: null,
          name: `${CHANNEL_PROVIDER_NAMES[input.provider]} tools`,
        });
        return ok(toolLevels(access.org.id));
      }),
    ),

    http.get(
      u("/v1/organizations/:org/channel-settings"),
      route(({ request, params }) => {
        const access: OrgAccess = orgAccess(request, param(params, "org"));
        return ok(toChannelSettings(access.org.id));
      }),
    ),

    http.put(
      u("/v1/organizations/:org/channel-settings"),
      route(async ({ request, params }) => {
        const access = consoleOrgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "change channel settings");
        const input = await body(request, settingsInput);
        const d = db();
        const at = now();
        const row = d.channel_settings.find((s) => s.organization_id === access.org.id);
        if (row) Object.assign(row, input, { updated_at: at, updated_by_user_id: access.user.id });
        else
          d.channel_settings.push({
            organization_id: access.org.id,
            ...input,
            updated_at: at,
            updated_by_user_id: access.user.id,
          });
        audit(access, "channel_settings.updated", { type: "channel_settings", id: null, name: "Channel settings" });
        return ok(toChannelSettings(access.org.id));
      }),
    ),

    // --- Inbound: the provider, authorized by its signature ---------------------
    http.post(
      u("/channels/inbound/:inbound_ref"),
      route(async ({ request, params }) => {
        const raw = await request.text();
        const timestamp = request.headers.get("X-Slack-Request-Timestamp") ?? "";
        const signature = request.headers.get("X-Slack-Signature") ?? "";
        // Refused before anything is read or stored, and alike whether the reference names an app: the
        // timestamp first, before any lookup, then an unknown app exactly as a bad signature.
        const seconds = Number(timestamp);
        if (!/^\d{1,12}$/.test(timestamp) || Math.abs(Date.now() / 1000 - seconds) > INBOUND_TOLERANCE_SECONDS) {
          throw new ApiFailure(
            401,
            "stale_request",
            "The request's timestamp is missing or more than five minutes off. Sign it with the current time.",
          );
        }
        const app = findAppByInboundRefCrossTenant(param(params, "inbound_ref"));
        const expected = await slackSignature(app?.signing_secret ?? UNKNOWN_APP_SECRET, timestamp, raw);
        if (!app || !constantTimeEqual(signature, expected)) throw unauthenticatedEvent();
        let json: unknown;
        try {
          json = JSON.parse(raw);
        } catch {
          throw new ApiFailure(400, "invalid_json", "The request body is not valid JSON.");
        }
        const kind = z.looseObject({ type: z.string(), challenge: z.string().optional() }).safeParse(json);
        if (!kind.success) {
          throw new ApiFailure(
            400,
            "validation_failed",
            "The request isn't a provider event: it needs a type. Send events as the provider sends them.",
          );
        }
        if (kind.data.type === "url_verification") return providerAnswer({ challenge: kind.data.challenge ?? "" });
        const delivery = callback.safeParse(json);
        if (!delivery.success) return providerAnswer({ challenge: null });
        const d = db();
        const at = now();
        // Everything from here is scoped to the app the event was delivered to.
        const seen = d.channel_inbound_events.some(
          (e) => sameWorkspace(e, app) && e.app_id === app.id && e.provider_event_id === delivery.data.event_id,
        );
        if (seen) return providerAnswer({ challenge: null });
        d.channel_inbound_events.push({
          organization_id: app.organization_id,
          workspace_id: app.workspace_id,
          app_id: app.id as ChannelAppId,
          provider_event_id: delivery.data.event_id,
          received_at: at,
        });
        const connection = liveConnectionOf(d, app);
        if (!connection) return providerAnswer({ challenge: null });
        const event = delivery.data.event;
        if (delivery.data.team_id !== connection.team_id) {
          return providerAnswer({ challenge: null });
        }
        connection.last_event_at = at;
        if (event.type === "app_uninstalled" || event.type === "tokens_revoked") {
          setConnectionStatus(connection, "reconnect_required", at);
        } else if (connection.status === "connected") {
          receiveMessage(d, connection, event, at);
        }
        return providerAnswer({ challenge: null });
      }),
    ),
  ];
}

/** The provider's format: the body alone, no envelope. */
function providerAnswer(data: { challenge: string | null }) {
  return new Response(JSON.stringify(data), { status: 200, headers: { "Content-Type": "application/json" } });
}
