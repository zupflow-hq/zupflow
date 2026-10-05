import { createFileRoute, Link } from "@tanstack/react-router";
import type { WorkspaceId } from "@zupflow/sdk";
import { AlertTriangleIcon, ArrowRightIcon, CheckIcon, HandIcon } from "lucide-react";
import { AppIcon, QueryError, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, Panel, SectionHeader } from "@/components/app/page";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { canChange, useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { useAgents, useInstallations, useRuns } from "@/features/queries";
import { RunTable } from "@/features/runs/run-table";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/_authed/$org/w/$ws/")({
  component: WorkspaceOverview,
});

function Stat({
  label,
  value,
  to,
  children,
}: {
  label: string;
  value: number | undefined;
  to: React.ReactNode;
  children?: React.ReactNode;
}) {
  return (
    <Panel className="flex min-w-0 flex-col justify-between gap-2 p-3 sm:gap-3 sm:p-4">
      <div className="flex items-start justify-between gap-2">
        <p className="truncate text-xs text-muted-foreground sm:text-sm">{label}</p>
        {to}
      </div>
      {value === undefined ? (
        <Skeleton className="h-8 w-12" />
      ) : (
        <p className="tabular text-2xl font-semibold tracking-tight sm:text-3xl">{value}</p>
      )}
      {children}
    </Panel>
  );
}

function WorkspaceOverview() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  useDocumentTitle(ws.name, org.name);
  const params = { org: org.slug, ws: ws.id };
  const installations = useInstallations(ws.id);
  const agents = useAgents(ws.id);
  const runs = useRuns(ws.id, { limit: 8 });
  const waiting = useRuns(ws.id, { status: "waiting_for_approval", limit: 5 });

  const broken = installations.data?.filter((i) => i.status === "reconnect_required") ?? [];
  const waitingRuns = waiting.data?.pages[0]?.items ?? [];
  const connected = installations.data?.filter((i) => i.status === "connected").length;
  const isEmpty = installations.data?.length === 0 || agents.data?.length === 0;

  return (
    <PageContainer>
      {(broken.length > 0 || waitingRuns.length > 0) && (
        <div className="mb-6 space-y-2">
          {broken.map((i) => (
            <Attention
              key={i.id}
              tone="danger"
              icon={<AlertTriangleIcon className="size-4" />}
              title={`${i.app.name} · ${i.label} needs to be reconnected`}
              body={`Agents with access can't use its tools until ${ws.environment === "production" ? "the customer reconnects it from your product" : "the account is reconnected"}. ${i.grant_count} ${i.grant_count === 1 ? "grant is" : "grants are"} affected.`}
              action={
                <Button asChild size="sm" variant="outline">
                  <Link to="/$org/w/$ws/installations/$installationId" params={{ ...params, installationId: i.id }}>
                    {ws.environment === "production" || !canChange(org) ? "View" : "Reconnect"}
                  </Link>
                </Button>
              }
            />
          ))}
          {waitingRuns.map((r) => (
            <Attention
              key={r.id}
              tone="warning"
              icon={<HandIcon className="size-4" />}
              title={`${r.agent_name} is waiting for a decision on a step`}
              body={`Your product collects the decision; the run resumes when your backend records it. “${r.input}”`}
              action={
                <Button asChild size="sm" variant="outline">
                  <Link to="/$org/w/$ws/runs/$runId" params={{ ...params, runId: r.id }}>
                    View run
                  </Link>
                </Button>
              }
            />
          ))}
        </div>
      )}

      {isEmpty && canChange(org) ? (
        <QuickStart
          orgSlug={org.slug}
          wsId={ws.id}
          hasInstallation={(installations.data?.length ?? 0) > 0}
          hasAgent={(agents.data?.length ?? 0) > 0}
          production={ws.environment === "production"}
        />
      ) : null}

      <div className="grid grid-cols-3 gap-2 sm:gap-4">
        <Stat
          label="Agents"
          value={agents.data?.length}
          to={
            <CardLink>
              <Link to="/$org/w/$ws/agents" params={params}>
                {OPEN}
              </Link>
            </CardLink>
          }
        >
          <p className="hidden text-xs text-muted-foreground sm:block">
            {agents.data ? `${agents.data.filter((a) => a.status === "active").length} active` : " "}
          </p>
        </Stat>
        <Stat
          label="Installations"
          value={installations.data?.length}
          to={
            <CardLink>
              <Link to="/$org/w/$ws/installations" params={params}>
                {OPEN}
              </Link>
            </CardLink>
          }
        >
          <p className={cn("hidden text-xs text-muted-foreground sm:block", broken.length > 0 && "text-destructive")}>
            {installations.data
              ? broken.length > 0
                ? `${broken.length} need${broken.length === 1 ? "s" : ""} reconnecting`
                : `${connected} connected`
              : " "}
          </p>
        </Stat>
        <Stat
          label="Runs, last 24h"
          value={ws.stats.runs_last_24h}
          to={
            <CardLink>
              <Link to="/$org/w/$ws/runs" params={params}>
                {OPEN}
              </Link>
            </CardLink>
          }
        >
          <p className="hidden text-xs text-muted-foreground sm:block">
            {waitingRuns.length > 0 ? `${waitingRuns.length} waiting for approval` : "None waiting for approval"}
          </p>
        </Stat>
      </div>

      <div className="mt-8 grid gap-8 lg:grid-cols-[minmax(0,1fr)_20rem]">
        <section className="min-w-0">
          <SectionHeader
            title="Recent runs"
            actions={
              <Button asChild variant="ghost" size="sm">
                <Link to="/$org/w/$ws/runs" params={params}>
                  View all
                </Link>
              </Button>
            }
          />
          <Panel className="overflow-hidden">
            {runs.isPending ? (
              <RowsSkeleton rows={6} />
            ) : runs.isError ? (
              <QueryError error={runs.error} onRetry={() => runs.refetch()} what="runs" className="m-3" />
            ) : (
              <RunTable
                runs={runs.data.pages[0]?.items ?? []}
                orgSlug={org.slug}
                empty="No runs yet. Start one from an agent's page."
              />
            )}
          </Panel>
        </section>
        <section>
          <SectionHeader title="Installations" />
          <Panel>
            {installations.isPending ? (
              <RowsSkeleton rows={3} columns={[60]} />
            ) : installations.isError ? (
              <QueryError
                error={installations.error}
                onRetry={() => installations.refetch()}
                what="installations"
                className="m-3"
              />
            ) : installations.data.length === 0 ? (
              <p className="p-6 text-center text-sm text-muted-foreground">Nothing connected yet.</p>
            ) : (
              <ul className="divide-y">
                {installations.data.map((i) => (
                  <li key={i.id}>
                    <Link
                      to="/$org/w/$ws/installations/$installationId"
                      params={{ ...params, installationId: i.id }}
                      className="flex items-center gap-3 px-4 py-3 hover:bg-accent/50"
                    >
                      <AppIcon icon={i.app.icon} size="sm" />
                      <div className="min-w-0 flex-1">
                        <p className="truncate text-sm font-medium">{i.label}</p>
                        <p className="truncate text-xs text-muted-foreground">{i.app.name}</p>
                      </div>
                      <span
                        role="img"
                        aria-label={i.status.replace("_", " ")}
                        className={cn(
                          "size-2 rounded-full",
                          i.status === "connected" && "bg-success",
                          i.status === "reconnect_required" && "bg-destructive",
                          i.status === "disconnected" && "bg-muted-foreground/40",
                        )}
                      />
                    </Link>
                  </li>
                ))}
              </ul>
            )}
          </Panel>
        </section>
      </div>
    </PageContainer>
  );
}

function CardLink({ children }: { children: React.ReactElement }) {
  return (
    <Button asChild variant="ghost" size="icon-xs" className="-mt-1 -mr-1 text-muted-foreground">
      {children}
    </Button>
  );
}

const OPEN = <ArrowRightIcon aria-label="Open" />;

function Attention({
  tone,
  icon,
  title,
  body,
  action,
}: {
  tone: "danger" | "warning";
  icon: React.ReactNode;
  title: string;
  body: string;
  action: React.ReactNode;
}) {
  return (
    <div
      className={cn(
        "flex flex-col gap-3 rounded-xl border p-3 pl-4 sm:flex-row sm:items-center",
        tone === "danger" ? "border-destructive/30 bg-danger-soft/60" : "border-warning/40 bg-warning-soft/60",
      )}
    >
      <span className={tone === "danger" ? "text-destructive" : "text-warning"}>{icon}</span>
      <div className="min-w-0 flex-1">
        <p className="text-sm font-medium">{title}</p>
        <p className="truncate text-sm text-muted-foreground">{body}</p>
      </div>
      {action}
    </div>
  );
}

function QuickStart({
  orgSlug,
  wsId,
  hasInstallation,
  hasAgent,
  production,
}: {
  orgSlug: string;
  wsId: WorkspaceId;
  hasInstallation: boolean;
  hasAgent: boolean;
  production: boolean;
}) {
  const params = { org: orgSlug, ws: wsId };
  const steps = [
    {
      done: hasInstallation,
      title: production ? "Customer connects an account" : "Connect an account",
      body: production
        ? "In Production your customer does this from your product, through Connect."
        : "Install an app and authorize the account it should act as.",
      to: "/$org/w/$ws/installations" as const,
    },
    {
      done: hasAgent,
      title: "Create an agent",
      body: "Give it instructions and a model, then grant it tools.",
      to: "/$org/w/$ws/agents" as const,
    },
  ];
  return (
    <Panel className="mb-6 grid divide-y sm:grid-cols-2 sm:divide-x sm:divide-y-0">
      {steps.map((s, i) => (
        <div key={s.title} className="flex gap-3 p-4">
          <span
            className={cn(
              "grid size-6 shrink-0 place-items-center rounded-full border font-mono text-[11px] text-muted-foreground",
              s.done && "border-success bg-success text-white",
            )}
          >
            {s.done ? <CheckIcon className="size-3.5" strokeWidth={3} /> : i + 1}
          </span>
          <div className="min-w-0 flex-1">
            <p className="text-sm font-medium">{s.title}</p>
            <p className="text-sm text-muted-foreground">{s.body}</p>
            {!s.done && !(production && s.to === "/$org/w/$ws/installations") ? (
              <Button asChild size="sm" variant="outline" className="mt-3">
                <Link to={s.to} params={params}>
                  Start
                  <ArrowRightIcon />
                </Link>
              </Button>
            ) : null}
          </div>
        </div>
      ))}
    </Panel>
  );
}
