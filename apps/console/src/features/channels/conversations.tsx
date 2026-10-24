import { Link } from "@tanstack/react-router";
import type {
  ChannelConnection,
  ChannelConversation,
  ChannelConversationId,
  ChannelConversationKind,
  ChannelMessage,
  Organization,
  Workspace,
} from "@zupflow/sdk";
import { ArrowLeftIcon, MessageSquareTextIcon, MessagesSquareIcon, PauseCircleIcon } from "lucide-react";
import { useState } from "react";
import { QueryError, RelativeTime } from "@/components/app/misc";
import { Panel } from "@/components/app/page";
import { StatusPill } from "@/components/app/status";
import { SearchInput } from "@/components/app/toolbar";
import { Button } from "@/components/ui/button";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { useChannelConversationLookup, useChannelConversations, useChannelMessages } from "@/features/queries";
import { dateTime, fullDateTime, shortId } from "@/lib/format";
import { useDebounced } from "@/lib/use-debounced";
import { cn } from "@/lib/utils";
import { CONVERSATION_KIND, conversationName, renderMentions } from "./labels";

const ALL = "all";
const KINDS: ChannelConversationKind[] = ["channel", "private_channel", "dm", "group_dm"];

/**
 * A connection's conversations, searched and paged on the server, beside the
 * chosen one's messages. On a phone one shows at a time.
 */
export function ConversationBrowser({
  org,
  workspace,
  connection,
  conversationId,
  thread,
  onOpen,
}: {
  org: Organization;
  workspace: Workspace;
  connection: ChannelConnection;
  conversationId: ChannelConversationId | undefined;
  thread: string | undefined;
  /** Opens a conversation, or one of its threads; nothing goes back to the list. */
  onOpen: (conversationId: ChannelConversationId | null, thread?: string) => void;
}) {
  const [query, setQuery] = useState("");
  const [kind, setKind] = useState<ChannelConversationKind | typeof ALL>(ALL);
  const q = useDebounced(query.trim());
  const conversations = useChannelConversations(workspace.id, connection.id, {
    q: q || undefined,
    kind: kind === ALL ? undefined : kind,
  });
  const items = conversations.data?.pages.flatMap((page) => page.items) ?? [];
  const total = conversations.data?.pages[0]?.pagination.total;
  // The chosen one may be past the loaded pages: its messages still load by ID.
  const [chosen, setChosen] = useState<ChannelConversation | null>(null);
  const listed = items.find((c) => c.id === conversationId) ?? (chosen?.id === conversationId ? chosen : null);
  // A link to one past the loaded pages (from a run, say) finds it on its own, so its name and state show.
  const lookup = useChannelConversationLookup(
    workspace.id,
    connection.id,
    conversationId,
    !listed && !conversations.isPending,
  );
  const selected = listed ?? lookup.data ?? null;

  return (
    <div className="grid gap-4 lg:grid-cols-[20rem_minmax(0,1fr)]">
      <section className={cn("min-w-0", conversationId && "hidden lg:block")} aria-label="Conversations">
        <div className="mb-3 flex flex-col gap-2 sm:flex-row lg:flex-col">
          <SearchInput
            value={query}
            onChange={setQuery}
            placeholder="Search conversations"
            className="sm:w-auto sm:flex-1 lg:w-full lg:flex-none"
          />
          <Select value={kind} onValueChange={(v) => setKind(v as typeof kind)}>
            <SelectTrigger aria-label="Kind of conversation" className="w-full bg-card sm:w-48 lg:w-full">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL}>All conversations</SelectItem>
              {KINDS.map((k) => (
                <SelectItem key={k} value={k}>
                  {CONVERSATION_KIND[k]}s
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <Panel className="overflow-hidden">
          {conversations.isError ? (
            <QueryError
              error={conversations.error}
              onRetry={() => conversations.refetch()}
              what="conversations"
              className="m-3"
            />
          ) : conversations.isPending ? (
            <div className="divide-y" role="status" aria-busy aria-label="Loading">
              {[60, 45, 70, 50].map((w) => (
                <div key={w} className="space-y-2 px-4 py-3">
                  <Skeleton className="h-3.5" style={{ width: `${w}%` }} />
                  <Skeleton className="h-3 w-24" />
                </div>
              ))}
            </div>
          ) : items.length === 0 ? (
            <p className="px-4 py-8 text-center text-sm text-muted-foreground">
              {q || kind !== ALL
                ? "No conversation matches."
                : "No conversations yet. They appear once someone messages the agent."}
            </p>
          ) : (
            <ul className="divide-y">
              {items.map((c) => (
                <li key={c.id}>
                  <button
                    type="button"
                    aria-current={c.id === conversationId ? "true" : undefined}
                    onClick={() => {
                      setChosen(c);
                      onOpen(c.id);
                    }}
                    className={cn(
                      "flex w-full items-start gap-3 px-4 py-3 text-left transition-colors outline-none hover:bg-accent/50 focus-visible:bg-accent/60",
                      c.id === conversationId && "bg-accent/60",
                    )}
                  >
                    <span className="min-w-0 flex-1">
                      <span className="block truncate text-sm font-medium">{conversationName(c)}</span>
                      <span className="block truncate text-xs text-muted-foreground">
                        {CONVERSATION_KIND[c.kind]}
                        {c.is_external ? " · Slack Connect" : ""}
                      </span>
                    </span>
                    <span className="flex shrink-0 flex-col items-end gap-1">
                      <RelativeTime iso={c.last_message_at} fallback="No messages" className="text-xs" />
                      {c.paused_until && Date.parse(c.paused_until) > Date.now() ? (
                        <StatusPill tone="warning">Paused</StatusPill>
                      ) : null}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
          {conversations.hasNextPage ? (
            <div className="flex items-center justify-between gap-2 border-t px-4 py-2.5">
              <span className="text-xs text-muted-foreground">
                {total !== undefined && total !== null ? `${items.length} of ${total.toLocaleString("en")}` : null}
              </span>
              <Button
                variant="outline"
                size="sm"
                onClick={() => conversations.fetchNextPage()}
                disabled={conversations.isFetchingNextPage}
              >
                {conversations.isFetchingNextPage ? <Spinner /> : null}
                Show more
              </Button>
            </div>
          ) : null}
        </Panel>
      </section>

      <section className={cn("min-w-0", !conversationId && "hidden lg:block")} aria-label="Messages">
        {conversationId ? (
          <Messages
            org={org}
            workspace={workspace}
            connection={connection}
            conversationId={conversationId}
            conversation={selected}
            thread={thread}
            onOpen={onOpen}
          />
        ) : (
          <Panel className="grid h-full min-h-64 place-items-center p-6 text-center">
            <div className="space-y-2">
              <MessagesSquareIcon className="mx-auto size-6 text-muted-foreground" />
              <p className="text-sm text-muted-foreground">Pick a conversation to read its messages.</p>
            </div>
          </Panel>
        )}
      </section>
    </div>
  );
}

function Messages({
  org,
  workspace,
  connection,
  conversationId,
  conversation,
  thread,
  onOpen,
}: {
  org: Organization;
  workspace: Workspace;
  connection: ChannelConnection;
  conversationId: ChannelConversationId;
  conversation: ChannelConversation | null;
  thread: string | undefined;
  onOpen: (conversationId: ChannelConversationId | null, thread?: string) => void;
}) {
  const messages = useChannelMessages(workspace.id, connection.id, conversationId, thread ? { thread } : {});
  // Pages come newest first; a conversation reads oldest first, older pages above.
  const items = [...(messages.data?.pages.flatMap((page) => page.items) ?? [])].reverse();
  const paused = conversation?.paused_until && Date.parse(conversation.paused_until) > Date.now();
  const title = conversation ? conversationName(conversation) : "Conversation";
  const back = () => onOpen(thread ? conversationId : null);

  return (
    <Panel className="flex min-h-64 flex-col overflow-hidden">
      <header className="flex flex-wrap items-center gap-2 border-b px-3 py-2.5 sm:px-4">
        <Button
          variant="ghost"
          size="icon-sm"
          aria-label={thread ? "Back to the conversation" : "Back to conversations"}
          className={cn(!thread && "lg:hidden")}
          onClick={back}
        >
          <ArrowLeftIcon />
        </Button>
        <div className="min-w-0 flex-1">
          <h3 className="truncate text-sm font-semibold">{thread ? `Thread in ${title}` : title}</h3>
          <p className="truncate font-mono text-xs text-muted-foreground">
            {conversation?.external_id ?? shortId(conversationId)}
          </p>
        </div>
        {conversation?.is_external ? <StatusPill tone="info">Slack Connect</StatusPill> : null}
        {conversation && !conversation.identity_is_member ? (
          <StatusPill tone="neutral">Agent not a member</StatusPill>
        ) : null}
      </header>
      {paused && conversation?.paused_until ? (
        <p className="flex items-start gap-2 border-b bg-warning-soft/50 px-4 py-2 text-sm">
          <PauseCircleIcon className="mt-0.5 size-4 shrink-0 text-warning" />
          <span>
            Too many runs here in a short time, so agents are paused in this conversation until{" "}
            <span title={fullDateTime(conversation.paused_until)}>{dateTime(conversation.paused_until)}</span>. Your
            organization sets the limit in its channel settings.
          </span>
        </p>
      ) : null}
      {messages.hasNextPage ? (
        <div className="flex justify-center border-b px-4 py-2">
          <Button
            variant="ghost"
            size="sm"
            onClick={() => messages.fetchNextPage()}
            disabled={messages.isFetchingNextPage}
          >
            {messages.isFetchingNextPage ? <Spinner /> : null}
            Show older messages
          </Button>
        </div>
      ) : null}
      {messages.isError ? (
        <QueryError error={messages.error} onRetry={() => messages.refetch()} what="messages" className="m-3" />
      ) : messages.isPending ? (
        <div className="space-y-5 p-4" role="status" aria-busy aria-label="Loading">
          {[70, 45, 60].map((w) => (
            <div key={w} className="flex gap-3">
              <Skeleton className="size-7 shrink-0 rounded-full" />
              <div className="flex-1 space-y-2">
                <Skeleton className="h-3 w-32" />
                <Skeleton className="h-3.5" style={{ width: `${w}%` }} />
              </div>
            </div>
          ))}
        </div>
      ) : items.length === 0 ? (
        <p className="px-4 py-10 text-center text-sm text-muted-foreground">
          No messages kept. Messages are kept for your organization's retention period.
        </p>
      ) : (
        <ol className="divide-y">
          {items.map((message) => (
            <MessageItem
              key={message.id}
              org={org}
              workspace={workspace}
              connection={connection}
              message={message}
              inThread={Boolean(thread)}
              onThread={() => onOpen(conversationId, message.thread_key)}
            />
          ))}
        </ol>
      )}
    </Panel>
  );
}

function MessageItem({
  org,
  workspace,
  connection,
  message,
  inThread,
  onThread,
}: {
  org: Organization;
  workspace: Workspace;
  connection: ChannelConnection;
  message: ChannelMessage;
  inThread: boolean;
  onThread: () => void;
}) {
  const outbound = message.direction === "outbound";
  const name = outbound
    ? connection.label
    : (message.author?.display_name ?? message.author?.handle ?? "Someone in the team");
  const isReply = message.thread_key !== "" && message.thread_key !== message.external_id;
  return (
    <li className={cn("flex gap-3 px-3 py-3 sm:px-4", outbound && "bg-brand-soft/30")}>
      <span
        aria-hidden
        className={cn(
          "grid size-7 shrink-0 place-items-center rounded-full text-xs font-semibold",
          outbound ? "bg-brand text-white dark:text-[#10191D]" : "bg-muted text-muted-foreground",
        )}
      >
        {outbound ? <MessageSquareTextIcon className="size-3.5" /> : name.slice(0, 1).toUpperCase()}
      </span>
      <div className="min-w-0 flex-1">
        <p className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
          <span className="text-sm font-medium">{name}</span>
          {outbound ? <StatusPill tone="info">Agent</StatusPill> : null}
          {message.author?.is_automated ? <StatusPill tone="neutral">Automated</StatusPill> : null}
          <time
            dateTime={message.sent_at}
            title={fullDateTime(message.sent_at)}
            className="text-xs text-muted-foreground"
          >
            {dateTime(message.sent_at)}
          </time>
          {message.edited_at && !message.deleted_at ? (
            <span className="text-xs text-muted-foreground">(edited)</span>
          ) : null}
        </p>
        {!inThread && isReply ? <p className="text-xs text-muted-foreground">Replied in a thread</p> : null}
        {message.deleted_at ? (
          <p className="mt-0.5 text-sm text-muted-foreground italic">This message was deleted.</p>
        ) : (
          <p className="mt-0.5 text-sm leading-relaxed break-words whitespace-pre-wrap">
            {renderMentions(message.text, connection)}
          </p>
        )}
        <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
          {message.run_id ? (
            <Link
              to="/$org/w/$ws/runs/$runId"
              params={{ org: org.slug, ws: workspace.id, runId: message.run_id }}
              className="text-brand-ink underline-offset-4 hover:underline"
            >
              {message.tool_call_id ? "Sent by a tool call in" : "Answer of"} run{" "}
              <span className="font-mono">{shortId(message.run_id)}</span>
            </Link>
          ) : null}
          {message.agent_hop > 0 ? (
            <span className="text-muted-foreground">
              {message.agent_hop} agent {message.agent_hop === 1 ? "hop" : "hops"}
            </span>
          ) : null}
          {!inThread && message.thread_key !== "" ? (
            <button
              type="button"
              onClick={onThread}
              className="text-muted-foreground underline-offset-4 hover:text-foreground hover:underline"
            >
              View thread
            </button>
          ) : null}
        </div>
      </div>
    </li>
  );
}
