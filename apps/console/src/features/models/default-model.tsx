import type { Environment, Organization } from "@zupflow/sdk";
import { toast } from "sonner";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { QueryError, RelativeTime } from "@/components/app/misc";
import { Panel } from "@/components/app/page";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister } from "@/features/context";
import { useDefaultModel, useModelPage, useSetDefaultModel } from "@/features/queries";
import { toastError } from "@/lib/errors";
import { modelLabel } from "./labels";
import { ModelPicker } from "./model-picker";

/**
 * The model agents in this environment run on unless they choose another.
 * Admins pick it from the environment's models; others see it.
 */
export function DefaultModelPanel({ org, environment }: { org: Organization; environment: Environment }) {
  const current = useDefaultModel(org.id, environment);
  // One row is enough to know whether there's anything to choose; the picker searches the rest.
  const models = useModelPage(org.id, { environment, limit: 1 });
  const set = useSetDefaultModel(org.id, environment);
  const admin = canAdminister(org);
  const env = ENVIRONMENT_LABEL[environment];

  if (current.isError || models.isError) {
    return (
      <QueryError
        error={current.error ?? models.error}
        onRetry={() => {
          void current.refetch();
          void models.refetch();
        }}
        what="the default model"
      />
    );
  }
  const chosen = current.data?.model ?? null;
  const items = models.data?.items ?? [];

  return (
    <Panel className="flex flex-col gap-4 p-4 sm:flex-row sm:items-center">
      <div className="min-w-0 flex-1 space-y-1">
        <p className="text-sm font-medium">Default model</p>
        <p className="text-sm text-muted-foreground">
          Agents in {env} run on it unless they choose another model.{" "}
          {current.data?.updated_at ? (
            <>
              Changed{current.data.updated_by ? ` by ${current.data.updated_by.name}` : ""}{" "}
              <RelativeTime iso={current.data.updated_at} />.
            </>
          ) : null}
        </p>
      </div>
      <div className="flex w-full items-center gap-2 sm:w-80">
        {current.isPending || models.isPending ? (
          <Skeleton className="h-9 w-full" />
        ) : admin && items.length > 0 ? (
          <>
            <ModelPicker
              orgId={org.id}
              environment={environment}
              label="Default model"
              value={chosen?.id ?? null}
              selected={chosen}
              none={<span className="text-muted-foreground">No default</span>}
              disabled={set.isPending}
              onChange={(next) => {
                if (next === (chosen?.id ?? null)) return;
                set.mutate(next, {
                  onSuccess: (d) =>
                    toast.success(d.model ? `${d.model.name} is the ${env} default` : `${env} has no default model`),
                  onError: (e) => toastError(e, "Couldn't change the default model"),
                });
              }}
            />
            {set.isPending ? <Spinner /> : null}
          </>
        ) : (
          <p className="text-sm">
            {chosen ? (
              modelLabel(chosen)
            ) : (
              <span className="text-muted-foreground">
                {items.length === 0 ? "Add a model to choose a default." : "None chosen"}
              </span>
            )}
          </p>
        )}
      </div>
    </Panel>
  );
}
