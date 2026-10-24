import { Link } from "@tanstack/react-router";
import {
  CHANNEL_PROVIDER_NAMES,
  type ChannelConnection,
  type ChannelConnectionId,
  type ChannelConversationId,
  type ChannelMessage,
  type Run,
  type RunEvent,
  type ToolCallRef,
} from "@zupflow/sdk";
import {
  BanIcon,
  CheckIcon,
  ChevronRightIcon,
  CircleDotIcon,
  HandIcon,
  MessageSquarePlusIcon,
  MessageSquareTextIcon,
  PlayIcon,
  SparklesIcon,
  VolumeXIcon,
  WrenchIcon,
  XIcon,
} from "lucide-react";
import { useState } from "react";
import { CopyButton, JsonBlock } from "@/components/app/misc";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { renderMentions, WAKE_REASON } from "@/features/channels/labels";
import { dateTime, fullDateTime, shortId } from "@/lib/format";
import { cn } from "@/lib/utils";
import type { RunChannelContext } from "./channel-context";

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

/** Where a call goes: an installation's app and label, or a channel connection's provider and label. */
function toolTarget(ref: ToolCallRef): string {
  return "installation_id" in ref
    ? `${ref.app_name} · ${ref.installation_label}`
    : `${CHANNEL_PROVIDER_NAMES[ref.provider]} · ${ref.connection_label}`;
}

/** The conversation a channel call reads or sends to, and how to name it: its thread, or another conversation. */
function channelDestination(
  ref: ToolCallRef,
  args: unknown,
  run: Run,
): { label: string; conversation: ChannelConversationId; thread?: string } | null {
  if ("installation_id" in ref) return null;
  const given = (args ?? {}) as { conversation?: unknown; thread?: unknown; person?: unknown };
  const origin = run.channel;
  if (ref.tool === "reply" && origin) {
    return { label: "in the thread that woke it", conversation: origin.conversation_id, thread: origin.thread_key };
  }
  if (typeof given.conversation === "string") {
    // An ID the model passed: the link names it, and the API judges it.
    const conversation = given.conversation as ChannelConversationId;
    const thread = typeof given.thread === "string" ? given.thread : undefined;
    if (origin && conversation === origin.conversation_id) {
      return {
        label: thread && thread !== origin.thread_key ? "in another thread here" : "in the conversation that woke it",
        conversation,
        thread: thread ?? origin.thread_key,
      };
    }
    return { label: `in ${shortId(conversation)}`, conversation, thread };
  }
  return null;
}

/** A link to a conversation, or one of its threads, in the workspace's channel browser. */
function ConversationLink({
  orgSlug,
  run,
  connectionId,
  conversation,
  thread,
  children,
}: {
  orgSlug: string | undefined;
  run: Run;
  connectionId: ChannelConnectionId;
  conversation: ChannelConversationId;
  thread?: string;
  children: React.ReactNode;
}) {
  if (!orgSlug) return null;
  return (
    <Link
      to="/$org/w/$ws/channels/$connectionId"
      params={{ org: orgSlug, ws: run.workspace_id, connectionId }}
      search={{ conversation, thread: thread || undefined }}
      className="text-xs text-brand-ink underline-offset-4 hover:underline"
    >
      {children}
    </Link>
  );
}

/** A channel message as the agent read it: who wrote it, when, and what it said. */
function ChannelQuote({
  message,
  connection,
  className,
}: {
  message: ChannelMessage;
  connection: ChannelConnection | undefined;
  className?: string;
}) {
  const author =
    message.direction === "outbound"
      ? (connection?.label ?? "The agent")
      : (message.author?.display_name ?? message.author?.handle ?? "Someone in the team");
  return (
    <div className={cn("rounded-lg border bg-muted/40 px-3 py-2.5", className)}>
      <p className="flex flex-wrap items-baseline gap-x-2 text-xs text-muted-foreground">
        <span className="font-medium text-foreground">{author}</span>
        {message.author?.is_automated ? <span>automated</span> : null}
        <time dateTime={message.sent_at} title={fullDateTime(message.sent_at)}>
          {dateTime(message.sent_at)}
        </time>
      </p>
      <p className="mt-0.5 text-sm break-words whitespace-pre-wrap">
        {message.deleted_at ? (
          <span className="text-muted-foreground italic">This message was deleted.</span>
        ) : (
          renderMentions(message.text, connection)
        )}
      </p>
    </div>
  );
}

/**
 * Messages by ID, as loaded. One that isn't loaded is either further back in
 * the thread, or, once everything is loaded and it arrived before the
 * retention period, removed by retention; otherwise the link opens the thread.
 */
function Quotes({
  ids,
  context,
  at,
  link,
}: {
  ids: string[];
  context: RunChannelContext | null | undefined;
  /** When the messages arrived, about: the event that names them. */
  at: string;
  link: React.ReactNode;
}) {
  if (context?.loading) return <Skeleton className="h-14 w-full" />;
  if (context?.error) {
    return (
      <div role="alert" className="flex flex-wrap items-center gap-x-2 text-xs">
        <span className="text-destructive">Couldn't load these messages.</span>
        <button type="button" className="text-brand-ink underline-offset-4 hover:underline" onClick={context.retry}>
          Try again
        </button>
      </div>
    );
  }
  const missing = ids.filter((id) => !context?.messages.get(id)).length;
  const past = context?.retainedSince != null && Date.parse(at) < context.retainedSince;
  const them = (n: number) => (n === 1 ? "it" : "them");
  return (
    <div className="space-y-2">
      {ids.map((id) => {
        const message = context?.messages.get(id);
        return message ? <ChannelQuote key={id} message={message} connection={context?.connection} /> : null;
      })}
      {missing === 0 ? null : context?.more ? (
        <p className="flex flex-wrap items-center gap-x-2 text-xs text-muted-foreground">
          {missing === 1 ? "One message is" : `${missing} messages are`} further back in the thread.
          <button
            type="button"
            className="text-brand-ink underline-offset-4 hover:underline disabled:opacity-50"
            disabled={context.loadingMore}
            onClick={context.loadMore}
          >
            {context.loadingMore ? "Loading…" : "Load older messages"}
          </button>
        </p>
      ) : past ? (
        <p className="text-xs text-muted-foreground">
          {missing === 1 ? "One message isn't" : `${missing} messages aren't`} kept any more: your organization's
          retention period removed {them(missing)}.
        </p>
      ) : (
        <p className="flex flex-wrap items-center gap-x-2 text-xs text-muted-foreground">
          {missing === 1 ? "One message isn't" : `${missing} messages aren't`} shown here.
          {link}
        </p>
      )}
    </div>
  );
}

function ToolCard({
  item,
  live,
  run,
  orgSlug,
}: {
  item: Extract<Item, { kind: "tool" }>;
  live: boolean;
  run: Run;
  orgSlug?: string;
}) {
  const [open, setOpen] = useState(false);
  const failed = item.finished?.type === "tool_call.failed";
  const destination = channelDestination(item.ref, item.started.data.arguments, run);
  return (
    <Collapsible open={open} onOpenChange={setOpen} className="rounded-lg border bg-card">
      <CollapsibleTrigger className="flex w-full items-center gap-2 rounded-lg px-3 py-2 text-left text-sm outline-none hover:bg-accent/40 focus-visible:ring-2 focus-visible:ring-ring/50">
        <ChevronRightIcon
          className={cn("size-3.5 shrink-0 text-muted-foreground transition-transform", open && "rotate-90")}
        />
        <code className="font-medium">{item.ref.tool}</code>
        <span className="min-w-0 flex-1 truncate text-muted-foreground">
          {toolTarget(item.ref)}
          {destination ? ` · ${destination.label}` : ""}
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
        <p className="flex flex-wrap items-center gap-x-3 font-mono text-[11px] text-muted-foreground">
          {item.ref.call_id}
          {destination && "channel_connection_id" in item.ref ? (
            <ConversationLink
              orgSlug={orgSlug}
              run={run}
              connectionId={item.ref.channel_connection_id}
              conversation={destination.conversation}
              thread={destination.thread}
            >
              Open conversation
            </ConversationLink>
          ) : null}
        </p>
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

/**
 * A channel run opens with the message that started it, as it was written,
 * with why it woke the agent. Once retention removed the message, the run's
 * input stands in for it.
 */
function OriginRow({
  run,
  context,
  orgSlug,
}: {
  run: Run & { channel: NonNullable<Run["channel"]> };
  context: RunChannelContext | null | undefined;
  orgSlug?: string;
}) {
  const origin = run.channel.origin_message_id ? context?.messages.get(run.channel.origin_message_id) : undefined;
  const provider = context?.connection ? CHANNEL_PROVIDER_NAMES[context.connection.provider] : "Slack";
  return (
    <Row at={run.created_at} start={run.created_at} icon={<MessageSquareTextIcon />} tone="neutral">
      <p className="mb-1.5 flex flex-wrap items-center gap-x-2 text-xs text-muted-foreground">
        <span>
          {WAKE_REASON[run.channel.reason]} · {provider}
          {context?.connection ? ` · ${context.connection.label}` : ""}
        </span>
        <ConversationLink
          orgSlug={orgSlug}
          run={run}
          connectionId={run.channel.connection_id}
          conversation={run.channel.conversation_id}
          thread={run.channel.thread_key}
        >
          Open {run.channel.thread_key ? "thread" : "conversation"}
        </ConversationLink>
      </p>
      {origin ? (
        <ChannelQuote message={origin} connection={context?.connection} />
      ) : context?.loading && run.channel.origin_message_id ? (
        <Skeleton className="h-14 w-full" />
      ) : (
        <div className="rounded-lg border bg-muted/40 px-3 py-2.5">
          <p className="mb-0.5 text-xs text-muted-foreground">
            {run.channel.origin_message_id ? "Input" : "Input · the message itself isn't kept any more"}
          </p>
          <p className="text-sm whitespace-pre-wrap">{run.input}</p>
          {run.channel.origin_message_id && context?.more ? (
            // The message is further back in the thread than what's loaded.
            <button
              type="button"
              className="mt-1 text-xs text-brand-ink underline-offset-4 hover:underline disabled:opacity-50"
              disabled={context.loadingMore}
              onClick={context.loadMore}
            >
              {context.loadingMore ? "Loading…" : "Load the message as written"}
            </button>
          ) : null}
        </div>
      )}
    </Row>
  );
}

export function RunTimeline({
  run,
  events,
  channel,
  orgSlug,
}: {
  run: Run;
  events: RunEvent[];
  /** A channel run's connection and thread; null or absent for any other run. */
  channel?: RunChannelContext | null;
  /** For links to the run's conversation; without it, none are shown. */
  orgSlug?: string;
}) {
  const items = fold(events);
  const live = run.status === "queued" || run.status === "running";
  const resolved = new Set(events.flatMap((e) => (e.type === "approval.resolved" ? [e.data.call_id] : [])));
  const start = run.created_at;
  const origin = run.channel?.origin_message_id ?? null;

  return (
    <ol aria-label="Run timeline" aria-live="polite">
      {run.channel ? (
        <OriginRow run={{ ...run, channel: run.channel }} context={channel} orgSlug={orgSlug} />
      ) : (
        <Row at={start} start={start} icon={<CircleDotIcon />} tone="neutral">
          <div className="rounded-lg border bg-muted/40 px-3 py-2.5">
            <p className="mb-0.5 text-xs text-muted-foreground">Input</p>
            <p className="text-sm whitespace-pre-wrap">{run.input}</p>
          </div>
        </Row>
      )}
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
              <ToolCard item={item} live={live} run={run} orgSlug={orgSlug} />
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
          case "channel_messages.received": {
            // The message that started the run already opens the timeline.
            const others = e.data.message_ids.filter((id) => id !== origin);
            if (e.data.outcome === "started" && others.length === 0) return null;
            const quoted = e.data.outcome === "steered" ? e.data.message_ids : others;
            const count = quoted.length;
            const title =
              e.data.outcome === "steered"
                ? `${count === 1 ? "A new message" : `${count} new messages`} joined while it worked`
                : e.data.outcome === "collected"
                  ? count === 0
                    ? "Waited for the run before this one"
                    : `Also answers ${count === 1 ? "a message" : `${count} messages`} that arrived while its last run worked`
                  : `Also read ${count === 1 ? "a message" : `${count} messages`} new in the thread since its last run`;
            return (
              <Row
                key={e.id}
                at={e.created_at}
                start={start}
                icon={<MessageSquarePlusIcon />}
                tone={e.data.outcome === "steered" ? "brand" : "neutral"}
                last={last}
              >
                <p className="pt-1 text-sm font-medium">{title}</p>
                <p className={cn("text-xs text-muted-foreground", count > 0 && "mb-2")}>
                  {e.data.outcome === "steered"
                    ? "Added to the run before its next step, so its answer accounts for them."
                    : e.data.outcome === "collected"
                      ? "Messages that arrive while it works in a thread wait for that run to finish, then start this one."
                      : "Earlier messages it hadn't seen, from people other than the one who woke it."}
                </p>
                {count > 0 ? (
                  <Quotes
                    ids={quoted}
                    context={channel}
                    at={e.created_at}
                    link={
                      <ConversationLink
                        orgSlug={orgSlug}
                        run={run}
                        connectionId={e.data.connection_id}
                        conversation={e.data.conversation_id}
                        thread={run.channel?.thread_key}
                      >
                        Open {run.channel?.thread_key ? "the thread" : "the conversation"}
                      </ConversationLink>
                    }
                  />
                ) : null}
              </Row>
            );
          }
          case "run.stayed_silent":
            return (
              <Row key={e.id} at={e.created_at} start={start} icon={<VolumeXIcon />} last={last}>
                <div className="rounded-lg border bg-card p-3 text-sm">
                  <p className="font-medium">Stayed silent</p>
                  <p className="text-muted-foreground">
                    It chose not to answer, and nothing was posted. Its reason: {e.data.reason}
                  </p>
                </div>
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
                    Wants to call <code>{e.data.tool}</code> on {toolTarget(e.data)}
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
