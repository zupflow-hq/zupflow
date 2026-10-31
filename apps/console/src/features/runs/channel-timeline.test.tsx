import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ChannelConnection, ChannelMessage, Run, RunEvent } from "@zupflow/sdk";
import { afterEach, describe, expect, it, vi } from "vitest";
import { newId } from "@/mocks/ids";
import type { RunChannelContext } from "./channel-context";
import { RunTimeline } from "./timeline";
import { inputParts } from "./token-usage";

vi.mock("@tanstack/react-router", async (original) => ({
  ...(await original<typeof import("@tanstack/react-router")>()),
  Link: ({ children, search }: { children: React.ReactNode; search?: { conversation?: string; thread?: string } }) => (
    <a href={`/channels?conversation=${search?.conversation ?? ""}&thread=${search?.thread ?? ""}`}>{children}</a>
  ),
}));

afterEach(cleanup);

const at = (s: number) => new Date(Date.UTC(2026, 9, 13, 12, 0, s)).toISOString();
const workspaceId = newId("workspace");
const runId = newId("run");
const connection: ChannelConnection = {
  id: newId("channel_connection"),
  workspace_id: workspaceId,
  provider: "slack",
  app_id: newId("channel_app"),
  agent_id: newId("agent"),
  short_id: "inst_7kq2",
  label: "Support agent",
  handler: "mention",
  status: "connected",
  team: { id: "T0HARB0R9Z", name: "Harbor", enterprise_id: null },
  identity: { external_id: "U0AGENT01", handle: "support-agent" },
  granted_scopes: [],
  connected_at: at(0),
  last_event_at: at(0),
  created_at: at(0),
  updated_at: at(0),
};
const conversationId = newId("channel_conversation");
const thread = "1760356800.000100";
const message = (text: string, s: number, author = "Rowan Ellis"): ChannelMessage => ({
  id: newId("channel_message"),
  workspace_id: workspaceId,
  connection_id: connection.id,
  conversation_id: conversationId,
  external_id: `17603568${s}.000100`,
  thread_key: thread,
  direction: "inbound",
  author: { id: newId("channel_person"), handle: "rowan", display_name: author, is_automated: false },
  text,
  mentions: [],
  run_id: null,
  tool_call_id: null,
  agent_hop: 0,
  sent_at: at(s),
  edited_at: null,
  deleted_at: null,
  created_at: at(s),
});
const origin = message("<@U0AGENT01> where is order 4521?", 0);
const followUp = message("Actually, 4522 as well.", 3);
const gone = newId("channel_message");

const run: Run = {
  id: runId,
  workspace_id: workspaceId,
  agent_id: connection.agent_id ?? newId("agent"),
  agent_name: "Support agent",
  session_id: newId("session"),
  trigger: "channel",
  channel: {
    connection_id: connection.id,
    conversation_id: conversationId,
    thread_key: thread,
    origin_message_id: origin.id,
    reason: "mentioned",
    silent_reason: null,
  },
  memory: "agent",
  status: "completed",
  input: origin.text,
  output: null,
  usage: {
    input_tokens: 1000,
    output_tokens: 20,
    cache_read_input_tokens: 600,
    cache_write_input_tokens: 150,
    tool_calls: 1,
  },
  error: null,
  created_at: at(1),
  started_at: at(1),
  finished_at: at(9),
};

let sequence = 0;
const event = <T extends RunEvent["type"]>(type: T, s: number, data: Extract<RunEvent, { type: T }>["data"]) =>
  ({
    id: newId("run_event"),
    run_id: runId,
    workspace_id: workspaceId,
    sequence: ++sequence,
    created_at: at(s),
    type,
    data,
  }) as RunEvent;
const received = (outcome: "started" | "steered" | "collected", ids: string[], s: number) =>
  event("channel_messages.received", s, {
    outcome,
    connection_id: connection.id,
    conversation_id: conversationId,
    message_ids: ids as ChannelMessage["id"][],
  });
const call = newId("tool_call");
const ref = {
  call_id: call,
  channel_connection_id: connection.id,
  connection_label: connection.label,
  provider: "slack" as const,
  tool: "reply",
};

const context: RunChannelContext = {
  connection,
  messages: new Map([origin, followUp].map((m) => [m.id, m])),
  loading: false,
  error: null,
  retry: vi.fn(),
  more: false,
  loadMore: vi.fn(),
  loadingMore: false,
  // Retention reaches back to before these messages: none is known to be gone.
  retainedSince: Date.parse(at(0)) - 1000,
};

describe("a channel run's timeline", () => {
  it("opens with the message that woke it, as written, and links its thread", () => {
    render(<RunTimeline run={run} events={[received("started", [origin.id], 1)]} channel={context} orgSlug="harbor" />);
    const timeline = screen.getByRole("list", { name: "Run timeline" });
    expect(within(timeline).getByText("@support-agent where is order 4521?")).toBeVisible();
    expect(within(timeline).getByText("Mentioned in a conversation · Slack · Support agent")).toBeVisible();
    expect(within(timeline).getByRole("link", { name: "Open thread" })).toHaveAttribute(
      "href",
      `/channels?conversation=${conversationId}&thread=${thread}`,
    );
    // The origin isn't quoted twice.
    expect(within(timeline).getAllByText("@support-agent where is order 4521?")).toHaveLength(1);
  });

  const steered = (channel: RunChannelContext) =>
    render(
      <RunTimeline
        run={run}
        events={[received("started", [origin.id], 1), received("steered", [followUp.id, gone], 4)]}
        channel={channel}
        orgSlug="harbor"
      />,
    );

  it("shows messages that joined mid-run, and links the thread for one it hasn't loaded", () => {
    steered(context);
    expect(screen.getByText("2 new messages joined while it worked")).toBeVisible();
    expect(screen.getByText("Actually, 4522 as well.")).toBeVisible();
    expect(screen.getByText(/One message isn't shown here/)).toBeVisible();
    expect(screen.getByRole("link", { name: "Open the thread" })).toBeVisible();
    // Nothing says retention removed it: that isn't known.
    expect(screen.queryByText(/retention/)).not.toBeInTheDocument();
  });

  it("loads further back for a message past the loaded page", async () => {
    const loadMore = vi.fn();
    steered({ ...context, more: true, loadMore });
    expect(screen.getByText(/One message is further back in the thread/)).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Load older messages" }));
    expect(loadMore).toHaveBeenCalledTimes(1);
    expect(screen.queryByText(/retention/)).not.toBeInTheDocument();
  });

  it("says retention removed a message only once it's past retention and everything is loaded", () => {
    steered({ ...context, retainedSince: Date.parse(at(30)) });
    expect(screen.getByText(/One message isn't kept any more: your organization's retention period/)).toBeVisible();
  });

  it("says when the messages couldn't be read, and retries", async () => {
    const retry = vi.fn();
    steered({ ...context, messages: new Map(), error: new Error("offline"), retry });
    expect(screen.getAllByRole("alert")[0]).toHaveTextContent("Couldn't load these messages.");
    await userEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(retry).toHaveBeenCalledTimes(1);
    expect(screen.queryByText(/retention/)).not.toBeInTheDocument();
  });

  it("says when a run answers messages collected while the last one worked", () => {
    const collected: Run = {
      ...run,
      channel: run.channel && { ...run.channel, origin_message_id: followUp.id, reason: "thread_follow_up" },
    };
    render(<RunTimeline run={collected} events={[received("collected", [followUp.id], 1)]} channel={context} />);
    expect(screen.getByText("Waited for the run before this one")).toBeVisible();
  });

  it("shows a channel send with where it went, and a silent run's reason", () => {
    render(
      <RunTimeline
        run={run}
        events={[
          event("tool_call.started", 5, { ...ref, arguments: { text: "On its way." } }),
          event("tool_call.completed", 6, { ...ref, result: {}, duration_ms: 120 }),
          event("run.stayed_silent", 7, { reason: "Nothing left to answer." }),
        ]}
        channel={context}
        orgSlug="harbor"
      />,
    );
    expect(screen.getByText(/in the thread that woke it/)).toBeVisible();
    expect(screen.getByText("Stayed silent")).toBeVisible();
    expect(screen.getByText(/Its reason: Nothing left to answer\./)).toBeVisible();
  });

  it("falls back to the input once the message is gone", () => {
    render(<RunTimeline run={run} events={[]} channel={{ ...context, messages: new Map() }} />);
    expect(screen.getByText(origin.text)).toBeVisible();
  });
});

describe("input tokens", () => {
  it("splits cached and written out of the input, never adding them to it", () => {
    expect(inputParts(run.usage)).toEqual({ cached: 600, written: 150, fresh: 250, total: 1000, cachedShare: 0.6 });
  });
});
