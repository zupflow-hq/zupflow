import { zodResolver } from "@hookform/resolvers/zod";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import type { AppDefinition, AppId, Environment, Organization } from "@zupflow/sdk";
import { useState } from "react";
import { useForm } from "react-hook-form";
import { toast } from "sonner";
import { z } from "zod";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { LineTabsList, LineTabsTrigger } from "@/components/app/line-tabs";
import { AppIcon, MonoValue, QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, Panel, Properties, Property, SectionHeader } from "@/components/app/page";
import { InstallationStatusPill } from "@/components/app/status";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Form, FormControl, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tabs, TabsContent } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { AuthTemplatePanel, AuthTypeLabel, ToolsPanel } from "@/features/apps/app-definition";
import { ConfigurationForm } from "@/features/apps/configuration-form";
import { EditAuthSheet } from "@/features/apps/edit-auth-sheet";
import { canAdminister, useDocumentTitle, useOrg } from "@/features/context";
import { useEnvironment } from "@/features/environment";
import {
  useOrgApp,
  useOrgAppConfiguration,
  useOrgAppInstallations,
  useUpdateOrgApp,
  useUpdateOrgAppConfiguration,
} from "@/features/queries";
import { useDetailCrumb } from "@/lib/crumbs";
import { applyFieldErrors, toastError } from "@/lib/errors";
import { idParam } from "@/lib/route-ids";
import { cn } from "@/lib/utils";

const TABS = ["overview", "configuration", "installations"] as const;
type Tab = (typeof TABS)[number];

export const Route = createFileRoute("/_authed/$org/apps/$appId")({
  params: idParam("appId", "app"),
  validateSearch: z.object({ tab: z.enum(TABS).optional() }),
  component: AppPage,
});

function AppPage() {
  const org = useOrg();
  const { appId } = Route.useParams();
  const { tab = "overview" } = Route.useSearch();
  const navigate = useNavigate({ from: Route.fullPath });
  const environment = useEnvironment();
  const app = useOrgApp(org.id, appId);
  useDetailCrumb(app.data?.name);
  useDocumentTitle(app.data?.name, "App catalog", org.name);

  if (app.isError) {
    return (
      <PageContainer>
        <QueryError error={app.error} onRetry={() => app.refetch()} what="this app" />
      </PageContainer>
    );
  }

  const config = app.data?.configurations.find((c) => c.environment === environment);

  return (
    <PageContainer>
      <header className="mb-6 flex flex-col gap-4 sm:flex-row sm:items-center">
        {app.data ? <AppIcon icon={app.data.icon} size="lg" /> : <Skeleton className="size-12 rounded-lg" />}
        <div className="min-w-0 flex-1 space-y-1.5">
          {app.data ? (
            <>
              <h1 className="text-[1.6rem] leading-tight font-semibold tracking-[-0.02em]">{app.data.name}</h1>
              <div className="flex flex-wrap items-center gap-2 text-sm text-muted-foreground">
                <Badge variant="outline" className="font-mono">
                  {app.data.kind}
                </Badge>
                <span>Organization app</span>
                <span aria-hidden>·</span>
                <AuthTypeLabel app={app.data} />
                <span aria-hidden>·</span>
                <span>Definition v{app.data.version}</span>
              </div>
            </>
          ) : (
            <>
              <Skeleton className="h-8 w-56" />
              <Skeleton className="h-4 w-72" />
            </>
          )}
        </div>
      </header>

      <Tabs
        value={tab}
        onValueChange={(value) => navigate({ search: { tab: value as Tab }, replace: true })}
        className="gap-6"
      >
        <LineTabsList>
          <LineTabsTrigger value="overview">Overview</LineTabsTrigger>
          <LineTabsTrigger value="configuration">
            {ENVIRONMENT_LABEL[environment]} configuration
            <span
              role="img"
              aria-label={config?.status === "ready" ? "ready" : "not ready"}
              className={cn(
                "size-1.5 rounded-full",
                config?.status === "ready"
                  ? "bg-success"
                  : config?.status === "incomplete"
                    ? "bg-warning"
                    : "bg-muted-foreground/40",
              )}
            />
          </LineTabsTrigger>
          <LineTabsTrigger value="installations">Installations</LineTabsTrigger>
        </LineTabsList>

        <TabsContent value="overview">
          {app.data ? <OverviewTab org={org} app={app.data} /> : <RowsSkeleton rows={4} />}
        </TabsContent>
        <TabsContent value="configuration">
          {app.data ? <EnvironmentTab key={environment} org={org} app={app.data} environment={environment} /> : null}
        </TabsContent>
        <TabsContent value="installations">
          <InstallationsTab org={org} appId={appId} environment={environment} />
        </TabsContent>
      </Tabs>
    </PageContainer>
  );
}

const detailsSchema = z.object({
  name: z.string().trim().min(1, "Required.").max(60),
  description: z.string().trim().max(280),
});

function OverviewTab({ org, app }: { org: Organization; app: AppDefinition }) {
  const update = useUpdateOrgApp(org.id, app.id);
  const admin = canAdminister(org);
  const [editingAuth, setEditingAuth] = useState(false);
  const form = useForm<z.infer<typeof detailsSchema>>({
    resolver: zodResolver(detailsSchema),
    values: { name: app.name, description: app.description },
  });

  return (
    <div className="grid gap-8 lg:grid-cols-[minmax(0,1fr)_20rem]">
      <div className="space-y-8">
        <ToolsPanel app={app} />
        <AuthTemplatePanel app={app} onEdit={admin ? () => setEditingAuth(true) : undefined} />
        <EditAuthSheet
          app={app}
          open={editingAuth}
          onOpenChange={setEditingAuth}
          pending={update.isPending}
          onSave={(auth, callbacks) =>
            update.mutate(
              { auth },
              {
                onSuccess: () => {
                  toast.success("Saved authentication");
                  callbacks.onSuccess();
                },
                onError: callbacks.onError,
              },
            )
          }
        />
      </div>
      <div className="space-y-8">
        <section>
          <SectionHeader title="Details" />
          <Panel className="p-4">
            <Form {...form}>
              <form
                className="space-y-4"
                onSubmit={form.handleSubmit((values) =>
                  update.mutate(values, {
                    onSuccess: () => toast.success("Saved app details"),
                    onError: (error) => {
                      if (!applyFieldErrors(form, error)) toastError(error, "Couldn't save");
                    },
                  }),
                )}
              >
                <fieldset disabled={!admin} className="space-y-4">
                  <FormField
                    control={form.control}
                    name="name"
                    render={({ field }) => (
                      <FormItem>
                        <FormLabel>Name</FormLabel>
                        <FormControl>
                          <Input {...field} />
                        </FormControl>
                        <FormMessage />
                      </FormItem>
                    )}
                  />
                  <FormField
                    control={form.control}
                    name="description"
                    render={({ field }) => (
                      <FormItem>
                        <FormLabel>Description</FormLabel>
                        <FormControl>
                          <Textarea rows={3} {...field} />
                        </FormControl>
                        <FormMessage />
                      </FormItem>
                    )}
                  />
                </fieldset>
                {admin ? (
                  <Button type="submit" size="sm" disabled={!form.formState.isDirty || update.isPending}>
                    {update.isPending ? <Spinner /> : null}
                    Save details
                  </Button>
                ) : null}
              </form>
            </Form>
          </Panel>
        </section>
        <section>
          <SectionHeader title="Identifiers" />
          <Panel>
            <Properties>
              <Property label="App ID" className="sm:grid-cols-1 sm:gap-1">
                <MonoValue value={app.id} />
              </Property>
              <Property label="Slug" className="sm:grid-cols-1 sm:gap-1">
                <MonoValue value={app.slug} />
              </Property>
              <Property label="Created" className="sm:grid-cols-1 sm:gap-1">
                <RelativeTime iso={app.created_at} />
              </Property>
            </Properties>
          </Panel>
        </section>
      </div>
    </div>
  );
}

function EnvironmentTab({
  org,
  app,
  environment,
}: {
  org: Organization;
  app: AppDefinition;
  environment: Environment;
}) {
  const configuration = useOrgAppConfiguration(org.id, app.id, environment);
  const update = useUpdateOrgAppConfiguration(org.id, app.id, environment);
  if (configuration.isError) {
    return (
      <QueryError
        error={configuration.error}
        onRetry={() => configuration.refetch()}
        what={`the ${ENVIRONMENT_LABEL[environment]} configuration`}
      />
    );
  }
  if (!configuration.data) {
    return (
      <Panel className="space-y-5 p-5">
        <Skeleton className="h-5 w-48" />
        {[0, 1, 2].map((i) => (
          <div key={i} className="space-y-2">
            <Skeleton className="h-4 w-28" />
            <Skeleton className="h-9 w-full" />
          </div>
        ))}
      </Panel>
    );
  }
  return (
    <ConfigurationForm
      app={app}
      configuration={configuration.data}
      canEdit={canAdminister(org)}
      pending={update.isPending}
      onSave={(input, callbacks) => update.mutate(input, callbacks)}
    />
  );
}

function InstallationsTab({ org, appId, environment }: { org: Organization; appId: AppId; environment: Environment }) {
  const installations = useOrgAppInstallations(org.id, appId, environment);
  if (installations.isError) {
    return <QueryError error={installations.error} onRetry={() => installations.refetch()} what="installations" />;
  }
  const refs = installations.data?.pages.flatMap((p) => p.items);
  const rows = refs ?? [];
  const total = installations.data?.pages[0]?.pagination.total;
  return (
    <section>
      <SectionHeader
        title={`${ENVIRONMENT_LABEL[environment]} installations`}
        description="Each belongs to one workspace and holds its own account connection. Tokens are never shared between them."
        actions={
          total !== undefined && total !== null ? (
            <span className="tabular text-sm text-muted-foreground">{total.toLocaleString("en")} total</span>
          ) : null
        }
      />
      <Panel className="overflow-hidden">
        {installations.isPending ? (
          <RowsSkeleton rows={3} />
        ) : rows.length === 0 ? (
          <p className="p-8 text-center text-sm text-muted-foreground">
            Not installed in any {ENVIRONMENT_LABEL[environment]} workspace yet.
          </p>
        ) : (
          <Table>
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead className="pl-4">Installation</TableHead>
                <TableHead>Workspace</TableHead>
                <TableHead className="hidden md:table-cell">Account</TableHead>
                <TableHead>Status</TableHead>
                <TableHead className="hidden pr-4 lg:table-cell">Connected</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.map(({ installation, workspace }) => (
                <TableRow key={installation.id}>
                  <TableCell className="py-3 pl-4 font-medium">
                    <Link
                      to="/$org/w/$ws/installations/$installationId"
                      params={{ org: org.slug, ws: workspace.id, installationId: installation.id }}
                      className="hover:underline"
                    >
                      {installation.label}
                    </Link>
                  </TableCell>
                  <TableCell>
                    <Link to="/$org/w/$ws" params={{ org: org.slug, ws: workspace.id }} className="hover:underline">
                      {workspace.name}
                    </Link>
                  </TableCell>
                  <TableCell className="hidden max-w-56 truncate text-muted-foreground md:table-cell">
                    {installation.connection?.account.email ?? installation.connection?.account.display_name ?? "—"}
                  </TableCell>
                  <TableCell>
                    <InstallationStatusPill status={installation.status} />
                  </TableCell>
                  <TableCell className="hidden pr-4 text-muted-foreground lg:table-cell">
                    <RelativeTime iso={installation.connection?.connected_at} />
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </Panel>
      {installations.hasNextPage ? (
        <div className="mt-4 flex justify-center">
          <Button
            variant="outline"
            onClick={() => installations.fetchNextPage()}
            disabled={installations.isFetchingNextPage}
          >
            {installations.isFetchingNextPage ? <Spinner /> : null}
            Load more
          </Button>
        </div>
      ) : null}
    </section>
  );
}
