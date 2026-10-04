import { createFileRoute } from "@tanstack/react-router";
import { type ConnectSettings, type Environment, type OrganizationId, ZupflowApiError } from "@zupflow/sdk";
import { PlusIcon, XIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { toast } from "sonner";
import { CodeTabs } from "@/components/app/code-block";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { QueryError, RelativeTime } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel, SectionHeader } from "@/components/app/page";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister, useDocumentTitle, useOrg } from "@/features/context";
import { useEnvironment } from "@/features/environment";
import { useConnectSettings, useUpdateConnectSettings } from "@/features/queries";
import { toastError } from "@/lib/errors";

export const Route = createFileRoute("/_authed/$org/connect")({
  staticData: { crumb: "Connect" },
  component: ConnectSettingsPage,
});

function ConnectSettingsPage() {
  const org = useOrg();
  useDocumentTitle("Connect", org.name);
  const env = useEnvironment();
  const settings = useConnectSettings(org.id, env);

  return (
    <PageContainer>
      <PageHeader
        title="Connect"
        description="Zupflow hosts the page where your users link an account, and runs the provider's sign-in for you. Your backend creates a session and sends the user to its URL; they come back to one of these return URLs."
      />
      <div className="grid gap-8 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.05fr)]">
        <div className="space-y-8">
          {settings.isError ? (
            <QueryError error={settings.error} onRetry={() => settings.refetch()} what="Connect settings" />
          ) : settings.data ? (
            <SettingsForm
              key={env}
              orgId={org.id}
              environment={env}
              settings={settings.data}
              canEdit={canAdminister(org)}
            />
          ) : (
            <Panel className="space-y-3 p-5">
              <Skeleton className="h-5 w-40" />
              <Skeleton className="h-9 w-full" />
              <Skeleton className="h-9 w-full" />
            </Panel>
          )}
        </div>
        <section>
          <SectionHeader
            title="Integrate"
            description={`Create a ${ENVIRONMENT_LABEL[env]} session on your server, then send the user to its URL.`}
          />
          <CodeTabs samples={samples(env, settings.data?.return_urls[0])} />
          <ol className="mt-4 space-y-2 text-sm text-muted-foreground">
            <li>
              <span className="font-medium text-foreground">1.</span> Your backend checks the signed-in user may connect
              accounts in that workspace, then creates the session with an environment-bound key.
            </li>
            <li>
              <span className="font-medium text-foreground">2.</span> The browser opens{" "}
              <code className="text-xs">url</code> in a popup or redirects to it. It can't change the workspace, app or
              environment.
            </li>
            <li>
              <span className="font-medium text-foreground">3.</span> On return, verify the result with{" "}
              <code className="text-xs">GET …/connect-sessions/{"{id}"}</code> or the{" "}
              <code className="text-xs">connect_session.completed</code> webhook. A redirect alone isn't proof.
            </li>
          </ol>
        </section>
      </div>
    </PageContainer>
  );
}

function samples(env: Environment, returnUrl: string | undefined) {
  const key = env === "production" ? "zf_live_…" : "zf_test_…";
  const ret = returnUrl ?? "https://app.example.com/settings/integrations";
  return [
    {
      label: "curl",
      code: `curl -X POST https://api.zupflow.com/v1/workspaces/$WORKSPACE_ID/connect-sessions \\
  -H "Authorization: Bearer ${key}" \\
  -H "Content-Type: application/json" \\
  -d '{
    "app_id": "$APP_ID",
    "intent": "create",
    "label": "Marketing",
    "installation_id": null,
    "return_url": "${ret}"
  }'`,
    },
    {
      label: "TypeScript",
      code: `import { createClient, createHttpTransport } from "@zupflow/sdk";

const zupflow = createClient(
  createHttpTransport({
    baseUrl: "https://api.zupflow.com",
    getToken: async () => process.env.ZUPFLOW_${env === "production" ? "LIVE" : "TEST"}_KEY!,
  }),
);

const session = await zupflow.connectSessions.create(workspaceId, {
  app_id: appId,
  intent: "create",
  label: "Marketing",
  installation_id: null,
  return_url: "${ret}",
});

// Zupflow hosts the page and the provider's sign-in. Send the user there;
// they come back to return_url with ?zupflow_session=<id>.
return redirect(session.url);`,
    },
  ];
}

function UrlList({
  label,
  description,
  placeholder,
  values,
  onChange,
  errors,
  disabled,
}: {
  label: string;
  description: string;
  placeholder: string;
  values: string[];
  onChange: (values: string[]) => void;
  errors: Record<number, string>;
  disabled: boolean;
}) {
  return (
    <fieldset className="space-y-2" disabled={disabled}>
      <legend className="text-sm font-medium">{label}</legend>
      <p className="text-sm text-muted-foreground">{description}</p>
      <ul className="space-y-2 pt-1">
        {values.map((value, i) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: rows are positional and edited in place
          <li key={i}>
            <div className="flex gap-2">
              <Input
                value={value}
                aria-label={`${label} ${i + 1}`}
                aria-invalid={Boolean(errors[i])}
                placeholder={placeholder}
                className="font-mono text-sm"
                onChange={(e) => onChange(values.map((v, j) => (j === i ? e.target.value : v)))}
              />
              <Button
                type="button"
                variant="ghost"
                size="icon"
                aria-label="Remove"
                onClick={() => onChange(values.filter((_, j) => j !== i))}
              >
                <XIcon />
              </Button>
            </div>
            {errors[i] ? <p className="mt-1 text-sm text-destructive">{errors[i]}</p> : null}
          </li>
        ))}
      </ul>
      {!disabled ? (
        <Button type="button" variant="outline" size="sm" onClick={() => onChange([...values, ""])}>
          <PlusIcon />
          Add
        </Button>
      ) : null}
    </fieldset>
  );
}

function SettingsForm({
  orgId,
  environment,
  settings,
  canEdit,
}: {
  orgId: OrganizationId;
  environment: Environment;
  settings: ConnectSettings;
  canEdit: boolean;
}) {
  const update = useUpdateConnectSettings(orgId, environment);
  const [returnUrls, setReturnUrls] = useState(settings.return_urls);
  const [origins, setOrigins] = useState(settings.allowed_origins);
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});
  useEffect(() => {
    setReturnUrls(settings.return_urls);
    setOrigins(settings.allowed_origins);
  }, [settings]);

  const pick = (prefix: string) =>
    Object.fromEntries(
      Object.entries(fieldErrors)
        .filter(([k]) => k.startsWith(`${prefix}.`))
        .map(([k, v]) => [Number(k.split(".")[1]), v]),
    );
  const dirty =
    JSON.stringify(returnUrls) !== JSON.stringify(settings.return_urls) ||
    JSON.stringify(origins) !== JSON.stringify(settings.allowed_origins);

  return (
    <Panel>
      <form
        className="space-y-7 p-5"
        onSubmit={(e) => {
          e.preventDefault();
          setFieldErrors({});
          update.mutate(
            {
              return_urls: returnUrls.map((v) => v.trim()).filter(Boolean),
              allowed_origins: origins.map((v) => v.trim()).filter(Boolean),
            },
            {
              onSuccess: () => toast.success(`Saved ${ENVIRONMENT_LABEL[environment]} Connect settings`),
              onError: (error) => {
                if (error instanceof ZupflowApiError && error.fields) setFieldErrors(error.fields);
                else toastError(error, "Couldn't save");
              },
            },
          );
        }}
      >
        <UrlList
          label="Return URLs"
          description="Exact URLs Connect may send people back to. No wildcards; query strings must match too."
          placeholder="https://app.example.com/settings/integrations"
          values={returnUrls}
          onChange={setReturnUrls}
          errors={pick("return_urls")}
          disabled={!canEdit}
        />
        <UrlList
          label="Allowed origins"
          description="Pages on these origins may open Connect in a popup and receive its completion message."
          placeholder="https://app.example.com"
          values={origins}
          onChange={setOrigins}
          errors={pick("allowed_origins")}
          disabled={!canEdit}
        />
        <div className="flex flex-wrap items-center gap-3 border-t pt-5">
          {canEdit ? (
            <Button type="submit" disabled={!dirty || update.isPending}>
              {update.isPending ? <Spinner /> : null}
              Save {ENVIRONMENT_LABEL[environment]} settings
            </Button>
          ) : (
            <p className="text-sm text-muted-foreground">Only owners and admins can change these.</p>
          )}
          <span className="text-xs text-muted-foreground">
            {settings.updated_at ? (
              <>
                Last changed <RelativeTime iso={settings.updated_at} />
              </>
            ) : (
              "Not configured yet"
            )}
          </span>
        </div>
      </form>
    </Panel>
  );
}
