/**
 * Webhook event types grouped by what they're about, for the lists that offer
 * every type: an endpoint's events and the delivery history's filter.
 */
const GROUPS: Record<string, string> = {
  installation: "Installations",
  connect_session: "Connect sessions",
  run: "Runs",
  app: "Apps",
  channel: "Channels",
  memory: "Memory",
  webhook: "Test events",
};

export interface EventGroup<T extends string> {
  /** What the group's types are about, such as `run` or `channel`. */
  key: string;
  label: string;
  types: T[];
}

/** What an event type is about: the part before its dot, with every `channel_…` type a channel's. */
function groupOf(type: string): string {
  const resource = type.slice(0, Math.max(type.indexOf("."), 0)) || type;
  return resource.startsWith("channel_") ? "channel" : resource;
}

function labelOf(key: string): string {
  const label = GROUPS[key] ?? key.replaceAll("_", " ");
  return `${label.charAt(0).toUpperCase()}${label.slice(1)}`;
}

/** `types` grouped by what they're about, each group and type in the order given. */
export function eventGroups<T extends string>(types: readonly T[]): EventGroup<T>[] {
  const groups = new Map<string, T[]>();
  for (const type of types) {
    const key = groupOf(type);
    groups.set(key, [...(groups.get(key) ?? []), type]);
  }
  return Array.from(groups, ([key, grouped]) => ({ key, label: labelOf(key), types: grouped }));
}
