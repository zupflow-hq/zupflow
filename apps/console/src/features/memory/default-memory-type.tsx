import type { MemoryTypeId, Organization } from "@zupflow/sdk";
import { AlertTriangleIcon } from "lucide-react";
import { toast } from "sonner";
import { QueryError, RelativeTime } from "@/components/app/misc";
import { Panel } from "@/components/app/page";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister } from "@/features/context";
import { useDefaultMemoryType, useMemoryTypeList, useSetDefaultMemoryType } from "@/features/queries";
import { memoryTypeRefusal, notServed } from "./labels";
import { MemoryTypePicker } from "./memory-type-picker";

/** The picker's value for no default. */
const NONE = "none";

/**
 * The memory type agents on the default remember with, in every workspace of
 * Sandbox and Production. Owners and admins choose it from the organization's
 * types, or none; others see it. Without one, agents on the default have no
 * memory, and the panel says so.
 */
export function DefaultMemoryTypePanel({ org }: { org: Organization }) {
  const current = useDefaultMemoryType(org.id);
  // One row is enough to know whether there's anything to choose; the picker pages the rest.
  const types = useMemoryTypeList(org.id, 1);
  const set = useSetDefaultMemoryType(org.id);
  const admin = canAdminister(org);

  if (current.isError || types.isError) {
    return (
      <QueryError
        error={current.error ?? types.error}
        onRetry={() => {
          void current.refetch();
          void types.refetch();
        }}
        what="the default memory type"
      />
    );
  }
  const pending = current.isPending || types.isPending;
  const chosen = current.data?.memory_type ?? null;
  const anyType = (types.data?.pages[0]?.items.length ?? 0) > 0;

  return (
    <Panel className="overflow-hidden">
      <div className="flex flex-col gap-4 p-4 sm:flex-row sm:items-center">
        <div className="min-w-0 flex-1 space-y-1">
          <p className="text-sm font-medium">Default memory type</p>
          <p className="text-sm text-muted-foreground">
            Agents on the default remember with it, in every workspace of Sandbox and Production.{" "}
            {current.data?.updated_at ? (
              <>
                Changed{current.data.updated_by ? ` by ${current.data.updated_by.name}` : ""}{" "}
                <RelativeTime iso={current.data.updated_at} />.
              </>
            ) : null}
          </p>
        </div>
        <div className="flex w-full items-center gap-2 sm:w-80">
          {pending ? (
            <Skeleton className="h-9 w-full" />
          ) : admin && anyType ? (
            <>
              <MemoryTypePicker
                orgId={org.id}
                label="Default memory type"
                value={chosen?.id ?? NONE}
                selected={chosen}
                after={[
                  { value: NONE, label: <span className="text-muted-foreground">No default</span>, search: "none" },
                ]}
                disabled={set.isPending}
                onChange={(next) => {
                  const id = next === NONE ? null : (next as MemoryTypeId);
                  if (id === (chosen?.id ?? null)) return;
                  set.mutate(id, {
                    onSuccess: (d) =>
                      toast.success(
                        d.memory_type
                          ? `${d.memory_type.name} is the default memory type`
                          : "No default memory type: agents on the default have no memory",
                      ),
                    onError: (e) =>
                      toast.error("Couldn't change the default memory type", { description: memoryTypeRefusal(e) }),
                  });
                }}
              />
              {set.isPending ? <Spinner /> : null}
            </>
          ) : (
            <p className="min-w-0 truncate text-sm">
              {chosen ? (
                chosen.name
              ) : (
                <span className="text-muted-foreground">
                  {admin ? "Create a memory type to choose a default." : "None chosen"}
                </span>
              )}
            </p>
          )}
        </div>
      </div>
      {pending ? null : !chosen ? (
        <p className="flex items-start gap-2 border-t bg-warning-soft/40 px-4 py-3 text-sm">
          <AlertTriangleIcon className="mt-0.5 size-4 shrink-0 text-warning" />
          <span>
            No default memory type, so agents on the default have no memory.{" "}
            {admin
              ? anyType
                ? "Choose one to give them memory."
                : "Create a memory type, then choose it here."
              : "An owner or admin can choose one."}
          </span>
        </p>
      ) : !chosen.served ? (
        <p className="flex items-start gap-2 border-t bg-warning-soft/40 px-4 py-3 text-sm">
          <AlertTriangleIcon className="mt-0.5 size-4 shrink-0 text-warning" />
          <span>
            {notServed(chosen)} {admin ? "Choose a type of a kind this server serves." : ""}
          </span>
        </p>
      ) : null}
    </Panel>
  );
}
