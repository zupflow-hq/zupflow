import { zodResolver } from "@hookform/resolvers/zod";
import type { Agent, CreateAgentInput, Environment, MemoryTypeId, ModelId, Workspace } from "@zupflow/sdk";
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
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { useOrg } from "@/features/context";
import { AgentMemoryNotice } from "@/features/memory/agent-memory";
import { MemoryTypePicker } from "@/features/memory/memory-type-picker";
import { ModelPicker } from "@/features/models/model-picker";
import { NoModelNotice, useModelSetup } from "@/features/models/model-setup";
import { useDefaultMemoryType } from "@/features/queries";

/** The model choice that means "no override": runs use the environment's default model. */
const DEFAULT_MODEL = "default";
/** The memory choices that aren't a memory type: the organization's default, or none. */
const DEFAULT_MEMORY = "default";
const NO_MEMORY = "off";

export const agentSchema = z.object({
  name: z.string().trim().min(1, "Name the agent.").max(60),
  description: z.string().trim().max(280),
  /** A model ID, or `DEFAULT_MODEL`. Named as the API names it, so its field errors land here. */
  model_id: z.string().min(1),
  /** A memory type ID, `DEFAULT_MEMORY` or `NO_MEMORY`. Named as the API names it, so its field errors land here. */
  memory_type_id: z.string().min(1),
  instructions: z.string().max(20_000),
});
export type AgentValues = z.infer<typeof agentSchema>;

/** What the API takes for the form's values. */
export function agentInput(values: AgentValues): CreateAgentInput {
  const { memory_type_id: memory, ...rest } = values;
  return {
    ...rest,
    model_id: values.model_id === DEFAULT_MODEL ? null : (values.model_id as ModelId),
    ...memoryInput(memory),
  };
}

/** The form's memory choice as the API takes it: `default` and `off` name no type. */
function memoryInput(choice: string): Pick<CreateAgentInput, "memory" | "memory_type_id"> {
  if (choice === DEFAULT_MEMORY) return { memory: "default", memory_type_id: null };
  if (choice === NO_MEMORY) return { memory: "off", memory_type_id: null };
  return { memory: "type", memory_type_id: choice as MemoryTypeId };
}

/** An agent's memory as the form's one choice. */
function memoryChoice(agent: Pick<Agent, "memory" | "memory_type_id">): string {
  if (agent.memory === "type" && agent.memory_type_id) return agent.memory_type_id;
  return agent.memory === "off" ? NO_MEMORY : DEFAULT_MEMORY;
}

export function useAgentForm(agent?: Agent) {
  return useForm<AgentValues>({
    resolver: zodResolver(agentSchema),
    values: agent
      ? {
          name: agent.name,
          description: agent.description,
          model_id: agent.model_id ?? DEFAULT_MODEL,
          memory_type_id: memoryChoice(agent),
          instructions: agent.instructions,
        }
      : undefined,
    // No model or memory type is assumed: a new agent uses the organization's defaults until it chooses its own.
    defaultValues: {
      name: "",
      description: "",
      model_id: DEFAULT_MODEL,
      memory_type_id: DEFAULT_MEMORY,
      instructions: "",
    },
  });
}

/**
 * `disabled` locks the fields while a save is pending. `readOnly` shows them to someone who can't change
 * them: still focusable, selectable and scrollable, which disabled fields are not.
 */
export function AgentFields({
  form,
  environment,
  disabled,
  readOnly,
}: {
  form: UseFormReturn<AgentValues>;
  /** The agent's workspace's: it chooses among that environment's models only. */
  environment: Environment;
  disabled?: boolean;
  readOnly?: boolean;
}) {
  const org = useOrg();
  const setup = useModelSetup(org, environment);
  const defaultMemory = useDefaultMemoryType(org.id);
  const usesDefault = form.watch("model_id") === DEFAULT_MODEL;
  const memory = form.watch("memory_type_id");
  return (
    <fieldset disabled={disabled} className="min-w-0 space-y-5">
      <FormField
        control={form.control}
        name="name"
        render={({ field }) => (
          <FormItem className="min-w-0">
            <FormLabel>Name</FormLabel>
            <FormControl>
              <Input placeholder="Publishing assistant" autoComplete="off" readOnly={readOnly} {...field} />
            </FormControl>
            <FormMessage />
          </FormItem>
        )}
      />
      {/* What it runs on and what it remembers with, side by side. */}
      <div className="grid gap-4 sm:grid-cols-2">
        <FormField
          control={form.control}
          name="model_id"
          render={({ field }) => (
            <FormItem className="min-w-0">
              <FormLabel>Model</FormLabel>
              {setup.isPending ? (
                <Skeleton className="h-9 w-full" />
              ) : (
                // "Organization default (Claude Sonnet 4.5)" first, then the environment's models, searched as you type.
                <ModelPicker
                  orgId={org.id}
                  environment={environment}
                  label="Model"
                  value={field.value === DEFAULT_MODEL ? null : (field.value as ModelId)}
                  onChange={(next) => field.onChange(next ?? DEFAULT_MODEL)}
                  none={
                    <>
                      Organization default{" "}
                      <span className="text-muted-foreground">({setup.defaultModel?.name ?? "none chosen"})</span>
                    </>
                  }
                  disabled={disabled || readOnly || (setup.empty && field.value === DEFAULT_MODEL)}
                />
              )}
              <FormMessage />
            </FormItem>
          )}
        />
        <FormField
          control={form.control}
          name="memory_type_id"
          render={({ field }) => (
            <FormItem className="min-w-0">
              <FormLabel>Memory</FormLabel>
              {defaultMemory.isPending ? (
                <Skeleton className="h-9 w-full" />
              ) : (
                // "Organization default (Notes)" first, then the organization's memory types, then off.
                <MemoryTypePicker
                  orgId={org.id}
                  label="Memory"
                  value={field.value}
                  onChange={field.onChange}
                  before={[
                    {
                      value: DEFAULT_MEMORY,
                      search: "organization default",
                      label: (
                        <>
                          Organization default{" "}
                          {defaultMemory.isError ? null : (
                            <span className="text-muted-foreground">
                              ({defaultMemory.data?.memory_type?.name ?? "none chosen"})
                            </span>
                          )}
                        </>
                      ),
                    },
                  ]}
                  after={[{ value: NO_MEMORY, search: "off none", label: "Off" }]}
                  disabled={disabled || readOnly}
                />
              )}
              <FormMessage />
            </FormItem>
          )}
        />
      </div>
      {/* Full width under the two fields, so the model and the memory stay aligned. */}
      {usesDefault && !setup.isPending && !setup.isError && !setup.defaultModel ? (
        <NoModelNotice org={org} environment={environment} reason={setup.empty ? "empty" : "no_default"} />
      ) : null}
      <AgentMemoryNotice org={org} choice={memory} />
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
      {/* Scrolls on a short screen, where a notice under the model or the memory can make it taller. */}
      <DialogContent className="max-h-[90dvh] overflow-y-auto sm:max-w-2xl">
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
            <AgentFields form={form} environment={workspace.environment} />
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
