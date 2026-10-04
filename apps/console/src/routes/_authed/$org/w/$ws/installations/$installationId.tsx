import { useQueries } from "@tanstack/react-query";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import type { Installation, Organization, Workspace } from "@zupflow/sdk";
import { EllipsisIcon, PencilIcon, RefreshCwIcon, Trash2Icon, UnplugIcon } from "lucide-react";
import { useEffect, useState } from "react";
import { toast } from "sonner";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { EnvBadge } from "@/components/app/environment";
import { AppIcon, MonoValue, QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, Panel, Properties, Property, SectionHeader } from "@/components/app/page";
import { InstallationStatusPill } from "@/components/app/status";
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
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister, useCurrentWorkspace, useDocumentTitle, useOrg } from "@/features/context";
import { ConnectError, ConnectProgress, useConnectFlow } from "@/features/installations/connect-flow";
import {
  useAgents,
  useDeleteInstallation,
  useDisconnectInstallation,
  useInstallation,
  useRenameInstallation,
} from "@/features/queries";
import { api } from "@/lib/api";
import { useDetailCrumb } from "@/lib/crumbs";
import { toastError } from "@/lib/errors";
import { fullDateTime } from "@/lib/format";
import { qk } from "@/lib/query-keys";
import { idParam } from "@/lib/route-ids";

export const Route = createFileRoute("/_authed/$org/w/$ws/installations/$installationId")({
  params: idParam("installationId", "installation"),
  component: InstallationPage,
});

function InstallationPage() {
  const org = useOrg();
  const ws = useCurrentWorkspace();
  const { installationId } = Route.useParams();
  const navigate = useNavigate();
  const installation = useInstallation(ws.id, installationId);
  const disconnect = useDisconnectInstallation(ws.id);
  const remove = useDeleteInstallation(ws.id);
  const [dialog, setDialog] = useState<null | "reconnect" | "rename" | "disconnect" | "delete">(null);
  useDetailCrumb(installation.data?.label);
  useDocumentTitle(installation.data?.label, "Installations", ws.name);

  if (installation.isError) {
    return (
      <PageContainer>
        <QueryError error={installation.error} onRetry={() => installation.refetch()} what="this installation" />
      </PageContainer>
    );
  }
  const i = installation.data;
  const needsReconnect = i && i.status !== "connected";

  return (
    <PageContainer>
      <header className="mb-6 flex flex-col gap-4 sm:flex-row sm:items-center">
        {i ? <AppIcon icon={i.app.icon} size="lg" /> : <Skeleton className="size-12 rounded-lg" />}
        <div className="min-w-0 flex-1 space-y-1.5">
          {i ? (
            <>
              <div className="flex flex-wrap items-center gap-2">
                <h2 className="text-xl leading-tight font-semibold tracking-[-0.02em]">{i.label}</h2>
                <InstallationStatusPill status={i.status} />
              </div>
              <p className="text-sm text-muted-foreground">
                {i.app.name} · {i.app.owner_scope === "workspace" ? "Workspace app" : "Organization app"} ·{" "}
                {i.app.auth_type === "oauth2" ? "OAuth" : "API token"}
              </p>
            </>
          ) : (
            <>
              <Skeleton className="h-8 w-48" />
              <Skeleton className="h-4 w-64" />
            </>
          )}
        </div>
        {i ? (
          <div className="flex items-center gap-2">
            {ws.environment === "sandbox" ? (
              <Button variant={needsReconnect ? "default" : "outline"} onClick={() => setDialog("reconnect")}>
                <RefreshCwIcon />
                Reconnect
              </Button>
            ) : null}
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button variant="outline" size="icon" aria-label="More actions">
                  <EllipsisIcon />
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end">
                <DropdownMenuItem onSelect={() => setDialog("rename")}>
                  <PencilIcon />
                  Rename
                </DropdownMenuItem>
                {i.status !== "disconnected" ? (
                  <DropdownMenuItem onSelect={() => setDialog("disconnect")}>
                    <UnplugIcon />
                    Disconnect
                  </DropdownMenuItem>
                ) : null}
                {canAdminister(org) ? (
                  <>
                    <DropdownMenuSeparator />
                    <DropdownMenuItem variant="destructive" onSelect={() => setDialog("delete")}>
                      <Trash2Icon />
                      Delete installation
                    </DropdownMenuItem>
                  </>
                ) : null}
              </DropdownMenuContent>
            </DropdownMenu>
          </div>
        ) : null}
      </header>

      {i?.status === "reconnect_required" ? (
        <p className="mb-6 rounded-xl border border-destructive/30 bg-danger-soft/60 p-3 text-sm">
          The provider stopped accepting this connection's credential. Agents' calls through it fail until the same
          account is reconnected
          {ws.environment === "production"
            ? ": your product starts a Connect session with intent reconnect and the customer signs in again."
            : "."}{" "}
          Grants are kept.
        </p>
      ) : i?.status === "disconnected" ? (
        <p className="mb-6 rounded-xl border bg-muted/50 p-3 text-sm text-muted-foreground">
          Disconnected. Local use stopped immediately; grants are kept and resume working if you reconnect the same
          account.
        </p>
      ) : null}

      <div className="grid gap-8 lg:grid-cols-[minmax(0,1fr)_20rem]">
        <div className="min-w-0 space-y-8">
          <section>
            <SectionHeader
              title="Connection"
              description="Credentials are encrypted for this installation and never shown here or to agents."
            />
            <Panel>
              {i ? (
                i.connection ? (
                  <Properties>
                    <Property label="Account">
                      <span className="font-medium">{i.connection.account.display_name}</span>
                      {i.connection.account.email ? (
                        <span className="text-muted-foreground"> · {i.connection.account.email}</span>
                      ) : null}
                    </Property>
                    <Property label="Provider account ID">
                      <MonoValue value={i.connection.account.external_id} />
                    </Property>
                    <Property label="Scopes granted">
                      {i.connection.granted_scopes.length > 0 ? (
                        <ul className="space-y-1">
                          {i.connection.granted_scopes.map((s) => (
                            <li key={s}>
                              <code className="text-[12px] break-all">{s}</code>
                            </li>
                          ))}
                        </ul>
                      ) : (
                        <span className="text-muted-foreground">Not applicable</span>
                      )}
                    </Property>
                    <Property label="Credential">
                      Version {i.connection.credential_version}
                      {i.connection.expires_at ? (
                        <span className="text-muted-foreground">
                          {" "}
                          · access token {Date.parse(i.connection.expires_at) > Date.now() ? "refreshes" : "refreshed"}{" "}
                          <RelativeTime iso={i.connection.expires_at} />
                        </span>
                      ) : null}
                    </Property>
                    <Property label="Connected">
                      <span title={fullDateTime(i.connection.connected_at)}>
                        <RelativeTime iso={i.connection.connected_at} />
                      </span>
                    </Property>
                  </Properties>
                ) : (
                  <p className="p-6 text-center text-sm text-muted-foreground">No account is connected.</p>
                )
              ) : (
                <RowsSkeleton rows={4} columns={[30, 40]} />
              )}
            </Panel>
          </section>
          {i ? <AgentsWithAccess org={org} workspace={ws} installation={i} /> : null}
        </div>
        <aside className="space-y-6">
          <section>
            <SectionHeader title="Details" />
            <Panel>
              {i ? (
                <Properties>
                  <Property label="Installation ID" className="sm:grid-cols-1 sm:gap-1">
                    <MonoValue value={i.id} />
                  </Property>
                  <Property label="Workspace" className="sm:grid-cols-1 sm:gap-1">
                    <span className="flex items-center gap-2">
                      {ws.name} <EnvBadge environment={ws.environment} size="sm" />
                    </span>
                  </Property>
                  <Property label="Configuration" className="sm:grid-cols-1 sm:gap-1">
                    v{i.configuration_version}
                  </Property>
                  <Property label="Created" className="sm:grid-cols-1 sm:gap-1">
                    <RelativeTime iso={i.created_at} />
                  </Property>
                </Properties>
              ) : (
                <RowsSkeleton rows={3} columns={[60]} />
              )}
            </Panel>
          </section>
        </aside>
      </div>

      {i ? (
        <>
          <ReconnectDialog
            workspace={ws}
            installation={i}
            open={dialog === "reconnect"}
            onOpenChange={(o) => setDialog(o ? "reconnect" : null)}
          />
          <RenameDialog
            workspace={ws}
            installation={i}
            open={dialog === "rename"}
            onOpenChange={(o) => setDialog(o ? "rename" : null)}
          />
          <ConfirmDialog
            open={dialog === "disconnect"}
            onOpenChange={(o) => setDialog(o ? "disconnect" : null)}
            title={`Disconnect ${i.label}?`}
            description={
              <>
                <p>Agents stop using this account immediately. Zupflow also asks {i.app.name} to revoke the token.</p>
                <p>Grants are kept, so reconnecting the same account restores access.</p>
              </>
            }
            actionLabel="Disconnect"
            pending={disconnect.isPending}
            onConfirm={() =>
              disconnect.mutate(i.id, {
                onSuccess: () => {
                  setDialog(null);
                  toast.success(`Disconnected ${i.label}`);
                },
                onError: (e) => toastError(e, "Couldn't disconnect"),
              })
            }
          />
          <ConfirmDialog
            open={dialog === "delete"}
            onOpenChange={(o) => setDialog(o ? "delete" : null)}
            title={`Delete ${i.label}?`}
            description={
              <p>
                The stored credential is destroyed and{" "}
                {i.grant_count === 0
                  ? "no agents lose"
                  : `${i.grant_count} ${i.grant_count === 1 ? "agent loses" : "agents lose"}`}{" "}
                access. The audit record is kept.
              </p>
            }
            confirmText={i.label}
            actionLabel="Delete installation"
            pending={remove.isPending}
            onConfirm={() =>
              remove.mutate(i.id, {
                onSuccess: () => {
                  toast.success(`Deleted ${i.label}`);
                  void navigate({ to: "/$org/w/$ws/installations", params: { org: org.slug, ws: ws.id } });
                },
                onError: (e) => toastError(e, "Couldn't delete"),
              })
            }
          />
        </>
      ) : null}
    </PageContainer>
  );
}

function AgentsWithAccess({
  org,
  workspace,
  installation,
}: {
  org: Organization;
  workspace: Workspace;
  installation: Installation;
}) {
  const agents = useAgents(workspace.id);
  const grants = useQueries({
    queries: (agents.data ?? []).map((a) => ({
      queryKey: qk.grants(workspace.id, a.id),
      queryFn: () => api.grants.list(workspace.id, a.id),
    })),
  });
  const loading = agents.isPending || grants.some((g) => g.isPending);
  const rows = (agents.data ?? []).flatMap((agent, idx) => {
    const grant = grants[idx]?.data?.find((g) => g.installation_id === installation.id);
    return grant ? [{ agent, grant }] : [];
  });

  return (
    <section>
      <SectionHeader title="Agents with access" description="Change access from each agent's Access tab." />
      <Panel>
        {loading ? (
          <RowsSkeleton rows={2} columns={[40, 30]} />
        ) : rows.length === 0 ? (
          <p className="p-6 text-center text-sm text-muted-foreground">No agent can use this installation yet.</p>
        ) : (
          <ul className="divide-y">
            {rows.map(({ agent, grant }) => (
              <li key={agent.id}>
                <Link
                  to="/$org/w/$ws/agents/$agentId"
                  params={{ org: org.slug, ws: workspace.id, agentId: agent.id }}
                  search={{ tab: "access" }}
                  className="flex flex-col gap-1 px-4 py-3 hover:bg-accent/50 sm:flex-row sm:items-center sm:gap-4"
                >
                  <span className="font-medium sm:w-48 sm:shrink-0">{agent.name}</span>
                  <span className="flex min-w-0 flex-wrap gap-1.5">
                    {grant.tools.map((t) => (
                      <code
                        key={t}
                        className="rounded border bg-muted/40 px-1.5 py-0.5 text-[11.5px]"
                        title={grant.approval_required.includes(t) ? "Requires approval" : undefined}
                      >
                        {t}
                        {grant.approval_required.includes(t) ? " ✋" : ""}
                      </code>
                    ))}
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

function ReconnectDialog({
  workspace,
  installation,
  open,
  onOpenChange,
}: {
  workspace: Workspace;
  installation: Installation;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const flow = useConnectFlow(workspace.id);
  // biome-ignore lint/correctness/useExhaustiveDependencies: reset only when the dialog closes.
  useEffect(() => {
    if (!open) flow.reset();
  }, [open]);
  const account = installation.connection?.account;
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Reconnect {installation.label}</DialogTitle>
          <DialogDescription>
            {account
              ? `Sign in again as ${account.email ?? account.display_name}. A different account is refused, so existing grants keep their meaning.`
              : "Authorize an account for this installation."}
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
                onClick={() =>
                  flow.start({
                    app_id: installation.app.id,
                    intent: "reconnect",
                    label: installation.label,
                    installation_id: installation.id,
                    return_url: null,
                  })
                }
              >
                {flow.starting ? <Spinner /> : null}
                Continue to Connect
              </Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

function RenameDialog({
  workspace,
  installation,
  open,
  onOpenChange,
}: {
  workspace: Workspace;
  installation: Installation;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const rename = useRenameInstallation(workspace.id, installation.id);
  const [label, setLabel] = useState(installation.label);
  useEffect(() => {
    if (open) setLabel(installation.label);
  }, [open, installation.label]);
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>Rename installation</DialogTitle>
        </DialogHeader>
        <form
          id="rename-installation"
          onSubmit={(e) => {
            e.preventDefault();
            rename.mutate(label.trim(), {
              onSuccess: () => {
                onOpenChange(false);
                toast.success("Renamed");
              },
              onError: (err) => toastError(err, "Couldn't rename"),
            });
          }}
          className="space-y-2"
        >
          <Label htmlFor="rename-label">Label</Label>
          <Input id="rename-label" value={label} maxLength={40} onChange={(e) => setLabel(e.target.value)} autoFocus />
        </form>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" form="rename-installation" disabled={!label.trim() || rename.isPending}>
            {rename.isPending ? <Spinner /> : null}
            Save
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
