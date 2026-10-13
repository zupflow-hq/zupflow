import { Link } from "@tanstack/react-router";
import type { Environment, Model, Organization } from "@zupflow/sdk";
import { ArrowRightIcon, SparklesIcon } from "lucide-react";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { canAdminister } from "@/features/context";
import { envSearch } from "@/features/environment";
import { useDefaultModel, useModelPage } from "@/features/queries";
import { cn } from "@/lib/utils";

/**
 * What an agent in this environment can run on: the environment's default,
 * and whether the organization added any model there (one row is enough to
 * know; pickers search the rest). `empty` only when neither exists: no agent
 * there can run until someone adds a model.
 */
export function useModelSetup(org: Organization, environment: Environment) {
  const any = useModelPage(org.id, { environment, limit: 1 });
  const defaults = useDefaultModel(org.id, environment);
  const defaultModel = defaults.data?.model ?? null;
  return {
    defaultModel,
    isPending: any.isPending || defaults.isPending,
    isError: any.isError || defaults.isError,
    /** Neither a default nor any model: nothing to run on. */
    empty: Boolean(any.data && defaults.data && any.data.items.length === 0 && !defaultModel),
  };
}

/** Models grouped by the connection they're reached through, in the order connections first appear. */
export function groupByConnection(models: readonly Model[]): { label: string; models: Model[] }[] {
  const groups = new Map<string, { label: string; models: Model[] }>();
  for (const m of models) {
    const group = groups.get(m.route.connection_id) ?? { label: m.route.label, models: [] };
    group.models.push(m);
    groups.set(m.route.connection_id, group);
  }
  return [...groups.values()];
}

/**
 * Explains why nothing can run yet and links to where it's fixed. Shown in the
 * agent form and when starting a run.
 */
export function NoModelNotice({
  org,
  environment,
  reason = "empty",
  className,
}: {
  org: Organization;
  environment: Environment;
  /** `empty`: no models at all. `no_default`: models exist, but none is the default and the agent chose none. */
  reason?: "empty" | "no_default";
  className?: string;
}) {
  const env = ENVIRONMENT_LABEL[environment];
  const admin = canAdminister(org);
  const what =
    reason === "empty"
      ? `${org.name} has no models in ${env} yet. Agents run on models you add from a provider, with your own key.`
      : `${env} has no default model, and this agent doesn't choose one.`;
  const fix =
    reason === "empty"
      ? admin
        ? "Connect a provider, add a model and make it the default."
        : "Ask an admin to connect a provider and choose a default model."
      : admin
        ? "Choose a default model, or a model for this agent."
        : "Ask an admin to choose a default model, or choose a model for this agent.";
  return (
    <div className={cn("flex gap-3 rounded-lg border bg-muted/40 p-3 text-sm", className)}>
      <SparklesIcon className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
      <div className="min-w-0 space-y-1.5">
        <p>
          {what} {fix}
        </p>
        <Link
          to="/$org/model-providers"
          params={{ org: org.slug }}
          search={envSearch(environment)}
          className="inline-flex items-center gap-1 font-medium underline-offset-4 hover:underline"
        >
          Open model providers
          <ArrowRightIcon className="size-3.5" />
        </Link>
      </div>
    </div>
  );
}
