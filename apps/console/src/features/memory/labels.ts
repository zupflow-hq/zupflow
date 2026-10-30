/**
 * How the Console names memory: write levels, kinds, settings, who wrote a
 * memory, and what each refusal means with what to do about it.
 */
import {
  type Agent,
  MEMORY_KIND_NOTES,
  type MemoryKind,
  type MemoryType,
  type MemoryWrites,
  type Organization,
} from "@zupflow/sdk";
import { canAdminister, canChange } from "@/features/context";
import { errorMessage, isApiError, isStatus } from "@/lib/errors";

/** Whether a type's agents may write their memory with their tools, as people choose it. */
export const WRITES: Record<MemoryWrites, { label: string; description: string }> = {
  allow: {
    label: "Allow",
    description: "Agents save, change and forget memories with their tools.",
  },
  approve: {
    label: "Require approval",
    description: "Each save, change or forget waits for a person to approve it, like any tool call that asks first.",
  },
  deny: {
    label: "Search only",
    description:
      "Agents search their memories but can't write them. People add them from an agent's Memories tab in Sandbox, or through the API.",
  },
};

/** A kind as people know it: its name when this server serves it, its key otherwise. */
export function kindName(kinds: readonly MemoryKind[] | undefined, kind: string): string {
  return kinds?.find((k) => k.kind === kind)?.name ?? kind;
}

/** Why a type's agents run without memory: this server doesn't serve its kind. */
export function notServed(type: Pick<MemoryType, "name" | "kind">): string {
  return `This server doesn't serve ${type.kind}, the kind of ${type.name}: agents using it run without memory, and its memories can't be read here.`;
}

/** A setting's label, for every kind alike: its schema's title, or its key in words when the schema gives none. */
export function settingLabel(key: string, schema: { title?: unknown }): string {
  if (typeof schema.title === "string" && schema.title.trim()) return schema.title.trim();
  const words = key.replaceAll("_", " ").trim();
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** The most memories one agent keeps in one workspace in a `notes` type, when its settings say. */
export function noteLimit(type: Pick<MemoryType, "kind" | "settings"> | undefined): number | null {
  const value = type?.kind === MEMORY_KIND_NOTES ? type.settings.max_notes : undefined;
  return typeof value === "number" && Number.isInteger(value) ? value : null;
}

/** The longest note the `notes` kind keeps, in characters. */
export const NOTE_CHARACTERS = 500;

/** Characters as a kind counts them: Unicode scalar values, never UTF-16 units. */
export const characters = (text: string) => [...text].length;

/** Line breaks other than `\r`, and tabs: the `notes` kind writes a space for each. */
const SPACED = new Set(["\n", "\t", "\u000B", "\u000C", "\u0085", "\u2028", "\u2029"]);

/**
 * A character the `notes` kind leaves out: a control character, or an
 * invisible one that can hide or reorder text (a tag character, a
 * bidirectional embedding, override or isolate, a zero-width space, a word
 * joiner or invisible operator, a byte order mark, the Mongolian vowel
 * separator, an interlinear annotation mark).
 */
function leftOut(code: number): boolean {
  return (
    code <= 0x1f ||
    (code >= 0x7f && code <= 0x9f) ||
    (code >= 0xe0000 && code <= 0xe007f) ||
    (code >= 0x202a && code <= 0x202e) ||
    (code >= 0x2066 && code <= 0x2069) ||
    code === 0x200b ||
    (code >= 0x2060 && code <= 0x2064) ||
    code === 0xfeff ||
    code === 0x180e ||
    (code >= 0xfff9 && code <= 0xfffb)
  );
}

/**
 * A note's text as the `notes` kind keeps it: each line break (`\r\n` is one)
 * and each tab becomes a space, without runs of them collapsing, the
 * characters `leftOut` names go, and the ends are trimmed. Its length is what
 * the kind holds to `NOTE_CHARACTERS`.
 */
export function noteText(text: string): string {
  const chars = [...text];
  let line = "";
  for (let i = 0; i < chars.length; i++) {
    const c = chars[i] ?? "";
    if (c === "\r") {
      if (chars[i + 1] === "\n") i++;
      line += " ";
    } else if (SPACED.has(c)) {
      line += " ";
    } else if (!leftOut(c.codePointAt(0) ?? 0)) {
      line += c;
    }
  }
  return line.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, "");
}

/** "a", "a and b", "a, b and c". */
export function listed(names: string[]): string {
  return names.length <= 1 ? (names[0] ?? "") : `${names.slice(0, -1).join(", ")} and ${names.at(-1)}`;
}

/** What to do when the agent has no memory type to save to, for this person. */
export function noMemoryFix(org: Organization, agent: Pick<Agent, "name" | "memory">): string {
  if (agent.memory === "off") {
    return canChange(org)
      ? `${agent.name}'s memory is off. Turn it on in its Configuration tab first.`
      : `${agent.name}'s memory is off. An owner, an admin or a developer can turn it on in its Configuration tab.`;
  }
  const none = `${agent.name} uses the organization's default memory type, and none is chosen.`;
  if (canAdminister(org)) {
    return `${none} Choose one on the Memory page, or choose a memory type for ${agent.name} in its Configuration tab.`;
  }
  return canChange(org)
    ? `${none} Ask an owner or admin to choose one, or choose a memory type for ${agent.name} in its Configuration tab.`
    : `${none} Ask an owner or admin to choose one, or a developer to choose a memory type for ${agent.name}.`;
}

/** What a person did with an agent's memories that the server refused. */
export type MemoryAction = "add" | "change" | "forget" | "forget_all";

/**
 * A refusal to save, change or forget an agent's memory, as what happened and
 * what to do. `null` for one that belongs on the text field instead.
 */
export function memoryRefusal(
  error: unknown,
  context: {
    org: Organization;
    agent: Pick<Agent, "name" | "memory">;
    type?: Pick<MemoryType, "name" | "kind">;
    action: MemoryAction;
  },
): string | null {
  const { org, agent, type, action } = context;
  if (isApiError(error, "invalid_memory_text")) return null;
  if (isApiError(error, "memory_off")) return noMemoryFix(org, agent);
  if (isApiError(error, "memory_full")) {
    return `${agent.name} keeps as many memories as ${type?.name ?? "its memory type"} allows. Forget one, or change one instead of adding another.`;
  }
  if (isApiError(error, "memory_kind_not_served")) {
    return `This server doesn't serve ${type ? `${type.kind}, the kind of ${type.name}` : "this memory type's kind"}, so its memories can't be saved or changed here. Ask whoever runs this server to serve it, or choose another memory type for ${agent.name}.`;
  }
  if (isApiError(error, "memory_kind_unsupported")) {
    return `The kind of ${type ? `${type.name} (${type.kind})` : "this memory type"} doesn't let people do that with its memories from the Console or the API.`;
  }
  if (isApiError(error, "unknown_memory_type")) {
    return `${type?.name ?? "This memory type"} was deleted meanwhile, so nothing was saved in it. The page now shows what's left.`;
  }
  if (isApiError(error, "console_read_only_in_production")) {
    return "In Production, memories are saved, changed and forgotten through the API. Try this in a Sandbox workspace.";
  }
  if (isApiError(error, "forbidden")) {
    return "Viewers can't change memories. Ask a developer, an admin or an owner.";
  }
  if (isStatus(error, 404)) {
    // Saving or forgetting everything names only the agent: it's the agent that's gone.
    if (action === "add") return `${agent.name} was deleted meanwhile, with its memories, so nothing was saved.`;
    if (action === "forget_all") {
      return `${agent.name} was deleted meanwhile, with its memories: nothing is left to forget.`;
    }
    return "This memory was already forgotten. The list now shows what's left.";
  }
  // An erasure goes a hundred at a time, so one cut off partway has forgotten some, and asking again forgets the rest.
  if (action === "forget_all" && (!isApiError(error) || error.status >= 500)) {
    return `${errorMessage(error)} Some of its memories may already be forgotten: the list shows what's left, and forgetting all again forgets the rest.`;
  }
  return errorMessage(error);
}

/** A refusal to create, change or delete a memory type, or to choose the default, that no field shows. */
export function memoryTypeRefusal(error: unknown): string {
  if (isApiError(error, "forbidden")) return "Only owners and admins change memory types. Ask one of them.";
  if (isApiError(error, "unknown_memory_type")) {
    return "That memory type was deleted meanwhile. Reload the page and choose another.";
  }
  if (isApiError(error, "unknown_memory_kind")) {
    return "This server no longer serves that kind. Reload the page and choose a kind it serves.";
  }
  if (isApiError(error, "invalid_memory_settings")) {
    return "The kind refused some settings. Check each one and try again.";
  }
  if (isApiError(error, "memory_type_name_taken")) return "Another memory type has this name. Choose another.";
  return errorMessage(error);
}
