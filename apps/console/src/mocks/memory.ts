/**
 * The mock's memory: the kinds it serves, each organization's memory types
 * and its default type, the memory each agent chooses, and what each agent
 * remembers. Types and the default are the organization's, the same in each
 * environment: every member and API key reads them, and owners and admins
 * change them, from the Console only. An agent's memories are its own in its
 * workspace: every member and API key reads them, and developers and up
 * change them, from the Console in Sandbox only.
 *
 * Every lookup carries the organization: a type is never found by its ID
 * alone, and another organization's type is the same as one that doesn't
 * exist. A memory is found by its whole scope: organization, workspace and
 * agent, as well as its ID, and only through a kind this server serves.
 */
import {
  AgentMemory,
  type DefaultMemoryType,
  type Memory,
  type MemoryKind,
  type MemoryOrigin,
  type MemoryType,
  type MemoryTypeId,
  MemoryWrites,
  type OrganizationId,
} from "@zupflow/sdk";
import { http } from "msw";
import { z } from "zod";
import { db } from "./db";
import {
  ADMINS,
  ApiFailure,
  announceLifecycle,
  assertConsoleMayAct,
  audit,
  body,
  clampedInt,
  consoleOrgAccess,
  created,
  DEVELOPERS,
  noContent,
  notFound,
  ok,
  orgAccess,
  param,
  requireRole,
  route,
  toUser,
  type WorkspaceAccess,
  workspaceAccess,
} from "./http";
import { isId, newId } from "./ids";
import { KNOWN_MEMORY_KINDS, type KnownKind, trimmed } from "./memory-kinds";
import type { AgentRow, Db, MemoryRow, MemoryTypeRow } from "./model";
import type { Rng } from "./random";
import { drawShortId, inWorkspace, sameWorkspace } from "./scope";
import { compareNames } from "./tools";

type Params = Record<string, string | readonly string[] | undefined>;

/** How many agents a refusal names before it counts the rest. */
const NAMED_HOLDERS = 5;
const MAX_NAME = 60;

/** A kind this server serves: one the mock knows and its database lists. */
function servedKind(d: Db, kind: string): KnownKind | undefined {
  return d.memory_kinds.includes(kind) ? KNOWN_MEMORY_KINDS.get(kind) : undefined;
}

/** A memory type of this organization. */
function memoryTypeIn(d: Db, organizationId: OrganizationId, memoryTypeId: string): MemoryTypeRow | undefined {
  return d.memory_types.find((t) => t.organization_id === organizationId && t.id === memoryTypeId);
}

/** The organization's default memory type, if one is chosen. */
function defaultMemoryTypeOf(d: Db, organizationId: OrganizationId): MemoryTypeRow | undefined {
  const row = d.default_memory_types.find((r) => r.organization_id === organizationId);
  return row?.memory_type_id ? memoryTypeIn(d, organizationId, row.memory_type_id) : undefined;
}

/** The organization's agents that name the type, in any workspace, oldest first. Agents on the default don't. */
function agentsNaming(d: Db, organizationId: OrganizationId, memoryTypeId: MemoryTypeId): AgentRow[] {
  return d.agents
    .filter((a) => a.organization_id === organizationId && a.memory === "type" && a.memory_type_id === memoryTypeId)
    .sort((a, b) => compareNames(a.created_at, b.created_at) || compareNames(a.id, b.id));
}

function toMemoryType(row: MemoryTypeRow): MemoryType {
  const d = db();
  return {
    id: row.id,
    name: row.name,
    kind: row.kind,
    settings: row.settings,
    writes: row.writes,
    served: servedKind(d, row.kind) !== undefined,
    agent_count: agentsNaming(d, row.organization_id, row.id).length,
    is_default: defaultMemoryTypeOf(d, row.organization_id)?.id === row.id,
    created_at: row.created_at,
    updated_at: row.updated_at,
  };
}

function toDefaultMemoryType(organizationId: OrganizationId): DefaultMemoryType {
  const d = db();
  const row = d.default_memory_types.find((r) => r.organization_id === organizationId);
  const memoryType = defaultMemoryTypeOf(d, organizationId);
  return {
    memory_type: memoryType ? toMemoryType(memoryType) : null,
    updated_at: row?.updated_at ?? null,
    updated_by: row?.updated_by_user_id ? toUser(row.updated_by_user_id) : null,
  };
}

/** A type that isn't the organization's, named by `field`: a body's `memory_type_id`, or a list's `memory_type`. */
const unknownMemoryType = (field = "memory_type_id") =>
  new ApiFailure(422, "unknown_memory_type", "Choose a memory type of this organization.", {
    [field]: "Choose a memory type of this organization.",
  });

const invalidMemory = (message: string) =>
  new ApiFailure(400, "validation_failed", "Some fields are invalid.", { memory_type_id: message });

/**
 * An agent's memory once a create or an update is applied: what was sent,
 * over what the agent had. `default` and `off` name no type, so they clear
 * the agent's; the agent ends on `type` with one of its organization's
 * types, or on another memory with none.
 */
export function agentMemory(
  organizationId: OrganizationId,
  current: Pick<AgentRow, "memory" | "memory_type_id">,
  sent: { memory?: AgentMemory; memory_type_id?: string | null },
): Pick<AgentRow, "memory" | "memory_type_id"> {
  const memory = sent.memory ?? current.memory;
  if (memory !== "type") {
    if (sent.memory_type_id != null) throw invalidMemory("Set memory to type to choose a memory type.");
    return { memory, memory_type_id: null };
  }
  const chosen = sent.memory_type_id === undefined ? current.memory_type_id : sent.memory_type_id;
  if (chosen === null) throw invalidMemory("Choose the memory type this agent uses.");
  const row = memoryTypeIn(db(), organizationId, chosen);
  if (!row) throw unknownMemoryType();
  return { memory: "type", memory_type_id: row.id };
}

/** Body fields of an agent's memory, as creating and updating an agent take them. */
export const agentMemoryInput = {
  memory: z.enum(AgentMemory.options, "Choose default, type or off.").optional(),
  // Unchecked here on purpose: finding the type is the check.
  memory_type_id: z.string().nullable().optional(),
};

function findMemoryType(organizationId: OrganizationId, params: Params): MemoryTypeRow {
  const id = param(params, "memory_type");
  const row = isId("memory_type", id) ? memoryTypeIn(db(), organizationId, id) : undefined;
  if (!row) throw notFound("Memory type");
  return row;
}

/** A type's name: trimmed, 1 to 60 characters. */
const typeName = z
  .string("Required.")
  .trim()
  .min(1, "Required.")
  .refine((value) => [...value].length <= MAX_NAME, `Use ${MAX_NAME} characters or fewer.`);
const settingsInput = z.record(z.string(), z.unknown(), "Use an object of the kind's settings.");
const writesInput = z.enum(MemoryWrites.options, "Choose allow, approve or deny.");

/**
 * Settings as the type's kind checks them, its defaults filled in; each
 * refused setting is named as `settings.<key>`. A kind this server doesn't
 * serve keeps them as given: nothing here can check them.
 */
function checkedSettings(d: Db, kind: string, settings: Record<string, unknown>): Record<string, unknown> {
  const served = servedKind(d, kind);
  if (!served) return settings;
  const checked = served.check(settings);
  if (checked.refused) {
    const fields = Object.fromEntries(Object.entries(checked.refused).map(([key, why]) => [`settings.${key}`, why]));
    throw new ApiFailure(422, "invalid_memory_settings", "Some settings are invalid.", fields);
  }
  return checked.settings;
}

/** A name is the organization's once: exactly, as written. */
function assertNameFree(d: Db, organizationId: OrganizationId, name: string, self: MemoryTypeId | null) {
  if (d.memory_types.some((t) => t.organization_id === organizationId && t.name === name && t.id !== self)) {
    throw new ApiFailure(409, "memory_type_name_taken", "Another memory type has that name.", {
      name: "Already taken.",
    });
  }
}

/**
 * Refuses deleting a type in use: the organization's default, or an agent's.
 * Names what holds it, so the person knows what to change first; never
 * another organization's.
 */
function assertMemoryTypeUnused(d: Db, row: MemoryTypeRow) {
  const isDefault = defaultMemoryTypeOf(d, row.organization_id)?.id === row.id;
  const agents = agentsNaming(d, row.organization_id, row.id);
  if (!isDefault && agents.length === 0) return;
  const workspaces = new Map(
    d.workspaces.filter((w) => w.organization_id === row.organization_id).map((w) => [w.id, w.name]),
  );
  const names = agents
    .slice(0, NAMED_HOLDERS)
    .map((agent) => `${agent.name} (${workspaces.get(agent.workspace_id) ?? "Deleted workspace"})`);
  if (agents.length > NAMED_HOLDERS) names.push(`${agents.length - NAMED_HOLDERS} more`);
  const reasons: string[] = [];
  const fields: Record<string, string> = {};
  if (isDefault) {
    reasons.push(`${row.name} is the organization's default memory type; choose another default first`);
    fields.default_memory_type = row.name;
  }
  if (names.length > 0) {
    reasons.push(`agents use it: ${names.join(", ")}; choose another memory for them first`);
    fields.agents = names.join(", ");
  }
  throw new ApiFailure(409, "memory_type_in_use", `Can't delete ${row.name}: ${reasons.join(". ")}.`, fields);
}

// Lists are by name (ASCII letters in any case), then ID, as the backend
// sorts them. A cursor is the page's last place in that order.

const asciiLower = (value: string) => value.replace(/[A-Z]/g, (c) => c.toLowerCase());

interface Place {
  name: string;
  id: string;
}

const placeOf = (row: MemoryTypeRow): Place => ({ name: asciiLower(row.name), id: row.id });
const comparePlaces = (a: Place, b: Place) => compareNames(a.name, b.name) || compareNames(a.id, b.id);

function encodeCursor(place: Place): string {
  const binary = Array.from(new TextEncoder().encode(`${place.id}\n${place.name}`), (b) => String.fromCharCode(b));
  return btoa(binary.join("")).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

function decodeCursor(cursor: string): Place {
  try {
    const binary = atob(cursor.replaceAll("-", "+").replaceAll("_", "/"));
    const raw = new TextDecoder("utf-8", { fatal: true }).decode(Uint8Array.from(binary, (c) => c.charCodeAt(0)));
    const split = raw.indexOf("\n");
    const id = raw.slice(0, split);
    if (split > 0 && isId("memory_type", id)) return { id, name: raw.slice(split + 1) };
  } catch {
    // Falls through to the refusal.
  }
  throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", {
    cursor: "Not a cursor from this list.",
  });
}

// ---------------------------------------------------------------------------
// What each agent remembers, in one memory type at a time.

/** The agent the path names, in the request's workspace: another workspace's or organization's is none. */
function agentIn(access: WorkspaceAccess, params: Params): AgentRow {
  const id = param(params, "agent");
  const row = isId("agent", id) ? db().agents.find((a) => inWorkspace(a, access.workspace) && a.id === id) : undefined;
  if (!row) throw notFound("Agent");
  return row;
}

/** Whether a memory is the agent's, in the agent's workspace. */
const keptBy = (agent: AgentRow) => (m: MemoryRow) => sameWorkspace(m, agent) && m.agent_id === agent.id;

/** A memory with the type that keeps it and that type's kind, which this server serves. */
interface Reached {
  row: MemoryRow;
  type: MemoryTypeRow;
  kind: KnownKind;
}

/**
 * The memory as this server reaches it: through its type's kind, when it
 * serves that kind. The backend finds a memory through the kinds it serves,
 * so another kind's memory is out of its reach.
 */
function reach(d: Db, row: MemoryRow): Reached | undefined {
  const type = memoryTypeIn(d, row.organization_id, row.memory_type_id);
  const kind = type && servedKind(d, type.kind);
  return type && kind ? { row, type, kind } : undefined;
}

/**
 * One of the agent's memories, in any of its types, by its ID: never found by
 * the ID alone, and a memory out of reach is the same as one that isn't there.
 */
function memoryOf(d: Db, agent: AgentRow, params: Params): Reached {
  const id = param(params, "memory");
  const row = isId("memory", id) ? d.memories.find((m) => keptBy(agent)(m) && m.id === id) : undefined;
  const reached = row && reach(d, row);
  if (!reached) throw notFound("Memory");
  return reached;
}

/** The memory type an agent uses now: its own, the organization's default for one on the default, or none. */
function currentTypeOf(d: Db, agent: AgentRow): MemoryTypeRow | undefined {
  if (agent.memory === "type" && agent.memory_type_id)
    return memoryTypeIn(d, agent.organization_id, agent.memory_type_id);
  if (agent.memory === "default") return defaultMemoryTypeOf(d, agent.organization_id);
  return undefined;
}

/** Refuses what the type's kind doesn't let people do with its memories (`MemoryKind.manage`). */
function assertManages(type: MemoryTypeRow, kind: KnownKind, what: keyof MemoryKind["manage"]) {
  if (!kind.description.manage[what]) {
    throw new ApiFailure(
      422,
      "memory_kind_unsupported",
      `${type.name}'s memory kind, ${type.kind}, can't ${what} memories from the API or the Console.`,
    );
  }
}

/** The kind of a type a list or a save names, when this server serves it and it lets people `what` its memories. */
function kindFor(d: Db, type: MemoryTypeRow, what: keyof MemoryKind["manage"]): KnownKind {
  const kind = servedKind(d, type.kind);
  if (!kind) {
    throw new ApiFailure(
      422,
      "memory_kind_not_served",
      `This server doesn't serve ${type.kind}, the memory kind of ${type.name}.`,
    );
  }
  assertManages(type, kind, what);
  return kind;
}

/** The text as the type's kind keeps it: `422 invalid_memory_text`, naming `text`, for text it doesn't keep. */
function keptText(kind: KnownKind, text: string): string {
  const kept = kind.text(text);
  if (kept === null) throw new ApiFailure(422, "invalid_memory_text", kind.textRule, { text: kind.textRule });
  return kept;
}

/** Who writes a memory's text: the API key, or the member from the Console. */
const originOf = (access: WorkspaceAccess): MemoryOrigin =>
  access.kind === "api_key"
    ? { type: "api_key", api_key_id: access.key.id }
    : { type: "member", user_id: access.user.id };

/** A memory as the API shows it: no organization (known from the request). */
const toMemory = ({ organization_id: _organization, ...memory }: MemoryRow): Memory => memory;

/** A memory as the audit log names it: by its agent and short ID, never its text. */
const memoryTarget = (agent: AgentRow, row: MemoryRow) => ({
  type: "memory",
  id: row.id,
  name: `${agent.name} · ${row.short_id}`,
});

/**
 * Announces a change to a memory, to the endpoints of its workspace's
 * environment: which memory, its agent and type, and who wrote its text,
 * never the text.
 */
function announceMemory(type: "memory.saved" | "memory.updated" | "memory.forgotten", row: MemoryRow, at: string) {
  const data = {
    object: "memory" as const,
    id: row.id,
    agent_id: row.agent_id,
    memory_type_id: row.memory_type_id,
    origin: row.origin,
  };
  announceLifecycle(row.organization_id, row.workspace_id, { type, data }, at);
}

/** The operating system's randomness, as the backend draws short IDs from it. */
const secureRandom: Rng = () => (crypto.getRandomValues(new Uint32Array(1))[0] ?? 0) / 2 ** 32;

/** How many short IDs a save draws, as the backend does: the first one free in the memory's scope is its. */
export const MEMORY_SHORT_ID_DRAWS = 4;

/** The first drawn short ID no memory of the scope holds; null when each one is taken. */
export function freeShortId(taken: ReadonlySet<string>, rng: Rng = secureRandom): string | null {
  const drawn = Array.from({ length: MEMORY_SHORT_ID_DRAWS }, () => drawShortId(rng));
  return drawn.find((shortId) => !taken.has(shortId)) ?? null;
}

// A list is newest first by when each memory was saved, to the millisecond,
// then by ID, as the backend orders it. A cursor is the page's last place in
// that order, `{millisecond}\n{memory ID}` in base64url without padding,
// and is read as strictly as the backend reads it.

interface Saved {
  at: number;
  id: string;
}

const savedPlace = (row: MemoryRow): Saved => ({ at: Date.parse(row.created_at), id: row.id });
/** Newest first: negative when `a` comes before `b`. */
const compareSaved = (a: Saved, b: Saved) => b.at - a.at || compareNames(b.id, a.id);

function toBase64Url(bytes: Uint8Array): string {
  const binary = Array.from(bytes, (b) => String.fromCharCode(b)).join("");
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

/** Base64url without padding, written the one way it can be, and UTF-8 inside; null for anything else. */
function fromBase64Url(value: string): string | null {
  if (!/^[A-Za-z0-9_-]*$/.test(value) || value.length % 4 === 1) return null;
  try {
    const bytes = Uint8Array.from(atob(value.replaceAll("-", "+").replaceAll("_", "/")), (c) => c.charCodeAt(0));
    if (toBase64Url(bytes) !== value) return null;
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    return null;
  }
}

const encodeSaved = (place: Saved) => toBase64Url(new TextEncoder().encode(`${place.at}\n${place.id}`));

/** A 64-bit signed integer, as Rust's `i64` parses one. */
const isI64 = (value: string) =>
  /^[+-]?[0-9]+$/.test(value) && BigInt(value) >= -(2n ** 63n) && BigInt(value) < 2n ** 63n;

function decodeSaved(cursor: string): Saved {
  const raw = fromBase64Url(cursor);
  const split = raw?.indexOf("\n") ?? -1;
  if (raw !== null && split >= 0) {
    const [at, id] = [raw.slice(0, split), raw.slice(split + 1)];
    if (isI64(at) && isId("memory", id)) return { at: Number(at), id };
  }
  throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", {
    cursor: "Not a cursor from this list.",
  });
}

const emptyPage = () => ok([], { meta: { pagination: { next_cursor: null, total: 0 } } });

export function createMemoryHandlers(u: (path: string) => string) {
  return [
    http.get(
      u("/v1/organizations/:org/memory-kinds"),
      route(({ request, params }) => {
        orgAccess(request, param(params, "org"));
        const d = db();
        const kinds = [...d.memory_kinds].sort(compareNames).flatMap((kind): MemoryKind[] => {
          const served = servedKind(d, kind);
          return served ? [served.description] : [];
        });
        return ok(kinds);
      }),
    ),

    http.get(
      u("/v1/organizations/:org/memory-types"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        const url = new URL(request.url);
        const cursor = url.searchParams.get("cursor");
        const after = cursor === null ? null : decodeCursor(cursor);
        const limit = clampedInt(url, "limit", 25, 100);
        const all = db()
          .memory_types.filter((t) => t.organization_id === access.org.id)
          .sort((a, b) => comparePlaces(placeOf(a), placeOf(b)));
        const rest = after ? all.filter((t) => comparePlaces(placeOf(t), after) > 0) : all;
        const page = rest.slice(0, limit);
        const last = page.at(-1);
        const next = rest.length > limit && last ? encodeCursor(placeOf(last)) : null;
        return ok(page.map(toMemoryType), { meta: { pagination: { next_cursor: next, total: all.length } } });
      }),
    ),

    http.post(
      u("/v1/organizations/:org/memory-types"),
      route(async ({ request, params }) => {
        const access = consoleOrgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "create memory types");
        const input = await body(
          request,
          z.object({
            name: typeName,
            kind: z.string("Choose a memory kind."),
            settings: settingsInput.optional(),
            writes: writesInput.optional(),
          }),
        );
        const d = db();
        if (!servedKind(d, input.kind)) {
          throw new ApiFailure(422, "unknown_memory_kind", "Choose a memory kind this server serves.", {
            kind: "Choose a memory kind this server serves.",
          });
        }
        const settings = checkedSettings(d, input.kind, input.settings ?? {});
        assertNameFree(d, access.org.id, input.name, null);
        const now = new Date().toISOString();
        const row: MemoryTypeRow = {
          id: newId("memory_type"),
          organization_id: access.org.id,
          name: input.name,
          kind: input.kind,
          settings,
          writes: input.writes ?? "allow",
          created_at: now,
          updated_at: now,
        };
        d.memory_types.push(row);
        audit(access, "memory_type.created", { type: "memory_type", id: row.id, name: row.name });
        return created(toMemoryType(row));
      }),
    ),

    http.get(
      u("/v1/organizations/:org/memory-types/:memory_type"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        return ok(toMemoryType(findMemoryType(access.org.id, params)));
      }),
    ),

    http.patch(
      u("/v1/organizations/:org/memory-types/:memory_type"),
      route(async ({ request, params }) => {
        const access = consoleOrgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "change memory types");
        const row = findMemoryType(access.org.id, params);
        const input = await body(
          request,
          z.object({ name: typeName.optional(), settings: settingsInput.optional(), writes: writesInput.optional() }),
        );
        const d = db();
        // Everything is checked before anything changes. The kind never does.
        const settings = input.settings === undefined ? row.settings : checkedSettings(d, row.kind, input.settings);
        if (input.name !== undefined) assertNameFree(d, access.org.id, input.name, row.id);
        row.name = input.name ?? row.name;
        row.settings = settings;
        row.writes = input.writes ?? row.writes;
        row.updated_at = new Date().toISOString();
        audit(access, "memory_type.updated", { type: "memory_type", id: row.id, name: row.name });
        return ok(toMemoryType(row));
      }),
    ),

    http.delete(
      u("/v1/organizations/:org/memory-types/:memory_type"),
      route(({ request, params }) => {
        const access = consoleOrgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "delete memory types");
        const row = findMemoryType(access.org.id, params);
        const d = db();
        assertMemoryTypeUnused(d, row);
        d.memory_types = d.memory_types.filter((t) => t !== row);
        // Its memories go with it, in every workspace, without a `memory.forgotten` for each.
        d.memories = d.memories.filter(
          (m) => !(m.organization_id === row.organization_id && m.memory_type_id === row.id),
        );
        audit(access, "memory_type.deleted", { type: "memory_type", id: row.id, name: row.name });
        return noContent();
      }),
    ),

    http.get(
      u("/v1/organizations/:org/default-memory-type"),
      route(({ request, params }) => {
        const access = orgAccess(request, param(params, "org"));
        return ok(toDefaultMemoryType(access.org.id));
      }),
    ),

    http.put(
      u("/v1/organizations/:org/default-memory-type"),
      route(async ({ request, params }) => {
        const access = consoleOrgAccess(request, param(params, "org"));
        requireRole(access, ADMINS, "choose the default memory type");
        const input = await body(
          request,
          z.object({ memory_type_id: z.string("Choose a memory type, or null for none.").nullable() }),
        );
        const d = db();
        const chosen = input.memory_type_id === null ? null : memoryTypeIn(d, access.org.id, input.memory_type_id);
        if (chosen === undefined) throw unknownMemoryType();
        const fields = {
          memory_type_id: chosen?.id ?? null,
          updated_at: new Date().toISOString(),
          updated_by_user_id: access.user.id,
        };
        const row = d.default_memory_types.find((r) => r.organization_id === access.org.id);
        if (row) Object.assign(row, fields);
        else d.default_memory_types.push({ organization_id: access.org.id, ...fields });
        audit(access, "default_memory_type.updated", {
          type: "default_memory_type",
          id: chosen?.id ?? null,
          name: chosen?.name ?? "None",
        });
        return ok(toDefaultMemoryType(access.org.id));
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/agents/:agent/memories"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const agent = agentIn(access, params);
        const url = new URL(request.url);
        const d = db();
        const named = url.searchParams.get("memory_type");
        const type = named === null ? currentTypeOf(d, agent) : memoryTypeIn(d, access.org.id, named);
        if (named !== null && !type) throw unknownMemoryType("memory_type");
        // No type to list: an agent whose memory is off, or on a default the organization hasn't chosen.
        if (!type) return emptyPage();
        const kind = kindFor(d, type, "list");
        // The cursor first, as the backend's store reads it, then the words.
        const cursor = url.searchParams.get("cursor");
        const after = cursor === null ? null : decodeSaved(cursor);
        const q = trimmed(url.searchParams.get("q") ?? "");
        const words = q === "" ? null : kind.words(q);
        if (words?.length === 0) return emptyPage();
        const limit = clampedInt(url, "limit", 25, 100);
        // Every one the agent keeps in the type, those saved in a direct conversation too.
        const all = d.memories
          .filter((m) => keptBy(agent)(m) && m.memory_type_id === type.id)
          .filter((m) => words === null || kind.holds(m.text, words))
          .sort((a, b) => compareSaved(savedPlace(a), savedPlace(b)));
        const rest = after ? all.filter((m) => compareSaved(savedPlace(m), after) > 0) : all;
        const page = rest.slice(0, limit);
        const last = page.at(-1);
        const next = rest.length > limit && last ? encodeSaved(savedPlace(last)) : null;
        return ok(page.map(toMemory), { meta: { pagination: { next_cursor: next, total: all.length } } });
      }),
    ),

    http.post(
      u("/v1/workspaces/:ws/agents/:agent/memories"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "save memories");
        assertConsoleMayAct(access, "save memories");
        const agent = agentIn(access, params);
        const input = await body(
          request,
          z.object({
            text: z.string("Write the memory."),
            // Unchecked here on purpose: finding the type is the check.
            memory_type_id: z.string("Choose a memory type of this organization.").optional(),
          }),
        );
        const d = db();
        const named = input.memory_type_id;
        const type = named === undefined ? currentTypeOf(d, agent) : memoryTypeIn(d, access.org.id, named);
        if (named !== undefined && !type) throw unknownMemoryType();
        if (!type) {
          throw new ApiFailure(
            422,
            "memory_off",
            `${agent.name} has no memory to save to: its memory is off, or the organization has no default memory type. Name a memory type.`,
          );
        }
        const kind = kindFor(d, type, "add");
        const text = keptText(kind, input.text);
        const scope = d.memories.filter((m) => keptBy(agent)(m) && m.memory_type_id === type.id);
        const limit = kind.limit(type.settings);
        if (scope.length >= limit) {
          throw new ApiFailure(
            422,
            "memory_full",
            `${agent.name} keeps ${limit} memories at most in ${type.name}. Forget one, or change one instead.`,
          );
        }
        const shortId = freeShortId(new Set(scope.map((m) => m.short_id)));
        if (shortId === null) {
          throw new ApiFailure(503, "unavailable", "The service is unavailable right now. Try again shortly.");
        }
        const now = new Date().toISOString();
        const row: MemoryRow = {
          id: newId("memory"),
          organization_id: access.org.id,
          workspace_id: access.workspace.id,
          agent_id: agent.id,
          memory_type_id: type.id,
          short_id: shortId,
          text,
          // From the API or the Console: for the whole workspace, never one conversation.
          channel_conversation_id: null,
          origin: originOf(access),
          created_at: now,
          updated_at: now,
        };
        d.memories.push(row);
        announceMemory("memory.saved", row, now);
        audit(access, "memory.saved", memoryTarget(agent, row), { workspace: access.workspace });
        return created(toMemory(row));
      }),
    ),

    http.get(
      u("/v1/workspaces/:ws/agents/:agent/memories/:memory"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        const { row, type, kind } = memoryOf(db(), agentIn(access, params), params);
        assertManages(type, kind, "list");
        return ok(toMemory(row));
      }),
    ),

    http.patch(
      u("/v1/workspaces/:ws/agents/:agent/memories/:memory"),
      route(async ({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "change memories");
        assertConsoleMayAct(access, "change memories");
        const agent = agentIn(access, params);
        // The memory first, then what its kind lets people do, then the text.
        const { row, type, kind } = memoryOf(db(), agent, params);
        assertManages(type, kind, "change");
        const input = await body(request, z.object({ text: z.string("Write the memory.") }));
        const text = keptText(kind, input.text);
        // The text it holds already changes nothing: not when, nor who wrote it.
        if (text === row.text) return ok(toMemory(row));
        const now = new Date().toISOString();
        Object.assign(row, { text, origin: originOf(access), updated_at: now });
        announceMemory("memory.updated", row, now);
        audit(access, "memory.updated", memoryTarget(agent, row), { workspace: access.workspace });
        return ok(toMemory(row));
      }),
    ),

    http.delete(
      u("/v1/workspaces/:ws/agents/:agent/memories/:memory"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "forget memories");
        assertConsoleMayAct(access, "forget memories");
        const agent = agentIn(access, params);
        const d = db();
        const { row, type, kind } = memoryOf(d, agent, params);
        assertManages(type, kind, "forget");
        d.memories = d.memories.filter((m) => m !== row);
        announceMemory("memory.forgotten", row, new Date().toISOString());
        audit(access, "memory.forgotten", memoryTarget(agent, row), { workspace: access.workspace });
        return noContent();
      }),
    ),

    http.delete(
      u("/v1/workspaces/:ws/agents/:agent/memories"),
      route(({ request, params }) => {
        const access = workspaceAccess(request, param(params, "ws"));
        requireRole(access, DEVELOPERS, "forget memories");
        assertConsoleMayAct(access, "forget memories");
        const agent = agentIn(access, params);
        const d = db();
        // An erasure: every type's, of a kind this server serves, whatever the kind lets people do. A kind it
        // doesn't serve keeps its memories out of reach.
        const reached = d.memories.filter(keptBy(agent)).flatMap((m) => reach(d, m) ?? []);
        const types = [...new Map(reached.map((r) => [r.type.id, r.type])).values()].sort((a, b) =>
          comparePlaces(placeOf(a), placeOf(b)),
        );
        const now = new Date().toISOString();
        // A type at once, as its kind forgets it: each memory, oldest first, then how many.
        for (const type of types) {
          const forgotten = reached
            .filter((r) => r.type.id === type.id)
            .map((r) => r.row)
            .sort((a, b) => compareNames(a.created_at, b.created_at) || compareNames(a.id, b.id));
          d.memories = d.memories.filter((m) => !forgotten.includes(m));
          for (const row of forgotten) announceMemory("memory.forgotten", row, now);
          const count = `${forgotten.length} ${forgotten.length === 1 ? "memory" : "memories"}`;
          audit(
            access,
            "agent.memory_forgotten",
            { type: "agent", id: agent.id, name: `${agent.name} · ${type.name} · ${count}` },
            { workspace: access.workspace },
          );
        }
        return noContent();
      }),
    ),
  ];
}
