import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { PlugIcon, PlusIcon } from "lucide-react";
import { z } from "zod";
import { AppIcon, QueryError, RelativeTime, RowsSkeleton, rowLinkProps } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { InstallationStatusPill } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { AddInstallationDialog } from "@/features/installations/add-installation-dialog";
import { useInstallations } from "@/features/queries";

export const Route = createFileRoute("/_authed/$org/w/$ws/installations/")({
  validateSearch: z.object({ add: z.boolean().optional(), app: z.string().optional() }),
  component: InstallationsPage,
});

function InstallationsPage() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  useDocumentTitle("Installations", ws.name);
  const navigate = useNavigate({ from: Route.fullPath });
  const { add, app } = Route.useSearch();
  const installations = useInstallations(ws.id);
  const production = ws.environment === "production";
  const setAdd = (open: boolean) => navigate({ search: open ? { add: true } : {}, replace: true });

  return (
    <PageContainer>
      <PageHeader
        title="Installations"
        description={
          production
            ? "Each installation is one connected account, usable only in this workspace. In Production, your customers connect accounts from your product through Connect."
            : "Each installation is one connected account, usable only in this workspace. Agents reach it through grants."
        }
        actions={
          production ? null : (
            <Button onClick={() => setAdd(true)}>
              <PlusIcon />
              Add installation
            </Button>
          )
        }
      />
      {installations.isError ? (
        <QueryError error={installations.error} onRetry={() => installations.refetch()} what="installations" />
      ) : installations.data?.length === 0 ? (
        <Panel>
          <Empty>
            <EmptyHeader>
              <EmptyMedia variant="icon">
                <PlugIcon />
              </EmptyMedia>
              <EmptyTitle>{production ? "No accounts connected yet" : "Connect the first account"}</EmptyTitle>
              <EmptyDescription>
                {production
                  ? "This customer connects accounts from your product, through Connect."
                  : "Pick an app that's ready for this environment and run Connect, the same flow your users will see."}
              </EmptyDescription>
            </EmptyHeader>
            <EmptyContent>
              {production ? (
                <Button asChild variant="outline">
                  <Link to="/$org/connect" params={{ org: org.slug }}>
                    Set up Connect
                  </Link>
                </Button>
              ) : (
                <Button onClick={() => setAdd(true)}>
                  <PlusIcon />
                  Add installation
                </Button>
              )}
            </EmptyContent>
          </Empty>
        </Panel>
      ) : (
        <Panel className="overflow-hidden">
          {installations.isPending ? (
            <RowsSkeleton rows={4} />
          ) : (
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead className="pl-4">Installation</TableHead>
                  <TableHead className="hidden md:table-cell">Account</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead className="hidden text-right sm:table-cell">Agents</TableHead>
                  <TableHead className="hidden pr-4 lg:table-cell">Connected</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {installations.data.map((i) => (
                  <TableRow
                    key={i.id}
                    {...rowLinkProps(() =>
                      navigate({
                        to: "/$org/w/$ws/installations/$installationId",
                        params: { org: org.slug, ws: ws.id, installationId: i.id },
                      }),
                    )}
                  >
                    <TableCell className="w-full max-w-0 py-3 pl-4">
                      <div className="flex items-center gap-3">
                        <AppIcon icon={i.app.icon} />
                        <div className="min-w-0">
                          <p className="truncate font-medium">{i.label}</p>
                          <p className="text-xs text-muted-foreground">
                            {i.app.name}
                            {i.app.owner_scope === "workspace" ? " · workspace app" : ""}
                          </p>
                        </div>
                      </div>
                    </TableCell>
                    <TableCell className="hidden max-w-60 truncate text-muted-foreground md:table-cell">
                      {i.connection?.account.email ?? i.connection?.account.display_name ?? "—"}
                    </TableCell>
                    <TableCell>
                      <InstallationStatusPill status={i.status} />
                    </TableCell>
                    <TableCell className="tabular hidden text-right sm:table-cell">{i.grant_count}</TableCell>
                    <TableCell className="hidden pr-4 text-muted-foreground lg:table-cell">
                      <RelativeTime iso={i.connection?.connected_at} />
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </Panel>
      )}
      {production ? null : (
        <AddInstallationDialog org={org} workspace={ws} open={Boolean(add)} onOpenChange={setAdd} initialAppId={app} />
      )}
    </PageContainer>
  );
}
