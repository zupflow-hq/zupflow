import { useNavigate } from "@tanstack/react-router";
import type { AppId, AvailableApp, Organization, Workspace } from "@zupflow/sdk";
import { ChevronLeftIcon } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { ENVIRONMENT_LABEL, EnvBadge } from "@/components/app/environment";
import { AppIcon, QueryError } from "@/components/app/misc";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister } from "@/features/context";
import { useAvailableApps, useOrgApps } from "@/features/queries";
import { cn } from "@/lib/utils";
import { ConnectError, ConnectProgress, useConnectFlow } from "./connect-flow";

export function AddInstallationDialog({
  org,
  workspace,
  open,
  onOpenChange,
  initialAppId,
}: {
  org: Organization;
  workspace: Workspace;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  initialAppId?: AppId;
}) {
  const navigate = useNavigate();
  const available = useAvailableApps(workspace.id);
  const catalog = useOrgApps(org.id);
  const flow = useConnectFlow(workspace.id);
  const [selected, setSelected] = useState<AvailableApp | null>(null);
  const [label, setLabel] = useState("");

  useEffect(() => {
    if (!open) {
      setSelected(null);
      setLabel("");
      flow.reset();
    }
  }, [open, flow.reset]);

  useEffect(() => {
    if (open && initialAppId && !selected && available.data) {
      const match = available.data.find((a) => a.app.id === initialAppId);
      if (match) setSelected(match);
    }
  }, [open, initialAppId, selected, available.data]);

  const unavailable = useMemo(() => {
    const ids = new Set(available.data?.map((a) => a.app.id));
    return (catalog.data ?? []).filter((a) => !ids.has(a.id));
  }, [available.data, catalog.data]);

  const step = flow.sessionId ? "connect" : selected ? "details" : "pick";
  const sameAppCount = (appId: string) => available.data?.find((a) => a.app.id === appId)?.app.installation_count ?? 0;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="gap-5 sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>
            {step === "pick" ? "Add installation" : step === "details" ? `Connect ${selected?.app.name}` : "Connect"}
          </DialogTitle>
          <DialogDescription className="flex flex-wrap items-center gap-1.5">
            {step === "pick" ? (
              <>
                Apps ready for <EnvBadge environment={workspace.environment} size="sm" /> in {workspace.name}.
              </>
            ) : step === "details" ? (
              "Name this installation so people can tell it apart from others of the same app."
            ) : (
              "Connect opens in its own window, just as it will inside your product."
            )}
          </DialogDescription>
        </DialogHeader>

        {step === "pick" ? (
          available.isError ? (
            <QueryError error={available.error} onRetry={() => available.refetch()} what="available apps" />
          ) : !available.data ? (
            <div className="space-y-2">
              {[0, 1, 2].map((i) => (
                <Skeleton key={i} className="h-14 w-full" />
              ))}
            </div>
          ) : (
            <div className="-mx-1 max-h-[55vh] space-y-1 overflow-y-auto px-1">
              {available.data.length === 0 ? (
                <p className="rounded-lg border border-dashed p-6 text-center text-sm text-muted-foreground">
                  No app is ready for {ENVIRONMENT_LABEL[workspace.environment]} yet.
                </p>
              ) : (
                available.data.map((a) => (
                  <button
                    key={a.app.id}
                    type="button"
                    onClick={() => {
                      setSelected(a);
                      setLabel(sameAppCount(a.app.id) > 0 ? "" : a.app.name);
                    }}
                    className="flex w-full items-center gap-3 rounded-lg border p-3 text-left transition-colors outline-none hover:bg-accent/50 focus-visible:ring-[3px] focus-visible:ring-ring/50"
                  >
                    <AppIcon icon={a.app.icon} />
                    <div className="min-w-0 flex-1">
                      <p className="flex items-center gap-2 text-sm font-medium">
                        {a.app.name}
                        {a.app.owner_scope === "workspace" ? (
                          <span className="rounded border px-1 text-[10px] font-normal text-muted-foreground">
                            Workspace app
                          </span>
                        ) : null}
                      </p>
                      <p className="truncate text-xs text-muted-foreground">
                        {a.app.auth.type === "oauth2" ? "OAuth" : "API token"} · {a.app.tools.length} tools
                      </p>
                    </div>
                  </button>
                ))
              )}
              {unavailable.length > 0 ? (
                <div className="pt-3">
                  <p className="mb-1.5 text-xs font-medium text-muted-foreground">
                    Not ready for {ENVIRONMENT_LABEL[workspace.environment]}
                  </p>
                  {unavailable.map((app) => (
                    <div key={app.id} className="flex items-center gap-3 rounded-lg p-2 opacity-60">
                      <AppIcon icon={app.icon} size="sm" />
                      <span className="flex-1 text-sm">{app.name}</span>
                      {canAdminister(org) ? (
                        <Button
                          variant="link"
                          size="xs"
                          className="h-auto p-0"
                          onClick={() => {
                            onOpenChange(false);
                            void navigate({
                              to: "/$org/apps/$appId",
                              params: { org: org.slug, appId: app.id },
                              search: { tab: "configuration" },
                            });
                          }}
                        >
                          Configure
                        </Button>
                      ) : (
                        <span className="text-xs text-muted-foreground">Ask an admin</span>
                      )}
                    </div>
                  ))}
                </div>
              ) : null}
            </div>
          )
        ) : step === "details" && selected ? (
          <form
            id="add-installation"
            className="space-y-4"
            onSubmit={(e) => {
              e.preventDefault();
              if (!label.trim()) return;
              flow.start({
                app_id: selected.app.id,
                intent: "create",
                label: label.trim(),
                installation_id: null,
                return_url: null,
              });
            }}
          >
            <div className="flex items-center gap-3 rounded-lg border bg-muted/30 p-3">
              <AppIcon icon={selected.app.icon} />
              <div className="min-w-0 flex-1 text-sm">
                <p className="font-medium">{selected.app.name}</p>
                <p className="text-muted-foreground">
                  {ENVIRONMENT_LABEL[workspace.environment]} configuration v{selected.configuration.version}
                </p>
              </div>
            </div>
            <div className="space-y-2">
              <Label htmlFor="installation-label">Label</Label>
              <Input
                id="installation-label"
                autoFocus
                placeholder="Marketing"
                value={label}
                maxLength={40}
                onChange={(e) => setLabel(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                A workspace can hold several installations of the same app — one per account.
              </p>
            </div>
            <ConnectError error={flow.error} />
          </form>
        ) : (
          <ConnectProgress
            flow={flow}
            doneLabel="View installation"
            onDone={(s) => {
              onOpenChange(false);
              if (s.installation_id) {
                void navigate({
                  to: "/$org/w/$ws/installations/$installationId",
                  params: { org: org.slug, ws: workspace.id, installationId: s.installation_id },
                });
              }
            }}
          />
        )}

        {step === "details" ? (
          <DialogFooter className={cn("sm:justify-between")}>
            <Button variant="ghost" onClick={() => setSelected(null)}>
              <ChevronLeftIcon />
              Back
            </Button>
            <Button type="submit" form="add-installation" disabled={!label.trim() || flow.starting}>
              {flow.starting ? <Spinner /> : null}
              Continue to Connect
            </Button>
          </DialogFooter>
        ) : null}
      </DialogContent>
    </Dialog>
  );
}
