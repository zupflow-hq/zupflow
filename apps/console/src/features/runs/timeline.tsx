import type { Run, RunEvent, ToolCallRef } from "@zupflow/sdk";
import {
  BanIcon,
  CheckIcon,
  ChevronRightIcon,
  CircleDotIcon,
  HandIcon,
  PlayIcon,
  SparklesIcon,
  WrenchIcon,
  XIcon,
} from "lucide-react";
import { useState } from "react";
import { CopyButton, JsonBlock } from "@/components/app/misc";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Spinner } from "@/components/ui/spinner";
import { shortId } from "@/lib/format";
import { cn } from "@/lib/utils";

type Item =
  | { kind: "event"; event: RunEvent }
  | {
      kind: "tool";
      ref: ToolCallRef;
      started: Extract<RunEvent, { type: "tool_call.started" }>;
      finished?: Extract<RunEvent, { type: "tool_call.completed" | "tool_call.failed" }>;
    };

/** Fold started/finished tool events into one item per call. */
function fold(events: RunEvent[]): Item[] {
  const items: Item[] = [];
  const tools = new Map<string, Extract<Item, { kind: "tool" }>>();
  for (const event of events) {
    if (event.type === "tool_call.started") {
      const item = { kind: "tool" as const, ref: event.data, started: event };
      tools.set(event.data.call_id, item);
      items.push(item);
    } else if (event.type === "tool_call.completed" || event.type === "tool_call.failed") {
      const item = tools.get(event.data.call_id);
      if (item) item.finished = event;
      else items.push({ kind: "event", event });
    } else {
      items.push({ kind: "event", event });
    }
  }
  return items;
}

function offset(startIso: string, iso: string) {
  const ms = Math.max(0, Date.parse(iso) - Date.parse(startIso));
  if (ms < 1000) return `+${ms}ms`;
  if (ms < 60_000) return `+${(ms / 1000).toFixed(1)}s`;
  return `+${Math.floor(ms / 60_000)}m${Math.round((ms % 60_000) / 1000)}s`;
}

function Row({
  at,
  start,
  icon,
  tone = "neutral",
  children,
  last,
}: {
  at: string;
  start: string;
  icon: React.ReactNode;
  tone?: "neutral" | "brand" | "success" | "danger" | "warning";
  children: React.ReactNode;
  last?: boolean;
}) {
  return (
    <li className="relative grid grid-cols-[3.75rem_1.75rem_minmax(0,1fr)] gap-x-2 sm:grid-cols-[4.5rem_1.75rem_minmax(0,1fr)]">
      <time dateTime={at} className="tabular pt-1 text-right font-mono text-[11px] text-muted-foreground">
        {offset(start, at)}
      </time>
      <div className="relative flex justify-center">
        {!last ? <span aria-hidden className="absolute top-7 bottom-0 w-px bg-border" /> : null}
        <span
          className={cn(
            "relative z-10 mt-0.5 grid size-6 place-items-center rounded-full border bg-card [&_svg]:size-3.5",
            tone === "brand" && "border-brand/50 text-brand-ink",
            tone === "success" && "border-success/50 text-success",
            tone === "danger" && "border-destructive/50 text-destructive",
            tone === "warning" && "border-warning/60 text-warning",
            tone === "neutral" && "text-muted-foreground",
          )}
        >
          {icon}
        </span>
      </div>
      <div className="min-w-0 pb-5">{children}</div>
    </li>
  );
}

function ToolCard({ item, live }: { item: Extract<Item, { kind: "tool" }>; live: boolean }) {
  const [open, setOpen] = useState(false);
  const failed = item.finished?.type === "tool_call.failed";
  return (
    <Collapsible open={open} onOpenChange={setOpen} className="rounded-lg border bg-card">
      <CollapsibleTrigger className="flex w-full items-center gap-2 rounded-lg px-3 py-2 text-left text-sm outline-none hover:bg-accent/40 focus-visible:ring-2 focus-visible:ring-ring/50">
        <ChevronRightIcon
          className={cn("size-3.5 shrink-0 text-muted-foreground transition-transform", open && "rotate-90")}
        />
        <code className="font-medium">{item.ref.tool}</code>
        <span className="min-w-0 flex-1 truncate text-muted-foreground">
          {item.ref.app_name} · {item.ref.installation_label}
        </span>
        {item.finished ? (
          <span
            className={cn("tabular shrink-0 font-mono text-xs", failed ? "text-destructive" : "text-muted-foreground")}
          >
            {failed ? "failed · " : ""}
            {item.finished.data.duration_ms} ms
          </span>
        ) : live ? (
          <Spinner className="size-3.5 text-brand-ink" />
        ) : null}
      </CollapsibleTrigger>
      <CollapsibleContent className="space-y-3 border-t px-3 py-3">
        <div>
          <p className="mb-1.5 text-xs font-medium text-muted-foreground">Arguments</p>
          <JsonBlock value={item.started.data.arguments} />
        </div>
        {item.finished?.type === "tool_call.completed" ? (
          <div>
            <p className="mb-1.5 text-xs font-medium text-muted-foreground">Result</p>
            <JsonBlock value={item.finished.data.result} />
          </div>
        ) : item.finished?.type === "tool_call.failed" ? (
          <div className="rounded-md border border-destructive/30 bg-danger-soft/50 p-2.5 text-sm">
            <code className="text-xs text-destructive">{item.finished.data.error.code}</code>
            <p>{item.finished.data.error.message}</p>
          </div>
        ) : null}
        <p className="font-mono text-[11px] text-muted-foreground">{item.ref.call_id}</p>
      </CollapsibleContent>
    </Collapsible>
  );
}

function PendingApproval({ workspaceId, runId, callId }: { workspaceId: string; runId: string; callId: string }) {
  const request = `POST /v1/workspaces/${workspaceId}/runs/${runId}/approvals
{ "call_id": "${callId}", "decision": "approved" }`;
  return (
    <div className="mt-3 space-y-3">
      <Collapsible>
        <CollapsibleTrigger className="group flex items-center gap-1 text-xs font-medium text-muted-foreground outline-none hover:text-foreground focus-visible:text-foreground">
          <ChevronRightIcon className="size-3.5 transition-transform group-data-[state=open]:rotate-90" />
          How your backend records the decision
        </CollapsibleTrigger>
        <CollapsibleContent className="pt-2">
          <div className="relative">
            <pre className="overflow-x-auto rounded-lg border bg-muted/50 p-3 pr-10 text-[12px] leading-relaxed">
              <code>{request}</code>
            </pre>
            <CopyButton value={request} label="Copy request" className="absolute top-1.5 right-1.5" />
          </div>
          <p className="mt-1.5 text-xs text-muted-foreground">
            Call it with an API key once your product has the decision. Subscribe to{" "}
            <code>run.waiting_for_approval</code> to know when one is needed.
          </p>
        </CollapsibleContent>
      </Collapsible>
    </div>
  );
}

export function RunTimeline({ run, events }: { run: Run; events: RunEvent[] }) {
  const items = fold(events);
  const live = run.status === "queued" || run.status === "running";
  const resolved = new Set(events.flatMap((e) => (e.type === "approval.resolved" ? [e.data.call_id] : [])));
  const start = run.created_at;

  return (
    <ol aria-label="Run timeline" aria-live="polite">
      <Row at={start} start={start} icon={<CircleDotIcon />} tone="neutral">
        <div className="rounded-lg border bg-muted/40 px-3 py-2.5">
          <p className="mb-0.5 text-xs text-muted-foreground">Input</p>
          <p className="text-sm whitespace-pre-wrap">{run.input}</p>
        </div>
      </Row>
      {items.map((item, i) => {
        const last = i === items.length - 1 && !live;
        if (item.kind === "tool") {
          const failed = item.finished?.type === "tool_call.failed";
          return (
            <Row
              key={item.ref.call_id}
              at={item.started.created_at}
              start={start}
              icon={failed ? <XIcon /> : <WrenchIcon />}
              tone={failed ? "danger" : item.finished ? "neutral" : "brand"}
              last={last}
            >
              <ToolCard item={item} live={live} />
            </Row>
          );
        }
        const e = item.event;
        switch (e.type) {
          case "run.queued":
            return (
              <Row key={e.id} at={e.created_at} start={start} icon={<CircleDotIcon />} last={last}>
                <p className="pt-1 text-sm text-muted-foreground">Queued</p>
              </Row>
            );
          case "run.started":
            return (
              <Row key={e.id} at={e.created_at} start={start} icon={<PlayIcon />} last={last}>
                <p className="pt-1 text-sm text-muted-foreground">
                  Started on <code className="text-foreground">{e.data.model}</code>
                </p>
              </Row>
            );
          case "message.assistant":
            return (
              <Row key={e.id} at={e.created_at} start={start} icon={<SparklesIcon />} tone="brand" last={last}>
                <p className="pt-0.5 text-sm leading-relaxed whitespace-pre-wrap">{e.data.text}</p>
              </Row>
            );
          case "approval.requested": {
            const pending = !resolved.has(e.data.call_id) && run.status === "waiting_for_approval";
            return (
              <Row
                key={e.id}
                at={e.created_at}
                start={start}
                icon={<HandIcon />}
                tone="warning"
                last={last && !pending}
              >
                <div
                  className={cn("rounded-lg border p-3", pending ? "border-warning/60 bg-warning-soft/60" : "bg-card")}
                >
                  <p className="text-sm font-medium">
                    Wants to call <code>{e.data.tool}</code> on {e.data.app_name} · {e.data.installation_label}
                  </p>
                  <p className="mb-2 text-sm text-muted-foreground">
                    {pending
                      ? "Paused until a decision is made in your product. Your backend sends it; the Console only shows it."
                      : "Approval was requested."}
                  </p>
                  <JsonBlock value={e.data.arguments} />
                  {pending ? (
                    <PendingApproval workspaceId={run.workspace_id} runId={run.id} callId={e.data.call_id} />
                  ) : null}
                </div>
              </Row>
            );
          }
          case "approval.resolved":
            return (
              <Row
                key={e.id}
                at={e.created_at}
                start={start}
                icon={e.data.decision === "approved" ? <CheckIcon /> : <XIcon />}
                tone={e.data.decision === "approved" ? "success" : "danger"}
                last={last}
              >
                <p className="pt-1 text-sm">
                  {e.data.decision === "approved" ? "Approved" : "Denied"} in your product
                  <span className="text-muted-foreground">
                    {" "}
                    · sent with{" "}
                    <code className="font-mono text-xs" title={e.data.api_key_id}>
                      {shortId(e.data.api_key_id)}
                    </code>
                  </span>
                </p>
              </Row>
            );
          case "run.completed":
            return (
              <Row key={e.id} at={e.created_at} start={start} icon={<CheckIcon />} tone="success" last>
                <p className="pt-1 text-sm font-medium text-success">Completed</p>
              </Row>
            );
          case "run.failed":
            return (
              <Row key={e.id} at={e.created_at} start={start} icon={<XIcon />} tone="danger" last>
                <div className="rounded-lg border border-destructive/30 bg-danger-soft/50 p-3 text-sm">
                  <p className="font-medium text-destructive">Failed</p>
                  <p>{e.data.error.message}</p>
                  <code className="text-xs text-muted-foreground">{e.data.error.code}</code>
                </div>
              </Row>
            );
          case "run.cancelled":
            return (
              <Row key={e.id} at={e.created_at} start={start} icon={<BanIcon />} last>
                <p className="pt-1 text-sm text-muted-foreground">Cancelled by {e.data.cancelled_by}</p>
              </Row>
            );
          default:
            return null;
        }
      })}
      {live ? (
        <li className="grid grid-cols-[3.75rem_1.75rem_minmax(0,1fr)] gap-x-2 sm:grid-cols-[4.5rem_1.75rem_minmax(0,1fr)]">
          <span />
          <span className="flex justify-center">
            <Spinner className="size-4 text-brand-ink" />
          </span>
          <p className="text-sm text-muted-foreground">Working…</p>
        </li>
      ) : null}
    </ol>
  );
}
