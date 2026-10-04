import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { RUN_STATUSES, type RunStatus } from "@zupflow/sdk";
import { z } from "zod";
import { QueryError, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { RUN_STATUS_LABEL } from "@/components/app/status";
import { Toolbar } from "@/components/app/toolbar";
import { Button } from "@/components/ui/button";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { useAgents, useRuns } from "@/features/queries";
import { RunTable } from "@/features/runs/run-table";

export const Route = createFileRoute("/_authed/$org/w/$ws/runs/")({
  validateSearch: z.object({
    agent: z.string().optional(),
    status: z.enum(RUN_STATUSES).optional(),
  }),
  component: RunsPage,
});

const ALL = "__all";

function RunsPage() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  useDocumentTitle("Runs", ws.name);
  const navigate = useNavigate({ from: Route.fullPath });
  const { agent, status } = Route.useSearch();
  const agents = useAgents(ws.id);
  const runs = useRuns(ws.id, { agent_id: agent, status, limit: 25 });
  const items = runs.data?.pages.flatMap((p) => p.items) ?? [];
  const total = runs.data?.pages[0]?.pagination.total;
  const filtered = Boolean(agent || status);

  return (
    <PageContainer>
      <PageHeader
        title="Runs"
        description="Every execution in this workspace, newest first. Runs record each tool call and its result; external actions are never retried blindly."
      />
      <Toolbar>
        <div className="flex flex-wrap gap-2">
          <Select
            value={agent ?? ALL}
            onValueChange={(v) =>
              navigate({ search: (s) => ({ ...s, agent: v === ALL ? undefined : v }), replace: true })
            }
          >
            <SelectTrigger aria-label="Agent" className="w-full bg-card sm:w-52">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL}>All agents</SelectItem>
              {agents.data?.map((a) => (
                <SelectItem key={a.id} value={a.id}>
                  {a.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Select
            value={status ?? ALL}
            onValueChange={(v) =>
              navigate({ search: (s) => ({ ...s, status: v === ALL ? undefined : (v as RunStatus) }), replace: true })
            }
          >
            <SelectTrigger aria-label="Status" className="w-full bg-card sm:w-48">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL}>Any status</SelectItem>
              {RUN_STATUSES.map((s) => (
                <SelectItem key={s} value={s}>
                  {RUN_STATUS_LABEL[s]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {filtered ? (
            <Button variant="ghost" onClick={() => navigate({ search: {}, replace: true })}>
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
            empty={filtered ? "No runs match those filters." : "No runs yet. Start one from an agent's page."}
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
