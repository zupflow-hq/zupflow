import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import {
  type AgentChannel,
  agentChannelDefaults,
  CHANNEL_TOOLS,
  type ChannelAppManager,
  type ChannelConnection,
  type ChannelProviderSettings,
  type ChannelToolLevel,
  type Organization,
  type Workspace,
  ZupflowApiError,
} from "@zupflow/sdk";
import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { newId } from "@/mocks/ids";
import { AgentChannelSettings } from "./agent-channel-settings";
import { ChannelProvidersSection, NoProviderReason } from "./channel-providers";
import { noProviderReason } from "./labels";
import { LinkTeamDialog } from "./link-team-dialog";

const { api } = vi.hoisted(() => ({
  api: {
    channelAppManagers: { create: vi.fn(), update: vi.fn() },
    agentChannels: { put: vi.fn() },
    channelSettings: { toolLevels: vi.fn(), providers: vi.fn(), setProviders: vi.fn() },
    channelConnections: { conversations: vi.fn() },
  },
}));
vi.mock("@/lib/api", () => ({ api }));
vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn() } }));

// Radix radio cards measure themselves; jsdom has no ResizeObserver.
globalThis.ResizeObserver ??= class {
  observe() {}
  unobserve() {}
  disconnect() {}
};

const at = "2026-10-13T12:00:00.000Z";
const org: Organization = {
  id: newId("organization"),
  name: "Fernhill",
  slug: "fernhill",
  role: "developer",
  created_at: at,
  setup: {
    has_app: true,
    has_ready_configuration: true,
    has_workspace: true,
    has_api_key: true,
    has_installation: true,
    has_agent: true,
    has_run: true,
  },
};
const workspace: Workspace = {
  id: newId("workspace"),
  organization_id: org.id,
  name: "Harbor (test)",
  slug: "harbor-test",
  environment: "sandbox",
  external_id: null,
  created_at: at,
  stats: { agents: 1, installations: 0, runs_last_24h: 0 },
};
const agentId = newId("agent");
const connection: ChannelConnection = {
  id: newId("channel_connection"),
  workspace_id: workspace.id,
  provider: "slack",
  app_id: newId("channel_app"),
  agent_id: agentId,
  short_id: "inst_7kq2",
  label: "Support agent",
  handler: "mention",
  status: "connected",
  team: { id: "T0HARB0R9Z", name: "Harbor", enterprise_id: null },
  identity: { external_id: "U0AGENT01", handle: "support-agent" },
  granted_scopes: [],
  connected_at: at,
  last_event_at: at,
  created_at: at,
  updated_at: at,
};
const channel: AgentChannel = {
  id: newId("agent_channel"),
  workspace_id: workspace.id,
  agent_id: agentId,
  connection_id: connection.id,
  ...agentChannelDefaults(),
  tools: ["read_thread", "reply"],
  approval_required: [],
  created_at: at,
  updated_at: at,
};
const levels = (overrides: Record<string, ChannelToolLevel["level"]>): ChannelToolLevel[] =>
  Object.entries(CHANNEL_TOOLS).map(([tool, spec]) => ({
    provider: "slack",
    tool,
    level: overrides[tool] ?? spec.level,
    confirmed: tool in overrides,
    effect: spec.effect,
    updated_at: null,
    updated_by: null,
  }));
const manager: ChannelAppManager = {
  id: newId("channel_app_manager"),
  workspace_id: workspace.id,
  provider: "slack",
  method: "configuration_token",
  team: { id: "T0HARB0R9Z", name: "Harbor" },
  status: "active",
  configuration_token: { set: true, last4: "q7Zk", updated_at: at },
  token_expires_at: null,
  app_count: 0,
  created_by: null,
  created_at: at,
  updated_at: at,
};

// Deliberately not usable tokens; assertions report booleans rather than values.
const configurationToken = "xoxe.xoxp-fixture-only-3b1d";
const refreshToken = "xoxe-fixture-refresh-only-77a0";

const clients: QueryClient[] = [];
function client() {
  const c = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity }, mutations: { retry: false } },
  });
  clients.push(c);
  return c;
}
const cachesHold = (c: QueryClient, text: string) =>
  JSON.stringify([
    ...c
      .getQueryCache()
      .getAll()
      .map((q) => q.state.data),
    ...c
      .getMutationCache()
      .getAll()
      .map((m) => [m.state.data, m.state.variables]),
  ]).includes(text);

beforeEach(() => vi.resetAllMocks());
afterEach(() => {
  cleanup();
  for (const c of clients.splice(0)) c.clear();
});

describe("linking a Slack team", () => {
  function Harness() {
    const [open, setOpen] = useState(true);
    return (
      <>
        <button type="button" onClick={() => setOpen(true)}>
          Reopen
        </button>
        <LinkTeamDialog workspaceId={workspace.id} provider="slack" open={open} onOpenChange={setOpen} />
      </>
    );
  }
  const paste = async (dialog: HTMLElement) => {
    await userEvent.type(within(dialog).getByLabelText("Configuration token"), configurationToken);
    await userEvent.type(within(dialog).getByLabelText("Refresh token"), refreshToken);
    await userEvent.click(within(dialog).getByRole("button", { name: "Link team" }));
  };

  it("asks for both tokens before sending anything", async () => {
    render(
      <QueryClientProvider client={client()}>
        <Harness />
      </QueryClientProvider>,
    );
    const dialog = await screen.findByRole("dialog", { name: "Link a Slack team" });
    await userEvent.click(within(dialog).getByRole("button", { name: "Link team" }));
    expect(within(dialog).getByText("Paste the configuration token.")).toBeVisible();
    expect(within(dialog).getByText("Paste the refresh token that came with it.")).toBeVisible();
    expect(api.channelAppManagers.create).not.toHaveBeenCalled();
  });

  it("sends the tokens once and keeps them nowhere afterwards", async () => {
    const c = client();
    api.channelAppManagers.create.mockResolvedValue(manager);
    render(
      <QueryClientProvider client={c}>
        <Harness />
      </QueryClientProvider>,
    );
    await paste(await screen.findByRole("dialog", { name: "Link a Slack team" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(api.channelAppManagers.create).toHaveBeenCalledTimes(1);
    const [wsId, sent] = api.channelAppManagers.create.mock.calls[0] ?? [];
    expect(wsId).toBe(workspace.id);
    expect(sent.provider).toBe("slack");
    expect(sent.method).toBe("configuration_token");
    expect(sent.configuration_token === configurationToken && sent.refresh_token === refreshToken).toBe(true);
    expect(cachesHold(c, configurationToken) || cachesHold(c, refreshToken)).toBe(false);
    await userEvent.click(screen.getByRole("button", { name: "Reopen" }));
    expect(await screen.findByLabelText("Configuration token")).toHaveValue("");
  });

  it("puts a refused token's error on its field", async () => {
    api.channelAppManagers.create.mockRejectedValue(
      new ZupflowApiError(422, {
        code: "invalid_configuration_token",
        message: "Slack refused this configuration token.",
        fields: { configuration_token: "Slack refused this token. Generate a new one and paste it again." },
      }),
    );
    render(
      <QueryClientProvider client={client()}>
        <Harness />
      </QueryClientProvider>,
    );
    const dialog = await screen.findByRole("dialog", { name: "Link a Slack team" });
    await paste(dialog);
    expect(
      await within(dialog).findByText("Slack refused this token. Generate a new one and paste it again."),
    ).toBeVisible();
    expect(within(dialog).getByLabelText("Configuration token")).toHaveAttribute("aria-invalid", "true");
  });

  it("says so when this server can't reach the provider", async () => {
    const message =
      "This deployment can't reach Slack, so it can't link teams there. Ask its operator to set ZUPFLOW_PUBLIC_URL to the https:// address Slack reaches it at.";
    api.channelAppManagers.create.mockRejectedValue(
      new ZupflowApiError(501, { code: "channel_providers_not_configured", message, fields: null }),
    );
    render(
      <QueryClientProvider client={client()}>
        <Harness />
      </QueryClientProvider>,
    );
    const dialog = await screen.findByRole("dialog", { name: "Link a Slack team" });
    await paste(dialog);
    expect(await within(dialog).findByRole("alert")).toHaveTextContent(message);
  });
});

describe("an agent's channel settings", () => {
  const renderSettings = (readOnly = false) =>
    render(
      <QueryClientProvider client={client()}>
        <AgentChannelSettings
          org={org}
          workspace={workspace}
          connection={connection}
          channel={channel}
          readOnly={readOnly}
        />
      </QueryClientProvider>,
    );

  it("offers tools within the organization's levels", async () => {
    api.channelSettings.toolLevels.mockResolvedValue(levels({ delete_message: "deny", send_message: "approve" }));
    renderSettings();
    const blocked = await screen.findByRole("checkbox", { name: /^Delete its message/ });
    expect(blocked).toBeDisabled();
    expect(within(blocked.closest("li") as HTMLElement).getByText("Blocked")).toBeVisible();
    // A tool the organization requires approval for asks first as soon as it's on, and can't stop asking.
    await userEvent.click(screen.getByRole("checkbox", { name: /^Send a message/ }));
    const ask = screen.getByRole("switch", { name: "Require approval for Send a message" });
    expect(ask).toBeChecked();
    expect(ask).toBeDisabled();
  });

  it("saves every setting, with the organization's approvals kept", async () => {
    api.channelSettings.toolLevels.mockResolvedValue(levels({ send_message: "approve" }));
    api.agentChannels.put.mockImplementation(async (_ws, _agent, _conn, input) => ({ ...channel, ...input }));
    renderSettings();
    await userEvent.click(await screen.findByRole("checkbox", { name: /^Send a message/ }));
    await userEvent.click(screen.getByRole("switch", { name: "Require approval for Reply" }));
    await userEvent.click(screen.getByRole("button", { name: "Save channel settings" }));
    await waitFor(() => expect(api.agentChannels.put).toHaveBeenCalledTimes(1));
    const [wsId, agent, conn, input] = api.agentChannels.put.mock.calls[0] ?? [];
    expect([wsId, agent, conn]).toEqual([workspace.id, agentId, connection.id]);
    expect(input.tools).toEqual(["read_thread", "reply", "send_message"]);
    expect([...input.approval_required].sort()).toEqual(["reply", "send_message"]);
    expect(input.audience_conversations).toEqual([]);
    expect(input.sender_list).toEqual([]);
    expect(input.persona).toBeNull();
  });

  it("checks the people it may talk to before saving", async () => {
    api.channelSettings.toolLevels.mockResolvedValue(levels({}));
    renderSettings();
    await userEvent.click(await screen.findByRole("radio", { name: /Only people you list/ }));
    await userEvent.type(screen.getByRole("textbox", { name: "Email addresses" }), "dana@harbor.example\nnope");
    await userEvent.click(screen.getByRole("button", { name: "Save channel settings" }));
    expect(await screen.findByText("nope isn't an email address.")).toBeVisible();
    expect(api.agentChannels.put).not.toHaveBeenCalled();
  });

  it("changes nothing for someone who can't", async () => {
    api.channelSettings.toolLevels.mockResolvedValue(levels({}));
    renderSettings(true);
    expect(await screen.findByRole("checkbox", { name: /^Reply/ })).toBeDisabled();
    expect(screen.queryByRole("button", { name: "Save channel settings" })).not.toBeInTheDocument();
  });

  it("keeps each team's card to its own controls", async () => {
    api.channelSettings.toolLevels.mockResolvedValue(levels({}));
    const other = { ...connection, id: newId("channel_connection"), team: { ...connection.team, id: "T0OTHER001" } };
    render(
      <QueryClientProvider client={client()}>
        <div data-testid="first">
          <AgentChannelSettings
            org={org}
            workspace={workspace}
            connection={connection}
            channel={channel}
            readOnly={false}
          />
        </div>
        <div data-testid="second">
          <AgentChannelSettings
            org={org}
            workspace={workspace}
            connection={other}
            channel={{ ...channel, id: newId("agent_channel"), connection_id: other.id }}
            readOnly={false}
          />
        </div>
      </QueryClientProvider>,
    );
    const first = within(screen.getByTestId("first"));
    const second = within(screen.getByTestId("second"));
    await second.findAllByRole("checkbox");
    // Clicking the second card's labels changes the second card, never the first.
    await userEvent.click(second.getByText("Answer direct messages"));
    expect(second.getByRole("switch", { name: /Answer direct messages/ })).not.toBeChecked();
    expect(first.getByRole("switch", { name: /Answer direct messages/ })).toBeChecked();
    await userEvent.click(second.getByText("Read messages"));
    expect(second.getByRole("checkbox", { name: /^Read messages/ })).toBeChecked();
    expect(first.getByRole("checkbox", { name: /^Read messages/ })).not.toBeChecked();
    const ids = [...document.querySelectorAll("[id]")].map((el) => el.id);
    expect(new Set(ids).size).toBe(ids.length);
  });
});

describe("the organization's chat providers", () => {
  const admin: Organization = { ...org, role: "admin" };
  const settings = (change: Partial<ChannelProviderSettings> = {}): ChannelProviderSettings => ({
    organization_id: org.id,
    allowed_providers: null,
    providers: [{ provider: "slack", reachable: true, allowed: true }],
    updated_at: null,
    updated_by: null,
    ...change,
  });
  const renderSection = (organization: Organization) =>
    render(
      <QueryClientProvider client={client()}>
        <ChannelProvidersSection org={organization} />
      </QueryClientProvider>,
    );

  it("allows every provider until an admin chooses, and saves the ones chosen", async () => {
    api.channelSettings.providers.mockResolvedValue(settings());
    api.channelSettings.setProviders.mockImplementation(async (_org, input) =>
      settings({
        allowed_providers: input.allowed_providers,
        providers: [{ provider: "slack", reachable: true, allowed: false }],
        updated_at: at,
      }),
    );
    renderSection(admin);
    const slack = await screen.findByRole("checkbox", { name: /^Slack/ });
    expect(screen.getByRole("radio", { name: /Every provider/ })).toBeChecked();
    expect(slack).toBeChecked();
    expect(slack).toBeDisabled();
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();

    await userEvent.click(screen.getByRole("radio", { name: /Only the ones you choose/ }));
    await userEvent.click(screen.getByRole("checkbox", { name: /^Slack/ }));
    expect(screen.getByText("With none chosen, no workspace can link a new team.")).toBeVisible();
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.channelSettings.setProviders).toHaveBeenCalledTimes(1));
    expect(api.channelSettings.setProviders.mock.calls[0]).toEqual([admin.id, { allowed_providers: [] }]);
    expect(await screen.findByRole("radio", { name: /Only the ones you choose/ })).toBeChecked();
    expect(screen.getByRole("checkbox", { name: /^Slack/ })).not.toBeChecked();
  });

  it("says which providers this server can't reach, and changes nothing for someone who can't", async () => {
    api.channelSettings.providers.mockResolvedValue(
      settings({
        allowed_providers: ["slack"],
        providers: [{ provider: "slack", reachable: false, allowed: true }],
        updated_at: at,
      }),
    );
    renderSection(org);
    expect(await screen.findByText("This server can't reach it")).toBeVisible();
    expect(screen.getByText(/^Allowed\. It's offered once the server reaches it\./)).toBeVisible();
    expect(screen.getByRole("checkbox", { name: /^Slack/ })).toBeDisabled();
    expect(screen.queryByRole("button", { name: "Save" })).not.toBeInTheDocument();
    expect(screen.getByText("Only owners and admins can change these.")).toBeVisible();
  });

  it("says an unchecked provider the server can't reach isn't allowed", async () => {
    api.channelSettings.providers.mockResolvedValue(
      settings({
        allowed_providers: [],
        providers: [{ provider: "slack", reachable: false, allowed: false }],
        updated_at: at,
      }),
    );
    renderSection(admin);
    expect(await screen.findByText(/^Not allowed, and the server can't reach it yet either\./)).toBeVisible();
    expect(screen.queryByText(/^Allowed\./)).not.toBeInTheDocument();
    // Checked again, it's allowed in the draft, and said so before it's saved.
    await userEvent.click(screen.getByRole("checkbox", { name: /^Slack/ }));
    expect(screen.getByText(/^Allowed\. It's offered once the server reaches it\./)).toBeVisible();
  });
});

describe("why no team can be linked", () => {
  const none: ChannelProviderSettings = {
    organization_id: org.id,
    allowed_providers: [],
    providers: [{ provider: "slack", reachable: true, allowed: false }],
    updated_at: at,
    updated_by: null,
  };

  it("says the organization allows none, and who can change that, to anyone who reads it", () => {
    for (const role of ["viewer", "developer"] as const) {
      const { container, unmount } = render(<NoProviderReason org={{ ...org, role }} settings={none} linked />);
      expect(container.textContent, role).toBe(
        "Your organization doesn't allow any chat provider, so no new team can be linked here. Teams already linked keep working. An owner or admin can change this in the organization's settings.",
      );
      unmount();
    }
    expect(noProviderReason(none)).toEqual({
      text: "Your organization doesn't allow any chat provider, so no team can be linked here.",
      choice: true,
    });
  });

  it("says the server can't reach the ones allowed, and names the organization only when it refuses one too", () => {
    const unreached = noProviderReason({
      ...none,
      allowed_providers: null,
      providers: [{ provider: "slack", reachable: false, allowed: true }],
    });
    expect(unreached.text).toMatch(/^This Zupflow server can't reach Slack yet, so no team can be linked here\./);
    expect(unreached.choice).toBe(false);
  });
});
