import type { NewToolLevel, Organization } from "@zupflow/sdk";
import { useEffect, useState } from "react";
import { toast } from "sonner";
import { ChoiceCards } from "@/components/app/choice-cards";
import { QueryError, RowsSkeleton } from "@/components/app/misc";
import { Panel, SectionHeader } from "@/components/app/page";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister } from "@/features/context";
import { useToolPolicy, useUpdateToolPolicy } from "@/features/queries";
import { toastError } from "@/lib/errors";

/**
 * The organization's tool policy: how calls that need approval are decided,
 * and the level of a tool nobody has set one for. Levels per tool are set on
 * each app's page.
 */
export function ToolPolicySettings({ org }: { org: Organization }) {
  const policy = useToolPolicy(org.id);
  const update = useUpdateToolPolicy(org.id);
  const admin = canAdminister(org);
  const [newToolLevel, setNewToolLevel] = useState<NewToolLevel | undefined>(undefined);
  useEffect(() => setNewToolLevel(policy.data?.new_tool_level), [policy.data?.new_tool_level]);

  return (
    <section>
      <SectionHeader
        title="Tool approvals"
        description="Applies to every workspace. Set each app tool's level on its app's page, and channel tools' below."
      />
      <Panel>
        {policy.isPending ? (
          <RowsSkeleton rows={2} columns={[60]} />
        ) : policy.isError ? (
          <QueryError error={policy.error} onRetry={() => policy.refetch()} what="the tool policy" className="m-3" />
        ) : (
          <div className="space-y-5 p-5">
            <div>
              <p className="text-sm font-medium">Who approves</p>
              <p className="mt-0.5 text-sm text-muted-foreground">
                A person, in your product: a call that needs approval waits until your backend sends their decision.
              </p>
            </div>
            <div className="space-y-2">
              <p className="text-sm font-medium" id="new-tool-level">
                New and changed tools
              </p>
              <p className="text-sm text-muted-foreground">
                For a tool whose level nobody has confirmed yet, and for a tool whose definition changed until someone
                accepts the change.
              </p>
              <ChoiceCards<NewToolLevel>
                label="New and changed tools"
                value={newToolLevel}
                onValueChange={setNewToolLevel}
                className="sm:grid-cols-2"
                options={[
                  {
                    value: "approve",
                    disabled: !admin,
                    content: (
                      <>
                        <span className="text-sm font-medium">Require approval</span>
                        <span className="text-xs text-muted-foreground">
                          They can be granted, and each call asks first.
                        </span>
                      </>
                    ),
                  },
                  {
                    value: "deny",
                    disabled: !admin,
                    content: (
                      <>
                        <span className="text-sm font-medium">Block</span>
                        <span className="text-xs text-muted-foreground">
                          They can't be granted or called until someone sets a level.
                        </span>
                      </>
                    ),
                  },
                ]}
              />
            </div>
            {admin ? (
              <Button
                disabled={!newToolLevel || newToolLevel === policy.data.new_tool_level || update.isPending}
                onClick={() =>
                  newToolLevel &&
                  update.mutate(
                    { mode: policy.data.mode, new_tool_level: newToolLevel },
                    {
                      onSuccess: () => toast.success("Saved tool approvals"),
                      onError: (e) => toastError(e, "Couldn't save"),
                    },
                  )
                }
              >
                {update.isPending ? <Spinner /> : null}
                Save
              </Button>
            ) : (
              <p className="text-sm text-muted-foreground">Only owners and admins can change these.</p>
            )}
          </div>
        )}
      </Panel>
    </section>
  );
}
