import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import type { Agent, ChannelAppManager, Organization, Workspace } from "@zupflow/sdk";
import {
  AlertTriangleIcon,
  EllipsisIcon,
  KeyRoundIcon,
  LinkIcon,
  MessagesSquareIcon,
  PlusIcon,
  UnlinkIcon,
} from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { z } from "zod";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { AppIcon, QueryError, RelativeTime, RowsSkeleton, rowLinkProps } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { CHANNEL_PROVIDER_ICON, PRODUCTION_CHANNELS } from "@/features/channels/labels";
import { LinkTeamDialog } from "@/features/channels/link-team-dialog";
import { ChannelAppStatusPill, ManagerStatusPill } from "@/features/channels/status";
import { canChange, useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { useAgents, useChannelAppManagers, useChannelApps, useDeleteChannelAppManager } from "@/features/queries";
import { toastError } from "@/lib/errors";

export const Route = createFileRoute("/_authed/$org/w/$ws/channels/")({
  validateSearch: z.object({ link: z.boolean().optional() }),
  component: ChannelsPage,
});

function ChannelsPage() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  useDocumentTitle("Channels", ws.name);
  const navigate = useNavigate({ from: Route.fullPath });
  const { link } = Route.useSearch();
  const managers = useChannelAppManagers(ws.id);
  const agents = useAgents(ws.id);
  const production = ws.environment === "production";
  const mayLink = !production && canChange(org);
  const setLink = (open: boolean) => navigate({ search: open ? { link: true } : {}, replace: true });
  const items = managers.data?.pages.flatMap((page) => page.items) ?? [];

  return (
    <PageContainer>
      <PageHeader
        title="Channels"
        description="Agents people can talk to in Slack. Link a Slack team once; then each agent gets its own app there, added from the agent's Channels tab."
        actions={
          mayLink ? (
            <Button onClick={() => setLink(true)}>
              <PlusIcon />
              Link Slack team
            </Button>
          ) : null
        }
      />
      {production ? <p className="mb-5 rounded-xl border bg-muted/40 p-3 text-sm">{PRODUCTION_CHANNELS}</p> : null}
      {managers.isError ? (
        <QueryError error={managers.error} onRetry={() => managers.refetch()} what="linked teams" />
      ) : managers.isPending ? (
        <TeamSkeleton />
      ) : items.length === 0 ? (
        <Panel>
          <Empty>
            <EmptyHeader>
              <EmptyMedia variant="icon">
                <MessagesSquareIcon />
              </EmptyMedia>
              <EmptyTitle>{mayLink ? "Link a Slack team" : "No Slack team linked yet"}</EmptyTitle>
              <EmptyDescription>
                {production
                  ? "This customer links their Slack team from your product."
                  : mayLink
                    ? "Paste a configuration token from Slack. Zupflow then creates an app for each agent you add, so people can mention it and send it messages."
                    : "Teams appear here once a developer links one."}
              </EmptyDescription>
            </EmptyHeader>
            {mayLink ? (
              <EmptyContent>
                <Button onClick={() => setLink(true)}>
                  <PlusIcon />
                  Link Slack team
                </Button>
              </EmptyContent>
            ) : null}
          </Empty>
        </Panel>
      ) : (
        <div className="space-y-4">
          {items.map((manager) => (
            <TeamCard key={manager.id} org={org} workspace={ws} manager={manager} agents={agents.data} />
          ))}
          {managers.hasNextPage ? (
            <div className="flex justify-center">
              <Button variant="outline" onClick={() => managers.fetchNextPage()} disabled={managers.isFetchingNextPage}>
                {managers.isFetchingNextPage ? <Spinner /> : null}
                Show more teams
              </Button>
            </div>
          ) : null}
        </div>
      )}
      {mayLink ? <LinkTeamDialog workspaceId={ws.id} open={Boolean(link)} onOpenChange={setLink} /> : null}
    </PageContainer>
  );
}

function TeamSkeleton() {
  return (
    <Panel>
      <div className="flex items-center gap-3 px-4 py-4">
        <Skeleton className="size-8 rounded-md" />
        <div className="flex-1 space-y-2">
          <Skeleton className="h-4 w-40" />
          <Skeleton className="h-3 w-64" />
        </div>
      </div>
      <div className="border-t">
        <RowsSkeleton rows={2} columns={[40, 18, 16]} />
      </div>
    </Panel>
  );
}

function TeamCard({
  org,
  workspace,
  manager,
  agents,
}: {
  org: Organization;
  workspace: Workspace;
  manager: ChannelAppManager;
  agents: Agent[] | undefined;
}) {
  const navigate = useNavigate();
  const apps = useChannelApps(workspace.id, { manager_id: manager.id });
  const unlink = useDeleteChannelAppManager(workspace.id);
  const [dialog, setDialog] = useState<null | "replace" | "unlink">(null);
  const mayChange = canChange(org);
  const sandbox = workspace.environment === "sandbox";
  const name = manager.team.name ?? "Slack team";
  const rows = apps.data?.pages.flatMap((page) => page.items) ?? [];
  const agentName = (id: string) => agents?.find((a) => a.id === id)?.name;

  return (
    <Panel className="overflow-hidden">
      <div className="flex items-start gap-3 px-4 py-3.5">
        <AppIcon icon={CHANNEL_PROVIDER_ICON[manager.provider]} />
        <div className="min-w-0 flex-1">
          <p className="flex flex-wrap items-center gap-2">
            <span className="truncate font-medium">{name}</span>
            <ManagerStatusPill status={manager.status} />
          </p>
          <p className="text-sm text-muted-foreground">
            {manager.team.id ? (
              <span className="font-mono text-xs">{manager.team.id}</span>
            ) : (
              "Named at its first install"
            )}
            {" · "}
            Linked by {manager.created_by?.name ?? "a former member"} <RelativeTime iso={manager.created_at} />
            {" · "}
            <span className="tabular">
              {manager.app_count} {manager.app_count === 1 ? "app" : "apps"}
            </span>
          </p>
          <p className="mt-0.5 flex flex-wrap items-center gap-x-1 text-xs text-muted-foreground">
            <KeyRoundIcon className="size-3" />
            Token ending <code className="font-mono">{manager.configuration_token.last4}</code>
            {manager.status === "active" && manager.token_expires_at ? (
              <>
                {" · renews "}
                <RelativeTime iso={manager.token_expires_at} />
              </>
            ) : null}
          </p>
        </div>
        {mayChange ? (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="ghost" size="icon" aria-label={`Actions for ${name}`}>
                <EllipsisIcon />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              {sandbox ? (
                <>
                  <DropdownMenuItem onSelect={() => setDialog("replace")}>
                    <LinkIcon />
                    Replace tokens
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                </>
              ) : null}
              <DropdownMenuItem variant="destructive" onSelect={() => setDialog("unlink")}>
                <UnlinkIcon />
                Unlink team
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        ) : null}
      </div>

      {manager.status === "token_expired" ? (
        <p className="flex items-start gap-2 border-t bg-danger-soft/50 px-4 py-2.5 text-sm">
          <AlertTriangleIcon className="mt-0.5 size-4 shrink-0 text-destructive" />
          <span>
            The configuration token couldn't be renewed. Installed apps keep answering, but adding, changing or removing
            agents' apps waits until{" "}
            {sandbox && mayChange ? (
              <button
                type="button"
                className="font-medium underline underline-offset-2"
                onClick={() => setDialog("replace")}
              >
                you paste a new token
              </button>
            ) : sandbox ? (
              "a developer pastes a new token"
            ) : (
              "your product sends a new one"
            )}
            .
          </span>
        </p>
      ) : null}

      <div className="border-t">
        {apps.isPending ? (
          <RowsSkeleton rows={2} columns={[40, 18, 16]} />
        ) : apps.isError ? (
          <QueryError error={apps.error} onRetry={() => apps.refetch()} what="this team's apps" className="m-3" />
        ) : rows.length === 0 ? (
          <p className="px-4 py-5 text-center text-sm text-muted-foreground">
            No agents here yet. Add one from its Channels tab.
          </p>
        ) : (
          <Table>
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead className="pl-4">Agent</TableHead>
                <TableHead>Status</TableHead>
                <TableHead className="hidden sm:table-cell">Conversations</TableHead>
                <TableHead className="hidden pr-4 md:table-cell">Added</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.map((app) => (
                <TableRow
                  key={app.id}
                  {...rowLinkProps(() =>
                    navigate({
                      to: "/$org/w/$ws/agents/$agentId",
                      params: { org: org.slug, ws: workspace.id, agentId: app.agent_id },
                      search: { tab: "channels" },
                    }),
                  )}
                >
                  <TableCell className="w-full max-w-0 py-3 pl-4">
                    <p className="truncate font-medium">{agentName(app.agent_id) ?? "Agent"}</p>
                    <p className="truncate font-mono text-xs text-muted-foreground">
                      {app.external_app_id ?? "Not created yet"}
                    </p>
                  </TableCell>
                  <TableCell>
                    <ChannelAppStatusPill status={app.status} />
                  </TableCell>
                  <TableCell className="hidden sm:table-cell">
                    {app.connection_id ? (
                      <Link
                        to="/$org/w/$ws/channels/$connectionId"
                        params={{ org: org.slug, ws: workspace.id, connectionId: app.connection_id }}
                        className="text-sm text-brand-ink underline-offset-4 hover:underline"
                      >
                        Browse
                      </Link>
                    ) : (
                      <span className="text-sm text-muted-foreground">—</span>
                    )}
                  </TableCell>
                  <TableCell className="hidden pr-4 text-muted-foreground md:table-cell">
                    <RelativeTime iso={app.created_at} />
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
        {apps.hasNextPage ? (
          <div className="flex justify-center border-t px-4 py-3">
            <Button variant="outline" size="sm" onClick={() => apps.fetchNextPage()} disabled={apps.isFetchingNextPage}>
              {apps.isFetchingNextPage ? <Spinner /> : null}
              Show more apps
            </Button>
          </div>
        ) : null}
      </div>

      {sandbox && mayChange ? (
        <LinkTeamDialog
          workspaceId={workspace.id}
          manager={manager}
          open={dialog === "replace"}
          onOpenChange={(open) => setDialog(open ? "replace" : null)}
        />
      ) : null}
      <ConfirmDialog
        open={dialog === "unlink"}
        onOpenChange={(open) => setDialog(open ? "unlink" : null)}
        title={`Unlink ${name}?`}
        description={
          manager.app_count > 0 ? (
            <p>
              {manager.app_count === 1 ? "An agent still has its app" : `${manager.app_count} agents still have apps`}{" "}
              in this team. Remove {manager.app_count === 1 ? "it" : "them"} from each agent's Channels tab first, then
              unlink the team.
            </p>
          ) : (
            <p>Zupflow forgets the team's configuration token. To add agents here again, link it with a new token.</p>
          )
        }
        actionLabel={manager.app_count > 0 ? "OK" : "Unlink team"}
        destructive={manager.app_count === 0}
        pending={unlink.isPending}
        onConfirm={() =>
          manager.app_count > 0
            ? setDialog(null)
            : unlink.mutate(manager.id, {
                onSuccess: () => {
                  setDialog(null);
                  toast.success(`Unlinked ${name}`);
                },
                onError: (e) => toastError(e, "Couldn't unlink the team"),
              })
        }
      />
    </Panel>
  );
}
