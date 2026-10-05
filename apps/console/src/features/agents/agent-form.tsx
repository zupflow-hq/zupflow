import { zodResolver } from "@hookform/resolvers/zod";
import type { Agent, Workspace } from "@zupflow/sdk";
import { useEffect } from "react";
import { type UseFormReturn, useForm } from "react-hook-form";
import { z } from "zod";
import { EnvBadge } from "@/components/app/environment";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { useModels } from "@/features/queries";

export const agentSchema = z.object({
  name: z.string().trim().min(1, "Name the agent.").max(60),
  description: z.string().trim().max(280),
  model: z.string().min(1, "Choose a model."),
  instructions: z.string().max(20_000),
});
export type AgentValues = z.infer<typeof agentSchema>;

export function useAgentForm(agent?: Agent) {
  return useForm<AgentValues>({
    resolver: zodResolver(agentSchema),
    values: agent
      ? {
          name: agent.name,
          description: agent.description,
          model: agent.model,
          instructions: agent.instructions,
        }
      : undefined,
    defaultValues: { name: "", description: "", model: "claude-sonnet-5-5", instructions: "" },
  });
}

/**
 * `disabled` locks the fields while a save is pending. `readOnly` shows them to someone who can't change
 * them: still focusable, selectable and scrollable, which disabled fields are not.
 */
export function AgentFields({
  form,
  disabled,
  readOnly,
}: {
  form: UseFormReturn<AgentValues>;
  disabled?: boolean;
  readOnly?: boolean;
}) {
  const models = useModels();
  return (
    <fieldset disabled={disabled} className="space-y-5">
      <div className="grid gap-4 sm:grid-cols-2">
        <FormField
          control={form.control}
          name="name"
          render={({ field }) => (
            <FormItem>
              <FormLabel>Name</FormLabel>
              <FormControl>
                <Input placeholder="Publishing assistant" autoComplete="off" readOnly={readOnly} {...field} />
              </FormControl>
              <FormMessage />
            </FormItem>
          )}
        />
        <FormField
          control={form.control}
          name="model"
          render={({ field }) => (
            <FormItem>
              <FormLabel>Model</FormLabel>
              <Select value={field.value} onValueChange={field.onChange} disabled={disabled || readOnly}>
                <FormControl>
                  <SelectTrigger className="w-full">
                    <SelectValue placeholder={models.isPending ? "Loading models…" : "Choose a model"} />
                  </SelectTrigger>
                </FormControl>
                <SelectContent>
                  {models.data?.map((m) => (
                    <SelectItem key={m.id} value={m.id}>
                      <span>{m.name}</span>
                      <span className="text-xs text-muted-foreground">{m.description}</span>
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <FormMessage />
            </FormItem>
          )}
        />
      </div>
      <FormField
        control={form.control}
        name="description"
        render={({ field }) => (
          <FormItem>
            <FormLabel>
              Description <span className="font-normal text-muted-foreground">(optional)</span>
            </FormLabel>
            <FormControl>
              <Input placeholder="What it's for, in one line" readOnly={readOnly} {...field} />
            </FormControl>
            <FormMessage />
          </FormItem>
        )}
      />
      <FormField
        control={form.control}
        name="instructions"
        render={({ field }) => (
          <FormItem>
            <FormLabel>Instructions</FormLabel>
            <FormControl>
              <Textarea
                rows={9}
                className="font-mono text-[13px] leading-relaxed"
                readOnly={readOnly}
                placeholder={
                  "You help the marketing team…\n\n- Read source material before drafting.\n- Never publish without an explicit request."
                }
                {...field}
              />
            </FormControl>
            <FormDescription>
              The agent's standing brief. Instructions can't grant tools — only grants can.
            </FormDescription>
            <FormMessage />
          </FormItem>
        )}
      />
    </fieldset>
  );
}

export function CreateAgentDialog({
  workspace,
  open,
  onOpenChange,
  pending,
  onCreate,
}: {
  workspace: Workspace;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  pending: boolean;
  onCreate: (values: AgentValues, form: UseFormReturn<AgentValues>) => void;
}) {
  const form = useAgentForm();
  useEffect(() => {
    if (!open) form.reset();
  }, [open, form]);
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Create agent</DialogTitle>
          <DialogDescription className="flex flex-wrap items-center gap-1.5">
            In <span className="font-medium text-foreground">{workspace.name}</span>
            <EnvBadge environment={workspace.environment} size="sm" />. It starts with no access; grant tools on its
            page.
          </DialogDescription>
        </DialogHeader>
        <Form {...form}>
          <form id="create-agent" onSubmit={form.handleSubmit((values) => onCreate(values, form))}>
            <AgentFields form={form} />
          </form>
        </Form>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" form="create-agent" disabled={pending}>
            {pending ? <Spinner /> : null}
            Create agent
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
