/**
 * Seeded channels: in each workspace named here, a linked team, one agent's
 * installed app with its connection and agent channel, and a conversation
 * with a short thread and a DM. Fixed scenarios and times relative to
 * `nowMs`, so resetting repeats them.
 */
import { agentChannelDefaults, type IdKind, type OrganizationId } from "@zupflow/sdk";
import { FAKE_SCOPES } from "./fake-channel";
import { newId } from "./ids";
import type { AgentRow, ChannelConversationRow, ChannelMessageRow, ChannelPersonRow, Db, WorkspaceRow } from "./model";
import { mulberry32, token } from "./random";
import { issueShortId } from "./scope";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

const BASE32 = "abcdefghijklmnopqrstuvwxyz234567";

interface ChannelSeed {
  organization: string;
  workspace: string;
  /** The agent the app is for, by name; the workspace's first agent when it has none by that name. */
  agent?: string;
  team: { id: string; name: string };
  people: { external_id: string; display_name: string; handle: string; email: string }[];
}

const SEEDS: ChannelSeed[] = [
  {
    organization: "northwind",
    workspace: "acme",
    team: { id: "T0ACME7Q2K", name: "Acme" },
    people: [
      { external_id: "U0ACME01", display_name: "Dana Whitfield", handle: "dana", email: "dana@acme.example" },
      { external_id: "U0ACME02", display_name: "Luis Romero", handle: "luis", email: "luis@acme.example" },
    ],
  },
  {
    organization: "northwind",
    workspace: "beta-labs",
    team: { id: "T0BETA4M8C", name: "Beta Labs" },
    people: [{ external_id: "U0BETA01", display_name: "Ines Park", handle: "ines", email: "ines@betalabs.example" }],
  },
  {
    organization: "northwind",
    workspace: "acme-sandbox",
    team: { id: "T0ACMESBX1", name: "Acme (test)" },
    people: [
      { external_id: "U0ASBX01", display_name: "Jordan Lee", handle: "jordan", email: "jordan@northwind.example" },
    ],
  },
  {
    organization: "larkspur",
    workspace: "harbor",
    team: { id: "T0HARB0R9Z", name: "Harbor" },
    people: [
      { external_id: "U0HARB01", display_name: "Rowan Ellis", handle: "rowan", email: "rowan@larkspur.example" },
    ],
  },
];

export function seedChannels(db: Db, nowMs: number) {
  const rng = mulberry32(20261015);
  const id = <K extends IdKind>(kind: K, agoMs: number) => newId(kind, { rng, at: nowMs - agoMs });
  const ago = (ms: number) => new Date(nowMs - ms).toISOString();
  const ref = () => Array.from({ length: 32 }, () => BASE32[Math.floor(rng() * 32)]).join("");

  for (const spec of SEEDS) {
    const org = db.organizations.find((o) => o.slug === spec.organization);
    const workspace = org && db.workspaces.find((w) => w.organization_id === org.id && w.slug === spec.workspace);
    if (!org || !workspace) continue;
    const agent = agentIn(db, org.id, workspace, spec.agent);
    if (!agent) continue;
    const scope = { organization_id: org.id, workspace_id: workspace.id };
    const linker = db.memberships.find((m) => m.organization_id === org.id && m.role === "owner")?.user_id ?? null;

    const manager = {
      id: id("channel_app_manager", 9 * DAY),
      ...scope,
      provider: "slack" as const,
      method: "configuration_token" as const,
      team_id: spec.team.id,
      team_name: spec.team.name,
      status: "active" as const,
      token_last4: token(4, rng),
      token_updated_at: ago(2 * HOUR),
      credential_version: 18,
      token_expires_at: new Date(nowMs + 10 * HOUR).toISOString(),
      created_by_user_id: linker,
      created_at: ago(9 * DAY),
      updated_at: ago(2 * HOUR),
    };
    db.channel_app_managers.push(manager);

    const app = {
      id: id("channel_app", 9 * DAY - HOUR),
      ...scope,
      manager_id: manager.id,
      agent_id: agent.id,
      provider: "slack" as const,
      external_app_id: `A0${token(9, rng).toUpperCase()}`,
      inbound_ref: ref(),
      signing_secret: token(32, rng),
      status: "installed" as const,
      last_error: null,
      created_at: ago(9 * DAY - HOUR),
      updated_at: ago(9 * DAY - 2 * HOUR),
    };
    db.channel_apps.push(app);

    const connection = {
      id: id("channel_connection", 9 * DAY - 2 * HOUR),
      ...scope,
      provider: "slack" as const,
      app_id: app.id,
      short_id: issueShortId(db.installation_short_ids, scope, rng),
      label: agent.name.slice(0, 40),
      handler: "mention" as const,
      status: "connected" as const,
      team_id: spec.team.id,
      team_name: spec.team.name,
      enterprise_id: null,
      identity_external_id: `U0${token(8, rng).toUpperCase()}`,
      handle: agent.name
        .toLowerCase()
        .replace(/[^a-z0-9]+/g, "-")
        .replace(/^-|-$/g, ""),
      granted_scopes: [...FAKE_SCOPES],
      connected_at: ago(9 * DAY - 2 * HOUR),
      last_event_at: ago(40 * MINUTE),
      created_at: ago(9 * DAY - 2 * HOUR),
      updated_at: ago(9 * DAY - 2 * HOUR),
    };
    db.channel_connections.push(connection);

    db.agent_channels.push({
      id: id("agent_channel", 9 * DAY - 2 * HOUR),
      ...scope,
      agent_id: agent.id,
      connection_id: connection.id,
      ...agentChannelDefaults(),
      tools: ["read_thread", "reply", "send_message"],
      approval_required: ["send_message"],
      created_at: ago(9 * DAY - 2 * HOUR),
      updated_at: ago(3 * DAY),
    });

    const people: ChannelPersonRow[] = spec.people.map((person, i) => ({
      id: id("channel_person", 8 * DAY - i * HOUR),
      ...scope,
      connection_id: connection.id,
      external_id: person.external_id,
      handle: person.handle,
      display_name: person.display_name,
      email: person.email,
      is_automated: false,
      first_seen_at: ago(8 * DAY - i * HOUR),
      updated_at: ago(8 * DAY - i * HOUR),
    }));
    db.channel_people.push(...people);

    const support: ChannelConversationRow = {
      id: id("channel_conversation", 8 * DAY),
      ...scope,
      connection_id: connection.id,
      external_id: `C0${token(8, rng).toUpperCase()}`,
      kind: "channel",
      name: "support",
      is_external: false,
      identity_is_member: true,
      last_message_at: ago(40 * MINUTE),
      paused_until: null,
      created_at: ago(8 * DAY),
      updated_at: ago(40 * MINUTE),
    };
    const dm: ChannelConversationRow = {
      ...support,
      id: id("channel_conversation", 6 * DAY),
      external_id: `D0${token(8, rng).toUpperCase()}`,
      kind: "dm",
      name: null,
      last_message_at: ago(3 * HOUR),
      created_at: ago(6 * DAY),
      updated_at: ago(3 * HOUR),
    };
    db.channel_conversations.push(support, dm);

    const [first, second] = [people[0], people[1] ?? people[0]];
    if (!first || !second) continue;
    const rootTs = ((nowMs - 50 * MINUTE) / 1000).toFixed(6);
    const message = (
      conversation: ChannelConversationRow,
      author: ChannelPersonRow | null,
      agoMs: number,
      text: string,
      threadKey: string,
    ): ChannelMessageRow => ({
      id: id("channel_message", agoMs),
      ...scope,
      connection_id: connection.id,
      conversation_id: conversation.id,
      external_id: ((nowMs - agoMs) / 1000).toFixed(6),
      thread_key: threadKey,
      direction: author ? "inbound" : "outbound",
      author_person_id: author?.id ?? null,
      text,
      mentions: author ? [connection.identity_external_id] : [],
      run_id: null,
      tool_call_id: null,
      agent_hop: 0,
      sent_at: ago(agoMs),
      edited_at: null,
      deleted_at: null,
      created_at: ago(agoMs),
    });
    db.channel_messages.push(
      message(
        support,
        first,
        50 * MINUTE,
        `<@${connection.identity_external_id}> can you check order 4521? It hasn't arrived.`,
        rootTs,
      ),
      message(support, null, 49 * MINUTE, "It shipped Monday and is out for delivery today.", rootTs),
      message(
        support,
        second,
        40 * MINUTE,
        `Thanks <@${connection.identity_external_id}>, that's what I needed.`,
        rootTs,
      ),
      message(dm, first, 3 * HOUR, "What's on the board for this week?", ""),
    );
  }
}

/** The workspace's agent by name, or its first active agent. */
function agentIn(db: Db, organizationId: OrganizationId, workspace: WorkspaceRow, name?: string): AgentRow | undefined {
  const agents = db.agents
    .filter((a) => a.organization_id === organizationId && a.workspace_id === workspace.id)
    .sort((a, b) => a.created_at.localeCompare(b.created_at) || a.id.localeCompare(b.id));
  return (
    (name ? agents.find((a) => a.name === name) : undefined) ?? agents.find((a) => a.status === "active") ?? agents[0]
  );
}
