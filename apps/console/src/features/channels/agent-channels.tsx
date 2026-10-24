import { Link } from "@tanstack/react-router";
import {
  type Agent,
  type AgentChannel,
  agentChannelDefaults,
  type ChannelApp,
  type ChannelAppManager,
  type ChannelAppManagerId,
  type ChannelConnection,
  type Organization,
  type Workspace,
} from "@zupflow/sdk";
import {
  AlertTriangleIcon,
  CheckCircle2Icon,
  EllipsisIcon,
  MessagesSquareIcon,
  PlusIcon,
  RefreshCwIcon,
  Trash2Icon,
  VolumeXIcon,
} from "lucide-react";
import { useEffect, useState } from "react";
import { toast } from "sonner";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { AppIcon, QueryError } from "@/components/app/misc";
import { Panel } from "@/components/app/page";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { canChange } from "@/features/context";
import { ConnectError, ConnectProgress, useConnectFlow } from "@/features/installations/connect-flow";
import {
  useAgentChannels,
  useChannelAppManagers,
  useChannelAppManagersById,
  useChannelApps,
  useChannelConnections,
  useCreateChannelApp,
  useDeleteAgentChannel,
  useDeleteChannelApp,
  usePutAgentChannel,
} from "@/features/queries";
import { toastError } from "@/lib/errors";
import { AgentChannelSettings } from "./agent-channel-settings";
import { appFailure, CHANNEL_PROVIDER_ICON, PRODUCTION_CHANNELS } from "./labels";
import { ChannelAppStatusPill, ConnectionStatusPill } from "./status";

/**
 * An agent in Slack: its own app in each linked team, how it gets there
 * (created by Zupflow, then allowed by someone in the team) and, once
 * installed, what it does there.
 */
export function AgentChannels({ org, workspace, agent }: { org: Organization; workspace: Workspace; agent: Agent }) {
  const apps = useChannelApps(workspace.id, { agent_id: agent.id });
  const managers = useChannelAppManagers(workspace.id);
  const connections = useChannelConnections(workspace.id, { agent_id: agent.id });
  const channels = useAgentChannels(workspace.id, agent.id);
  const appList = apps.data?.pages.flatMap((page) => page.items) ?? [];
  const managerList = managers.data?.pages.flatMap((page) => page.items) ?? [];
  const connectionList = connections.data?.pages.flatMap((page) => page.items) ?? [];
  // An app's team may be past the loaded pages of teams: it's read on its own.
  const unlisted = [...new Set(appList.map((a) => a.manager_id))].filter((id) => !managerList.some((m) => m.id === id));
  const looked = useChannelAppManagersById(workspace.id, unlisted);
  // A failed app doesn't hold its team: the agent can be added there again.
  const holding = new Set(appList.filter((a) => a.status !== "failed").map((a) => a.manager_id));
  const open = managerList.filter((m) => !holding.has(m.id));
  const offerable = open.some((m) => m.status === "active");

  // An agent has an app per team at most, so its apps and connections are read in full. Teams load
  // further only while none on the loaded pages can take the agent; the picker loads more on request.
  useEffect(() => {
    if (apps.hasNextPage && !apps.isFetchingNextPage) void apps.fetchNextPage();
  }, [apps]);
  useEffect(() => {
    if (connections.hasNextPage && !connections.isFetchingNextPage) void connections.fetchNextPage();
  }, [connections]);
  useEffect(() => {
    if (managers.data && !offerable && managers.hasNextPage && !managers.isFetchingNextPage) {
      void managers.fetchNextPage();
    }
  }, [managers, offerable]);

  const queries = [apps, managers, connections, channels];
  const failed = queries.find((q) => q.isError);
  if (failed) {
    return (
      <QueryError
        error={failed.error}
        onRetry={() => {
          for (const q of queries) void q.refetch();
        }}
        what="this agent's channels"
      />
    );
  }
  if (!apps.data || !managers.data || !connections.data || !channels.data) {
    return (
      <Panel>
        <div className="flex items-center gap-3 px-4 py-4">
          <Skeleton className="size-8 rounded-md" />
          <div className="flex-1 space-y-2">
            <Skeleton className="h-4 w-40" />
            <Skeleton className="h-3 w-56" />
          </div>
        </div>
        <div className="space-y-3 border-t p-4">
          <Skeleton className="h-4 w-2/3" />
          <Skeleton className="h-4 w-1/2" />
        </div>
      </Panel>
    );
  }

  const managerOf = (id: ChannelAppManagerId) =>
    managerList.find((m) => m.id === id) ?? looked.find((q) => q.data?.id === id)?.data;

  return (
    <div className="space-y-4">
      {appList.map((app) => (
        <AppCard
          key={app.id}
          org={org}
          workspace={workspace}
          agent={agent}
          app={app}
          manager={managerOf(app.manager_id)}
          connection={connectionList.find((c) => c.id === app.connection_id)}
          channel={channels.data.find((c) => c.connection_id === app.connection_id)}
        />
      ))}
      <AddToSlack
        org={org}
        workspace={workspace}
        agent={agent}
        teams={open}
        hasApps={appList.length > 0}
        anyLinked={managerList.length > 0}
        more={managers.hasNextPage}
        loadingMore={managers.isFetchingNextPage}
        onLoadMore={() => void managers.fetchNextPage()}
      />
    </div>
  );
}

/** Adds the agent to a linked team: Zupflow creates its app, then someone in the team allows it. */
function AddToSlack({
  org,
  workspace,
  agent,
  teams,
  hasApps,
  anyLinked,
  more,
  loadingMore,
  onLoadMore,
}: {
  org: Organization;
  workspace: Workspace;
  agent: Agent;
  /** Linked teams the agent has no app in, on the loaded pages. */
  teams: ChannelAppManager[];
  hasApps: boolean;
  anyLinked: boolean;
  /** Whether more linked teams remain to be loaded. */
  more: boolean;
  loadingMore: boolean;
  onLoadMore: () => void;
}) {
  const create = useCreateChannelApp(workspace.id);
  const active = teams.filter((t) => t.status === "active");
  const [team, setTeam] = useState<ChannelAppManagerId | undefined>(undefined);
  const chosen = active.find((t) => t.id === team)?.id ?? active[0]?.id;
  const production = workspace.environment === "production";
  const mayChange = canChange(org);
  // Nothing to offer once the agent is in every linked team.
  if (hasApps && teams.length === 0 && !more) return null;

  const body = production
    ? PRODUCTION_CHANNELS
    : !mayChange
      ? "Developers, admins and owners can add agents to Slack."
      : !anyLinked
        ? "Link a Slack team on this workspace's Channels page first. Then add the agent here: Zupflow creates its own Slack app, and someone in the team allows it."
        : active.length === 0
          ? "Every linked team's configuration token has expired. Paste a new one on the Channels page, then add the agent."
          : `Zupflow creates ${agent.name}'s own Slack app in the team, named after it, so people can mention it and send it messages. Then someone in the team allows it.`;
  const canAdd = !production && mayChange && active.length > 0;

  return (
    <Panel>
      <Empty className={hasApps ? "py-8" : undefined}>
        <EmptyHeader>
          <EmptyMedia variant="icon">
            <MessagesSquareIcon />
          </EmptyMedia>
          <EmptyTitle>{hasApps ? "Add to another Slack team" : `Add ${agent.name} to Slack`}</EmptyTitle>
          <EmptyDescription>{body}</EmptyDescription>
        </EmptyHeader>
        {canAdd ? (
          <EmptyContent>
            <div className="flex w-full max-w-sm flex-col gap-2 sm:flex-row sm:justify-center">
              {active.length > 1 || more ? (
                <Select value={chosen} onValueChange={(v) => setTeam(v as ChannelAppManagerId)}>
                  <SelectTrigger aria-label="Slack team" className="w-full bg-card sm:flex-1">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {active.map((t) => (
                      <SelectItem key={t.id} value={t.id}>
                        {t.team.name ?? `Slack team ending ${t.configuration_token.last4}`}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              ) : null}
              <Button
                className={active.length > 1 || more ? undefined : "w-full sm:w-auto"}
                disabled={create.isPending || !chosen}
                onClick={() =>
                  chosen &&
                  create.mutate(
                    { agentId: agent.id, managerId: chosen },
                    { onError: (e) => toastError(e, "Couldn't add the agent to Slack") },
                  )
                }
              >
                {create.isPending ? <Spinner /> : <PlusIcon />}
                Add to Slack
              </Button>
            </div>
            {active.length === 1 && !more ? (
              <p className="text-xs text-muted-foreground">In {active[0]?.team.name ?? "the linked Slack team"}.</p>
            ) : null}
            {more ? (
              <Button variant="ghost" size="sm" disabled={loadingMore} onClick={onLoadMore}>
                {loadingMore ? <Spinner /> : null}
                Show more teams
              </Button>
            ) : null}
          </EmptyContent>
        ) : !production && mayChange && !anyLinked ? (
          <EmptyContent>
            <Button asChild variant="outline">
              <Link to="/$org/w/$ws/channels" params={{ org: org.slug, ws: workspace.id }} search={{ link: true }}>
                Link a Slack team
              </Link>
            </Button>
          </EmptyContent>
        ) : null}
      </Empty>
    </Panel>
  );
}

function AppCard({
  org,
  workspace,
  agent,
  app,
  manager,
  connection,
  channel,
}: {
  org: Organization;
  workspace: Workspace;
  agent: Agent;
  app: ChannelApp;
  manager: ChannelAppManager | undefined;
  connection: ChannelConnection | undefined;
  channel: AgentChannel | undefined;
}) {
  const remove = useDeleteChannelApp(workspace.id);
  const put = usePutAgentChannel(workspace.id, agent.id);
  const stop = useDeleteAgentChannel(workspace.id, agent.id);
  const [dialog, setDialog] = useState<null | "install" | "remove" | "stop">(null);
  const mayChange = canChange(org);
  const production = workspace.environment === "production";
  const team = manager?.team.name ?? connection?.team.name ?? "Slack";
  const managerActive = manager?.status === "active";
  const needsInstall =
    app.status === "awaiting_install" || (app.status === "installed" && connection?.status === "reconnect_required");
  const installable = !production && mayChange && managerActive && needsInstall;

  return (
    <Panel className="overflow-hidden">
      <div className="flex items-start gap-3 px-4 py-3.5">
        <AppIcon icon={CHANNEL_PROVIDER_ICON[app.provider]} />
        <div className="min-w-0 flex-1">
          <p className="flex flex-wrap items-center gap-2">
            <span className="truncate font-medium">{team}</span>
            <ChannelAppStatusPill status={app.status} />
            {connection && connection.status !== "connected" ? (
              <ConnectionStatusPill status={connection.status} />
            ) : null}
          </p>
          <p className="truncate text-sm text-muted-foreground">
            {connection ? (
              <>
                <span className="font-medium text-foreground">@{connection.identity.handle}</span>
                {" · "}
                <Link
                  to="/$org/w/$ws/channels/$connectionId"
                  params={{ org: org.slug, ws: workspace.id, connectionId: connection.id }}
                  className="text-brand-ink underline-offset-4 hover:underline"
                >
                  Conversations
                </Link>
              </>
            ) : (
              <span className="font-mono text-xs">{app.external_app_id ?? "Its own Slack app"}</span>
            )}
          </p>
        </div>
        {installable && app.status === "installed" ? (
          <Button size="sm" onClick={() => setDialog("install")}>
            <RefreshCwIcon />
            Install again
          </Button>
        ) : null}
        {mayChange && app.status !== "deleting" ? (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="ghost" size="icon" aria-label={`Actions for ${team}`}>
                <EllipsisIcon />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              {connection && connection.status !== "disconnected" ? (
                channel ? (
                  <DropdownMenuItem onSelect={() => setDialog("stop")}>
                    <VolumeXIcon />
                    Stop answering here
                  </DropdownMenuItem>
                ) : (
                  <DropdownMenuItem
                    onSelect={() =>
                      put.mutate(
                        { connectionId: connection.id, ...agentChannelDefaults() },
                        {
                          onSuccess: () => toast.success(`${agent.name} answers in ${team} again`),
                          onError: (e) => toastError(e, "Couldn't turn answering on"),
                        },
                      )
                    }
                  >
                    <MessagesSquareIcon />
                    Answer here again
                  </DropdownMenuItem>
                )
              ) : null}
              <DropdownMenuSeparator />
              <DropdownMenuItem variant="destructive" onSelect={() => setDialog("remove")}>
                <Trash2Icon />
                Remove from Slack
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        ) : null}
      </div>

      <AppBody
        org={org}
        workspace={workspace}
        agent={agent}
        app={app}
        team={team}
        manager={manager}
        connection={connection}
        channel={channel}
        installable={installable}
        onInstall={() => setDialog("install")}
      />

      <InstallDialog
        workspace={workspace}
        agent={agent}
        app={app}
        team={team}
        open={dialog === "install"}
        onOpenChange={(open) => setDialog(open ? "install" : null)}
      />
      {connection ? (
        <ConfirmDialog
          open={dialog === "stop"}
          onOpenChange={(open) => setDialog(open ? "stop" : null)}
          title={`Stop ${agent.name} answering in ${team}?`}
          description={
            <p>
              Messages to @{connection.identity.handle} stop waking it, and its settings here are cleared. The app stays
              installed, so you can turn answering on again.
            </p>
          }
          actionLabel="Stop answering"
          pending={stop.isPending}
          onConfirm={() =>
            stop.mutate(connection.id, {
              onSuccess: () => {
                setDialog(null);
                toast.success(`${agent.name} stopped answering in ${team}`);
              },
              onError: (e) => toastError(e, "Couldn't stop it answering"),
            })
          }
        />
      ) : null}
      <ConfirmDialog
        open={dialog === "remove"}
        onOpenChange={(open) => setDialog(open ? "remove" : null)}
        title={`Remove ${agent.name} from ${team}?`}
        description={
          <>
            <p>
              Zupflow deletes {agent.name}'s app in Slack, so people there can't mention it or message it. Runs in
              progress can't send there again.
            </p>
            <p>
              Its conversations stay readable until your retention period removes them. Adding it again makes a new app.
            </p>
          </>
        }
        actionLabel="Remove from Slack"
        pending={remove.isPending}
        onConfirm={() =>
          remove.mutate(app.id, {
            onSuccess: () => {
              setDialog(null);
              toast.success(`Removed ${agent.name} from ${team}`);
            },
            onError: (e) => toastError(e, "Couldn't remove the app"),
          })
        }
      />
    </Panel>
  );
}

function AppBody({
  org,
  workspace,
  agent,
  app,
  team,
  manager,
  connection,
  channel,
  installable,
  onInstall,
}: {
  org: Organization;
  workspace: Workspace;
  agent: Agent;
  app: ChannelApp;
  team: string;
  manager: ChannelAppManager | undefined;
  connection: ChannelConnection | undefined;
  channel: AgentChannel | undefined;
  installable: boolean;
  onInstall: () => void;
}) {
  const production = workspace.environment === "production";
  const mayChange = canChange(org);
  const tokenNote =
    manager && manager.status !== "active"
      ? " The team's configuration token has expired, so nothing can change here until someone pastes a new one on the Channels page."
      : "";

  if (app.status === "creating" || app.status === "updating" || app.status === "deleting") {
    return (
      <div className="flex items-start gap-3 border-t px-4 py-4 text-sm">
        <Spinner className="mt-0.5 size-4 text-brand-ink" />
        <p className="text-muted-foreground">
          {app.status === "creating"
            ? `Creating ${agent.name}'s app in ${team}. Slack makes about one new app a minute per team, so this can take a moment.`
            : app.status === "updating"
              ? `Sending ${agent.name}'s new name and description to Slack.`
              : `Removing ${agent.name}'s app from Slack.`}
        </p>
      </div>
    );
  }
  if (app.status === "failed") {
    return (
      <p className="flex items-start gap-2 border-t bg-danger-soft/50 px-4 py-3 text-sm">
        <AlertTriangleIcon className="mt-0.5 size-4 shrink-0 text-destructive" />
        <span>{appFailure(app.last_error)}</span>
      </p>
    );
  }
  if (app.status === "awaiting_install" || (connection && connection.status === "reconnect_required")) {
    const reinstall = app.status !== "awaiting_install";
    return (
      <div className="flex flex-col gap-3 border-t px-4 py-4 sm:flex-row sm:items-center">
        <ol className="min-w-0 flex-1 space-y-2 text-sm">
          <li className="flex items-center gap-2 text-muted-foreground">
            <CheckCircle2Icon className="size-4 shrink-0 text-success" />
            App created in {team}
          </li>
          <li className="flex items-start gap-2">
            <span
              aria-hidden
              className="mt-0.5 grid size-4 shrink-0 place-items-center rounded-full border border-foreground/30 text-[10px] font-semibold"
            >
              2
            </span>
            <span className="min-w-0 space-y-0.5">
              <span className="block font-medium">
                {reinstall
                  ? "Install it again: Slack removed it or revoked its token"
                  : "Someone in the team allows it in Slack"}
              </span>
              <span className="block text-xs text-muted-foreground">
                {production
                  ? "Your product sends a team admin to hosted Connect with a channel_app session; they click Allow in Slack."
                  : !mayChange
                    ? "A developer, admin or owner installs it."
                    : `Opens Slack's Allow page for ${agent.name}'s app. Sign in as someone who may install apps in ${team}.${tokenNote}`}
              </span>
            </span>
          </li>
        </ol>
        {installable && !reinstall ? (
          <Button onClick={onInstall} className="shrink-0">
            Install in Slack
          </Button>
        ) : null}
      </div>
    );
  }
  if (!connection) {
    return <p className="border-t px-4 py-4 text-sm text-muted-foreground">Installed. Its connection is loading.</p>;
  }
  if (connection.status === "disconnected") {
    return (
      <p className="border-t px-4 py-4 text-sm text-muted-foreground">
        Disconnected: it doesn't hear or answer messages in {team}.
      </p>
    );
  }
  if (!channel) {
    return (
      <p className="border-t px-4 py-4 text-sm text-muted-foreground">
        Installed, but {agent.name} doesn't answer here: messages to @{connection.identity.handle} don't wake it.
        {mayChange ? " Turn answering on from the menu." : ""}
      </p>
    );
  }
  return (
    <div className="border-t">
      <AgentChannelSettings
        org={org}
        workspace={workspace}
        connection={connection}
        channel={channel}
        readOnly={!mayChange}
      />
    </div>
  );
}

/**
 * The install step: hosted Connect, in its own window, sends the person to
 * Slack's Allow page for this agent's app and back; the Console follows the
 * session until the API says it finished.
 */
function InstallDialog({
  workspace,
  agent,
  app,
  team,
  open,
  onOpenChange,
}: {
  workspace: Workspace;
  agent: Agent;
  app: ChannelApp;
  team: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const flow = useConnectFlow(workspace.id);
  const { reset } = flow;
  useEffect(() => {
    if (!open) reset();
  }, [open, reset]);
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Install {agent.name} in Slack</DialogTitle>
          <DialogDescription>
            Slack asks someone in {team} to allow {agent.name}'s app. Once they do, people can mention it and send it
            messages, and it answers with the settings on this tab.
          </DialogDescription>
        </DialogHeader>
        {flow.sessionId ? (
          <ConnectProgress flow={flow} onDone={() => onOpenChange(false)} />
        ) : (
          <>
            <ConnectError error={flow.error} />
            <DialogFooter>
              <Button variant="outline" onClick={() => onOpenChange(false)}>
                Cancel
              </Button>
              <Button
                disabled={flow.starting}
                onClick={() => flow.start({ target: { kind: "channel_app", app: app.id }, return_url: null })}
              >
                {flow.starting ? <Spinner /> : null}
                Continue to Slack
              </Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
