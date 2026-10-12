/**
 * Scope checks shared by the mock's handlers, serializers and run engine.
 * No dependencies, so the run engine and seed can use them without pulling in
 * the HTTP layer.
 */
import type { AppId, Environment, OrganizationId, WorkspaceId } from "@zupflow/sdk";
import type { AppRow, ConfigurationRow, Db, InstallationShortIdRow, RunEventRow, RunScope } from "./model";
import type { Rng } from "./random";

export type Scoped = { organization_id: OrganizationId; workspace_id: WorkspaceId };

/** Whether a row belongs to this workspace, checked at both layers: organization and workspace. */
export function inWorkspace(row: Scoped, workspace: { id: WorkspaceId; organization_id: OrganizationId }) {
  return row.organization_id === workspace.organization_id && row.workspace_id === workspace.id;
}

/** Whether two rows share an organization and a workspace. */
export function sameWorkspace(a: Scoped, b: Scoped) {
  return a.organization_id === b.organization_id && a.workspace_id === b.workspace_id;
}

/**
 * The app a workspace-scoped row refers to, within that row's own organization
 * and workspace: an organization app, or that workspace's own app. Never by
 * the app's ID alone.
 */
export function appIn(apps: readonly AppRow[], row: Scoped & { app_id: AppId }) {
  return apps.find(
    (a) =>
      a.organization_id === row.organization_id &&
      a.id === row.app_id &&
      (a.owner_workspace_id === null || a.owner_workspace_id === row.workspace_id),
  );
}

/**
 * An app's configuration in one environment, matched on the app's whole
 * scope: its organization, and its workspace for a workspace app (null for an
 * organization app).
 */
export function configurationOf(
  configurations: readonly ConfigurationRow[],
  app: Pick<AppRow, "id" | "organization_id" | "owner_workspace_id">,
  environment: Environment,
) {
  return configurations.find(
    (c) =>
      c.organization_id === app.organization_id &&
      c.workspace_id === app.owner_workspace_id &&
      c.app_id === app.id &&
      c.environment === environment,
  );
}

/** Where a run's timeline is stored: under its full scope, never under the run's ID alone. */
export function runEventsKey(scope: RunScope) {
  return `${scope.organization_id}/${scope.workspace_id}/${scope.id}`;
}

/** A run's timeline: the rows under its scoped key that also carry its scope and ID. */
export function timelineOf(db: Db, scope: RunScope): RunEventRow[] {
  return (db.run_events[runEventsKey(scope)] ?? []).filter((e) => sameWorkspace(e, scope) && e.run_id === scope.id);
}

const SHORT_ID_ALPHABET = "abcdefghijklmnopqrstuvwxyz234567";

/** How many short IDs an issue draws before giving up, as the backend does. */
export const SHORT_ID_DRAWS = 8;

/**
 * Issues a new installation short ID in one workspace (decision 0047): six
 * lowercase base32 characters never issued there before, recorded under the
 * workspace's full scope so it is never issued again, even after the
 * installation is deleted. Gives up after `SHORT_ID_DRAWS` taken draws
 * rather than reuse one.
 */
export function issueShortId(issued: InstallationShortIdRow[], scope: Scoped, rng: Rng = Math.random): string {
  for (let draw = 0; draw < SHORT_ID_DRAWS; draw++) {
    let shortId = "";
    for (let i = 0; i < 6; i++) shortId += SHORT_ID_ALPHABET[Math.floor(rng() * SHORT_ID_ALPHABET.length)];
    if (issued.some((row) => sameWorkspace(row, scope) && row.short_id === shortId)) continue;
    issued.push({
      organization_id: scope.organization_id,
      workspace_id: scope.workspace_id,
      short_id: shortId,
    });
    return shortId;
  }
  throw new Error("No new short ID could be issued in this workspace.");
}
