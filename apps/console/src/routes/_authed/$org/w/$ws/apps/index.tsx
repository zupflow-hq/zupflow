import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { LockIcon, PackageIcon, PlusIcon } from "lucide-react";
import { toast } from "sonner";
import { z } from "zod";
import { EnvBadge } from "@/components/app/environment";
import { AppIcon, QueryError, RowsSkeleton, rowLinkProps } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel, SectionHeader } from "@/components/app/page";
import { ConfigStatus } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { AuthTypeLabel } from "@/features/apps/app-definition";
import { CreateAppDialog } from "@/features/apps/create-app-dialog";
import { canAdminister, canChange, useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { useAvailableApps, useCreateWorkspaceApp, useWorkspaceApps } from "@/features/queries";

export const Route = createFileRoute("/_authed/$org/w/$ws/apps/")({
  validateSearch: z.object({ create: z.boolean().optional() }),
  component: WorkspaceAppsPage,
});

function WorkspaceAppsPage() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  useDocumentTitle("Workspace apps", ws.name);
  const navigate = useNavigate({ from: Route.fullPath });
  const { create } = Route.useSearch();
  const apps = useWorkspaceApps(ws.id);
  const available = useAvailableApps(ws.id);
  const createApp = useCreateWorkspaceApp(ws.id);
  const admin = canAdminister(org);
  const setCreate = (open: boolean) => navigate({ search: open ? { create: true } : {}, replace: true });
  const fromCatalog = available.data?.filter((a) => a.app.owner_scope === "organization") ?? [];

  return (
    <PageContainer>
      <PageHeader
        title="Workspace apps"
        description={
          <>
            Apps only {ws.name} can see and install, configured for its single environment. They never join the
            organization catalog.
          </>
        }
        actions={
          admin ? (
            <Button onClick={() => setCreate(true)}>
              <PlusIcon />
              Create workspace app
            </Button>
          ) : null
        }
      />

      {apps.isError ? (
        <QueryError error={apps.error} onRetry={() => apps.refetch()} what="workspace apps" />
      ) : apps.data?.length === 0 ? (
        <Panel>
          <Empty>
            <EmptyHeader>
              <EmptyMedia variant="icon">
                <PackageIcon />
              </EmptyMedia>
              <EmptyTitle>No private apps</EmptyTitle>
              <EmptyDescription>
                Use a workspace app when only this customer runs the MCP server — their own CMS, for example.
              </EmptyDescription>
            </EmptyHeader>
            {admin ? (
              <EmptyContent>
                <Button variant="outline" onClick={() => setCreate(true)}>
                  <PlusIcon />
                  Create workspace app
                </Button>
              </EmptyContent>
            ) : (
              <EmptyContent>
                <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
                  <LockIcon className="size-3" /> Creating apps needs the admin or owner role.
                </p>
              </EmptyContent>
            )}
          </Empty>
        </Panel>
      ) : (
        <Panel className="overflow-hidden">
          {apps.isPending ? (
            <RowsSkeleton rows={2} />
          ) : (
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead className="pl-4">App</TableHead>
                  <TableHead className="hidden sm:table-cell">Auth</TableHead>
                  <TableHead>Configuration</TableHead>
                  <TableHead className="hidden pr-4 text-right md:table-cell">Installed</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {apps.data.map((app) => (
                  <TableRow
                    key={app.id}
                    {...rowLinkProps(() =>
                      navigate({ to: "/$org/w/$ws/apps/$appId", params: { org: org.slug, ws: ws.id, appId: app.id } }),
                    )}
                  >
                    <TableCell className="w-full max-w-0 py-3 pl-4">
                      <div className="flex items-center gap-3">
                        <AppIcon icon={app.icon} />
                        <div className="min-w-0">
                          <p className="truncate font-medium">{app.name}</p>
                          <p className="max-w-xs truncate text-xs text-muted-foreground">{app.description}</p>
                        </div>
                      </div>
                    </TableCell>
                    <TableCell className="hidden text-muted-foreground sm:table-cell">
                      <AuthTypeLabel app={app} />
                    </TableCell>
                    <TableCell>
                      <ConfigStatus status={app.configurations[0]?.status ?? "unconfigured"} />
                    </TableCell>
                    <TableCell className="tabular hidden pr-4 text-right md:table-cell">
                      {app.installation_count}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </Panel>
      )}

      <section className="mt-10">
        <SectionHeader
          title="From the organization catalog"
          description={
            <span className="inline-flex flex-wrap items-center gap-1.5">
              Catalog apps ready for <EnvBadge environment={ws.environment} size="sm" /> are installable here too.
            </span>
          }
          actions={
            <Button asChild variant="ghost" size="sm">
              <Link to="/$org/apps" params={{ org: org.slug }}>
                Open catalog
              </Link>
            </Button>
          }
        />
        <Panel>
          {available.isPending ? (
            <RowsSkeleton rows={3} columns={[50]} />
          ) : fromCatalog.length === 0 ? (
            <p className="p-6 text-center text-sm text-muted-foreground">
              No catalog app is ready for this environment.
            </p>
          ) : (
            <ul className="divide-y">
              {fromCatalog.map(({ app }) => (
                <li key={app.id} className="flex items-center gap-3 px-4 py-3">
                  <AppIcon icon={app.icon} size="sm" />
                  <span className="flex-1 text-sm font-medium">{app.name}</span>
                  {ws.environment === "sandbox" && canChange(org) ? (
                    <Button asChild variant="outline" size="sm">
                      <Link
                        to="/$org/w/$ws/installations"
                        params={{ org: org.slug, ws: ws.id }}
                        search={{ add: true, app: app.id }}
                      >
                        Install
                      </Link>
                    </Button>
                  ) : ws.environment === "production" ? (
                    <span className="text-xs text-muted-foreground">Installed by customers through Connect</span>
                  ) : null}
                </li>
              ))}
            </ul>
          )}
        </Panel>
      </section>

      <CreateAppDialog
        open={Boolean(create) && admin}
        onOpenChange={setCreate}
        scope={{ type: "workspace", name: ws.name, environment: ws.environment }}
        pending={createApp.isPending}
        onCreate={(input, callbacks) =>
          createApp.mutate(input, {
            onSuccess: (app) => {
              callbacks.onSuccess(app);
              toast.success(`Created ${app.name}`);
              void navigate({
                to: "/$org/w/$ws/apps/$appId",
                params: { org: org.slug, ws: ws.id, appId: app.id },
                search: { tab: "configuration" },
              });
            },
            onError: callbacks.onError,
          })
        }
      />
    </PageContainer>
  );
}
