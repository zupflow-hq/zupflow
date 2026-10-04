import { zodResolver } from "@hookform/resolvers/zod";
import type { AppDefinition, ClientConfiguration, UpdateConfigurationInput } from "@zupflow/sdk";
import { InfoIcon, LockIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { useForm } from "react-hook-form";
import { toast } from "sonner";
import { z } from "zod";
import { ENVIRONMENT_LABEL, EnvBadge } from "@/components/app/environment";
import { MonoValue, RelativeTime } from "@/components/app/misc";
import { Panel } from "@/components/app/page";
import { ConfigStatus } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Spinner } from "@/components/ui/spinner";
import { applyFieldErrors, toastError } from "@/lib/errors";

const FIELD_LABEL: Record<string, string> = {
  mcp_server_url: "MCP server URL",
  oauth_client_id: "client ID",
  oauth_client_secret: "client secret",
};

const schema = z.object({
  mcp_server_url: z
    .string()
    .trim()
    .refine((v) => v === "" || /^https:\/\//.test(v), "Use an https:// URL."),
  oauth_client_id: z.string().trim(),
  oauth_client_secret: z.string(),
  oauth_scopes: z.array(z.string()),
});
type Values = z.infer<typeof schema>;

function toValues(configuration: ClientConfiguration): Values {
  return {
    mcp_server_url: configuration.mcp.server_url ?? "",
    oauth_client_id: configuration.oauth?.client_id ?? "",
    oauth_client_secret: "",
    oauth_scopes: configuration.oauth?.scopes ?? [],
  };
}

/**
 * One environment's client configuration. Secrets are write-only: the form
 * shows whether one is stored and its last four characters, never the value.
 */
export function ConfigurationForm({
  app,
  configuration,
  canEdit,
  pending,
  onSave,
}: {
  app: AppDefinition;
  configuration: ClientConfiguration;
  canEdit: boolean;
  pending: boolean;
  onSave: (
    input: UpdateConfigurationInput,
    callbacks: { onSuccess: (saved: ClientConfiguration) => void; onError: (error: unknown) => void },
  ) => void;
}) {
  const oauth = app.auth.type === "oauth2" ? app.auth : null;
  const [replacingSecret, setReplacingSecret] = useState(false);
  const env = configuration.environment;

  const form = useForm<Values>({
    resolver: zodResolver(schema),
    defaultValues: toValues(configuration),
  });

  // A new version (saved here or elsewhere) closes "replace secret" and, unless
  // the person is mid-edit, shows the stored values.
  // biome-ignore lint/correctness/useExhaustiveDependencies: keyed on the version on purpose.
  useEffect(() => {
    setReplacingSecret(false);
    if (!form.formState.isDirty) form.reset(toValues(configuration));
  }, [configuration.version]);

  const secret = configuration.oauth?.client_secret;
  const showSecretInput = !secret?.set || replacingSecret;

  return (
    <div className="grid gap-6 lg:grid-cols-[minmax(0,1fr)_17rem]">
      <Panel>
        <div className="flex flex-wrap items-center justify-between gap-3 border-b px-5 py-4">
          <div className="flex items-center gap-2">
            <EnvBadge environment={env} />
            <ConfigStatus status={configuration.status} />
          </div>
          <span className="text-xs text-muted-foreground">
            {configuration.version > 0 ? (
              <>
                Version {configuration.version}
                {configuration.updated_by ? ` · ${configuration.updated_by.name}` : ""} ·{" "}
                <RelativeTime iso={configuration.updated_at} />
              </>
            ) : (
              "Never configured"
            )}
          </span>
        </div>

        {configuration.missing.length > 0 ? (
          <div className="flex gap-2 border-b bg-warning-soft/50 px-5 py-3 text-sm">
            <InfoIcon className="mt-0.5 size-4 shrink-0 text-warning" />
            <p>
              Add the {configuration.missing.map((m) => FIELD_LABEL[m] ?? m).join(", ")} to make {app.name} installable
              in {ENVIRONMENT_LABEL[env]} workspaces.
            </p>
          </div>
        ) : null}

        <Form {...form}>
          <form
            className="space-y-6 p-5"
            onSubmit={form.handleSubmit((values) => {
              const input: UpdateConfigurationInput = { mcp_server_url: values.mcp_server_url || null };
              if (oauth) {
                input.oauth_client_id = values.oauth_client_id || null;
                input.oauth_scopes = values.oauth_scopes;
                if (values.oauth_client_secret) input.oauth_client_secret = values.oauth_client_secret;
              }
              onSave(input, {
                onSuccess: (saved) => {
                  form.reset(toValues(saved));
                  toast.success(`Saved ${ENVIRONMENT_LABEL[env]} configuration`);
                },
                onError: (error) => {
                  if (!applyFieldErrors(form, error)) toastError(error, "Couldn't save the configuration");
                },
              });
            })}
          >
            <fieldset disabled={!canEdit} className="space-y-6">
              <FormField
                control={form.control}
                name="mcp_server_url"
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>MCP server URL</FormLabel>
                    <FormControl>
                      <Input className="font-mono text-sm" placeholder="https://mcp.example.com/mcp" {...field} />
                    </FormControl>
                    <FormDescription>
                      Streamable HTTP endpoint. Must be a public HTTPS host; private and link-local addresses are
                      refused.
                    </FormDescription>
                    <FormMessage />
                  </FormItem>
                )}
              />

              {oauth ? (
                <>
                  <FormField
                    control={form.control}
                    name="oauth_client_id"
                    render={({ field }) => (
                      <FormItem>
                        <FormLabel>Client ID</FormLabel>
                        <FormControl>
                          <Input className="font-mono text-sm" autoComplete="off" {...field} />
                        </FormControl>
                        <FormMessage />
                      </FormItem>
                    )}
                  />
                  <FormField
                    control={form.control}
                    name="oauth_client_secret"
                    render={({ field }) => (
                      <FormItem>
                        <FormLabel>Client secret</FormLabel>
                        {showSecretInput ? (
                          <FormControl>
                            <Input
                              type="password"
                              className="font-mono text-sm"
                              autoComplete="new-password"
                              placeholder={secret?.set ? "Paste the new secret" : "Paste the client secret"}
                              {...field}
                            />
                          </FormControl>
                        ) : (
                          <div className="flex h-9 items-center justify-between gap-2 rounded-md border bg-muted/40 px-3">
                            <span className="flex items-center gap-2 font-mono text-sm">
                              <LockIcon className="size-3.5 text-muted-foreground" />
                              ••••••••{secret?.last4}
                            </span>
                            {canEdit ? (
                              <Button type="button" variant="ghost" size="xs" onClick={() => setReplacingSecret(true)}>
                                Replace
                              </Button>
                            ) : null}
                          </div>
                        )}
                        <FormDescription>
                          Encrypted with a {ENVIRONMENT_LABEL[env]}-only key. It can't be read back, only replaced.
                          {secret?.updated_at ? (
                            <>
                              {" "}
                              Last set <RelativeTime iso={secret.updated_at} />.
                            </>
                          ) : null}
                        </FormDescription>
                        <FormMessage />
                      </FormItem>
                    )}
                  />
                  {oauth.available_scopes.length > 0 ? (
                    <FormField
                      control={form.control}
                      name="oauth_scopes"
                      render={({ field }) => (
                        <FormItem>
                          <FormLabel>Scopes requested at consent</FormLabel>
                          <div className="divide-y rounded-lg border">
                            {oauth.available_scopes.map((scope) => {
                              const checked = field.value.includes(scope.name);
                              const id = `scope-${env}-${scope.name}`;
                              return (
                                <div key={scope.name} className="flex items-start gap-3 px-3 py-2.5">
                                  <Checkbox
                                    id={id}
                                    checked={checked}
                                    onCheckedChange={(v) =>
                                      field.onChange(
                                        v ? [...field.value, scope.name] : field.value.filter((s) => s !== scope.name),
                                      )
                                    }
                                    className="mt-0.5"
                                  />
                                  <Label htmlFor={id} className="grid gap-0.5 font-normal">
                                    <span className="font-mono text-[12.5px] break-all">{scope.name}</span>
                                    <span className="text-xs text-muted-foreground">{scope.description}</span>
                                  </Label>
                                </div>
                              );
                            })}
                          </div>
                          <FormDescription>Request the fewest scopes the granted tools need.</FormDescription>
                          <FormMessage />
                        </FormItem>
                      )}
                    />
                  ) : null}
                </>
              ) : null}
            </fieldset>

            {canEdit ? (
              <div className="flex items-center gap-2 border-t pt-5">
                <Button type="submit" disabled={pending || (!form.formState.isDirty && !replacingSecret)}>
                  {pending ? <Spinner /> : null}
                  Save {ENVIRONMENT_LABEL[env]} configuration
                </Button>
                {form.formState.isDirty || replacingSecret ? (
                  <Button
                    type="button"
                    variant="ghost"
                    onClick={() => {
                      form.reset();
                      setReplacingSecret(false);
                    }}
                  >
                    Discard
                  </Button>
                ) : null}
              </div>
            ) : (
              <p className="border-t pt-4 text-sm text-muted-foreground">
                Only owners and admins can change configurations.
              </p>
            )}
          </form>
        </Form>
      </Panel>

      <aside className="space-y-4 text-sm">
        {oauth && configuration.oauth ? (
          <div className="space-y-2">
            <p className="font-medium">Redirect URI</p>
            <p className="text-muted-foreground">Register this exact URL with the provider's OAuth client.</p>
            <MonoValue value={configuration.oauth.redirect_uri} className="w-full" />
          </div>
        ) : null}
        <div className="space-y-2">
          <p className="font-medium">No fallback between environments</p>
          <p className="text-muted-foreground">
            {ENVIRONMENT_LABEL[env]} workspaces only ever use this configuration. If it isn't ready, Connect is blocked
            there — the other environment is never used instead.
          </p>
        </div>
      </aside>
    </div>
  );
}
