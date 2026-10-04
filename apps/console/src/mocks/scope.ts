/**
 * Scope checks shared by the mock's handlers, serializers and run engine.
 * No dependencies, so the run engine and seed can use them without pulling in
 * the HTTP layer.
 */
import type { AppId, Environment, OrganizationId, WorkspaceId } from "@zupflow/sdk";
import type { AppRow, ConfigurationRow, Db, RunEventRow, RunScope } from "./model";

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
