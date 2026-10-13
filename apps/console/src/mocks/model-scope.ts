/**
 * Which model an agent runs on, and what holds a model in use. Every lookup
 * names the organization and the environment: a model is never found by its
 * ID alone, and one environment's default never stands in for the other's.
 * No dependencies on the HTTP layer, so the run engine and the seed can use
 * them.
 */
import type { Environment, ModelId, OrganizationId } from "@zupflow/sdk";
import type { AgentRow, Db, ModelConnectionRow, ModelRow, RunModelRow, RunRow, WorkspaceRow } from "./model";

/** A model of this organization in this environment. */
export function modelIn(
  db: Db,
  organizationId: OrganizationId,
  environment: Environment,
  modelId: string,
): ModelRow | undefined {
  return db.models.find(
    (m) => m.organization_id === organizationId && m.environment === environment && m.id === modelId,
  );
}

/** The environment's default model, if one is chosen and still there. */
export function defaultModelOf(db: Db, organizationId: OrganizationId, environment: Environment): ModelRow | undefined {
  const row = db.default_models.find((d) => d.organization_id === organizationId && d.environment === environment);
  return row?.model_id ? modelIn(db, organizationId, environment, row.model_id) : undefined;
}

/** The workspace an agent belongs to, within the agent's own organization. */
export function workspaceOf(
  db: Db,
  agent: Pick<AgentRow, "organization_id" | "workspace_id">,
): WorkspaceRow | undefined {
  return db.workspaces.find((w) => w.organization_id === agent.organization_id && w.id === agent.workspace_id);
}

/**
 * The model an agent's run uses: its override, or its workspace's
 * environment's default. Undefined when it has neither: a run is refused then,
 * never given a model silently.
 */
export function effectiveModel(
  db: Db,
  agent: Pick<AgentRow, "organization_id" | "workspace_id" | "model_id">,
): ModelRow | undefined {
  const workspace = workspaceOf(db, agent);
  if (!workspace) return undefined;
  return agent.model_id
    ? modelIn(db, agent.organization_id, workspace.environment, agent.model_id)
    : defaultModelOf(db, agent.organization_id, workspace.environment);
}

/** Agents of the organization, in workspaces of this environment, that override the default with one of `modelIds`. */
export function agentsUsing(
  db: Db,
  organizationId: OrganizationId,
  environment: Environment,
  modelIds: ReadonlySet<ModelId>,
): { agent: AgentRow; workspace: WorkspaceRow }[] {
  const workspaces = new Map(
    db.workspaces
      .filter((w) => w.organization_id === organizationId && w.environment === environment)
      .map((w) => [w.id, w]),
  );
  return db.agents.flatMap((agent) => {
    const workspace = workspaces.get(agent.workspace_id);
    return agent.organization_id === organizationId && workspace && agent.model_id && modelIds.has(agent.model_id)
      ? [{ agent, workspace }]
      : [];
  });
}

/** What a new run of this agent records as its model; undefined when it has none, and the run is refused. */
export function runModelFor(
  db: Db,
  agent: Pick<AgentRow, "organization_id" | "workspace_id" | "model_id">,
): RunModelRow | undefined {
  const model = effectiveModel(db, agent);
  return model
    ? {
        model_id: model.id,
        connection_id: model.connection_id,
        provider_model_id: model.provider_model_id,
        key_version: null,
      }
    : undefined;
}

/**
 * The run's own model and connection, found in the run's organization and its
 * workspace's environment. Undefined once either was removed.
 */
export function runRoute(
  db: Db,
  run: Pick<RunRow, "organization_id" | "workspace_id" | "model">,
): { model: ModelRow; connection: ModelConnectionRow } | undefined {
  const workspace = workspaceOf(db, run);
  if (!workspace) return undefined;
  const model = modelIn(db, run.organization_id, workspace.environment, run.model.model_id);
  const connection = db.model_connections.find(
    (c) =>
      c.organization_id === run.organization_id &&
      c.environment === workspace.environment &&
      c.id === run.model.connection_id,
  );
  return model && connection && model.connection_id === connection.id ? { model, connection } : undefined;
}
