import { createFileRoute, Link } from "@tanstack/react-router";
import type { Environment, Organization, OrganizationId, OrganizationSetup } from "@zupflow/sdk";
import { ArrowRightIcon, CheckIcon } from "lucide-react";
import { ENVIRONMENT_LABEL } from "@/components/app/environment";
import { AppIcon, QueryError, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel, SectionHeader } from "@/components/app/page";
import { RunsChart } from "@/components/app/runs-chart";
import { ConfigStatus } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import { Progress } from "@/components/ui/progress";
import { Skeleton } from "@/components/ui/skeleton";
import { useDocumentTitle, useOrg } from "@/features/context";
import { useEnvironment } from "@/features/environment";
import { useOrgApps, useUsage, useWorkspacePage } from "@/features/queries";
import { compact } from "@/lib/format";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/_authed/$org/")({
  staticData: { crumb: "Overview" },
  component: OverviewPage,
});

function OverviewPage() {
  const org = useOrg();
  const environment = useEnvironment();
  useDocumentTitle("Overview", org.name);
  return (
    <PageContainer>
      <PageHeader
        title={org.name}
        description={`${ENVIRONMENT_LABEL[environment]} activity across every workspace in this organization.`}
      />
      <SetupChecklist org={org} />
      <EnvironmentActivity orgId={org.id} environment={environment} />
      <div className="mt-8 grid gap-8 lg:grid-cols-[minmax(0,1.35fr)_minmax(0,1fr)]">
        <WorkspaceSummary org={org} environment={environment} />
        <CatalogReadiness org={org} environment={environment} />
      </div>
    </PageContainer>
  );
}

const STEPS: {
  key: keyof OrganizationSetup;
  title: string;
  body: string;
  cta: string;
}[] = [
  {
    key: "has_app",
    title: "Add an app to the catalog",
    body: "Register the MCP app your agents will use.",
    cta: "Add app",
  },
  {
    key: "has_ready_configuration",
    title: "Configure an environment",
    body: "Give Sandbox or Production its client credentials.",
    cta: "Configure",
  },
  {
    key: "has_workspace",
    title: "Create a workspace",
    body: "One per customer of your product, fixed to an environment.",
    cta: "Create workspace",
  },
  {
    key: "has_api_key",
    title: "Create an API key",
    body: "Your backend calls Zupflow with an environment-bound key.",
    cta: "Create key",
  },
  {
    key: "has_installation",
    title: "Connect an account",
    body: "Run Connect so a user authorizes their account.",
    cta: "Connect",
  },
  {
    key: "has_agent",
    title: "Create an agent",
    body: "Name it, choose a model, write its instructions.",
    cta: "Create agent",
  },
  {
    key: "has_run",
    title: "Start a run",
    body: "Grant tools, then send the agent its first message.",
    cta: "Start a run",
  },
];

function SetupChecklist({ org }: { org: Organization }) {
  const done = STEPS.filter((s) => org.setup[s.key]).length;
  if (done === STEPS.length) return null;
  const next = STEPS.find((s) => !org.setup[s.key]);
  const o = { org: org.slug };

  const link = (key: keyof OrganizationSetup) => {
    switch (key) {
      case "has_app":
        return { to: "/$org/apps", params: o, search: { create: true } } as const;
      case "has_ready_configuration":
        return { to: "/$org/apps", params: o } as const;
      case "has_workspace":
        return { to: "/$org/workspaces", params: o, search: { create: true } } as const;
      case "has_api_key":
        return { to: "/$org/api-keys", params: o } as const;
      default:
        return { to: "/$org/workspaces", params: o } as const;
    }
  };

  return (
    <Panel className="mb-8 overflow-hidden">
      <div className="flex flex-col gap-3 border-b px-5 py-4 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <h2 className="font-semibold">Set up {org.name}</h2>
          <p className="text-sm text-muted-foreground">
            {done} of {STEPS.length} done. Each step unlocks the next; none of them grants the others.
          </p>
        </div>
        <Progress value={(done / STEPS.length) * 100} className="h-1.5 sm:w-48 [&>div]:bg-brand" />
      </div>
      <ol className="divide-y">
        {STEPS.map((step, i) => {
          const complete = org.setup[step.key];
          const isNext = step === next;
          return (
            <li key={step.key} className={cn("flex items-center gap-4 px-5 py-3", isNext && "bg-brand-soft/40")}>
              <span
                className={cn(
                  "grid size-6 shrink-0 place-items-center rounded-full border font-mono text-[11px]",
                  complete && "border-success bg-success text-white",
                  isNext && "border-brand-ink text-brand-ink",
                  !complete && !isNext && "text-muted-foreground",
                )}
              >
                {complete ? <CheckIcon className="size-3.5" strokeWidth={3} /> : i + 1}
              </span>
              <div className="min-w-0 flex-1">
                <p
                  className={cn(
                    "text-sm font-medium",
                    complete && "text-muted-foreground line-through decoration-muted-foreground/40",
                  )}
                >
                  {step.title}
                </p>
                {!complete ? <p className="text-sm text-muted-foreground">{step.body}</p> : null}
              </div>
              {isNext ? (
                <Button asChild size="sm">
                  <Link {...link(step.key)}>
                    {step.cta}
                    <ArrowRightIcon />
                  </Link>
                </Button>
              ) : null}
            </li>
          );
        })}
      </ol>
    </Panel>
  );
}

function Metric({ label, value, tone }: { label: string; value: string; tone?: "danger" }) {
  return (
    <div className="min-w-0">
      <p className="text-xs text-muted-foreground">{label}</p>
      <p className={cn("tabular mt-0.5 text-xl font-semibold tracking-tight", tone === "danger" && "text-destructive")}>
        {value}
      </p>
    </div>
  );
}

function EnvironmentActivity({ orgId, environment }: { orgId: OrganizationId; environment: Environment }) {
  const usage = useUsage(orgId, environment);
  const workspaces = useWorkspacePage(orgId, { environment, limit: 1 });
  const count = workspaces.data?.pagination.total;
  const totals = usage.data?.totals;
  const failureRate = totals && totals.runs > 0 ? (totals.failed_runs / totals.runs) * 100 : 0;

  return (
    <Panel className="p-5">
      <div className="mb-4 flex items-center justify-between gap-2">
        <p className="text-sm text-muted-foreground">Last 14 days</p>
      </div>
      {usage.isError ? (
        <QueryError error={usage.error} onRetry={() => usage.refetch()} what="activity" />
      ) : (
        <div className="grid gap-6 lg:grid-cols-[13rem_minmax(0,1fr)]">
          <div className="grid grid-cols-2 gap-4 sm:grid-cols-4 lg:grid-cols-1 lg:content-start">
            {totals && count !== undefined && count !== null ? (
              <>
                <Metric label="Workspaces" value={compact(count)} />
                <Metric label="Runs" value={compact(totals.runs)} />
                <Metric
                  label="Failure rate"
                  value={`${failureRate.toFixed(failureRate > 0 && failureRate < 10 ? 1 : 0)}%`}
                  tone={failureRate >= 10 ? "danger" : undefined}
                />
                <Metric label="Tool calls" value={compact(totals.tool_calls)} />
              </>
            ) : (
              [0, 1, 2, 3].map((i) => (
                <div key={i} className="space-y-2">
                  <Skeleton className="h-3 w-14" />
                  <Skeleton className="h-6 w-12" />
                </div>
              ))
            )}
          </div>
          {usage.data ? (
            usage.data.totals.runs > 0 ? (
              <RunsChart days={usage.data.days} label={`${ENVIRONMENT_LABEL[environment]} runs per day`} />
            ) : (
              <div className="grid h-44 place-items-center rounded-lg border border-dashed text-sm text-muted-foreground">
                No runs in {ENVIRONMENT_LABEL[environment]} yet.
              </div>
            )
          ) : (
            <Skeleton className="h-44 w-full" />
          )}
        </div>
      )}
    </Panel>
  );
}

function WorkspaceSummary({ org, environment }: { org: Organization; environment: Environment }) {
  const workspaces = useWorkspacePage(org.id, { environment, sort: "activity", limit: 6 });
  const active = workspaces.data?.items.filter((w) => w.stats.runs_last_24h > 0) ?? [];
  return (
    <section>
      <SectionHeader
        title="Most active workspaces"
        description="By runs in the last 24 hours."
        actions={
          <Button asChild variant="ghost" size="sm">
            <Link to="/$org/workspaces" params={{ org: org.slug }} search={{ sort: "activity" }}>
              View all
            </Link>
          </Button>
        }
      />
      <Panel>
        {workspaces.isPending ? (
          <RowsSkeleton rows={4} columns={[35, 15, 20]} />
        ) : workspaces.isError ? (
          <QueryError error={workspaces.error} onRetry={() => workspaces.refetch()} what="workspaces" className="m-3" />
        ) : active.length === 0 ? (
          <p className="p-6 text-center text-sm text-muted-foreground">
            No {ENVIRONMENT_LABEL[environment]} runs in the last 24 hours.
          </p>
        ) : (
          <ul className="divide-y">
            {active.map((ws) => (
              <li key={ws.id}>
                <Link
                  to="/$org/w/$ws"
                  params={{ org: org.slug, ws: ws.id }}
                  className="flex items-center gap-3 px-4 py-3 transition-colors hover:bg-accent/50"
                >
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-sm font-medium">{ws.name}</p>
                    <p className="truncate font-mono text-xs text-muted-foreground">{ws.slug}</p>
                  </div>
                  <span className="tabular hidden w-20 text-right text-sm text-muted-foreground sm:block">
                    {ws.stats.agents} {ws.stats.agents === 1 ? "agent" : "agents"}
                  </span>
                  <span className="tabular w-20 text-right text-sm">
                    {ws.stats.runs_last_24h} <span className="text-muted-foreground">runs</span>
                  </span>
                </Link>
              </li>
            ))}
          </ul>
        )}
      </Panel>
    </section>
  );
}

function CatalogReadiness({ org, environment }: { org: Organization; environment: Environment }) {
  const apps = useOrgApps(org.id);
  return (
    <section>
      <SectionHeader
        title="Catalog readiness"
        description={`Workspaces can install an app once its ${ENVIRONMENT_LABEL[environment]} configuration is ready.`}
      />
      <Panel>
        {apps.isPending ? (
          <RowsSkeleton rows={4} columns={[40, 20]} />
        ) : apps.isError ? (
          <QueryError error={apps.error} onRetry={() => apps.refetch()} what="apps" className="m-3" />
        ) : apps.data.length === 0 ? (
          <div className="flex flex-col items-center gap-3 p-6 text-center">
            <p className="text-sm text-muted-foreground">Your catalog is empty.</p>
            <Button asChild size="sm" variant="outline">
              <Link to="/$org/apps" params={{ org: org.slug }} search={{ create: true }}>
                Add an app
              </Link>
            </Button>
          </div>
        ) : (
          <ul className="divide-y">
            {apps.data.map((app) => (
              <li key={app.id}>
                <Link
                  to="/$org/apps/$appId"
                  params={{ org: org.slug, appId: app.id }}
                  search={{ tab: "configuration" }}
                  className="flex items-center gap-3 px-4 py-3 transition-colors hover:bg-accent/50"
                >
                  <AppIcon icon={app.icon} size="sm" />
                  <span className="min-w-0 flex-1 truncate text-sm font-medium">{app.name}</span>
                  <ConfigStatus
                    status={app.configurations.find((c) => c.environment === environment)?.status ?? "unconfigured"}
                  />
                </Link>
              </li>
            ))}
          </ul>
        )}
      </Panel>
    </section>
  );
}
