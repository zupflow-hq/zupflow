import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { z } from "zod";
import { EnvBadge } from "@/components/app/environment";
import { LineTabsList, LineTabsTrigger } from "@/components/app/line-tabs";
import { AppIcon, QueryError, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, Panel } from "@/components/app/page";
import { ConfigStatus } from "@/components/app/status";
import { Badge } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent } from "@/components/ui/tabs";
import { AuthTemplatePanel, AuthTypeLabel, ToolsPanel } from "@/features/apps/app-definition";
import { ConfigurationForm } from "@/features/apps/configuration-form";
import { canAdminister, useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { useUpdateWorkspaceAppConfiguration, useWorkspaceApp, useWorkspaceAppConfiguration } from "@/features/queries";
import { useDetailCrumb } from "@/lib/crumbs";
import { idParam } from "@/lib/route-ids";

const TABS = ["overview", "configuration"] as const;

export const Route = createFileRoute("/_authed/$org/w/$ws/apps/$appId")({
  params: idParam("appId", "app"),
  validateSearch: z.object({ tab: z.enum(TABS).optional() }),
  component: WorkspaceAppPage,
});

function WorkspaceAppPage() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  const { appId } = Route.useParams();
  const { tab = "overview" } = Route.useSearch();
  const navigate = useNavigate({ from: Route.fullPath });
  const app = useWorkspaceApp(ws.id, appId);
  const configuration = useWorkspaceAppConfiguration(ws.id, appId);
  const update = useUpdateWorkspaceAppConfiguration(ws.id, appId);
  useDetailCrumb(app.data?.name);
  useDocumentTitle(app.data?.name, ws.name);

  if (app.isError) {
    return (
      <PageContainer>
        <QueryError error={app.error} onRetry={() => app.refetch()} what="this app" />
      </PageContainer>
    );
  }

  return (
    <PageContainer>
      <header className="mb-6 flex items-center gap-4">
        {app.data ? <AppIcon icon={app.data.icon} size="lg" /> : <Skeleton className="size-12 rounded-lg" />}
        <div className="min-w-0 flex-1 space-y-1.5">
          {app.data ? (
            <>
              <h2 className="text-xl leading-tight font-semibold tracking-[-0.02em]">{app.data.name}</h2>
              <div className="flex flex-wrap items-center gap-2 text-sm text-muted-foreground">
                <Badge variant="outline" className="font-mono">
                  {app.data.kind}
                </Badge>
                <span>Private to {ws.name}</span>
                <EnvBadge environment={ws.environment} size="sm" />
                <span aria-hidden>·</span>
                <AuthTypeLabel app={app.data} />
              </div>
            </>
          ) : (
            <>
              <Skeleton className="h-8 w-48" />
              <Skeleton className="h-4 w-64" />
            </>
          )}
        </div>
      </header>
      <Tabs
        value={tab}
        onValueChange={(v) => navigate({ search: { tab: v as (typeof TABS)[number] }, replace: true })}
        className="gap-6"
      >
        <LineTabsList>
          <LineTabsTrigger value="overview">Overview</LineTabsTrigger>
          <LineTabsTrigger value="configuration">
            Configuration
            {configuration.data ? <ConfigStatus status={configuration.data.status} className="h-5" /> : null}
          </LineTabsTrigger>
        </LineTabsList>
        <TabsContent value="overview">
          {app.data ? (
            <div className="space-y-8">
              <ToolsPanel app={app.data} />
              <AuthTemplatePanel app={app.data} />
            </div>
          ) : (
            <RowsSkeleton />
          )}
        </TabsContent>
        <TabsContent value="configuration">
          {configuration.isError ? (
            <QueryError error={configuration.error} onRetry={() => configuration.refetch()} what="the configuration" />
          ) : app.data && configuration.data ? (
            <ConfigurationForm
              app={app.data}
              configuration={configuration.data}
              canEdit={canAdminister(org)}
              pending={update.isPending}
              onSave={(input, callbacks) => update.mutate(input, callbacks)}
            />
          ) : (
            <Panel className="space-y-4 p-5">
              <Skeleton className="h-5 w-40" />
              <Skeleton className="h-9 w-full" />
            </Panel>
          )}
        </TabsContent>
      </Tabs>
    </PageContainer>
  );
}
