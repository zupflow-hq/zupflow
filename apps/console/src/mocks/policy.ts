/**
 * The mock's tool policy (decision 0040): one policy per organization, and
 * the organization's level for each tool of its catalog apps, by name. A
 * level is suggested from a tool's hints when an installation first lists it
 * and is in force only once a person confirms it; until then, and for a
 * changed tool, the policy's `new_tool_level` applies. A workspace app's
 * tools have no organization level: the grant alone decides.
 */
import type {
  AppId,
  AppToolLevel,
  NewToolLevel,
  OrganizationId,
  ToolEffect,
  ToolLevel,
  ToolPolicy,
  UserId,
} from "@zupflow/sdk";
import { ApiFailure } from "./http";
import type { AppRow, AppToolLevelRow, Db, ToolPolicyRow } from "./model";

/** What an organization that never saved its policy reads. */
export const DEFAULT_POLICY = { mode: "manual", new_tool_level: "approve" } as const;

export function policyRowOf(db: Db, organizationId: OrganizationId): ToolPolicyRow | undefined {
  return db.tool_policies.find((p) => p.organization_id === organizationId);
}

export function newToolLevelOf(db: Db, organizationId: OrganizationId): NewToolLevel {
  return policyRowOf(db, organizationId)?.new_tool_level ?? DEFAULT_POLICY.new_tool_level;
}

/** The organization's level rows for one of its apps, matched on the organization and the app. */
export function levelRowsOf(db: Db, organizationId: OrganizationId, appId: AppId): AppToolLevelRow[] {
  return db.app_tool_levels.filter((l) => l.organization_id === organizationId && l.app_id === appId);
}

/**
 * The level in force for one tool of an app: its confirmed level, else the
 * policy's level for new tools. `null` for a workspace app, which the
 * organization's levels don't cover.
 */
export function levelInForce(db: Db, app: AppRow, tool: string): ToolLevel | null {
  if (app.owner_workspace_id !== null) return null;
  const row = levelRowsOf(db, app.organization_id, app.id).find((l) => l.tool === tool);
  return row?.confirmed ? row.level : newToolLevelOf(db, app.organization_id);
}

/** A tool's suggested level from its hints: read-only runs, anything else asks. */
export const suggestedLevel = (effect: ToolEffect): ToolLevel => (effect === "read" ? "allow" : "approve");

/** Suggests a level for each listed tool the organization has none for yet; never changes an existing one. */
export function suggestLevels(
  db: Db,
  organizationId: OrganizationId,
  appId: AppId,
  listed: readonly { name: string; effect: ToolEffect }[],
  now: string,
) {
  const known = new Set(levelRowsOf(db, organizationId, appId).map((l) => l.tool));
  for (const tool of listed) {
    if (known.has(tool.name)) continue;
    known.add(tool.name);
    db.app_tool_levels.push({
      organization_id: organizationId,
      app_id: appId,
      tool: tool.name,
      level: suggestedLevel(tool.effect),
      confirmed: false,
      destructive: tool.effect === "destructive",
      updated_at: now,
      updated_by_user_id: null,
    });
  }
}

/** Sets levels by name, each confirmed by `userId`; answers the rows written. */
export function setLevels(
  db: Db,
  organizationId: OrganizationId,
  appId: AppId,
  levels: readonly { tool: string; level: ToolLevel }[],
  userId: UserId | null,
  now: string,
): AppToolLevelRow[] {
  const rows = levelRowsOf(db, organizationId, appId);
  return levels.map(({ tool, level }) => {
    let row = rows.find((l) => l.tool === tool);
    if (!row) {
      row = {
        organization_id: organizationId,
        app_id: appId,
        tool,
        level,
        confirmed: true,
        destructive: false,
        updated_at: now,
        updated_by_user_id: userId,
      };
      db.app_tool_levels.push(row);
    }
    row.level = level;
    row.confirmed = true;
    row.updated_at = now;
    row.updated_by_user_id = userId;
    return row;
  });
}

export function toToolPolicy(
  db: Db,
  organizationId: OrganizationId,
  toUser: (id: UserId) => ToolPolicy["updated_by"],
): ToolPolicy {
  const row = policyRowOf(db, organizationId);
  return {
    organization_id: organizationId,
    mode: row?.mode ?? DEFAULT_POLICY.mode,
    new_tool_level: row?.new_tool_level ?? DEFAULT_POLICY.new_tool_level,
    updated_at: row?.updated_at ?? null,
    updated_by: row?.updated_by_user_id ? toUser(row.updated_by_user_id) : null,
  };
}

export function toAppToolLevel(row: AppToolLevelRow, toUser: (id: UserId) => AppToolLevel["updated_by"]): AppToolLevel {
  return {
    app_id: row.app_id,
    tool: row.tool,
    level: row.level,
    confirmed: row.confirmed,
    destructive: row.destructive,
    updated_at: row.updated_at,
    updated_by: row.updated_by_user_id ? toUser(row.updated_by_user_id) : null,
  };
}

/** The most levels one change sets, and the longest tool name. */
const MAX_LEVELS = 100;
const MAX_TOOL_NAME = 128;
const LEVELS: readonly string[] = ["allow", "approve", "deny"];

/**
 * `UpdateAppToolLevelsInput.levels`, checked item by item as the backend
 * checks it: every problem is reported under `levels`, the first one found.
 */
export function levelsInput(value: unknown): { tool: string; level: ToolLevel }[] {
  const refuse = (message: string): never => {
    throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", { levels: message });
  };
  if (value === undefined) refuse("Required.");
  if (!Array.isArray(value)) return refuse("Must be a list of levels.");
  if (value.length === 0) refuse("Set at least one level.");
  if (value.length > MAX_LEVELS) refuse("Set 100 levels or fewer at a time.");
  const levels: { tool: string; level: ToolLevel }[] = [];
  for (const item of value as unknown[]) {
    const record = typeof item === "object" && item !== null ? (item as Record<string, unknown>) : {};
    const tool = typeof record.tool === "string" ? record.tool : null;
    const level =
      typeof record.level === "string" && LEVELS.includes(record.level) ? (record.level as ToolLevel) : null;
    if (!tool) return refuse("Name the tool.");
    // Characters, as the backend counts them.
    if ([...tool].length > MAX_TOOL_NAME) return refuse("Use 128 characters or fewer.");
    if (!level) return refuse("Choose allow, approve or deny.");
    if (levels.some((l) => l.tool === tool)) return refuse("Name each tool once.");
    levels.push({ tool, level });
  }
  return levels;
}
