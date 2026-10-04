import { createFileRoute, Link } from "@tanstack/react-router";
import { TERMINAL_RUN_STATUSES } from "@zupflow/sdk";
import { BanIcon } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { MonoValue, QueryError, RelativeTime } from "@/components/app/misc";
import { PageContainer, Panel, Properties, Property, SectionHeader } from "@/components/app/page";
import { RunStatusPill } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { useCancelRun, useRun, useRunEvents } from "@/features/queries";
import { RunTimeline } from "@/features/runs/timeline";
import { useDetailCrumb } from "@/lib/crumbs";
import { toastError } from "@/lib/errors";
import { duration, fullDateTime, shortId } from "@/lib/format";
import { idParam } from "@/lib/route-ids";

export const Route = createFileRoute("/_authed/$org/w/$ws/runs/$runId")({
  params: idParam("runId", "run"),
  component: RunPage,
});

function RunPage() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  const { runId } = Route.useParams();
  const run = useRun(ws.id, runId);
  const live = run.data ? !TERMINAL_RUN_STATUSES.includes(run.data.status) : true;
  const events = useRunEvents(ws.id, runId, live);
  const cancel = useCancelRun(ws.id, runId);
  const [cancelling, setCancelling] = useState(false);
  useDetailCrumb(run.data ? `Run ${shortId(run.data.id)}` : undefined);
  useDocumentTitle(run.data ? `Run ${shortId(run.data.id)}` : "Run", ws.name);

  if (run.isError) {
    return (
      <PageContainer>
        <QueryError error={run.error} onRetry={() => run.refetch()} what="this run" />
      </PageContainer>
    );
  }

  const r = run.data;
  return (
    <PageContainer>
      <header className="mb-6 flex flex-col gap-4 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0 space-y-1.5">
          {r ? (
            <>
              <div className="flex flex-wrap items-center gap-2">
                <h2 className="text-xl leading-tight font-semibold tracking-[-0.02em]">
                  Run <span className="font-mono text-[1.15rem] font-medium">{shortId(r.id)}</span>
                </h2>
                <RunStatusPill status={r.status} />
              </div>
              <p className="text-sm text-muted-foreground">
                <Link
                  to="/$org/w/$ws/agents/$agentId"
                  params={{ org: org.slug, ws: ws.id, agentId: r.agent_id }}
                  className="font-medium text-foreground hover:underline"
                >
                  {r.agent_name}
                </Link>{" "}
                · started <RelativeTime iso={r.created_at} /> ·{" "}
                {r.trigger === "console" ? "from the Console" : "via the API"}
              </p>
            </>
          ) : (
            <>
              <Skeleton className="h-8 w-52" />
              <Skeleton className="h-4 w-72" />
            </>
          )}
        </div>
        {r && !TERMINAL_RUN_STATUSES.includes(r.status) ? (
          <Button variant="outline" onClick={() => setCancelling(true)}>
            <BanIcon />
            Cancel run
          </Button>
        ) : null}
      </header>

      <div className="grid gap-8 lg:grid-cols-[minmax(0,1fr)_19rem]">
        <section className="min-w-0">
          <SectionHeader
            title="Timeline"
            description="Tool calls, approvals and messages, in order. Times are from the moment the run was queued."
          />
          <Panel className="p-4 sm:p-5">
            {events.isError ? (
              <QueryError error={events.error} onRetry={() => events.refetch()} what="events" />
            ) : r && events.data ? (
              <RunTimeline run={r} events={events.data} />
            ) : (
              <div className="space-y-5">
                {[70, 45, 85, 55].map((w) => (
                  <div key={w} className="flex gap-3">
                    <Skeleton className="h-4 w-12" />
                    <Skeleton className="size-6 rounded-full" />
                    <Skeleton className="h-10" style={{ width: `${w}%` }} />
                  </div>
                ))}
              </div>
            )}
          </Panel>
        </section>

        <aside className="space-y-6">
          {r?.output ? (
            <section>
              <SectionHeader title="Output" />
              <Panel className="p-4 text-sm leading-relaxed whitespace-pre-wrap">{r.output}</Panel>
            </section>
          ) : null}
          <section>
            <SectionHeader title="Details" />
            <Panel>
              {r ? (
                <Properties>
                  <Property label="Run ID" className="sm:grid-cols-1 sm:gap-1">
                    <MonoValue value={r.id} />
                  </Property>
                  <Property label="Session" className="sm:grid-cols-1 sm:gap-1">
                    <MonoValue value={r.session_id} />
                  </Property>
                  <Property label="Timing" className="sm:grid-cols-1 sm:gap-1">
                    <span title={fullDateTime(r.started_at)}>{duration(r.started_at, r.finished_at)}</span>
                    {r.finished_at ? (
                      <span className="text-muted-foreground">
                        {" "}
                        · finished <RelativeTime iso={r.finished_at} />
                      </span>
                    ) : null}
                  </Property>
                  <Property label="Usage" className="sm:grid-cols-1 sm:gap-1">
                    <span className="tabular">
                      {r.usage.tool_calls} tool {r.usage.tool_calls === 1 ? "call" : "calls"} ·{" "}
                      {r.usage.input_tokens.toLocaleString("en")} in · {r.usage.output_tokens.toLocaleString("en")} out
                    </span>
                  </Property>
                </Properties>
              ) : (
                <div className="space-y-4 p-4">
                  {[0, 1, 2, 3].map((i) => (
                    <Skeleton key={i} className="h-9 w-full" />
                  ))}
                </div>
              )}
            </Panel>
          </section>
        </aside>
      </div>

      <ConfirmDialog
        open={cancelling}
        onOpenChange={setCancelling}
        title="Cancel this run?"
        description={
          <p>It stops before its next step. Actions it already completed on external accounts can't be undone.</p>
        }
        actionLabel="Cancel run"
        pending={cancel.isPending}
        onConfirm={() =>
          cancel.mutate(undefined, {
            onSuccess: () => {
              setCancelling(false);
              toast.success("Run cancelled");
            },
            onError: (e) => toastError(e, "Couldn't cancel the run"),
          })
        }
      />
    </PageContainer>
  );
}
