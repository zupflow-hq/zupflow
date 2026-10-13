import type { Environment, ModelId, Organization } from "@zupflow/sdk";
import { Skeleton } from "@/components/ui/skeleton";
import { useModelSetup } from "@/features/models/model-setup";
import { useModel } from "@/features/queries";
import { modelLabel } from "./labels";

/**
 * The model an agent runs on, as a short line: its override with the route,
 * or "Default (Claude Sonnet 4.5)", or that it has none. A failed lookup says
 * so instead of loading forever.
 */
export function AgentModelName({
  org,
  environment,
  modelId,
}: {
  org: Organization;
  environment: Environment;
  modelId: ModelId | null;
}) {
  const setup = useModelSetup(org, environment);
  const model = useModel(org.id, modelId);
  if (modelId) {
    if (model.data) return <span>{modelLabel(model.data)}</span>;
    if (model.isError) return <span>Model not found</span>;
    return <Skeleton className="inline-block h-3.5 w-28 align-middle" />;
  }
  if (setup.defaultModel) return <span>Default ({setup.defaultModel.name})</span>;
  if (setup.isError) return <span>Default model</span>;
  if (setup.isPending) return <Skeleton className="inline-block h-3.5 w-28 align-middle" />;
  return <span>No model</span>;
}
