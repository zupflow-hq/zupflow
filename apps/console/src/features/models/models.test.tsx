import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import {
  type DefaultModel,
  type Environment,
  type Model,
  type ModelConnection,
  type Organization,
  ZupflowApiError,
} from "@zupflow/sdk";
import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Form } from "@/components/ui/form";
import { AgentFields, agentInput, useAgentForm } from "@/features/agents/agent-form";
import { newId } from "@/mocks/ids";
import { AddConnectionDialog } from "./connection-dialogs";
import { DefaultModelPanel } from "./default-model";

const { api, scope } = vi.hoisted(() => ({
  api: {
    modelConnections: { create: vi.fn() },
    models: { list: vi.fn(), get: vi.fn(), getDefault: vi.fn() },
  },
  scope: {} as { org: Organization },
}));
vi.mock("@/lib/api", () => ({ api }));
vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn() } }));
vi.mock("@/features/context", async (original) => ({
  ...(await original<typeof import("@/features/context")>()),
  useOrg: () => scope.org,
}));
vi.mock("@tanstack/react-router", async (original) => ({
  ...(await original<typeof import("@tanstack/react-router")>()),
  Link: ({ children, search }: { children: React.ReactNode; search?: { env?: Environment } }) => (
    <a href={search?.env === "sandbox" ? "/fernhill/sandbox/model-providers" : "/fernhill/model-providers"}>
      {children}
    </a>
  ),
}));

const at = "2026-10-13T12:00:00.000Z";
const org: Organization = {
  id: newId("organization"),
  name: "Fernhill",
  slug: "fernhill",
  role: "admin",
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
// Deliberately not a usable key; assertions report booleans rather than values.
const providerKey = "provider-key-fixture-only-9f3c";
const connection: ModelConnection = {
  id: newId("model_connection"),
  organization_id: org.id,
  environment: "sandbox",
  kind: "openrouter",
  label: "OpenRouter",
  api_key: { set: true, last4: "9f3c", updated_at: at },
  key_version: 1,
  status: "active",
  model_count: 0,
  created_at: at,
  updated_at: at,
};
const model = (name: string, kind: ModelConnection["kind"], label: string): Model => ({
  id: newId("model"),
  organization_id: org.id,
  environment: "sandbox",
  route: { connection_id: newId("model_connection"), kind, label },
  provider_model_id: name.toLowerCase().replaceAll(" ", "-"),
  name,
  author: "openai",
  context_window: 400_000,
  supports_tools: true,
  status: "available",
  is_default: false,
  agent_count: 0,
  created_at: at,
  updated_at: at,
});
const page = (items: Model[]) => ({ items, pagination: { next_cursor: null, total: items.length } });
const noDefault: DefaultModel = { environment: "sandbox", model: null, updated_at: null, updated_by: null };

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

// cmdk scrolls the active option into view; jsdom has no layout.
Element.prototype.scrollIntoView ??= () => {};
// Radix radio cards measure themselves; jsdom has no ResizeObserver.
globalThis.ResizeObserver ??= class {
  observe() {}
  unobserve() {}
  disconnect() {}
};

beforeEach(() => {
  vi.resetAllMocks();
  scope.org = org;
});
afterEach(() => {
  cleanup();
  for (const c of clients.splice(0)) c.clear();
});

describe("connecting a provider", () => {
  function Harness({ onCreated }: { onCreated: (c: ModelConnection) => void }) {
    const [open, setOpen] = useState(true);
    return (
      <>
        <button type="button" onClick={() => setOpen(true)}>
          Reopen
        </button>
        <AddConnectionDialog org={org} environment="sandbox" open={open} onOpenChange={setOpen} onCreated={onCreated} />
      </>
    );
  }

  it("sends the key once, for this environment, and keeps it nowhere afterwards", async () => {
    const c = client();
    const onCreated = vi.fn();
    api.modelConnections.create.mockResolvedValue(connection);
    render(
      <QueryClientProvider client={c}>
        <Harness onCreated={onCreated} />
      </QueryClientProvider>,
    );
    const dialog = await screen.findByRole("dialog", { name: "Connect a model provider" });
    await userEvent.type(within(dialog).getByLabelText("API key"), providerKey);
    await userEvent.click(within(dialog).getByRole("button", { name: "Connect" }));
    await waitFor(() => expect(onCreated).toHaveBeenCalledTimes(1));
    expect(api.modelConnections.create.mock.calls[0]?.[0]).toBe(org.id);
    const sent = api.modelConnections.create.mock.calls[0]?.[1];
    expect(sent.environment).toBe("sandbox");
    expect(sent.kind).toBe("openrouter");
    expect(sent.api_key === providerKey).toBe(true);
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(cachesHold(c, providerKey)).toBe(false);
    await userEvent.click(screen.getByRole("button", { name: "Reopen" }));
    expect(await screen.findByLabelText("API key")).toHaveValue("");
  });

  it("puts a refused key's error on the key field and saves nothing", async () => {
    const c = client();
    api.modelConnections.create.mockRejectedValue(
      new ZupflowApiError(422, {
        code: "invalid_api_key",
        message: "OpenRouter refused this key.",
        fields: { api_key: "OpenRouter refused this key. Check it and paste it again." },
      }),
    );
    const onCreated = vi.fn();
    render(
      <QueryClientProvider client={c}>
        <Harness onCreated={onCreated} />
      </QueryClientProvider>,
    );
    const dialog = await screen.findByRole("dialog", { name: "Connect a model provider" });
    await userEvent.type(within(dialog).getByLabelText("API key"), providerKey);
    await userEvent.click(within(dialog).getByRole("button", { name: "Connect" }));
    expect(await within(dialog).findByText("OpenRouter refused this key. Check it and paste it again.")).toBeVisible();
    expect(onCreated).not.toHaveBeenCalled();
  });
});

describe("the agent form's model", () => {
  function Fields() {
    const form = useAgentForm();
    return (
      <Form {...form}>
        <form>
          <AgentFields form={form} environment="sandbox" />
        </form>
        <output data-testid="input">{JSON.stringify(agentInput(form.watch()))}</output>
      </Form>
    );
  }
  const renderFields = () =>
    render(
      <QueryClientProvider client={client()}>
        <Fields />
      </QueryClientProvider>,
    );

  it("starts on the organization default, named, and assumes no model", async () => {
    const sonnet = { ...model("Claude Sonnet 4.5", "openrouter", "OpenRouter"), is_default: true };
    api.models.list.mockResolvedValue(page([sonnet, model("GPT-5", "openai", "OpenAI")]));
    api.models.getDefault.mockResolvedValue({ ...noDefault, model: sonnet });
    renderFields();
    expect(await screen.findByText("(Claude Sonnet 4.5)")).toBeVisible();
    // Only enough to know whether models exist: the picker searches the rest as people type.
    expect(api.models.list).toHaveBeenCalledWith(org.id, { environment: "sandbox", limit: 1 });
    expect(api.models.getDefault).toHaveBeenCalledWith(org.id, "sandbox");
    expect(JSON.parse(screen.getByTestId("input").textContent ?? "{}").model_id).toBeNull();
  });

  it("explains how to add a model when the environment has none", async () => {
    api.models.list.mockResolvedValue(page([]));
    api.models.getDefault.mockResolvedValue(noDefault);
    renderFields();
    expect(await screen.findByText(/Fernhill has no models in Sandbox yet/)).toBeVisible();
    expect(screen.getByRole("link", { name: "Open model providers" })).toHaveAttribute(
      "href",
      "/fernhill/sandbox/model-providers",
    );
    expect(screen.getByRole("combobox")).toBeDisabled();
  });

  it("warns when models exist but none is the default", async () => {
    api.models.list.mockResolvedValue(page([model("GPT-5", "openai", "OpenAI")]));
    api.models.getDefault.mockResolvedValue(noDefault);
    renderFields();
    expect(await screen.findByText("(none chosen)")).toBeVisible();
    expect(screen.getByText(/Sandbox has no default model, and this agent doesn't choose one/)).toBeVisible();
  });
});

describe("model pickers", () => {
  it("search the environment's models on the server as people type", async () => {
    const gpt = model("GPT-5", "openai", "OpenAI");
    api.models.list.mockResolvedValue(page([gpt, model("Claude Haiku 4.5", "anthropic", "Anthropic")]));
    api.models.getDefault.mockResolvedValue(noDefault);
    render(
      <QueryClientProvider client={client()}>
        <DefaultModelPanel org={org} environment="sandbox" />
      </QueryClientProvider>,
    );
    await userEvent.click(await screen.findByRole("combobox", { name: "Default model" }));
    expect(await screen.findByText("GPT-5")).toBeVisible();
    expect(api.models.list).toHaveBeenCalledWith(org.id, { environment: "sandbox", limit: 20 });
    await userEvent.type(screen.getByPlaceholderText("Search name, model ID or author"), "gpt");
    await waitFor(() =>
      expect(api.models.list).toHaveBeenCalledWith(org.id, { environment: "sandbox", q: "gpt", limit: 20 }),
    );
    expect(api.models.list.mock.calls.every(([, query]) => query.limit <= 20)).toBe(true);
  });

  it("show a default chosen beyond the first matches", async () => {
    const far = { ...model("Zephyr 9", "openrouter", "OpenRouter"), is_default: true };
    api.models.list.mockResolvedValue(page([model("GPT-5", "openai", "OpenAI")]));
    api.models.getDefault.mockResolvedValue({ ...noDefault, model: far });
    render(
      <QueryClientProvider client={client()}>
        <DefaultModelPanel org={org} environment="sandbox" />
      </QueryClientProvider>,
    );
    const trigger = await screen.findByRole("combobox", { name: "Default model" });
    expect(trigger).toHaveTextContent("Zephyr 9");
    expect(trigger).toHaveTextContent("via OpenRouter");
  });
});
