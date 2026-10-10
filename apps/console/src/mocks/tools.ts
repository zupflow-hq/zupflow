/**
 * The mock's tool registry (decision 0038): the tools an installation's MCP
 * server lists to it, kept per installation. It mirrors the backend's rules:
 * a new name is pinned as listed; a known one listed with another definition
 * keeps it beside the pinned one as `latest`, changed and unreviewed; one
 * listed as pinned again drops any latest; a name missing from a listing is
 * gone, and an installation keeps at most `MAX_GONE` gone tools, dropping
 * those gone longest. A listing never replaces a pinned definition or changes
 * an effect.
 *
 * The mock has no MCP servers, so discovery lists the app's own tool
 * definitions, at once.
 */
import type { InstallationTool, ToolDefinition, ToolEffect, ToolText } from "@zupflow/sdk";
import type { Db, InstallationRow, InstallationToolRow } from "./model";
import { suggestLevels } from "./policy";
import { appIn, sameWorkspace } from "./scope";

/** One tool of a listing, as the MCP client would hand it over. */
export interface ListedTool {
  name: string;
  effect: ToolEffect;
  text: ToolText;
}

type InstallationScope = Pick<InstallationRow, "id" | "organization_id" | "workspace_id">;

/** The most tools a listing records, and the most gone tools an installation keeps. */
export const MAX_GONE = 500;

/** The hints a server would give for an effect; the effect is read back from them. */
const HINTS: Record<ToolEffect, Record<string, unknown>> = {
  read: { readOnlyHint: true },
  write: { readOnlyHint: false, destructiveHint: false },
  destructive: { readOnlyHint: false, destructiveHint: true },
};

/** What an app's tool definition says, as its server would list it. */
export function listedFrom(tool: ToolDefinition): ListedTool {
  return {
    name: tool.name,
    effect: tool.effect,
    text: {
      title: tool.title,
      description: tool.description,
      input_schema: { type: "object" },
      output_schema: null,
      annotations: HINTS[tool.effect],
    },
  };
}

/** Byte order of UTF-8, as the backend sorts names. */
export function compareNames(a: string, b: string): number {
  const [x, y] = [new TextEncoder().encode(a), new TextEncoder().encode(b)];
  for (let i = 0; i < Math.min(x.length, y.length); i++) {
    const d = (x[i] ?? 0) - (y[i] ?? 0);
    if (d !== 0) return d;
  }
  return x.length - y.length;
}

/** JSON with every object's keys sorted, so equal definitions read the same. */
function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value !== null && typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>)
      .filter(([, v]) => v !== undefined)
      .sort(([a], [b]) => compareNames(a, b));
    return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

/**
 * A definition's fingerprint: 64 hex digits, stable for equal definitions.
 * The backend's is the SHA-256 of its canonical JSON; the mock's is eight
 * FNV-1a rounds, which is enough to tell definitions apart in a demo.
 */
export function fingerprint(text: ToolText): string {
  const json = canonical(text);
  let out = "";
  for (let round = 0; round < 8; round++) {
    let h = (2166136261 ^ Math.imul(round + 1, 0x9e3779b1)) >>> 0;
    for (let i = 0; i < json.length; i++) {
      h ^= json.charCodeAt(i);
      h = Math.imul(h, 16777619);
    }
    out += (h >>> 0).toString(16).padStart(8, "0");
  }
  return out;
}

/** Whether two definitions say the same, as the backend compares their hashes. */
const same = (a: ToolText, b: ToolText) => canonical(a) === canonical(b);

/** Rows of exactly this installation, matched on its full scope. */
const ofInstallation = (installation: InstallationScope) => (row: InstallationToolRow) =>
  sameWorkspace(row, installation) && row.installation_id === installation.id;

/** The installation's tools, by name. */
export function toolsOf(db: Db, installation: InstallationScope): InstallationToolRow[] {
  return db.installation_tools.filter(ofInstallation(installation)).sort((a, b) => compareNames(a.name, b.name));
}

/** Records a complete listing of the installation's tools. */
export function recordListing(db: Db, installation: InstallationScope, listed: readonly ListedTool[], now: string) {
  const known = toolsOf(db, installation);
  for (const tool of listed) {
    const existing = known.find((k) => k.name === tool.name);
    if (!existing) {
      db.installation_tools.push({
        organization_id: installation.organization_id,
        workspace_id: installation.workspace_id,
        installation_id: installation.id,
        name: tool.name,
        effect: tool.effect,
        pinned: tool.text,
        latest: null,
        first_seen_at: now,
        seen_at: now,
        changed_at: null,
        gone_at: null,
      });
      continue;
    }
    existing.gone_at = null;
    existing.seen_at = now;
    if (same(tool.text, existing.pinned)) {
      existing.latest = null;
      existing.changed_at = null;
    } else if (!existing.latest || !same(tool.text, existing.latest)) {
      existing.latest = tool.text;
      existing.changed_at = now;
    }
  }
  for (const existing of known) {
    if (existing.gone_at === null && !listed.some((tool) => tool.name === existing.name)) existing.gone_at = now;
  }
  // Keep the most recently gone; drop the rest, those gone longest.
  const gone = toolsOf(db, installation)
    .filter((t) => t.gone_at !== null)
    .sort((a, b) => (b.gone_at ?? "").localeCompare(a.gone_at ?? "") || compareNames(a.name, b.name));
  const dropped = new Set(gone.slice(MAX_GONE));
  if (dropped.size > 0) db.installation_tools = db.installation_tools.filter((t) => !dropped.has(t));
}

/**
 * Lists the installation's tools from its app, in the installation's own
 * scope. Only a connected installation lists anything.
 */
export function discoverTools(db: Db, installation: InstallationRow, now: string) {
  if (installation.status !== "connected" || !installation.connection) return;
  const app = appIn(db.apps, installation);
  if (!app) return;
  const listed = app.tools.map(listedFrom);
  recordListing(db, installation, listed, now);
  // As the backend's worker does after recording: suggest a level for each tool the organization has none for.
  if (app.owner_workspace_id === null) suggestLevels(db, installation.organization_id, app.id, listed, now);
}

/**
 * Accepts a changed tool's latest definition, which must be the one the
 * reviewer saw (`latestHash`): it becomes the pinned one. Answers the tool, or
 * why not.
 */
export function acceptChange(
  db: Db,
  installation: InstallationScope,
  name: string,
  latestHash: string,
): InstallationToolRow | "not_found" | "not_changed" | "stale" {
  const tool = toolsOf(db, installation).find((t) => t.name === name);
  if (!tool) return "not_found";
  if (tool.gone_at !== null || tool.latest === null) return "not_changed";
  if (fingerprint(tool.latest) !== latestHash) return "stale";
  tool.pinned = tool.latest;
  tool.latest = null;
  tool.changed_at = null;
  return tool;
}

/** Removes the installation's tools, with the installation. */
export function forgetTools(db: Db, installation: InstallationScope) {
  const mine = ofInstallation(installation);
  db.installation_tools = db.installation_tools.filter((row) => !mine(row));
  db.tool_refreshes = db.tool_refreshes.filter(
    (row) => !(sameWorkspace(row, installation) && row.installation_id === installation.id),
  );
}

export function toInstallationTool(row: InstallationToolRow): InstallationTool {
  return {
    name: row.name,
    effect: row.effect,
    status: row.gone_at !== null ? "gone" : row.latest !== null ? "changed" : "current",
    pinned: row.pinned,
    pinned_hash: fingerprint(row.pinned),
    latest: row.latest,
    latest_hash: row.latest ? fingerprint(row.latest) : null,
    first_seen_at: row.first_seen_at,
    seen_at: row.seen_at,
    changed_at: row.changed_at,
    gone_at: row.gone_at,
  };
}

/** How long after an accepted refresh the next one is refused. */
export const REFRESH_INTERVAL_MS = 60_000;

/**
 * Claims the installation's next refresh: true when none was accepted in the
 * last minute (and records this one), false otherwise.
 */
export function claimRefresh(db: Db, installation: InstallationScope, nowMs: number): boolean {
  const mine = (row: Db["tool_refreshes"][number]) =>
    sameWorkspace(row, installation) && row.installation_id === installation.id;
  const live = db.tool_refreshes.find((row) => mine(row) && Date.parse(row.until) > nowMs);
  if (live) return false;
  db.tool_refreshes = db.tool_refreshes.filter((row) => !mine(row) && Date.parse(row.until) > nowMs);
  db.tool_refreshes.push({
    organization_id: installation.organization_id,
    workspace_id: installation.workspace_id,
    installation_id: installation.id,
    until: new Date(nowMs + REFRESH_INTERVAL_MS).toISOString(),
  });
  return true;
}
