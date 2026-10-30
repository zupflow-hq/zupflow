/**
 * The mock's memory: the kinds it serves, each organization's memory types
 * and its default type, and the memory each agent chooses. Types and the
 * default are the organization's, the same in both environments: every
 * member and API key reads them, and owners and admins change them, from the
 * Console only.
 *
 * Every lookup carries the organization: a type is never found by its ID
 * alone, and another organization's type is the same as one that doesn't
 * exist.
 */
import {
  AgentMemory,
  type DefaultMemoryType,
  type MemoryKind,
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
  audit,
  body,
  clampedInt,
  consoleOrgAccess,
  created,
  noContent,
  notFound,
  ok,
  orgAccess,
  param,
  requireRole,
  route,
  toUser,
} from "./http";
import { isId, newId } from "./ids";
import { KNOWN_MEMORY_KINDS, type KnownKind } from "./memory-kinds";
import type { AgentRow, Db, MemoryTypeRow } from "./model";
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

const unknownMemoryType = () =>
  new ApiFailure(422, "unknown_memory_type", "Choose a memory type of this organization.", {
    memory_type_id: "Choose a memory type of this organization.",
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
  ];
}
