import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import type { Agent, Organization, Workspace } from "@zupflow/sdk";
import { EllipsisIcon, PauseIcon, PlayIcon, Trash2Icon } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { z } from "zod";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { LineTabsList, LineTabsTrigger } from "@/components/app/line-tabs";
import { MonoValue, QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, Panel, Properties, Property, SectionHeader } from "@/components/app/page";
import { AgentStatusPill } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Form } from "@/components/ui/form";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { Tabs, TabsContent } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { AgentAccess } from "@/features/agents/access";
import { AgentFields, useAgentForm } from "@/features/agents/agent-form";
import { StartRunSheet } from "@/features/agents/start-run-sheet";
import { canChange, useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { useAgent, useDeleteAgent, useModels, useRuns, useUpdateAgent } from "@/features/queries";
import { RunTable } from "@/features/runs/run-table";
import { useDetailCrumb } from "@/lib/crumbs";
import { applyFieldErrors, toastError } from "@/lib/errors";
import { idParam } from "@/lib/route-ids";

const TABS = ["overview", "access", "configuration"] as const;

export const Route = createFileRoute("/_authed/$org/w/$ws/agents/$agentId")({
  params: idParam("agentId", "agent"),
  validateSearch: z.object({ tab: z.enum(TABS).optional(), run: z.boolean().optional() }),
  component: AgentPage,
});

function AgentPage() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  const { agentId } = Route.useParams();
  const { tab = "overview", run: runOpen } = Route.useSearch();
  const navigate = useNavigate({ from: Route.fullPath });
  const agent = useAgent(ws.id, agentId);
  const update = useUpdateAgent(ws.id, agentId);
  const remove = useDeleteAgent(ws.id);
  const models = useModels();
  const [deleting, setDeleting] = useState(false);
  useDetailCrumb(agent.data?.name);
  useDocumentTitle(agent.data?.name, "Agents", ws.name);

  if (agent.isError) {
    return (
      <PageContainer>
        <QueryError error={agent.error} onRetry={() => agent.refetch()} what="this agent" />
      </PageContainer>
    );
  }

  const a = agent.data;
  const mayChange = canChange(org);
  const runBlocked = !mayChange
    ? "Viewers can't start runs. Ask a developer or an admin."
    : ws.environment === "production"
      ? "In Production, runs start from your product through the API. Test here in a Sandbox workspace."
      : a?.status === "paused"
        ? "Resume the agent to start runs."
        : null;
  const setRunOpen = (open: boolean) => navigate({ search: (s) => ({ ...s, run: open || undefined }), replace: true });

  return (
    <PageContainer>
      <header className="mb-6 flex flex-col gap-4 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0 space-y-1.5">
          {a ? (
            <>
              <div className="flex flex-wrap items-center gap-2">
                <h2 className="text-xl leading-tight font-semibold tracking-[-0.02em]">{a.name}</h2>
                <AgentStatusPill status={a.status} />
              </div>
              <p className="text-sm text-muted-foreground">
                {a.description || "No description"} · {models.data?.find((m) => m.id === a.model)?.name ?? a.model}
              </p>
            </>
          ) : (
            <>
              <Skeleton className="h-8 w-60" />
              <Skeleton className="h-4 w-80" />
            </>
          )}
        </div>
        {a ? (
          <div className="flex shrink-0 items-center gap-2">
            {runBlocked ? (
              <Tooltip>
                <TooltipTrigger asChild>
                  {/* biome-ignore lint/a11y/noNoninteractiveTabindex: a disabled button can't take focus, so its wrapper carries the tooltip. */}
                  <span tabIndex={0}>
                    <Button disabled>
                      <PlayIcon />
                      Start run
                    </Button>
                  </span>
                </TooltipTrigger>
                <TooltipContent className="max-w-64">{runBlocked}</TooltipContent>
              </Tooltip>
            ) : (
              <Button onClick={() => setRunOpen(true)}>
                <PlayIcon />
                Start run
              </Button>
            )}
            {mayChange ? (
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button variant="outline" size="icon" aria-label="More actions">
                    {update.isPending ? <Spinner /> : <EllipsisIcon />}
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuItem
                    onSelect={() =>
                      update.mutate(
                        { status: a.status === "active" ? "paused" : "active" },
                        {
                          onSuccess: (next) =>
                            toast.success(next.status === "paused" ? `Paused ${next.name}` : `Resumed ${next.name}`),
                          onError: (e) => toastError(e),
                        },
                      )
                    }
                  >
                    {a.status === "active" ? <PauseIcon /> : <PlayIcon />}
                    {a.status === "active" ? "Pause agent" : "Resume agent"}
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                  <DropdownMenuItem variant="destructive" onSelect={() => setDeleting(true)}>
                    <Trash2Icon />
                    Delete agent
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            ) : null}
          </div>
        ) : null}
      </header>

      <Tabs
        value={tab}
        onValueChange={(v) => navigate({ search: { tab: v as (typeof TABS)[number] }, replace: true })}
        className="gap-6"
      >
        <LineTabsList>
          <LineTabsTrigger value="overview">Overview</LineTabsTrigger>
          <LineTabsTrigger value="access">
            Access
            {a ? (
              <span className="tabular rounded-full bg-muted px-1.5 text-xs text-muted-foreground">
                {a.grant_count}
              </span>
            ) : null}
          </LineTabsTrigger>
          <LineTabsTrigger value="configuration">Configuration</LineTabsTrigger>
        </LineTabsList>
        <TabsContent value="overview">
          {a ? <OverviewTab org={org} workspace={ws} agent={a} /> : <RowsSkeleton />}
        </TabsContent>
        <TabsContent value="access">{a ? <AgentAccess org={org} workspace={ws} agent={a} /> : null}</TabsContent>
        <TabsContent value="configuration">
          {a ? <ConfigurationTab workspace={ws} agent={a} readOnly={!mayChange} /> : null}
        </TabsContent>
      </Tabs>

      {a ? (
        <>
          <StartRunSheet
            orgSlug={org.slug}
            workspace={ws}
            agent={a}
            open={Boolean(runOpen) && !runBlocked}
            onOpenChange={setRunOpen}
          />
          <ConfirmDialog
            open={deleting}
            onOpenChange={setDeleting}
            title={`Delete ${a.name}?`}
            description={
              <p>Its grants are removed immediately. Past runs stay in the run history. This can't be undone.</p>
            }
            confirmText={a.name}
            actionLabel="Delete agent"
            pending={remove.isPending}
            onConfirm={() =>
              remove.mutate(a.id, {
                onSuccess: () => {
                  toast.success(`Deleted ${a.name}`);
                  void navigate({ to: "/$org/w/$ws/agents", params: { org: org.slug, ws: ws.id } });
                },
                onError: (e) => toastError(e, "Couldn't delete the agent"),
              })
            }
          />
        </>
      ) : null}
    </PageContainer>
  );
}

function OverviewTab({ org, workspace, agent }: { org: Organization; workspace: Workspace; agent: Agent }) {
  const runs = useRuns(workspace.id, { agent_id: agent.id, limit: 10 });
  return (
    <div className="grid gap-8 lg:grid-cols-[minmax(0,1fr)_18rem]">
      <section className="min-w-0">
        <SectionHeader
          title="Recent runs"
          actions={
            <Button asChild variant="ghost" size="sm">
              <Link to="/$org/w/$ws/runs" params={{ org: org.slug, ws: workspace.id }} search={{ agent: agent.id }}>
                View all
              </Link>
            </Button>
          }
        />
        <Panel className="overflow-hidden">
          {runs.isPending ? (
            <RowsSkeleton rows={5} />
          ) : runs.isError ? (
            <QueryError error={runs.error} onRetry={() => runs.refetch()} what="runs" className="m-3" />
          ) : (
            <RunTable
              runs={runs.data.pages[0]?.items ?? []}
              orgSlug={org.slug}
              showAgent={false}
              empty={canChange(org) ? "No runs yet. Start one to see the agent work." : "No runs yet."}
            />
          )}
        </Panel>
      </section>
      <section>
        <SectionHeader title="Details" />
        <Panel>
          <Properties>
            <Property label="Agent ID" className="sm:grid-cols-1 sm:gap-1">
              <MonoValue value={agent.id} />
            </Property>
            <Property label="Runs, last 24h" className="sm:grid-cols-1 sm:gap-1">
              <span className="tabular">{agent.runs_last_24h}</span>
            </Property>
            <Property label="Last run" className="sm:grid-cols-1 sm:gap-1">
              <RelativeTime iso={agent.last_run_at} />
            </Property>
            <Property label="Updated" className="sm:grid-cols-1 sm:gap-1">
              <RelativeTime iso={agent.updated_at} />
            </Property>
            <Property label="Memory" className="sm:grid-cols-1 sm:gap-1">
              <span className="text-muted-foreground">
                None. Each run gets its session context only; long-term memory isn't available yet.
              </span>
            </Property>
          </Properties>
        </Panel>
      </section>
    </div>
  );
}

function ConfigurationTab({ workspace, agent, readOnly }: { workspace: Workspace; agent: Agent; readOnly: boolean }) {
  const update = useUpdateAgent(workspace.id, agent.id);
  const form = useAgentForm(agent);
  return (
    <Panel className="max-w-3xl">
      <Form {...form}>
        <form
          className="space-y-6 p-5"
          onSubmit={form.handleSubmit((values) =>
            update.mutate(values, {
              onSuccess: () => toast.success("Saved configuration"),
              onError: (error) => {
                if (!applyFieldErrors(form, error)) toastError(error, "Couldn't save");
              },
            }),
          )}
        >
          <AgentFields form={form} readOnly={readOnly} />
          {readOnly ? (
            <p className="border-t pt-5 text-xs text-muted-foreground">
              You can view this configuration. Owners, admins and developers can change it.
            </p>
          ) : (
            <div className="flex items-center gap-2 border-t pt-5">
              <Button type="submit" disabled={!form.formState.isDirty || update.isPending}>
                {update.isPending ? <Spinner /> : null}
                Save configuration
              </Button>
              {form.formState.isDirty ? (
                <Button type="button" variant="ghost" onClick={() => form.reset()}>
                  Discard
                </Button>
              ) : null}
              <span className="ml-auto text-xs text-muted-foreground">Applies to runs started after you save.</span>
            </div>
          )}
        </form>
      </Form>
    </Panel>
  );
}
