import { zodResolver } from "@hookform/resolvers/zod";
import { useNavigate } from "@tanstack/react-router";
import type { Agent, Organization, Workspace } from "@zupflow/sdk";
import { AlertTriangleIcon } from "lucide-react";
import { useEffect } from "react";
import { useForm } from "react-hook-form";
import { z } from "zod";
import { EnvBadge } from "@/components/app/environment";
import { Button } from "@/components/ui/button";
import { Form, FormControl, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Sheet, SheetContent, SheetDescription, SheetFooter, SheetHeader, SheetTitle } from "@/components/ui/sheet";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { NoModelNotice, useModelSetup } from "@/features/models/model-setup";
import { useCreateRun } from "@/features/queries";
import { applyFieldErrors, toastError } from "@/lib/errors";

const schema = z.object({
  input: z.string().trim().min(1, "Write a message for the agent.").max(8000),
});

export function StartRunSheet({
  org,
  workspace,
  agent,
  open,
  onOpenChange,
}: {
  org: Organization;
  workspace: Workspace;
  agent: Agent;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const navigate = useNavigate();
  const create = useCreateRun(workspace.id, agent.id);
  const setup = useModelSetup(org, workspace.environment);
  // With no model of its own and no default, a run is refused; say so before anyone types a message.
  const noModel = !agent.model_id && !setup.isPending && !setup.isError && !setup.defaultModel;
  const form = useForm<z.infer<typeof schema>>({
    resolver: zodResolver(schema),
    defaultValues: { input: "" },
  });
  useEffect(() => {
    if (!open) form.reset();
  }, [open, form]);

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent className="w-full gap-0 sm:max-w-md">
        <SheetHeader className="border-b">
          <SheetTitle>Start a run</SheetTitle>
          <SheetDescription className="flex flex-wrap items-center gap-1.5">
            {agent.name} in {workspace.name} <EnvBadge environment={workspace.environment} size="sm" />
          </SheetDescription>
        </SheetHeader>
        <Form {...form}>
          <form
            id="start-run"
            className="flex-1 space-y-5 overflow-y-auto p-4"
            onSubmit={form.handleSubmit((values) =>
              create.mutate(
                { input: values.input },
                {
                  onSuccess: (run) => {
                    onOpenChange(false);
                    void navigate({
                      to: "/$org/w/$ws/runs/$runId",
                      params: { org: org.slug, ws: workspace.id, runId: run.id },
                    });
                  },
                  onError: (error) => {
                    if (!applyFieldErrors(form, error)) toastError(error, "Couldn't start the run");
                  },
                },
              ),
            )}
          >
            {noModel ? (
              <NoModelNotice
                org={org}
                environment={workspace.environment}
                reason={setup.empty ? "empty" : "no_default"}
              />
            ) : null}
            {workspace.environment === "production" ? (
              <p className="flex gap-2 rounded-lg border border-production/40 bg-production-soft/50 p-3 text-sm">
                <AlertTriangleIcon className="mt-0.5 size-4 shrink-0 text-production-ink" />
                This is Production. Tool calls act on real connected accounts; approvals still apply.
              </p>
            ) : null}
            <FormField
              control={form.control}
              name="input"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Message</FormLabel>
                  <FormControl>
                    <Textarea
                      rows={6}
                      autoFocus
                      placeholder="Draft a post announcing the autumn release"
                      onKeyDown={(e) => {
                        if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                          e.preventDefault();
                          e.currentTarget.form?.requestSubmit();
                        }
                      }}
                      {...field}
                    />
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />
          </form>
        </Form>
        <SheetFooter className="flex-row justify-end border-t">
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" form="start-run" disabled={create.isPending || noModel}>
            {create.isPending ? <Spinner /> : null}
            Start run
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
