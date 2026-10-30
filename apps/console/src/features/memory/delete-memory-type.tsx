import type { MemoryType, Organization, ZupflowApiError } from "@zupflow/sdk";
import { useEffect } from "react";
import { toast } from "sonner";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { useDeleteMemoryType } from "@/features/queries";
import { isApiError } from "@/lib/errors";
import { listed, memoryTypeRefusal } from "./labels";

/**
 * Deletes a memory type, once its name is typed. The server refuses while the
 * type is the default or an agent's (`409 memory_type_in_use`), naming what
 * holds it: the dialog stays open and says what to change first.
 */
export function DeleteMemoryTypeDialog({
  org,
  type,
  open,
  onOpenChange,
}: {
  org: Organization;
  type: MemoryType;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const remove = useDeleteMemoryType(org.id);
  const { reset } = remove;
  useEffect(() => {
    if (open) reset();
  }, [open, reset]);
  const inUse = isApiError(remove.error, "memory_type_in_use") ? remove.error : null;
  const holders: string[] = [];
  if (type.is_default) holders.push("it's the organization's default");
  if (type.agent_count > 0)
    holders.push(`${type.agent_count} ${type.agent_count === 1 ? "agent uses" : "agents use"} it`);

  return (
    <ConfirmDialog
      open={open}
      onOpenChange={onOpenChange}
      title={`Delete ${type.name}?`}
      description={
        <>
          <p>
            Its memories, in every workspace of Sandbox and Production, are forgotten with it. This can't be undone.
          </p>
          {inUse ? (
            <InUse name={type.name} error={inUse} />
          ) : remove.isError ? (
            <p
              role="alert"
              className="rounded-lg border border-destructive/30 bg-danger-soft/50 p-3 text-left text-foreground"
            >
              {memoryTypeRefusal(remove.error)}
            </p>
          ) : holders.length > 0 ? (
            <p>It's in use: {listed(holders)}. Deleting is refused until nothing holds it.</p>
          ) : null}
        </>
      }
      confirmText={type.name}
      actionLabel="Delete memory type"
      pending={remove.isPending}
      onConfirm={() =>
        remove.mutate(type.id, {
          onSuccess: () => {
            onOpenChange(false);
            toast.success(`Deleted ${type.name}`);
          },
        })
      }
    />
  );
}

/** What holds a type the server wouldn't delete, as its refusal names them, with what to change first. */
function InUse({ name, error }: { name: string; error: ZupflowApiError }) {
  const fields = error.fields ?? {};
  const isDefault = fields.default_memory_type !== undefined;
  const agents = fields.agents;
  return (
    <div
      role="alert"
      className="space-y-2 rounded-lg border border-destructive/30 bg-danger-soft/50 p-3 text-left text-foreground"
    >
      <p className="font-medium">{name} is in use, so it wasn't deleted.</p>
      {isDefault || agents ? (
        <ul className="list-disc space-y-1 pl-5">
          {isDefault ? (
            <li>It's the organization's default memory type. Choose another default, or none, first.</li>
          ) : null}
          {agents ? (
            <li>Agents use it: {agents}. In each agent's Configuration tab, choose another memory first.</li>
          ) : null}
        </ul>
      ) : (
        <p>{error.message}</p>
      )}
    </div>
  );
}
