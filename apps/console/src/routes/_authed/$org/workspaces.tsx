import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { BoxesIcon, PlusIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { z } from "zod";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { QueryError, RelativeTime, RowsSkeleton, rowLinkProps } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { SearchInput, Segmented, Toolbar } from "@/components/app/toolbar";
import { Button } from "@/components/ui/button";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Spinner } from "@/components/ui/spinner";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { canAdminister, useDocumentTitle, useOrg } from "@/features/context";
import { useEnvironment } from "@/features/environment";
import { useWorkspaceList } from "@/features/queries";
import { CreateWorkspaceDialog } from "@/features/workspaces/create-workspace-dialog";
import { useDebounced } from "@/lib/use-debounced";

export const Route = createFileRoute("/_authed/$org/workspaces")({
  staticData: { crumb: "Workspaces" },
  validateSearch: z.object({
    create: z.boolean().optional(),
    q: z.string().optional(),
    sort: z.enum(["name", "activity"]).optional(),
  }),
  component: WorkspacesPage,
});

function WorkspacesPage() {
  const org = useOrg();
  const environment = useEnvironment();
  useDocumentTitle("Workspaces", org.name);
  const navigate = useNavigate({ from: Route.fullPath });
  const { create, q: urlQuery = "", sort = "name" } = Route.useSearch();
  const [query, setQuery] = useState(urlQuery);
  const q = useDebounced(query.trim());
  const admin = canAdminister(org);

  useEffect(() => {
    if (q !== urlQuery) void navigate({ search: (s) => ({ ...s, q: q || undefined }), replace: true });
  }, [q, urlQuery, navigate]);

  const workspaces = useWorkspaceList(org.id, { environment, q: q || undefined, sort, limit: 25 });
  const rows = workspaces.data?.pages.flatMap((p) => p.items) ?? [];
  const total = workspaces.data?.pages[0]?.pagination.total ?? null;
  const setCreate = (open: boolean) =>
    navigate({ search: (s) => ({ ...s, create: open || undefined }), replace: true });
  const empty = total === 0 && !q;

  return (
    <PageContainer>
      <PageHeader
        title="Workspaces"
        description={`One per customer of your product, each isolated: its own agents, installations and runs. Showing ${ENVIRONMENT_LABEL[environment]}.`}
        actions={
          admin ? (
            <Button onClick={() => setCreate(true)}>
              <PlusIcon />
              Create workspace
            </Button>
          ) : null
        }
      />

      {workspaces.isError ? (
        <QueryError error={workspaces.error} onRetry={() => workspaces.refetch()} what="workspaces" />
      ) : empty ? (
        <Panel>
          <Empty>
            <EmptyHeader>
              <EmptyMedia variant="icon">
                <BoxesIcon />
              </EmptyMedia>
              <EmptyTitle>No {ENVIRONMENT_LABEL[environment]} workspaces yet</EmptyTitle>
              <EmptyDescription>
                Most products create one per customer from their backend with{" "}
                <code className="text-xs">POST /v1/organizations/{"{org}"}/workspaces</code>.
              </EmptyDescription>
            </EmptyHeader>
            {admin ? (
              <EmptyContent>
                <Button onClick={() => setCreate(true)}>
                  <PlusIcon />
                  Create workspace
                </Button>
              </EmptyContent>
            ) : null}
          </Empty>
        </Panel>
      ) : (
        <>
          <Toolbar>
            <SearchInput value={query} onChange={setQuery} placeholder="Search name, slug or external ID" />
            <div className="flex items-center gap-3">
              {total !== null ? (
                <span className="tabular text-sm text-muted-foreground">
                  {total.toLocaleString("en")} {total === 1 ? "workspace" : "workspaces"}
                </span>
              ) : null}
              <Segmented
                label="Sort"
                value={sort}
                onChange={(next) =>
                  navigate({ search: (s) => ({ ...s, sort: next === "name" ? undefined : next }), replace: true })
                }
                options={[
                  { value: "name", label: "A–Z" },
                  { value: "activity", label: "Most active" },
                ]}
              />
            </div>
          </Toolbar>
          <Panel className="overflow-hidden">
            {workspaces.isPending ? (
              <RowsSkeleton rows={8} />
            ) : (
              <Table>
                <TableHeader>
                  <TableRow className="hover:bg-transparent">
                    <TableHead className="pl-4">Workspace</TableHead>
                    <TableHead className="hidden lg:table-cell">External ID</TableHead>
                    <TableHead className="hidden text-right md:table-cell">Agents</TableHead>
                    <TableHead className="hidden text-right md:table-cell">Installations</TableHead>
                    <TableHead className="text-right">Runs, 24h</TableHead>
                    <TableHead className="hidden pr-4 xl:table-cell">Created</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {rows.length === 0 ? (
                    <TableRow className="hover:bg-transparent">
                      <TableCell colSpan={6} className="py-10 text-center text-muted-foreground">
                        No {ENVIRONMENT_LABEL[environment]} workspace matches “{q}”.
                      </TableCell>
                    </TableRow>
                  ) : (
                    rows.map((w) => (
                      <TableRow
                        key={w.id}
                        {...rowLinkProps(() => navigate({ to: "/$org/w/$ws", params: { org: org.slug, ws: w.id } }))}
                      >
                        <TableCell className="w-full max-w-0 py-3 pl-4">
                          <p className="truncate font-medium">{w.name}</p>
                          <p className="truncate font-mono text-xs text-muted-foreground">{w.slug}</p>
                        </TableCell>
                        <TableCell className="hidden font-mono text-xs text-muted-foreground lg:table-cell">
                          {w.external_id ?? "—"}
                        </TableCell>
                        <TableCell className="tabular hidden text-right md:table-cell">{w.stats.agents}</TableCell>
                        <TableCell className="tabular hidden text-right md:table-cell">
                          {w.stats.installations}
                        </TableCell>
                        <TableCell className="tabular text-right">{w.stats.runs_last_24h}</TableCell>
                        <TableCell className="hidden pr-4 whitespace-nowrap text-muted-foreground xl:table-cell">
                          <RelativeTime iso={w.created_at} />
                        </TableCell>
                      </TableRow>
                    ))
                  )}
                </TableBody>
              </Table>
            )}
          </Panel>
          {workspaces.hasNextPage ? (
            <div className="mt-4 flex flex-col items-center gap-2">
              <Button
                variant="outline"
                onClick={() => workspaces.fetchNextPage()}
                disabled={workspaces.isFetchingNextPage}
              >
                {workspaces.isFetchingNextPage ? <Spinner /> : null}
                Load more
              </Button>
              <p className="tabular text-xs text-muted-foreground">
                {rows.length.toLocaleString("en")} of {total?.toLocaleString("en")} shown
              </p>
            </div>
          ) : null}
        </>
      )}
      <CreateWorkspaceDialog org={org} environment={environment} open={Boolean(create)} onOpenChange={setCreate} />
    </PageContainer>
  );
}
