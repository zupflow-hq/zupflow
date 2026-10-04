import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { RUN_STATUSES, type RunStatus } from "@zupflow/sdk";
import { z } from "zod";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { QueryError, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { RUN_STATUS_LABEL } from "@/components/app/status";
import { Toolbar } from "@/components/app/toolbar";
import { Button } from "@/components/ui/button";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { useDocumentTitle, useOrg } from "@/features/context";
import { useEnvironment } from "@/features/environment";
import { useOrganizationRuns } from "@/features/queries";
import { RunTable } from "@/features/runs/run-table";
import { WorkspacePicker } from "@/features/workspaces/workspace-picker";

export const Route = createFileRoute("/_authed/$org/runs")({
  staticData: { crumb: "Runs" },
  validateSearch: z.object({
    workspace: z.string().optional(),
    status: z.enum(RUN_STATUSES).optional(),
  }),
  component: OrganizationRunsPage,
});

const ANY = "__any";

/** Every run in the current environment, across all workspaces: the place to watch the system. */
function OrganizationRunsPage() {
  const org = useOrg();
  const environment = useEnvironment();
  useDocumentTitle("Runs", org.name);
  const navigate = useNavigate({ from: Route.fullPath });
  const { workspace, status } = Route.useSearch();
  const runs = useOrganizationRuns(org.id, { environment, workspace_id: workspace, status, limit: 25 });
  const items = runs.data?.pages.flatMap((p) => p.items) ?? [];
  const total = runs.data?.pages[0]?.pagination.total;
  const filtered = Boolean(workspace || status);

  return (
    <PageContainer>
      <PageHeader
        title="Runs"
        description={`Every ${ENVIRONMENT_LABEL[environment]} run across your workspaces, newest first. Open one to see its tool calls and approvals.`}
      />
      <Toolbar>
        <div className="flex flex-wrap gap-2">
          <WorkspacePicker
            orgId={org.id}
            environment={environment}
            value={workspace}
            onChange={(id) => navigate({ search: (s) => ({ ...s, workspace: id }), replace: true })}
          />
          <Select
            value={status ?? ANY}
            onValueChange={(v) =>
              navigate({ search: (s) => ({ ...s, status: v === ANY ? undefined : (v as RunStatus) }), replace: true })
            }
          >
            <SelectTrigger aria-label="Status" className="w-full bg-card sm:w-48">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ANY}>Any status</SelectItem>
              {RUN_STATUSES.map((s) => (
                <SelectItem key={s} value={s}>
                  {RUN_STATUS_LABEL[s]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {filtered ? (
            <Button
              variant="ghost"
              onClick={() =>
                navigate({ search: (s) => ({ ...s, workspace: undefined, status: undefined }), replace: true })
              }
            >
              Clear filters
            </Button>
          ) : null}
        </div>
        {total !== undefined && total !== null ? (
          <p className="tabular text-sm text-muted-foreground">
            {total.toLocaleString("en")} {total === 1 ? "run" : "runs"}
          </p>
        ) : null}
      </Toolbar>
      <Panel className="overflow-hidden">
        {runs.isPending ? (
          <RowsSkeleton rows={8} />
        ) : runs.isError ? (
          <QueryError error={runs.error} onRetry={() => runs.refetch()} what="runs" className="m-3" />
        ) : (
          <RunTable
            runs={items}
            orgSlug={org.slug}
            empty={filtered ? "No runs match those filters." : `No ${ENVIRONMENT_LABEL[environment]} runs yet.`}
          />
        )}
      </Panel>
      {runs.hasNextPage ? (
        <div className="mt-4 flex justify-center">
          <Button variant="outline" onClick={() => runs.fetchNextPage()} disabled={runs.isFetchingNextPage}>
            {runs.isFetchingNextPage ? <Spinner /> : null}
            Load more
          </Button>
        </div>
      ) : null}
    </PageContainer>
  );
}
