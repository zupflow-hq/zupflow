import type { ChannelConnection, ChannelMessage, OrganizationId, Run, WorkspaceId } from "@zupflow/sdk";
import { useMemo } from "react";
import { useChannelConnection, useChannelMessages, useChannelSettings } from "@/features/queries";

/**
 * What a channel run's timeline shows besides its events: the connection it
 * came through and the messages of its thread, so the message that started it
 * and the ones that joined it read as they were written. Null for any other run.
 *
 * Messages load a page at a time, newest first: one the timeline names but
 * hasn't loaded may be further back (`more`), and only one older than the
 * organization's retention period is known to be gone.
 */
export interface RunChannelContext {
  connection: ChannelConnection | undefined;
  /** The thread's messages loaded so far, by ID. */
  messages: ReadonlyMap<string, ChannelMessage>;
  loading: boolean;
  /** Why the messages couldn't be read; null when they could. */
  error: unknown;
  retry: () => void;
  /** Whether older messages remain to be loaded. */
  more: boolean;
  loadMore: () => void;
  loadingMore: boolean;
  /** Messages that arrived before this instant are past retention; null while it isn't known. */
  retainedSince: number | null;
}

/** A page of the thread: a thread longer than this loads further back on request. */
const THREAD_PAGE = 100;
const DAY = 86_400_000;

export function useRunChannelContext(
  organizationId: OrganizationId,
  workspaceId: WorkspaceId,
  run: Run | undefined,
): RunChannelContext | null {
  const channel = run?.channel ?? null;
  const connection = useChannelConnection(workspaceId, channel?.connection_id);
  const settings = useChannelSettings(organizationId);
  const messages = useChannelMessages(
    workspaceId,
    channel?.connection_id,
    channel?.conversation_id,
    // A DM's top level has no thread; its messages come from the conversation.
    channel?.thread_key ? { thread: channel.thread_key, limit: THREAD_PAGE } : { limit: THREAD_PAGE },
  );
  const byId = useMemo(
    () => new Map((messages.data?.pages.flatMap((page) => page.items) ?? []).map((m) => [m.id, m])),
    [messages.data],
  );
  if (!channel) return null;
  const retentionDays = settings.data?.message_retention_days;
  return {
    connection: connection.data,
    messages: byId,
    loading: messages.isPending && !messages.isError,
    error: messages.error,
    retry: () => void messages.refetch(),
    more: messages.hasNextPage,
    loadMore: () => void messages.fetchNextPage(),
    loadingMore: messages.isFetchingNextPage,
    retainedSince: retentionDays ? Date.now() - retentionDays * DAY : null,
  };
}
