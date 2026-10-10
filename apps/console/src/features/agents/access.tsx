import { Link } from "@tanstack/react-router";
import type { Agent, AgentGrant, Installation, InstallationTool, Organization, Workspace } from "@zupflow/sdk";
import { AlertTriangleIcon, PlugIcon, ShieldIcon } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { toast } from "sonner";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { AppIcon, QueryError, RowsSkeleton } from "@/components/app/misc";
import { Panel } from "@/components/app/page";
import { InstallationStatusPill, StatusPill, ToolEffectTag } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { canChange } from "@/features/context";
import {
  useCreateGrant,
  useDeleteGrant,
  useGrants,
  useInstallations,
  useInstallationTools,
  useUpdateGrant,
} from "@/features/queries";
import { toastError } from "@/lib/errors";
import { cn } from "@/lib/utils";

/** "a", "a and b", "a, b and c". */
function listed(names: string[]): string {
  return names.length <= 1 ? (names[0] ?? "") : `${names.slice(0, -1).join(", ")} and ${names.at(-1)}`;
}

export function AgentAccess({ org, workspace, agent }: { org: Organization; workspace: Workspace; agent: Agent }) {
  const installations = useInstallations(workspace.id);
  const grants = useGrants(workspace.id, agent.id);

  if (installations.isError || grants.isError) {
    return (
      <QueryError
        error={installations.error ?? grants.error}
        onRetry={() => {
          void installations.refetch();
          void grants.refetch();
        }}
        what="access"
      />
    );
  }
  if (!installations.data || !grants.data)
    return (
      <Panel>
        <RowsSkeleton rows={3} columns={[45, 20]} />
      </Panel>
    );

  if (installations.data.length === 0) {
    return (
      <Panel>
        <Empty>
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <PlugIcon />
            </EmptyMedia>
            <EmptyTitle>Nothing to grant yet</EmptyTitle>
            <EmptyDescription>
              {canChange(org)
                ? "This workspace has no installations. Connect an account first, then come back to grant tools."
                : "This workspace has no installations, so no agent has access to any tools yet."}
            </EmptyDescription>
          </EmptyHeader>
          <EmptyContent>
            <Button asChild>
              <Link to="/$org/w/$ws/installations" params={{ org: org.slug, ws: workspace.id }}>
                Go to installations
              </Link>
            </Button>
          </EmptyContent>
        </Empty>
      </Panel>
    );
  }

  const sorted = [...installations.data].sort((a, b) => {
    const ga = grants.data.some((g) => g.installation_id === a.id) ? 0 : 1;
    const gb = grants.data.some((g) => g.installation_id === b.id) ? 0 : 1;
    return ga - gb || a.app.name.localeCompare(b.app.name) || a.label.localeCompare(b.label);
  });

  return (
    <div className="space-y-4">
      <p className="flex items-start gap-2 text-sm text-muted-foreground">
        <ShieldIcon className="mt-0.5 size-4 shrink-0" />
        <span>
          Access is granted per installation. Granting one Google Workspace installation never grants another, and an
          agent never sees credentials — only tool results.
          {canChange(org) ? null : " You can view access; owners, admins and developers can change it."}
        </span>
      </p>
      {sorted.map((installation) => (
        <GrantCard
          key={installation.id}
          org={org}
          workspace={workspace}
          agent={agent}
          installation={installation}
          grant={grants.data.find((g) => g.installation_id === installation.id)}
        />
      ))}
    </div>
  );
}

function GrantCard({
  org,
  workspace,
  agent,
  installation,
  grant,
}: {
  org: Organization;
  workspace: Workspace;
  agent: Agent;
  installation: Installation;
  grant: AgentGrant | undefined;
}) {
  // The installation's own registry, a page at a time: what its app listed to this account.
  const registry = useInstallationTools(workspace.id, installation.id);
  const items = registry.data?.pages.flatMap((page) => page.items) ?? [];
  // A gone tool is never offered. One the grant still holds is refused when called, and saving drops it.
  const offered = items.filter((t) => t.status !== "gone");
  const gone = new Set(items.filter((t) => t.status === "gone").map((t) => t.name));
  const [editing, setEditing] = useState(false);
  const [revoking, setRevoking] = useState(false);
  const create = useCreateGrant(workspace.id, agent.id);
  const update = useUpdateGrant(workspace.id, agent.id);
  const remove = useDeleteGrant(workspace.id, agent.id);

  const initial = useMemo(() => ({ tools: grant?.tools ?? [], approval: grant?.approval_required ?? [] }), [grant]);
  const [selected, setSelected] = useState<string[]>(initial.tools);
  const [approval, setApproval] = useState<string[]>(initial.approval);
  useEffect(() => {
    setSelected(initial.tools);
    setApproval(initial.approval);
  }, [initial]);

  const goneGranted = selected.filter((t) => gone.has(t));
  const titleOf = (name: string) => items.find((t) => t.name === name)?.pinned.title ?? name;
  const granted = grant ? grant.tools.filter((t) => !gone.has(t)).length : 0;
  const toSave = selected.filter((t) => !gone.has(t));
  const open = Boolean(grant) || editing;
  const dirty =
    goneGranted.length > 0 ||
    selected.length !== initial.tools.length ||
    selected.some((t) => !initial.tools.includes(t)) ||
    approval.length !== initial.approval.length ||
    approval.some((t) => !initial.approval.includes(t));

  const toggleTool = (tool: InstallationTool, on: boolean) => {
    setSelected((s) => (on ? [...s, tool.name] : s.filter((t) => t !== tool.name)));
    if (on && tool.effect === "destructive") setApproval((a) => [...new Set([...a, tool.name])]);
    if (!on) setApproval((a) => a.filter((t) => t !== tool.name));
  };

  const save = () => {
    const done = {
      onSuccess: () => {
        toast.success(grant ? "Access updated" : `Granted access to ${installation.label}`);
        setEditing(false);
      },
      onError: (e: unknown) => toastError(e, "Couldn't save access"),
    };
    const input = { tools: toSave, approval_required: approval.filter((t) => toSave.includes(t)) };
    if (grant) update.mutate({ grantId: grant.id, ...input }, done);
    else create.mutate({ installation_id: installation.id, ...input }, done);
  };

  const pending = create.isPending || update.isPending;
  const mayChange = canChange(org);

  return (
    <Panel className={cn(!open && "bg-card/60")}>
      <div className="flex flex-wrap items-center gap-3 px-4 py-3">
        <AppIcon icon={installation.app.icon} />
        <div className="min-w-0 flex-1">
          <p className="flex flex-wrap items-center gap-2 font-medium">
            {installation.label}
            {installation.status !== "connected" ? <InstallationStatusPill status={installation.status} /> : null}
          </p>
          <p className="text-sm text-muted-foreground">
            {installation.app.name}
            {installation.connection
              ? ` · ${installation.connection.account.email ?? installation.connection.account.display_name}`
              : ""}
          </p>
        </div>
        {grant ? (
          <>
            <span className="tabular text-sm text-muted-foreground">
              {registry.data && !registry.hasNextPage
                ? `${granted} of ${offered.length} tools`
                : `${granted} ${granted === 1 ? "tool" : "tools"}`}
            </span>
            {mayChange ? (
              <Button
                variant="ghost"
                size="sm"
                className="text-destructive hover:text-destructive"
                onClick={() => setRevoking(true)}
              >
                Revoke
              </Button>
            ) : null}
          </>
        ) : !editing && mayChange ? (
          <Button variant="outline" size="sm" onClick={() => setEditing(true)}>
            Grant access
          </Button>
        ) : null}
      </div>

      {open ? (
        <div className="border-t">
          {installation.status !== "connected" ? (
            <p className="flex items-center gap-2 border-b bg-danger-soft/50 px-4 py-2 text-sm">
              <AlertTriangleIcon className="size-4 text-destructive" />
              Calls through this installation fail until it is reconnected.
            </p>
          ) : null}
          {goneGranted.length > 0 ? (
            <p className="flex items-start gap-2 border-b bg-warning-soft/40 px-4 py-2 text-sm">
              <AlertTriangleIcon className="mt-0.5 size-4 shrink-0 text-warning" />
              <span className="min-w-0 break-words">
                {installation.app.name} no longer offers {listed(goneGranted.map(titleOf))}. Calls to{" "}
                {goneGranted.length === 1 ? "it" : "them"} are refused
                {!mayChange
                  ? "."
                  : toSave.length > 0
                    ? `, and saving removes ${goneGranted.length === 1 ? "it" : "them"} from this grant.`
                    : ". Nothing else is granted here, so revoke access instead."}
              </span>
            </p>
          ) : null}
          {registry.isError ? (
            <QueryError
              error={registry.error}
              onRetry={() => registry.refetch()}
              what="this installation's tools"
              className="m-3"
            />
          ) : registry.isPending ? (
            <div className="space-y-3 p-4">
              <Skeleton className="h-4 w-2/3" />
              <Skeleton className="h-4 w-1/2" />
            </div>
          ) : offered.length === 0 && !registry.hasNextPage ? (
            <div className="p-6 text-center text-sm">
              <p className="font-medium">No tools to grant yet</p>
              <p className="mt-1 text-muted-foreground">
                {installation.status === "connected"
                  ? `Tools appear here once ${installation.app.name} lists them to this account.`
                  : "Tools are listed once an account is connected."}
              </p>
            </div>
          ) : (
            <ul className="divide-y">
              {offered.map((tool) => {
                const checked = selected.includes(tool.name);
                const id = `${installation.id}-${tool.name}`;
                const title = tool.pinned.title ?? tool.name;
                return (
                  <li key={tool.name} className="flex items-center gap-3 px-4 py-2.5">
                    <Checkbox
                      id={id}
                      checked={checked}
                      disabled={!mayChange}
                      onCheckedChange={(v) => toggleTool(tool, v === true)}
                    />
                    <Label
                      htmlFor={id}
                      className={cn(
                        "min-w-0 flex-1 flex-col items-start gap-0.5 font-normal",
                        mayChange && "cursor-pointer",
                      )}
                    >
                      <span className="flex min-w-0 flex-wrap items-center gap-2">
                        <span className="break-words font-medium">{title}</span>
                        <ToolEffectTag effect={tool.effect} />
                        {tool.status === "changed" ? <StatusPill tone="warning">Changed</StatusPill> : null}
                      </span>
                      {tool.pinned.description ? (
                        <span className="text-xs break-words text-muted-foreground">{tool.pinned.description}</span>
                      ) : null}
                      {tool.status === "changed" ? (
                        <span className="text-xs text-muted-foreground">
                          Its definition changed. Calls use it as first listed until the change is reviewed.
                        </span>
                      ) : null}
                    </Label>
                    <div className={cn("flex shrink-0 items-center gap-2", !checked && "opacity-40")}>
                      <Label
                        htmlFor={`${id}-approval`}
                        className="hidden text-xs font-normal text-muted-foreground sm:block"
                      >
                        Ask first
                      </Label>
                      <Switch
                        id={`${id}-approval`}
                        aria-label={`Require approval for ${title}`}
                        disabled={!checked || !mayChange}
                        checked={approval.includes(tool.name)}
                        onCheckedChange={(on) =>
                          setApproval((a) => (on ? [...a, tool.name] : a.filter((t) => t !== tool.name)))
                        }
                      />
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
          {registry.hasNextPage ? (
            <div className="flex justify-center border-t px-4 py-3">
              <Button
                variant="outline"
                size="sm"
                onClick={() => registry.fetchNextPage()}
                disabled={registry.isFetchingNextPage}
              >
                {registry.isFetchingNextPage ? <Spinner /> : null}
                Show more tools
              </Button>
            </div>
          ) : null}
          {mayChange && (dirty || (editing && !grant)) ? (
            <div className="flex items-center justify-end gap-2 border-t px-4 py-3">
              <Button
                variant="ghost"
                size="sm"
                onClick={() => {
                  setSelected(initial.tools);
                  setApproval(initial.approval);
                  setEditing(false);
                }}
              >
                Cancel
              </Button>
              <Button size="sm" disabled={pending || toSave.length === 0} onClick={save}>
                {pending ? <Spinner /> : null}
                {grant ? "Save access" : "Grant access"}
              </Button>
            </div>
          ) : null}
        </div>
      ) : null}

      <ConfirmDialog
        open={revoking}
        onOpenChange={setRevoking}
        title={`Revoke access to ${installation.label}?`}
        description={
          <p>
            {agent.name} immediately loses every tool on this installation. Runs in progress can't call it again. The
            connection itself stays in place.
          </p>
        }
        actionLabel="Revoke access"
        pending={remove.isPending}
        onConfirm={() =>
          grant &&
          remove.mutate(grant.id, {
            onSuccess: () => {
              toast.success(`Revoked access to ${installation.label}`);
              setRevoking(false);
            },
            onError: (e) => toastError(e, "Couldn't revoke access"),
          })
        }
      />
    </Panel>
  );
}
