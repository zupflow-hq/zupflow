/**
 * How the Console names channel things: providers, statuses, tools and
 * settings, in words people use. Zupflow's own names avoid "bot": a
 * connection acts as its identity, and apps posting on their own are
 * automated accounts.
 */
import {
  type AppIcon,
  CHANNEL_PROVIDER_NAMES,
  type ChannelAppManagerStatus,
  type ChannelAppStatus,
  type ChannelConnection,
  type ChannelConnectionStatus,
  type ChannelConversation,
  type ChannelConversationKind,
  type ChannelProvider,
  type ChannelProviderSettings,
  type ChannelToolName,
  type ChannelWakeReason,
} from "@zupflow/sdk";

/** Why the Console doesn't link teams or add agents in Production: the customer's product does, through the API. */
export const PRODUCTION_CHANNELS =
  "In Production, your product links teams and adds agents through the API or hosted Connect, so the customer's own admin does it from your product. The Console shows what they set up.";

/** What whoever runs the server does so it reaches each provider. */
export const CHANNEL_PROVIDER_SETUP: Record<ChannelProvider, string> = {
  slack: "Whoever runs it turns Slack on by giving the server a public HTTPS address.",
};

/** Providers by name, for a sentence: "Slack", "Slack or Teams", "Slack, Teams or Chat". */
export function providerNames(providers: readonly ChannelProvider[], conjunction = "or"): string {
  const names = providers.map((p) => CHANNEL_PROVIDER_NAMES[p]);
  return names.length <= 1
    ? (names[0] ?? "")
    : `${names.slice(0, -1).join(", ")} ${conjunction} ${names[names.length - 1]}`;
}

/** The one provider copy can name, when there is exactly one; null otherwise. */
export function onlyProvider(providers: readonly ChannelProvider[]): ChannelProvider | null {
  const distinct = [...new Set(providers)];
  return distinct.length === 1 ? (distinct[0] ?? null) : null;
}

/**
 * Why no team can be linked in this workspace, from the organization's
 * choice and what this server reaches: none allowed, or none of the allowed
 * reached. Without the organization's choice, both are possible. `linked`:
 * teams are linked here already. `choice` says whether the organization's
 * choice is part of why, so the page can say who changes it.
 */
export function noProviderReason(
  settings: ChannelProviderSettings | undefined,
  linked = false,
): { text: string; choice: boolean } {
  const team = linked ? "no new team" : "no team";
  if (!settings) {
    return {
      text: "No chat provider is available here: this Zupflow server reaches none that your organization allows.",
      choice: true,
    };
  }
  const allowed = settings.providers.filter((p) => p.allowed).map((p) => p.provider);
  if (allowed.length === 0) {
    return {
      text: `Your organization doesn't allow any chat provider, so ${team} can be linked here.${linked ? " Teams already linked keep working." : ""}`,
      choice: true,
    };
  }
  const refused = settings.providers.filter((p) => p.reachable && !p.allowed).map((p) => p.provider);
  return {
    text: [
      `This Zupflow server can't reach ${providerNames(allowed)} yet, so ${team} can be linked here.`,
      ...allowed.map((p) => CHANNEL_PROVIDER_SETUP[p]),
      refused.length > 0 ? `Your organization doesn't allow ${providerNames(refused)}.` : "",
    ]
      .filter(Boolean)
      .join(" "),
    choice: refused.length > 0,
  };
}

/** What a team of a provider the organization stopped allowing still does. */
export function notAllowedNote(provider: ChannelProvider): string {
  const name = CHANNEL_PROVIDER_NAMES[provider];
  return `Your organization doesn't allow ${name} any more. Agents already here keep working, but no new ${name} team can be linked.`;
}

/** What each provider calls a team, when it isn't "team": said once where a team is linked. */
export const CHANNEL_TEAM_TERM: Record<ChannelProvider, string | null> = { slack: "what Slack calls a workspace" };

/** What each provider calls a conversation shared with another organization. */
export const SHARED_CONVERSATION: Record<ChannelProvider, string> = { slack: "Slack Connect" };

/** A provider's mark in the Console: a glyph on its color, like an app's icon. */
export const CHANNEL_PROVIDER_ICON: Record<ChannelProvider, AppIcon> = {
  slack: { glyph: "#", color: "#4A154B" },
};

type Tone = "success" | "warning" | "danger" | "neutral" | "info" | "live";

export const MANAGER_STATUS: Record<ChannelAppManagerStatus, [Tone, string]> = {
  active: ["success", "Linked"],
  token_expired: ["danger", "Token expired"],
  disconnected: ["neutral", "Disconnected"],
};

export const APP_STATUS: Record<ChannelAppStatus, [Tone, string]> = {
  creating: ["live", "Creating"],
  awaiting_install: ["warning", "Awaiting install"],
  installed: ["success", "Installed"],
  updating: ["live", "Updating"],
  deleting: ["live", "Removing"],
  deleted: ["neutral", "Removed"],
  failed: ["danger", "Failed"],
};

export const CONNECTION_STATUS: Record<ChannelConnectionStatus, [Tone, string]> = {
  connected: ["success", "Connected"],
  reconnect_required: ["danger", "Reinstall required"],
  disconnected: ["neutral", "Disconnected"],
};

export const CONVERSATION_KIND: Record<ChannelConversationKind, string> = {
  channel: "Channel",
  private_channel: "Private channel",
  dm: "Direct message",
  group_dm: "Group DM",
};

/** A conversation's name as people see it in the provider: `#support`, or what kind it is when it has none. */
export function conversationName(conversation: Pick<ChannelConversation, "kind" | "name">): string {
  if (conversation.name) {
    return conversation.kind === "channel" || conversation.kind === "private_channel"
      ? `#${conversation.name}`
      : conversation.name;
  }
  return CONVERSATION_KIND[conversation.kind];
}

export const WAKE_REASON: Record<ChannelWakeReason, string> = {
  direct_message: "Sent a direct message",
  mentioned: "Mentioned in a conversation",
  thread_follow_up: "Followed up in a thread it answered",
};

/** Why an app couldn't be made or changed, by its stable code, with what to do. */
export function appFailure(code: string | null, provider: ChannelProvider): string {
  const name = CHANNEL_PROVIDER_NAMES[provider];
  // A failed app doesn't hold its team: the agent is offered there again below the card.
  const again = "then add the agent to the team again below.";
  switch (code) {
    case "app_limit_reached":
      return `The team has as many apps as its plan allows. Remove an app in ${name} or upgrade the team's plan, ${again}`;
    case "invalid_configuration_token":
    case "token_expired":
      return `The team's configuration token stopped working. Paste a new one on the Channels page, ${again}`;
    case "rate_limited":
      return `${name} asked Zupflow to slow down. Wait a minute, ${again}`;
    case null:
      return `${name} refused to create the app. Add the agent to the team again below.`;
    default:
      return `${name} refused to create the app (${code}). Add the agent to the team again below.`;
  }
}

/** What each channel tool does, for the people choosing levels and grants. */
export const CHANNEL_TOOL_COPY: Record<ChannelToolName, { title: string; description: string }> = {
  list_conversations: { title: "List conversations", description: "Channels, DMs and threads it can see." },
  get_conversation: { title: "Get a conversation", description: "A conversation's details and members." },
  read_messages: { title: "Read messages", description: "A conversation's recent messages." },
  read_thread: { title: "Read a thread", description: "Every message in one thread, oldest first." },
  search_messages: { title: "Search messages", description: "Messages this connection has kept that match." },
  find_people: { title: "Find people", description: "People by name, handle or email." },
  get_person: { title: "Get a person", description: "A person's name, title and time zone." },
  get_attachment: { title: "Read an attachment", description: "A file's text, up to 100 KB." },
  reply: { title: "Reply", description: "Answer in the thread that woke it." },
  send_message: { title: "Send a message", description: "Post in another conversation or thread." },
  send_direct_message: { title: "Send a direct message", description: "Message a person directly." },
  react: { title: "React", description: "Add or remove an emoji reaction." },
  edit_message: { title: "Edit its message", description: "Change a message it sent." },
  delete_message: { title: "Delete its message", description: "Remove a message it sent." },
};

export function toolTitle(tool: string): string {
  return CHANNEL_TOOL_COPY[tool as ChannelToolName]?.title ?? tool;
}

/** Slack's mention markup (`<@U123>`) as a handle: the agent's own by name, anyone else's by their ID. */
export function renderMentions(text: string, connection: Pick<ChannelConnection, "identity"> | undefined): string {
  return text.replace(/<@([A-Z0-9]+)(?:\|([^>]*))?>/g, (_, id: string, label?: string) =>
    connection && id === connection.identity.external_id ? `@${connection.identity.handle}` : `@${label ?? id}`,
  );
}
