import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { BlocksIcon, PlusIcon } from "lucide-react";
import { useMemo, useState } from "react";
import { toast } from "sonner";
import { z } from "zod";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { AppIcon, QueryError, RowsSkeleton, rowLinkProps } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { ConfigStatus } from "@/components/app/status";
import { SearchInput, Toolbar } from "@/components/app/toolbar";
import { Button } from "@/components/ui/button";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { AuthTypeLabel } from "@/features/apps/app-definition";
import { CreateAppDialog } from "@/features/apps/create-app-dialog";
import { canAdminister, useDocumentTitle, useOrg } from "@/features/context";
import { useEnvironment } from "@/features/environment";
import { useCreateOrgApp, useOrgApps } from "@/features/queries";

export const Route = createFileRoute("/_authed/$org/apps/")({
  validateSearch: z.object({ create: z.boolean().optional() }),
  component: CatalogPage,
});

function CatalogPage() {
  const org = useOrg();
  useDocumentTitle("App catalog", org.name);
  const navigate = useNavigate({ from: Route.fullPath });
  const { create } = Route.useSearch();
  const apps = useOrgApps(org.id);
  const createApp = useCreateOrgApp(org.id);
  const [query, setQuery] = useState("");
  const admin = canAdminister(org);
  const environment = useEnvironment();

  const rows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (apps.data ?? []).filter(
      (a) => !q || a.name.toLowerCase().includes(q) || a.slug.includes(q) || a.description.toLowerCase().includes(q),
    );
  }, [apps.data, query]);

  const setCreate = (open: boolean) => navigate({ search: open ? { create: true } : {}, replace: true });
  const status = (app: (typeof rows)[number]) =>
    app.configurations.find((c) => c.environment === environment)?.status ?? "unconfigured";

  return (
    <PageContainer>
      <PageHeader
        title="App catalog"
        description={`Apps your whole organization can offer. The definition is shared; each environment has its own client configuration. Showing ${ENVIRONMENT_LABEL[environment]} readiness.`}
        actions={
          admin ? (
            <Button onClick={() => setCreate(true)}>
              <PlusIcon />
              Add app
            </Button>
          ) : null
        }
      />

      {apps.isError ? (
        <QueryError error={apps.error} onRetry={() => apps.refetch()} what="the catalog" />
      ) : apps.data?.length === 0 ? (
        <Panel>
          <Empty>
            <EmptyHeader>
              <EmptyMedia variant="icon">
                <BlocksIcon />
              </EmptyMedia>
              <EmptyTitle>Add your first app</EmptyTitle>
              <EmptyDescription>
                Register an MCP server once. You can add it now and configure Sandbox and Production later.
              </EmptyDescription>
            </EmptyHeader>
            {admin ? (
              <EmptyContent>
                <Button onClick={() => setCreate(true)}>
                  <PlusIcon />
                  Add app
                </Button>
              </EmptyContent>
            ) : null}
          </Empty>
        </Panel>
      ) : (
        <>
          <Toolbar>
            <SearchInput value={query} onChange={setQuery} placeholder="Search apps" />
          </Toolbar>
          <Panel className="overflow-hidden">
            {apps.isPending ? (
              <RowsSkeleton rows={5} columns={[38, 12, 14, 14]} />
            ) : (
              <Table>
                <TableHeader>
                  <TableRow className="hover:bg-transparent">
                    <TableHead className="pl-4">App</TableHead>
                    <TableHead className="hidden md:table-cell">Auth</TableHead>
                    <TableHead className="hidden text-right lg:table-cell">Tools</TableHead>
                    <TableHead className="pr-4">{ENVIRONMENT_LABEL[environment]} configuration</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {rows.length === 0 ? (
                    <TableRow className="hover:bg-transparent">
                      <TableCell colSpan={4} className="py-10 text-center text-muted-foreground">
                        No app matches “{query}”.
                      </TableCell>
                    </TableRow>
                  ) : (
                    rows.map((app) => (
                      <TableRow
                        key={app.id}
                        {...rowLinkProps(() =>
                          navigate({
                            to: "/$org/apps/$appId",
                            params: { org: org.slug, appId: app.id },
                            search: { tab: "configuration" },
                          }),
                        )}
                      >
                        <TableCell className="w-full max-w-0 py-3 pl-4">
                          <div className="flex items-center gap-3">
                            <AppIcon icon={app.icon} />
                            <div className="min-w-0">
                              <p className="font-medium">{app.name}</p>
                              <p className="truncate text-xs text-muted-foreground">
                                {app.description || <span className="font-mono">{app.slug}</span>}
                              </p>
                            </div>
                          </div>
                        </TableCell>
                        <TableCell className="hidden text-muted-foreground md:table-cell">
                          <AuthTypeLabel app={app} />
                        </TableCell>
                        <TableCell className="tabular hidden text-right lg:table-cell">{app.tools.length}</TableCell>
                        <TableCell className="pr-4">
                          <ConfigStatus status={status(app)} />
                        </TableCell>
                      </TableRow>
                    ))
                  )}
                </TableBody>
              </Table>
            )}
          </Panel>
        </>
      )}

      <CreateAppDialog
        open={Boolean(create)}
        onOpenChange={setCreate}
        scope={{ type: "organization" }}
        pending={createApp.isPending}
        onCreate={(input, callbacks) =>
          createApp.mutate(input, {
            onSuccess: (app) => {
              callbacks.onSuccess(app);
              toast.success(`Added ${app.name} to the catalog`);
              void navigate({
                to: "/$org/apps/$appId",
                params: { org: org.slug, appId: app.id },
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
