import { zodResolver } from "@hookform/resolvers/zod";
import {
  type Environment,
  MODEL_PROVIDER_KINDS,
  MODEL_PROVIDERS,
  type ModelConnection,
  type ModelProviderKind,
  type Organization,
} from "@zupflow/sdk";
import { ExternalLinkIcon } from "lucide-react";
import { useEffect } from "react";
import { useForm } from "react-hook-form";
import { z } from "zod";
import { ChoiceCards } from "@/components/app/choice-cards";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { StatusPill } from "@/components/app/status";
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
import { Spinner } from "@/components/ui/spinner";
import { useCreateModelConnection, useReplaceModelConnectionKey } from "@/features/queries";
import { applyFieldErrors, toastError } from "@/lib/errors";

const OTHER_ENVIRONMENT: Record<Environment, Environment> = { sandbox: "production", production: "sandbox" };

const PROVIDER_BLURB: Record<ModelProviderKind, string> = {
  openai: "GPT models, straight from OpenAI.",
  anthropic: "Claude models, straight from Anthropic.",
  openrouter: "Models from many authors through one key.",
};

export function ConnectionStatusPill({ connection }: { connection: Pick<ModelConnection, "status"> }) {
  return connection.status === "active" ? (
    <StatusPill tone="success">Active</StatusPill>
  ) : (
    <StatusPill tone="danger">Key refused</StatusPill>
  );
}

const apiKey = z
  .string()
  .trim()
  .min(20, "Paste the whole key.")
  .max(500)
  .regex(/^\S+$/, "Keys have no spaces. Paste the key alone.");

const connectionSchema = z.object({
  kind: z.enum(MODEL_PROVIDER_KINDS, "Choose a provider."),
  label: z.string().trim().min(1, "Name the connection.").max(60),
  api_key: apiKey,
});
type ConnectionValues = z.infer<typeof connectionSchema>;

function KeyHelp({ kind }: { kind: ModelProviderKind }) {
  const provider = MODEL_PROVIDERS[kind];
  return (
    <FormDescription>
      We check the key with {provider.name} before saving it, and never show it again.{" "}
      <a
        href={provider.keys_url}
        target="_blank"
        rel="noreferrer"
        className="inline-flex items-center gap-0.5 font-medium text-foreground underline-offset-4 hover:underline"
      >
        Create a key
        <ExternalLinkIcon className="size-3" />
      </a>
    </FormDescription>
  );
}

/** Pick a provider, name the connection, paste a key. The server checks the key before anything is saved. */
export function AddConnectionDialog({
  org,
  environment,
  open,
  onOpenChange,
  onCreated,
}: {
  org: Organization;
  environment: Environment;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated: (connection: ModelConnection) => void;
}) {
  const create = useCreateModelConnection(org.id);
  const form = useForm<ConnectionValues>({
    resolver: zodResolver(connectionSchema),
    defaultValues: { kind: "openrouter", label: MODEL_PROVIDERS.openrouter.name, api_key: "" },
  });
  const { reset: forgetRequest } = create;
  useEffect(() => {
    // A closed dialog forgets the key it was given, in the form and in the request it sent.
    if (!open) {
      form.reset();
      forgetRequest();
    }
  }, [open, form, forgetRequest]);
  const kind = form.watch("kind");

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Connect a model provider</DialogTitle>
          <DialogDescription>
            For {ENVIRONMENT_LABEL[environment]} only. {ENVIRONMENT_LABEL[OTHER_ENVIRONMENT[environment]]} keeps its own
            connections and keys.
          </DialogDescription>
        </DialogHeader>
        <Form {...form}>
          <form
            id="add-connection"
            className="space-y-5"
            onSubmit={form.handleSubmit((values) =>
              create.mutate(
                { ...values, environment },
                {
                  onSuccess: (connection) => {
                    onOpenChange(false);
                    onCreated(connection);
                  },
                  onError: (error) => {
                    if (!applyFieldErrors(form, error)) toastError(error, "Couldn't connect the provider");
                  },
                },
              ),
            )}
          >
            <FormField
              control={form.control}
              name="kind"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Provider</FormLabel>
                  <ChoiceCards
                    label="Provider"
                    value={field.value}
                    onValueChange={(next) => {
                      // Keep a label someone typed; follow the provider while it's still the suggested one.
                      const label = form.getValues("label").trim();
                      if (!label || label === MODEL_PROVIDERS[field.value].name) {
                        form.setValue("label", MODEL_PROVIDERS[next].name);
                      }
                      field.onChange(next);
                    }}
                    className="sm:grid-cols-3"
                    options={MODEL_PROVIDER_KINDS.map((k) => ({
                      value: k,
                      content: (
                        <>
                          <span className="text-sm font-medium">{MODEL_PROVIDERS[k].name}</span>
                          <span className="text-xs text-muted-foreground">{PROVIDER_BLURB[k]}</span>
                        </>
                      ),
                    }))}
                  />
                  <FormMessage />
                </FormItem>
              )}
            />
            <FormField
              control={form.control}
              name="label"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Name</FormLabel>
                  <FormControl>
                    <Input autoComplete="off" {...field} />
                  </FormControl>
                  <FormDescription>Shown with its models, to tell connections apart.</FormDescription>
                  <FormMessage />
                </FormItem>
              )}
            />
            <FormField
              control={form.control}
              name="api_key"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>API key</FormLabel>
                  <FormControl>
                    <Input
                      type="password"
                      autoComplete="off"
                      spellCheck={false}
                      className="font-mono"
                      placeholder="Paste the key"
                      {...field}
                    />
                  </FormControl>
                  <KeyHelp kind={kind} />
                  <FormMessage />
                </FormItem>
              )}
            />
          </form>
        </Form>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" form="add-connection" disabled={create.isPending}>
            {create.isPending ? <Spinner /> : null}
            {create.isPending ? "Checking key…" : "Connect"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

const replaceSchema = z.object({ api_key: apiKey });

/** A new key for a connection. A key the provider refuses changes nothing. */
export function ReplaceKeyDialog({
  org,
  connection,
  open,
  onOpenChange,
  onReplaced,
}: {
  org: Organization;
  connection: ModelConnection;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onReplaced: (connection: ModelConnection) => void;
}) {
  const replace = useReplaceModelConnectionKey(org.id, connection.id);
  const form = useForm<z.infer<typeof replaceSchema>>({
    resolver: zodResolver(replaceSchema),
    defaultValues: { api_key: "" },
  });
  const { reset: forgetRequest } = replace;
  useEffect(() => {
    if (!open) {
      form.reset();
      forgetRequest();
    }
  }, [open, form, forgetRequest]);
  const provider = MODEL_PROVIDERS[connection.kind].name;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Replace key</DialogTitle>
          <DialogDescription>
            {connection.label} uses the key ending{" "}
            <code className="text-xs text-foreground">{connection.api_key.last4}</code>. Runs started after you save use
            the new key. If {provider} refuses it, the current key stays.
          </DialogDescription>
        </DialogHeader>
        <Form {...form}>
          <form
            id="replace-key"
            onSubmit={form.handleSubmit((values) =>
              replace.mutate(values.api_key, {
                onSuccess: (next) => {
                  onOpenChange(false);
                  onReplaced(next);
                },
                onError: (error) => {
                  if (!applyFieldErrors(form, error)) toastError(error, "Couldn't replace the key");
                },
              }),
            )}
          >
            <FormField
              control={form.control}
              name="api_key"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>New API key</FormLabel>
                  <FormControl>
                    <Input
                      type="password"
                      autoComplete="off"
                      spellCheck={false}
                      className="font-mono"
                      placeholder="Paste the key"
                      autoFocus
                      {...field}
                    />
                  </FormControl>
                  <KeyHelp kind={connection.kind} />
                  <FormMessage />
                </FormItem>
              )}
            />
          </form>
        </Form>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" form="replace-key" disabled={replace.isPending}>
            {replace.isPending ? <Spinner /> : null}
            {replace.isPending ? "Checking key…" : "Replace key"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

const nameSchema = z.object({ name: z.string().trim().min(1, "Enter a name.").max(60) });

/** One name field: renaming a connection or a model. */
export function RenameDialog({
  title,
  description,
  initial,
  open,
  onOpenChange,
  pending,
  onSave,
}: {
  title: string;
  description: string;
  initial: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  pending: boolean;
  onSave: (name: string, setError: (message: string) => void) => void;
}) {
  const form = useForm<z.infer<typeof nameSchema>>({
    resolver: zodResolver(nameSchema),
    values: { name: initial },
  });
  useEffect(() => {
    if (!open) form.reset({ name: initial });
  }, [open, form, initial]);
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>
        <Form {...form}>
          <form
            id="rename"
            onSubmit={form.handleSubmit((values) =>
              onSave(values.name, (message) => form.setError("name", { type: "server", message })),
            )}
          >
            <FormField
              control={form.control}
              name="name"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Name</FormLabel>
                  <FormControl>
                    <Input autoComplete="off" autoFocus {...field} />
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />
          </form>
        </Form>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" form="rename" disabled={pending}>
            {pending ? <Spinner /> : null}
            Save
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
