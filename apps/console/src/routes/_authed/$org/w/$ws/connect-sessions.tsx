import { createFileRoute, Link } from "@tanstack/react-router";
import { Link2Icon } from "lucide-react";
import { AppIcon, QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel } from "@/components/app/page";
import { ConnectSessionStatusPill } from "@/components/app/status";
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { useConnectSessions } from "@/features/queries";
import { shortId } from "@/lib/format";

export const Route = createFileRoute("/_authed/$org/w/$ws/connect-sessions")({
  staticData: { crumb: "Connect sessions" },
  component: ConnectSessionsPage,
});

function ConnectSessionsPage() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  useDocumentTitle("Connect sessions", ws.name);
  const sessions = useConnectSessions(ws.id);

  return (
    <PageContainer>
      <PageHeader
        title="Connect sessions"
        description="Every attempt to connect or reconnect an account here. Sessions are single-use and expire after 15 minutes."
      />
      {sessions.isError ? (
        <QueryError error={sessions.error} onRetry={() => sessions.refetch()} what="Connect sessions" />
      ) : sessions.data?.length === 0 ? (
        <Panel>
          <Empty>
            <EmptyHeader>
              <EmptyMedia variant="icon">
                <Link2Icon />
              </EmptyMedia>
              <EmptyTitle>No Connect sessions yet</EmptyTitle>
              <EmptyDescription>
                They appear when your backend calls{" "}
                <code className="text-xs">POST /v1/workspaces/{"{ws}"}/connect-sessions</code> or someone adds an
                installation here.
              </EmptyDescription>
            </EmptyHeader>
          </Empty>
        </Panel>
      ) : (
        <Panel className="overflow-hidden">
          {sessions.isPending ? (
            <RowsSkeleton rows={5} />
          ) : (
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead className="pl-4">App</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead className="hidden md:table-cell">Intent</TableHead>
                  <TableHead className="hidden lg:table-cell">Authorized by</TableHead>
                  <TableHead className="hidden pr-4 sm:table-cell">Started</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {sessions.data.map((s) => (
                  <TableRow key={s.id} className="hover:bg-transparent">
                    <TableCell className="w-full max-w-0 py-3 pl-4">
                      <div className="flex items-center gap-3">
                        <AppIcon icon={s.app.icon} size="sm" />
                        <div className="min-w-0">
                          <p className="truncate font-medium">
                            {s.status === "completed" && s.installation_id ? (
                              <Link
                                to="/$org/w/$ws/installations/$installationId"
                                params={{ org: org.slug, ws: ws.id, installationId: s.installation_id }}
                                className="hover:underline"
                              >
                                {s.app.name} · {s.label}
                              </Link>
                            ) : (
                              `${s.app.name} · ${s.label}`
                            )}
                          </p>
                          <p className="font-mono text-xs text-muted-foreground">{shortId(s.id)}</p>
                        </div>
                      </div>
                    </TableCell>
                    <TableCell>
                      <div className="flex flex-col items-start gap-1">
                        <ConnectSessionStatusPill status={s.status} />
                        {s.failure_code ? (
                          <code className="text-[11px] text-muted-foreground">{s.failure_code}</code>
                        ) : null}
                      </div>
                    </TableCell>
                    <TableCell className="hidden text-muted-foreground capitalize md:table-cell">{s.intent}</TableCell>
                    <TableCell className="hidden font-mono text-xs text-muted-foreground lg:table-cell">
                      {s.subject}
                    </TableCell>
                    <TableCell className="hidden pr-4 text-muted-foreground sm:table-cell">
                      <RelativeTime iso={s.created_at} />
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </Panel>
      )}
    </PageContainer>
  );
}
