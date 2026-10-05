import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { BotIcon, PlusIcon } from "lucide-react";
import { toast } from "sonner";
import { z } from "zod";
import { QueryError, RelativeTime, RowsSkeleton, rowLinkProps } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { AgentStatusPill } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { CreateAgentDialog } from "@/features/agents/agent-form";
import { canChange, useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { useAgents, useCreateAgent, useModels } from "@/features/queries";
import { applyFieldErrors, toastError } from "@/lib/errors";

export const Route = createFileRoute("/_authed/$org/w/$ws/agents/")({
  validateSearch: z.object({ create: z.boolean().optional() }),
  component: AgentsPage,
});

function AgentsPage() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  useDocumentTitle("Agents", ws.name);
  const navigate = useNavigate({ from: Route.fullPath });
  const { create } = Route.useSearch();
  const agents = useAgents(ws.id);
  const models = useModels();
  const createAgent = useCreateAgent(ws.id);
  const setCreate = (open: boolean) => navigate({ search: open ? { create: true } : {}, replace: true });
  const modelName = (id: string) => models.data?.find((m) => m.id === id)?.name ?? id;
  const mayChange = canChange(org);

  return (
    <PageContainer>
      <PageHeader
        title="Agents"
        description="Each agent keeps its configuration and grants across runs. It can only use the installations and tools you grant it here."
        actions={
          mayChange ? (
            <Button onClick={() => setCreate(true)}>
              <PlusIcon />
              Create agent
            </Button>
          ) : null
        }
      />
      {agents.isError ? (
        <QueryError error={agents.error} onRetry={() => agents.refetch()} what="agents" />
      ) : agents.data?.length === 0 ? (
        <Panel>
          <Empty>
            <EmptyHeader>
              <EmptyMedia variant="icon">
                <BotIcon />
              </EmptyMedia>
              <EmptyTitle>No agents in {ws.name} yet</EmptyTitle>
              <EmptyDescription>
                {mayChange
                  ? "Create one, grant it tools on an installation, then start a run."
                  : "Agents appear here once one is created."}
              </EmptyDescription>
            </EmptyHeader>
            {mayChange ? (
              <EmptyContent>
                <Button onClick={() => setCreate(true)}>
                  <PlusIcon />
                  Create agent
                </Button>
              </EmptyContent>
            ) : null}
          </Empty>
        </Panel>
      ) : (
        <Panel className="overflow-hidden">
          {agents.isPending ? (
            <RowsSkeleton rows={3} />
          ) : (
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead className="pl-4">Agent</TableHead>
                  <TableHead className="hidden lg:table-cell">Model</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead className="hidden text-right sm:table-cell">Grants</TableHead>
                  <TableHead className="hidden text-right md:table-cell">Runs, 24h</TableHead>
                  <TableHead className="hidden pr-4 xl:table-cell">Last run</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {agents.data.map((a) => (
                  <TableRow
                    key={a.id}
                    {...rowLinkProps(() =>
                      navigate({
                        to: "/$org/w/$ws/agents/$agentId",
                        params: { org: org.slug, ws: ws.id, agentId: a.id },
                      }),
                    )}
                  >
                    <TableCell className="max-w-0 py-3 pl-4 sm:max-w-none">
                      <p className="font-medium">{a.name}</p>
                      <p className="truncate text-xs text-muted-foreground sm:max-w-md">
                        {a.description || "No description"}
                      </p>
                    </TableCell>
                    <TableCell className="hidden text-muted-foreground lg:table-cell">{modelName(a.model)}</TableCell>
                    <TableCell>
                      <AgentStatusPill status={a.status} />
                    </TableCell>
                    <TableCell className="tabular hidden text-right sm:table-cell">
                      {a.grant_count === 0 ? <span className="text-muted-foreground">None</span> : a.grant_count}
                    </TableCell>
                    <TableCell className="tabular hidden text-right md:table-cell">{a.runs_last_24h}</TableCell>
                    <TableCell className="hidden pr-4 text-muted-foreground xl:table-cell">
                      <RelativeTime iso={a.last_run_at} />
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </Panel>
      )}
      <CreateAgentDialog
        workspace={ws}
        open={Boolean(create) && mayChange}
        onOpenChange={setCreate}
        pending={createAgent.isPending}
        onCreate={(values, form) =>
          createAgent.mutate(values, {
            onSuccess: (agent) => {
              toast.success(`Created ${agent.name}`);
              void navigate({
                to: "/$org/w/$ws/agents/$agentId",
                params: { org: org.slug, ws: ws.id, agentId: agent.id },
                search: { tab: "access" },
              });
            },
            onError: (error) => {
              if (!applyFieldErrors(form, error)) toastError(error, "Couldn't create the agent");
            },
          })
        }
      />
    </PageContainer>
  );
}
