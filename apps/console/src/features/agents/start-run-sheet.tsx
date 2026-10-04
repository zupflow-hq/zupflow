import { zodResolver } from "@hookform/resolvers/zod";
import { useNavigate } from "@tanstack/react-router";
import type { Agent, Workspace } from "@zupflow/sdk";
import { AlertTriangleIcon } from "lucide-react";
import { useEffect } from "react";
import { useForm } from "react-hook-form";
import { z } from "zod";
import { EnvBadge } from "@/components/app/environment";
import { Button } from "@/components/ui/button";
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Sheet, SheetContent, SheetDescription, SheetFooter, SheetHeader, SheetTitle } from "@/components/ui/sheet";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { useCreateRun } from "@/features/queries";
import { applyFieldErrors, toastError } from "@/lib/errors";

const schema = z.object({
  input: z.string().trim().min(1, "Write a message for the agent.").max(8000),
  subject: z.string().trim().max(200),
});

export function StartRunSheet({
  orgSlug,
  workspace,
  agent,
  open,
  onOpenChange,
}: {
  orgSlug: string;
  workspace: Workspace;
  agent: Agent;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const navigate = useNavigate();
  const create = useCreateRun(workspace.id, agent.id);
  const form = useForm<z.infer<typeof schema>>({
    resolver: zodResolver(schema),
    defaultValues: { input: "", subject: "" },
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
                { input: values.input, subject: values.subject || null },
                {
                  onSuccess: (run) => {
                    onOpenChange(false);
                    void navigate({
                      to: "/$org/w/$ws/runs/$runId",
                      params: { org: orgSlug, ws: workspace.id, runId: run.id },
                    });
                  },
                  onError: (error) => {
                    if (!applyFieldErrors(form, error)) toastError(error, "Couldn't start the run");
                  },
                },
              ),
            )}
          >
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
            <FormField
              control={form.control}
              name="subject"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>
                    On behalf of <span className="font-normal text-muted-foreground">(optional)</span>
                  </FormLabel>
                  <FormControl>
                    <Input className="font-mono text-sm" placeholder="user_2f81c0aa" {...field} />
                  </FormControl>
                  <FormDescription>Your product's ID for the person this run acts for.</FormDescription>
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
          <Button type="submit" form="start-run" disabled={create.isPending}>
            {create.isPending ? <Spinner /> : null}
            Start run
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
