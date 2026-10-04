import { createFileRoute, useNavigate } from "@tanstack/react-router";
import type { AuditEvent } from "@zupflow/sdk";
import { BotIcon, KeyRoundIcon, LockIcon, ServerCogIcon, UserIcon } from "lucide-react";
import { z } from "zod";
import { EnvBadge } from "@/components/app/environment";
import { QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { Toolbar } from "@/components/app/toolbar";
import { Button } from "@/components/ui/button";
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister, useDocumentTitle, useOrg } from "@/features/context";
import { useAuditEvents } from "@/features/queries";
import { WorkspacePicker } from "@/features/workspaces/workspace-picker";
import { fullDateTime } from "@/lib/format";

export const Route = createFileRoute("/_authed/$org/audit-log")({
  staticData: { crumb: "Audit log" },
  validateSearch: z.object({ workspace: z.string().optional() }),
  component: AuditLogPage,
});

const ACTOR_ICON = {
  user: UserIcon,
  api_key: KeyRoundIcon,
  system: ServerCogIcon,
} as const;

/** `installation.reconnect_required` → "Installation reconnect required" */
function describe(action: string) {
  const [resource = "", verb = ""] = action.split(".");
  const noun = resource.replaceAll("_", " ");
  return `${noun.charAt(0).toUpperCase()}${noun.slice(1)} ${verb.replaceAll("_", " ")}`;
}

function AuditLogPage() {
  const org = useOrg();
  useDocumentTitle("Audit log", org.name);
  const navigate = useNavigate({ from: Route.fullPath });
  const { workspace } = Route.useSearch();
  const allowed = canAdminister(org);
  const events = useAuditEvents(org.id, workspace);
  const items = events.data?.pages.flatMap((p) => p.items) ?? [];

  if (!allowed) {
    return (
      <PageContainer>
        <PageHeader title="Audit log" />
        <Panel>
          <Empty>
            <EmptyHeader>
              <EmptyMedia variant="icon">
                <LockIcon />
              </EmptyMedia>
              <EmptyTitle>Owners and admins only</EmptyTitle>
              <EmptyDescription>Ask an organization owner if you need to review security events.</EmptyDescription>
            </EmptyHeader>
          </Empty>
        </Panel>
      </PageContainer>
    );
  }

  return (
    <PageContainer>
      <PageHeader
        title="Audit log"
        description="Security-relevant changes across the organization. Records name what changed, never credential values."
      />
      <Toolbar>
        <WorkspacePicker
          orgId={org.id}
          value={workspace}
          onChange={(id) => navigate({ search: (s) => ({ ...s, workspace: id }), replace: true })}
          allLabel="All workspaces and organization"
        />
      </Toolbar>
      <Panel>
        {events.isError ? (
          <QueryError error={events.error} onRetry={() => events.refetch()} what="the audit log" className="m-3" />
        ) : events.isPending ? (
          <RowsSkeleton rows={8} columns={[45, 20, 15]} />
        ) : items.length === 0 ? (
          <p className="p-10 text-center text-sm text-muted-foreground">No events for this filter.</p>
        ) : (
          <ul className="divide-y">
            {items.map((e) => (
              <AuditRow key={e.id} event={e} workspaceName={e.workspace?.name} />
            ))}
          </ul>
        )}
      </Panel>
      {events.hasNextPage ? (
        <div className="mt-4 flex justify-center">
          <Button variant="outline" onClick={() => events.fetchNextPage()} disabled={events.isFetchingNextPage}>
            {events.isFetchingNextPage ? <Spinner /> : null}
            Load older events
          </Button>
        </div>
      ) : null}
    </PageContainer>
  );
}

function AuditRow({ event, workspaceName }: { event: AuditEvent; workspaceName: string | undefined }) {
  const Icon = event.target.type === "agent" ? BotIcon : ACTOR_ICON[event.actor.type];
  return (
    <li className="flex gap-3 px-4 py-3">
      <span className="mt-0.5 grid size-7 shrink-0 place-items-center rounded-full bg-muted text-muted-foreground">
        <Icon className="size-3.5" />
      </span>
      <div className="min-w-0 flex-1">
        <p className="text-sm">
          <span className="font-medium">{describe(event.action)}</span>
          <span className="text-muted-foreground"> · </span>
          <span>{event.target.name}</span>
        </p>
        <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
          <span>{event.actor.name}</span>
          {event.actor.type !== "user" ? (
            <span className="rounded border px-1">{event.actor.type === "api_key" ? "API key" : "system"}</span>
          ) : null}
          {workspaceName ? <span>in {workspaceName}</span> : null}
          {event.environment ? <EnvBadge environment={event.environment} size="sm" /> : null}
          <code className="text-[11px]">{event.action}</code>
        </p>
      </div>
      <span className="shrink-0 text-xs text-muted-foreground" title={fullDateTime(event.created_at)}>
        <RelativeTime iso={event.created_at} />
      </span>
    </li>
  );
}
