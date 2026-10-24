import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { CHANNEL_PROVIDER_NAMES } from "@zupflow/sdk";
import { z } from "zod";
import { AppIcon, MonoValue, QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, Panel, Properties, Property, SectionHeader } from "@/components/app/page";
import { Skeleton } from "@/components/ui/skeleton";
import { ConversationBrowser } from "@/features/channels/conversations";
import { CHANNEL_PROVIDER_ICON } from "@/features/channels/labels";
import { ConnectionStatusPill } from "@/features/channels/status";
import { useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { useAgents, useChannelConnection } from "@/features/queries";
import { useDetailCrumb } from "@/lib/crumbs";
import { idParam, idSearch } from "@/lib/route-ids";

export const Route = createFileRoute("/_authed/$org/w/$ws/channels/$connectionId")({
  params: idParam("connectionId", "channel_connection"),
  validateSearch: z.object({
    conversation: idSearch("channel_conversation").optional(),
    // A thread key looks like a number: a hand-edited link can lose its quotes, and opens the conversation instead.
    thread: z.string().optional().catch(undefined),
  }),
  component: ConnectionPage,
});

function ConnectionPage() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  const { connectionId } = Route.useParams();
  const { conversation, thread } = Route.useSearch();
  const navigate = useNavigate({ from: Route.fullPath });
  const connection = useChannelConnection(ws.id, connectionId);
  const agents = useAgents(ws.id);
  const c = connection.data;
  const agent = c?.agent_id ? agents.data?.find((a) => a.id === c.agent_id) : undefined;
  const title = c ? `@${c.identity.handle}` : undefined;
  useDetailCrumb(title);
  useDocumentTitle(title, "Channels", ws.name);

  if (connection.isError) {
    return (
      <PageContainer>
        <QueryError error={connection.error} onRetry={() => connection.refetch()} what="this connection" />
      </PageContainer>
    );
  }

  return (
    <PageContainer>
      <header className="mb-6 flex items-center gap-3">
        {c ? (
          <AppIcon icon={CHANNEL_PROVIDER_ICON[c.provider]} size="lg" />
        ) : (
          <Skeleton className="size-12 rounded-lg" />
        )}
        <div className="min-w-0 flex-1 space-y-1.5">
          {c ? (
            <>
              <div className="flex flex-wrap items-center gap-2">
                <h2 className="truncate text-xl leading-tight font-semibold tracking-[-0.02em]">{title}</h2>
                <ConnectionStatusPill status={c.status} />
              </div>
              <p className="truncate text-sm text-muted-foreground">
                {CHANNEL_PROVIDER_NAMES[c.provider]} · {c.team.name ?? c.team.id}
                {c.agent_id ? (
                  <>
                    {" · "}
                    <Link
                      to="/$org/w/$ws/agents/$agentId"
                      params={{ org: org.slug, ws: ws.id, agentId: c.agent_id }}
                      search={{ tab: "channels" }}
                      className="font-medium text-foreground hover:underline"
                    >
                      {agent?.name ?? c.label}
                    </Link>
                  </>
                ) : null}
              </p>
            </>
          ) : (
            <>
              <Skeleton className="h-7 w-44" />
              <Skeleton className="h-4 w-64" />
            </>
          )}
        </div>
      </header>

      {c?.status === "reconnect_required" ? (
        <p className="mb-6 rounded-xl border border-destructive/30 bg-danger-soft/60 p-3 text-sm">
          Slack uninstalled the app or revoked its token, so the agent doesn't hear or answer messages here. Install it
          again from the agent's Channels tab.
        </p>
      ) : c?.status === "disconnected" ? (
        <p className="mb-6 rounded-xl border bg-muted/50 p-3 text-sm text-muted-foreground">
          Disconnected. Its conversations stay readable until your organization's retention period removes them.
        </p>
      ) : null}

      <SectionHeader
        title="Conversations"
        description="Where people talked to the agent, most recent first. The agent's own messages link to the run that sent them."
      />
      {c ? (
        <ConversationBrowser
          org={org}
          workspace={ws}
          connection={c}
          conversationId={conversation}
          thread={thread}
          onOpen={(id, nextThread) =>
            navigate({
              search: { conversation: id ?? undefined, thread: nextThread || undefined },
              replace: true,
            })
          }
        />
      ) : (
        <Panel>
          <RowsSkeleton rows={4} columns={[50, 20]} />
        </Panel>
      )}

      <section className="mt-8">
        <SectionHeader title="Connection" />
        <Panel>
          {c ? (
            <Properties>
              <Property label="Identity">
                <span className="font-medium">@{c.identity.handle}</span>{" "}
                <span className="font-mono text-xs text-muted-foreground">{c.identity.external_id}</span>
              </Property>
              <Property label="Team">
                {c.team.name ?? "Unnamed"} <span className="font-mono text-xs text-muted-foreground">{c.team.id}</span>
              </Property>
              <Property label="Short ID">
                <MonoValue value={c.short_id} />
                <p className="text-xs text-muted-foreground">Names this connection's tools to the agent.</p>
              </Property>
              <Property label="Scopes granted">
                <span className="flex flex-wrap gap-1.5">
                  {c.granted_scopes.map((s) => (
                    <code key={s} className="rounded border bg-muted/40 px-1.5 py-0.5 text-[11.5px]">
                      {s}
                    </code>
                  ))}
                </span>
              </Property>
              <Property label="Connected">
                <RelativeTime iso={c.connected_at} />
              </Property>
              <Property label="Last event">
                <RelativeTime iso={c.last_event_at} fallback="None yet" />
              </Property>
              <Property label="Connection ID">
                <MonoValue value={c.id} />
              </Property>
            </Properties>
          ) : (
            <RowsSkeleton rows={4} columns={[30, 40]} />
          )}
        </Panel>
      </section>
    </PageContainer>
  );
}
