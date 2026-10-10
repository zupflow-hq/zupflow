import type { Installation, InstallationTool, Organization, ToolText, Workspace } from "@zupflow/sdk";
import { RefreshCwIcon } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import { JsonBlock, QueryError, RelativeTime, RowsSkeleton } from "@/components/app/misc";
import { Panel, SectionHeader } from "@/components/app/page";
import { StatusPill, ToolEffectTag } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Spinner } from "@/components/ui/spinner";
import { canAdminister, canChange } from "@/features/context";
import {
  useAcceptToolChange,
  useInstallationTools,
  useRefreshInstallationTools,
  useToolPolicy,
} from "@/features/queries";
import { changedToolCalls } from "@/features/tool-policy/levels";
import { errorMessage, isApiError, toastError } from "@/lib/errors";
import { cn } from "@/lib/utils";

/**
 * The tools the installation's app listed to this account. The first
 * definition of each is pinned; a changed one is kept beside it, unreviewed,
 * and a tool the app stopped listing is gone.
 */
export function InstallationTools({
  org,
  workspace,
  installation,
}: {
  org: Organization;
  workspace: Workspace;
  installation: Installation;
}) {
  const tools = useInstallationTools(workspace.id, installation.id);
  const refresh = useRefreshInstallationTools(workspace.id, installation.id);
  const [resting, rest] = useRest(REFRESH_REST_MS);
  const connected = installation.status === "connected";
  const mayRefresh = canChange(org) && workspace.environment === "sandbox" && connected;
  const items = tools.data?.pages.flatMap((page) => page.items) ?? [];
  const changed = items.filter((t) => t.status === "changed").length;
  const [reviewing, setReviewing] = useState<InstallationTool | null>(null);
  const mayReview = canAdminister(org);
  const policy = useToolPolicy(org.id);

  return (
    <section>
      <SectionHeader
        title="Tools"
        description={
          changed > 0
            ? `${changed} ${changed === 1 ? "tool's definition" : "tools' definitions"} changed. Until a change is accepted, ${changedToolCalls(policy.data?.new_tool_level)}.`
            : `What ${installation.app.name} lists to this account, as first listed.`
        }
        actions={
          mayRefresh ? (
            <Button
              variant="outline"
              size="sm"
              disabled={refresh.isPending || resting}
              title={resting ? "Refreshed less than a minute ago" : undefined}
              onClick={() =>
                refresh.mutate(undefined, {
                  onSuccess: () => {
                    rest();
                    toast.success("Refreshing tools");
                  },
                  onError: (e) => {
                    // Refreshed less than a minute ago: say so, and wait like after a success.
                    if (isApiError(e, "refresh_too_soon")) {
                      rest();
                      toast.error(errorMessage(e));
                    } else {
                      toastError(e, "Couldn't refresh tools");
                    }
                  },
                })
              }
            >
              {refresh.isPending ? <Spinner /> : <RefreshCwIcon />}
              Refresh tools
            </Button>
          ) : null
        }
      />
      {canChange(org) && workspace.environment === "production" ? (
        <p className="mb-3 text-sm text-muted-foreground">
          In Production, your product refreshes tools through the API.
        </p>
      ) : null}
      <Panel>
        {tools.isPending ? (
          <RowsSkeleton rows={3} columns={[45, 12]} />
        ) : tools.isError ? (
          <QueryError
            error={tools.error}
            onRetry={() => tools.refetch()}
            what="this installation's tools"
            className="m-3"
          />
        ) : items.length === 0 ? (
          <div className="p-6 text-center text-sm">
            <p className="font-medium">No tools discovered yet</p>
            <p className="mt-1 text-muted-foreground">
              {connected
                ? `Tools appear here once ${installation.app.name} lists them to this account.`
                : "Tools are listed once an account is connected."}
            </p>
          </div>
        ) : (
          <ul className="divide-y">
            {items.map((tool) => (
              <ToolRow key={tool.name} tool={tool} onReview={mayReview ? () => setReviewing(tool) : null} />
            ))}
          </ul>
        )}
      </Panel>
      <ReviewChangeDialog
        workspace={workspace}
        installation={installation}
        tool={reviewing}
        onClose={() => setReviewing(null)}
      />
      {tools.hasNextPage ? (
        <div className="mt-4 flex justify-center">
          <Button variant="outline" onClick={() => tools.fetchNextPage()} disabled={tools.isFetchingNextPage}>
            {tools.isFetchingNextPage ? <Spinner /> : null}
            Show more tools
          </Button>
        </div>
      ) : null}
    </section>
  );
}

/** The server takes one refresh of an installation a minute. */
const REFRESH_REST_MS = 60_000;

/** A pause: true for `ms` after `rest()` is called, cleared on unmount. */
function useRest(ms: number): [boolean, () => void] {
  const [resting, setResting] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );
  const rest = useCallback(() => {
    if (timer.current) clearTimeout(timer.current);
    setResting(true);
    timer.current = setTimeout(() => {
      timer.current = null;
      setResting(false);
    }, ms);
  }, [ms]);
  return [resting, rest];
}

function ToolRow({ tool, onReview }: { tool: InstallationTool; onReview: (() => void) | null }) {
  const gone = tool.status === "gone";
  return (
    <li className="px-4 py-3">
      <div className="flex flex-col gap-1.5 sm:flex-row sm:items-start sm:gap-4">
        <div className={cn("min-w-0 flex-1", gone && "opacity-60")}>
          <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm font-medium">
            <span className="break-words">{tool.pinned.title ?? tool.name}</span>
            <code className="text-xs font-normal break-all text-muted-foreground">{tool.name}</code>
          </p>
          {/* A changed tool shows its pinned text beside the new one, below. */}
          {tool.pinned.description && tool.status !== "changed" ? (
            <p className="text-sm break-words text-muted-foreground">{tool.pinned.description}</p>
          ) : null}
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {tool.status === "changed" ? <StatusPill tone="warning">Changed</StatusPill> : null}
          {gone ? <StatusPill tone="neutral">Gone</StatusPill> : null}
          <ToolEffectTag effect={tool.effect} />
        </div>
      </div>
      {gone ? (
        <p className="mt-1.5 text-xs text-muted-foreground">
          Not listed since <RelativeTime iso={tool.gone_at} />.
        </p>
      ) : tool.status === "changed" && tool.latest ? (
        <ChangedDefinition pinned={tool.pinned} latest={tool.latest} changedAt={tool.changed_at} onReview={onReview} />
      ) : null}
    </li>
  );
}

const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

/** The parts of a definition that changed but show no text: everything hashed counts. */
function hiddenChanges(pinned: ToolText, latest: ToolText): string[] {
  return [
    !same(pinned.input_schema, latest.input_schema) ? "inputs" : null,
    !same(pinned.output_schema, latest.output_schema) ? "outputs" : null,
    !same(pinned.annotations, latest.annotations) ? "hints" : null,
  ].filter((part): part is string => part !== null);
}

/** "inputs", "inputs and hints", "inputs, outputs and hints". */
function listed(parts: string[]): string {
  return parts.length <= 1 ? (parts[0] ?? "") : `${parts.slice(0, -1).join(", ")} and ${parts.at(-1)}`;
}

function ChangedDefinition({
  pinned,
  latest,
  changedAt,
  onReview,
}: {
  pinned: ToolText;
  latest: ToolText;
  changedAt: string | null;
  onReview: (() => void) | null;
}) {
  const hidden = hiddenChanges(pinned, latest);
  const textChanged = pinned.title !== latest.title || pinned.description !== latest.description;
  return (
    <div className="mt-2 rounded-lg border border-warning/40 bg-warning-soft/40 p-3 text-sm">
      <p className="text-xs text-muted-foreground">
        The definition changed <RelativeTime iso={changedAt} fallback="recently" /> and isn't reviewed.
      </p>
      {hidden.length > 0 ? (
        <p className="mt-1 text-xs font-medium">
          Its {listed(hidden)} changed{textChanged ? "" : "; its title and description didn't"}.
        </p>
      ) : null}
      <dl className="mt-2 grid gap-3 sm:grid-cols-2">
        <div className="min-w-0">
          <dt className="text-xs font-medium text-muted-foreground">Pinned</dt>
          <dd className="mt-0.5 break-words">
            <TextOf text={pinned} />
          </dd>
        </div>
        <div className="min-w-0">
          <dt className="text-xs font-medium text-muted-foreground">New, not reviewed</dt>
          <dd className="mt-0.5 break-words">
            <TextOf text={latest} />
          </dd>
        </div>
      </dl>
      {onReview ? (
        <Button variant="outline" size="sm" className="mt-3 bg-card" onClick={onReview}>
          Review change
        </Button>
      ) : (
        <p className="mt-2 text-xs text-muted-foreground">Owners and admins review changes.</p>
      )}
    </div>
  );
}

/** Each part of a definition, pinned and new, for a side-by-side review. */
const PARTS: { label: string; of: (text: ToolText) => unknown }[] = [
  { label: "Title", of: (t) => t.title },
  { label: "Description", of: (t) => t.description },
  { label: "Inputs", of: (t) => t.input_schema },
  { label: "Outputs", of: (t) => t.output_schema },
  { label: "Hints", of: (t) => t.annotations },
];

/**
 * The whole pinned and new definitions, part by part, with the parts that
 * differ marked. Accepting pins exactly the definition shown: if the app
 * changed it again meanwhile, the server refuses and the new one is shown.
 */
function ReviewChangeDialog({
  workspace,
  installation,
  tool,
  onClose,
}: {
  workspace: Workspace;
  installation: Installation;
  tool: InstallationTool | null;
  onClose: () => void;
}) {
  const accept = useAcceptToolChange(workspace.id, installation.id);
  const latest = tool?.latest ?? null;
  return (
    <Dialog open={tool !== null} onOpenChange={(open) => (open ? null : onClose())}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>Review change to {tool?.name}</DialogTitle>
          <DialogDescription>
            Calls use the pinned definition until you accept the new one. Accept only if the new one asks for nothing
            you wouldn't allow.
          </DialogDescription>
        </DialogHeader>
        {tool && latest ? (
          <div className="space-y-4">
            {PARTS.map(({ label, of }) => {
              const [before, after] = [of(tool.pinned), of(latest)];
              const changed = !same(before, after);
              return (
                <div key={label} className="min-w-0">
                  <p className="mb-1.5 flex items-center gap-2 text-xs font-medium text-muted-foreground">
                    {label}
                    {changed ? <StatusPill tone="warning">Changed</StatusPill> : <span>· Same</span>}
                  </p>
                  {changed ? (
                    <div className="grid gap-2 sm:grid-cols-2">
                      <PartValue caption="Pinned" value={before} />
                      <PartValue caption="New" value={after} />
                    </div>
                  ) : (
                    <PartValue caption={null} value={before} />
                  )}
                </div>
              );
            })}
          </div>
        ) : null}
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            Keep pinned
          </Button>
          <Button
            disabled={accept.isPending || !tool?.latest_hash}
            onClick={() => {
              if (!tool?.latest_hash) return;
              accept.mutate(
                { name: tool.name, latest_hash: tool.latest_hash },
                {
                  onSuccess: () => {
                    toast.success(`Accepted the change to ${tool.name}`);
                    onClose();
                  },
                  onError: (e) => {
                    if (isApiError(e, "stale_definition", "tool_not_changed")) onClose();
                    toastError(e, "Couldn't accept the change");
                  },
                },
              );
            }}
          >
            {accept.isPending ? <Spinner /> : null}
            Accept change
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function PartValue({ caption, value }: { caption: string | null; value: unknown }) {
  return (
    <div className="min-w-0">
      {caption ? <p className="mb-1 text-xs text-muted-foreground">{caption}</p> : null}
      {value === null || value === undefined ? (
        <p className="text-sm text-muted-foreground">None</p>
      ) : typeof value === "string" ? (
        <p className="rounded-lg border bg-muted/30 p-2 text-sm break-words">{value}</p>
      ) : (
        <JsonBlock value={value} />
      )}
    </div>
  );
}

function TextOf({ text }: { text: ToolText }) {
  return (
    <>
      {text.title ? <span className="block font-medium">{text.title}</span> : null}
      {text.description ?? <span className="text-muted-foreground">No description</span>}
    </>
  );
}
